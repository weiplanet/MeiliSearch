use std::cmp::min;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;
use std::time::Instant;

use either::Either;
use indexmap::IndexMap;
use milli::tokenizer::{Analyzer, AnalyzerConfig, Token};
use milli::{AscDesc, FieldId, FieldsIdsMap, Filter, MatchingWords, SortError};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::index::error::FacetError;

use super::error::{IndexError, Result};
use super::index::Index;

pub type Document = IndexMap<String, Value>;
type MatchesInfo = BTreeMap<String, Vec<MatchInfo>>;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct MatchInfo {
    start: usize,
    length: usize,
}

pub const DEFAULT_SEARCH_LIMIT: usize = 20;
const fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

pub const DEFAULT_CROP_LENGTH: usize = 10;
pub const fn default_crop_length() -> usize {
    DEFAULT_CROP_LENGTH
}

const DEFAULT_CROP_MARKER: &str = "…";
pub fn default_crop_marker() -> String {
    DEFAULT_CROP_MARKER.to_string()
}

const DEFAULT_HIGHLIGHT_PRE_TAG: &str = "<em>";
pub fn default_highlight_pre_tag() -> String {
    DEFAULT_HIGHLIGHT_PRE_TAG.to_string()
}

const DEFAULT_HIGHLIGHT_POST_TAG: &str = "</em>";
pub fn default_highlight_post_tag() -> String {
    DEFAULT_HIGHLIGHT_POST_TAG.to_string()
}

/// The maximimum number of results that the engine
/// will be able to return in one search call.
pub const HARD_RESULT_LIMIT: usize = 1000;

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub offset: Option<usize>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    pub attributes_to_retrieve: Option<BTreeSet<String>>,
    pub attributes_to_crop: Option<Vec<String>>,
    #[serde(default = "default_crop_length")]
    pub crop_length: usize,
    pub attributes_to_highlight: Option<HashSet<String>>,
    // Default to false
    #[serde(default = "Default::default")]
    pub matches: bool,
    pub filter: Option<Value>,
    pub sort: Option<Vec<String>>,
    pub facets_distribution: Option<Vec<String>>,
    #[serde(default = "default_highlight_pre_tag")]
    pub highlight_pre_tag: String,
    #[serde(default = "default_highlight_post_tag")]
    pub highlight_post_tag: String,
    #[serde(default = "default_crop_marker")]
    pub crop_marker: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    #[serde(flatten)]
    pub document: Document,
    #[serde(rename = "_formatted", skip_serializing_if = "Document::is_empty")]
    pub formatted: Document,
    #[serde(rename = "_matchesInfo", skip_serializing_if = "Option::is_none")]
    pub matches_info: Option<MatchesInfo>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub nb_hits: u64,
    pub exhaustive_nb_hits: bool,
    pub query: String,
    pub limit: usize,
    pub offset: usize,
    pub processing_time_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets_distribution: Option<BTreeMap<String, BTreeMap<String, u64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exhaustive_facets_count: Option<bool>,
}

#[derive(Copy, Clone, Default)]
struct FormatOptions {
    highlight: bool,
    crop: Option<usize>,
}

impl FormatOptions {
    pub fn merge(self, other: Self) -> Self {
        Self {
            highlight: self.highlight || other.highlight,
            crop: self.crop.or(other.crop),
        }
    }
}

impl Index {
    pub fn perform_search(&self, query: SearchQuery) -> Result<SearchResult> {
        let before_search = Instant::now();
        let rtxn = self.read_txn()?;

        let mut search = self.search(&rtxn);

        if let Some(ref query) = query.q {
            search.query(query);
        }

        // Make sure that a user can't get more documents than the hard limit,
        // we align that on the offset too.
        let offset = min(query.offset.unwrap_or(0), HARD_RESULT_LIMIT);
        let limit = min(query.limit, HARD_RESULT_LIMIT.saturating_sub(offset));

        search.offset(offset);
        search.limit(limit);

        if let Some(ref filter) = query.filter {
            if let Some(facets) = parse_filter(filter)? {
                search.filter(facets);
            }
        }

        if let Some(ref sort) = query.sort {
            let sort = match sort.iter().map(|s| AscDesc::from_str(s)).collect() {
                Ok(sorts) => sorts,
                Err(asc_desc_error) => {
                    return Err(IndexError::Milli(SortError::from(asc_desc_error).into()))
                }
            };

            search.sort_criteria(sort);
        }

        let milli::SearchResult {
            documents_ids,
            matching_words,
            candidates,
            ..
        } = search.execute()?;

        let fields_ids_map = self.fields_ids_map(&rtxn).unwrap();

        let displayed_ids = self
            .displayed_fields_ids(&rtxn)?
            .map(|fields| fields.into_iter().collect::<BTreeSet<_>>())
            .unwrap_or_else(|| fields_ids_map.iter().map(|(id, _)| id).collect());

        let fids = |attrs: &BTreeSet<String>| {
            let mut ids = BTreeSet::new();
            for attr in attrs {
                if attr == "*" {
                    ids = displayed_ids.clone();
                    break;
                }

                if let Some(id) = fields_ids_map.id(attr) {
                    ids.insert(id);
                }
            }
            ids
        };

        // The attributes to retrieve are the ones explicitly marked as to retrieve (all by default),
        // but these attributes must be also be present
        // - in the fields_ids_map
        // - in the the displayed attributes
        let to_retrieve_ids: BTreeSet<_> = query
            .attributes_to_retrieve
            .as_ref()
            .map(fids)
            .unwrap_or_else(|| displayed_ids.clone())
            .intersection(&displayed_ids)
            .cloned()
            .collect();

        let attr_to_highlight = query.attributes_to_highlight.unwrap_or_default();

        let attr_to_crop = query.attributes_to_crop.unwrap_or_default();

        // Attributes in `formatted_options` correspond to the attributes that will be in `_formatted`
        // These attributes are:
        // - the attributes asked to be highlighted or cropped (with `attributesToCrop` or `attributesToHighlight`)
        // - the attributes asked to be retrieved: these attributes will not be highlighted/cropped
        // But these attributes must be also present in displayed attributes
        let formatted_options = compute_formatted_options(
            &attr_to_highlight,
            &attr_to_crop,
            query.crop_length,
            &to_retrieve_ids,
            &fields_ids_map,
            &displayed_ids,
        );

        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);

        let formatter = Formatter::new(
            &analyzer,
            (query.highlight_pre_tag, query.highlight_post_tag),
            query.crop_marker,
        );

        let mut documents = Vec::new();

        let documents_iter = self.documents(&rtxn, documents_ids)?;

        for (_id, obkv) in documents_iter {
            let mut document = make_document(&to_retrieve_ids, &fields_ids_map, obkv)?;

            let matches_info = query
                .matches
                .then(|| compute_matches(&matching_words, &document, &analyzer));

            let formatted = format_fields(
                &document,
                &fields_ids_map,
                &formatter,
                &matching_words,
                &formatted_options,
            )?;

            if let Some(sort) = query.sort.as_ref() {
                insert_geo_distance(sort, &mut document);
            }

            let hit = SearchHit {
                document,
                formatted,
                matches_info,
            };
            documents.push(hit);
        }

        let nb_hits = candidates.len();

        let facets_distribution = match query.facets_distribution {
            Some(ref fields) => {
                let mut facets_distribution = self.facets_distribution(&rtxn);
                if fields.iter().all(|f| f != "*") {
                    facets_distribution.facets(fields);
                }
                let distribution = facets_distribution.candidates(candidates).execute()?;

                Some(distribution)
            }
            None => None,
        };

        let exhaustive_facets_count = facets_distribution.as_ref().map(|_| false); // not implemented yet

        let result = SearchResult {
            exhaustive_nb_hits: false, // not implemented yet
            hits: documents,
            nb_hits,
            query: query.q.clone().unwrap_or_default(),
            limit: query.limit,
            offset: query.offset.unwrap_or_default(),
            processing_time_ms: before_search.elapsed().as_millis(),
            facets_distribution,
            exhaustive_facets_count,
        };
        Ok(result)
    }
}

fn insert_geo_distance(sorts: &[String], document: &mut Document) {
    lazy_static::lazy_static! {
        static ref GEO_REGEX: Regex =
            Regex::new(r"_geoPoint\(\s*([[:digit:].\-]+)\s*,\s*([[:digit:].\-]+)\s*\)").unwrap();
    };
    if let Some(capture_group) = sorts.iter().find_map(|sort| GEO_REGEX.captures(sort)) {
        // TODO: TAMO: milli encountered an internal error, what do we want to do?
        let base = [
            capture_group[1].parse().unwrap(),
            capture_group[2].parse().unwrap(),
        ];
        let geo_point = &document.get("_geo").unwrap_or(&json!(null));
        if let Some((lat, lng)) = geo_point["lat"].as_f64().zip(geo_point["lng"].as_f64()) {
            let distance = milli::distance_between_two_points(&base, &[lat, lng]);
            document.insert("_geoDistance".to_string(), json!(distance.round() as usize));
        }
    }
}

fn compute_matches<A: AsRef<[u8]>>(
    matcher: &impl Matcher,
    document: &Document,
    analyzer: &Analyzer<A>,
) -> MatchesInfo {
    let mut matches = BTreeMap::new();

    for (key, value) in document {
        let mut infos = Vec::new();
        compute_value_matches(&mut infos, value, matcher, analyzer);
        if !infos.is_empty() {
            matches.insert(key.clone(), infos);
        }
    }
    matches
}

fn compute_value_matches<'a, A: AsRef<[u8]>>(
    infos: &mut Vec<MatchInfo>,
    value: &Value,
    matcher: &impl Matcher,
    analyzer: &Analyzer<'a, A>,
) {
    match value {
        Value::String(s) => {
            let analyzed = analyzer.analyze(s);
            let mut start = 0;
            for (word, token) in analyzed.reconstruct() {
                if token.is_word() {
                    if let Some(length) = matcher.matches(&token) {
                        infos.push(MatchInfo { start, length });
                    }
                }

                start += word.len();
            }
        }
        Value::Array(vals) => vals
            .iter()
            .for_each(|val| compute_value_matches(infos, val, matcher, analyzer)),
        Value::Object(vals) => vals
            .values()
            .for_each(|val| compute_value_matches(infos, val, matcher, analyzer)),
        Value::Number(number) => {
            compute_value_matches(infos, &Value::String(number.to_string()), matcher, analyzer)
        }
        _ => (),
    }
}

fn compute_formatted_options(
    attr_to_highlight: &HashSet<String>,
    attr_to_crop: &[String],
    query_crop_length: usize,
    to_retrieve_ids: &BTreeSet<FieldId>,
    fields_ids_map: &FieldsIdsMap,
    displayed_ids: &BTreeSet<FieldId>,
) -> BTreeMap<FieldId, FormatOptions> {
    let mut formatted_options = BTreeMap::new();

    add_highlight_to_formatted_options(
        &mut formatted_options,
        attr_to_highlight,
        fields_ids_map,
        displayed_ids,
    );

    add_crop_to_formatted_options(
        &mut formatted_options,
        attr_to_crop,
        query_crop_length,
        fields_ids_map,
        displayed_ids,
    );

    // Should not return `_formatted` if no valid attributes to highlight/crop
    if !formatted_options.is_empty() {
        add_non_formatted_ids_to_formatted_options(&mut formatted_options, to_retrieve_ids);
    }

    formatted_options
}

fn add_highlight_to_formatted_options(
    formatted_options: &mut BTreeMap<FieldId, FormatOptions>,
    attr_to_highlight: &HashSet<String>,
    fields_ids_map: &FieldsIdsMap,
    displayed_ids: &BTreeSet<FieldId>,
) {
    for attr in attr_to_highlight {
        let new_format = FormatOptions {
            highlight: true,
            crop: None,
        };

        if attr == "*" {
            for id in displayed_ids {
                formatted_options.insert(*id, new_format);
            }
            break;
        }

        if let Some(id) = fields_ids_map.id(attr) {
            if displayed_ids.contains(&id) {
                formatted_options.insert(id, new_format);
            }
        }
    }
}

fn add_crop_to_formatted_options(
    formatted_options: &mut BTreeMap<FieldId, FormatOptions>,
    attr_to_crop: &[String],
    crop_length: usize,
    fields_ids_map: &FieldsIdsMap,
    displayed_ids: &BTreeSet<FieldId>,
) {
    for attr in attr_to_crop {
        let mut split = attr.rsplitn(2, ':');
        let (attr_name, attr_len) = match split.next().zip(split.next()) {
            Some((len, name)) => {
                let crop_len = len.parse::<usize>().unwrap_or(crop_length);
                (name, crop_len)
            }
            None => (attr.as_str(), crop_length),
        };

        if attr_name == "*" {
            for id in displayed_ids {
                formatted_options
                    .entry(*id)
                    .and_modify(|f| f.crop = Some(attr_len))
                    .or_insert(FormatOptions {
                        highlight: false,
                        crop: Some(attr_len),
                    });
            }
        }

        if let Some(id) = fields_ids_map.id(attr_name) {
            if displayed_ids.contains(&id) {
                formatted_options
                    .entry(id)
                    .and_modify(|f| f.crop = Some(attr_len))
                    .or_insert(FormatOptions {
                        highlight: false,
                        crop: Some(attr_len),
                    });
            }
        }
    }
}

fn add_non_formatted_ids_to_formatted_options(
    formatted_options: &mut BTreeMap<FieldId, FormatOptions>,
    to_retrieve_ids: &BTreeSet<FieldId>,
) {
    for id in to_retrieve_ids {
        formatted_options.entry(*id).or_insert(FormatOptions {
            highlight: false,
            crop: None,
        });
    }
}

fn make_document(
    attributes_to_retrieve: &BTreeSet<FieldId>,
    field_ids_map: &FieldsIdsMap,
    obkv: obkv::KvReaderU16,
) -> Result<Document> {
    let mut document = serde_json::Map::new();

    // recreate the original json
    for (key, value) in obkv.iter() {
        let value = serde_json::from_slice(value)?;
        let key = field_ids_map
            .name(key)
            .expect("Missing field name")
            .to_string();

        document.insert(key, value);
    }

    // select the attributes to retrieve
    let attributes_to_retrieve = attributes_to_retrieve
        .iter()
        .map(|&fid| field_ids_map.name(fid).expect("Missing field name"));

    let document = permissive_json_pointer::select_values(&document, attributes_to_retrieve);

    // then we need to convert the `serde_json::Map` into an `IndexMap`.
    let document = document.into_iter().collect();

    Ok(document)
}

fn format_fields<A: AsRef<[u8]>>(
    document: &Document,
    field_ids_map: &FieldsIdsMap,
    formatter: &Formatter<A>,
    matching_words: &impl Matcher,
    formatted_options: &BTreeMap<FieldId, FormatOptions>,
) -> Result<Document> {
    // Convert the `IndexMap` into a `serde_json::Map`.
    let document = document
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let selectors: Vec<_> = formatted_options
        .keys()
        // This unwrap must be safe since we got the ids from the fields_ids_map just
        // before.
        .map(|&fid| field_ids_map.name(fid).unwrap())
        .collect();

    let mut document = permissive_json_pointer::select_values(&document, selectors.iter().copied());

    permissive_json_pointer::map_leaf_values(&mut document, selectors, |key, value| {
        // To get the formatting option of each key we need to see all the rules that applies
        // to the value and merge them together. eg. If a user said he wanted to highlight `doggo`
        // and crop `doggo.name`. `doggo.name` needs to be highlighted + cropped while `doggo.age` is only
        // highlighted.
        let format = formatted_options
            .iter()
            .filter(|(field, _option)| {
                let name = field_ids_map.name(**field).unwrap();
                milli::is_faceted_by(name, key) || milli::is_faceted_by(key, name)
            })
            .fold(FormatOptions::default(), |acc, (_, option)| {
                acc.merge(*option)
            });
        // TODO: remove this useless clone
        *value = formatter.format_value(value.clone(), matching_words, format);
    });

    // we need to convert back the `serde_json::Map` into an `IndexMap`.
    let document = document.into_iter().collect();

    Ok(document)
}

/// trait to allow unit testing of `format_fields`
trait Matcher {
    fn matches(&self, w: &Token) -> Option<usize>;
}

#[cfg(test)]
impl Matcher for BTreeMap<&str, Option<usize>> {
    fn matches(&self, w: &Token) -> Option<usize> {
        self.get(w.text()).cloned().flatten()
    }
}

impl Matcher for MatchingWords {
    fn matches(&self, w: &Token) -> Option<usize> {
        self.matching_bytes(w)
    }
}

struct Formatter<'a, A> {
    analyzer: &'a Analyzer<'a, A>,
    highlight_tags: (String, String),
    crop_marker: String,
}

impl<'a, A: AsRef<[u8]>> Formatter<'a, A> {
    pub fn new(
        analyzer: &'a Analyzer<'a, A>,
        highlight_tags: (String, String),
        crop_marker: String,
    ) -> Self {
        Self {
            analyzer,
            highlight_tags,
            crop_marker,
        }
    }

    fn format_value(
        &self,
        value: Value,
        matcher: &impl Matcher,
        format_options: FormatOptions,
    ) -> Value {
        match value {
            Value::String(old_string) => {
                let value = self.format_string(old_string, matcher, format_options);
                Value::String(value)
            }
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(|v| {
                        self.format_value(
                            v,
                            matcher,
                            FormatOptions {
                                highlight: format_options.highlight,
                                crop: None,
                            },
                        )
                    })
                    .collect(),
            ),
            Value::Object(object) => Value::Object(
                object
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            self.format_value(
                                v,
                                matcher,
                                FormatOptions {
                                    highlight: format_options.highlight,
                                    crop: None,
                                },
                            ),
                        )
                    })
                    .collect(),
            ),
            Value::Number(number) => {
                let number_string_value =
                    self.format_string(number.to_string(), matcher, format_options);
                Value::String(number_string_value)
            }
            value => value,
        }
    }

    fn format_string(
        &self,
        s: String,
        matcher: &impl Matcher,
        format_options: FormatOptions,
    ) -> String {
        let analyzed = self.analyzer.analyze(&s);

        let mut tokens = analyzed.reconstruct();
        let mut crop_marker_before = false;

        let tokens_interval: Box<dyn Iterator<Item = (&str, Token)>> = match format_options.crop {
            Some(crop_len) if crop_len > 0 => {
                let mut buffer = Vec::new();
                let mut tokens = tokens.by_ref().peekable();

                while let Some((word, token)) =
                    tokens.next_if(|(_, token)| matcher.matches(token).is_none())
                {
                    buffer.push((word, token));
                }

                match tokens.next() {
                    Some(token) => {
                        let mut total_count: usize = buffer
                            .iter()
                            .filter(|(_, token)| token.is_separator().is_none())
                            .count();

                        let crop_len_before = crop_len / 2;
                        // check if start will be cropped.
                        crop_marker_before = total_count > crop_len_before;

                        let before_iter = buffer.into_iter().skip_while(move |(_, token)| {
                            if token.is_separator().is_none() {
                                total_count -= 1;
                            }
                            total_count >= crop_len_before
                        });

                        // rebalance remaining word count after the match.
                        let crop_len_after = if crop_marker_before {
                            crop_len.saturating_sub(crop_len_before + 1)
                        } else {
                            crop_len.saturating_sub(total_count + 1)
                        };

                        let mut taken_after = 0;
                        let after_iter = tokens.take_while(move |(_, token)| {
                            let take = taken_after < crop_len_after;
                            if token.is_separator().is_none() {
                                taken_after += 1;
                            }
                            take
                        });

                        let iter = before_iter.chain(Some(token)).chain(after_iter);

                        Box::new(iter)
                    }
                    // If no word matches in the attribute
                    None => {
                        let mut count = 0;
                        let mut tokens = buffer.into_iter();
                        let mut out: String = tokens
                            .by_ref()
                            .take_while(move |(_, token)| {
                                let take = count < crop_len;
                                if token.is_separator().is_none() {
                                    count += 1;
                                }
                                take
                            })
                            .map(|(word, _)| word)
                            .collect();

                        // if there are remaining tokens after formatted interval,
                        // put a crop marker at the end.
                        if tokens.next().is_some() {
                            out.push_str(&self.crop_marker);
                        }

                        return out;
                    }
                }
            }
            _ => Box::new(tokens.by_ref()),
        };

        let out = if crop_marker_before {
            self.crop_marker.clone()
        } else {
            String::new()
        };

        let mut out = tokens_interval.fold(out, |mut out, (word, token)| {
            // Check if we need to do highlighting or computed matches before calling
            // Matcher::match since the call is expensive.
            if format_options.highlight && token.is_word() {
                if let Some(length) = matcher.matches(&token) {
                    match word.get(..length).zip(word.get(length..)) {
                        Some((head, tail)) => {
                            out.push_str(&self.highlight_tags.0);
                            out.push_str(head);
                            out.push_str(&self.highlight_tags.1);
                            out.push_str(tail);
                        }
                        // if we are in the middle of a character
                        // or if all the word should be highlighted,
                        // we highlight the complete word.
                        None => {
                            out.push_str(&self.highlight_tags.0);
                            out.push_str(word);
                            out.push_str(&self.highlight_tags.1);
                        }
                    }
                    return out;
                }
            }
            out.push_str(word);
            out
        });

        // if there are remaining tokens after formatted interval,
        // put a crop marker at the end.
        if tokens.next().is_some() {
            out.push_str(&self.crop_marker);
        }

        out
    }
}

fn parse_filter(facets: &Value) -> Result<Option<Filter>> {
    match facets {
        Value::String(expr) => {
            let condition = Filter::from_str(expr)?;
            Ok(condition)
        }
        Value::Array(arr) => parse_filter_array(arr),
        v => Err(FacetError::InvalidExpression(&["Array"], v.clone()).into()),
    }
}

fn parse_filter_array(arr: &[Value]) -> Result<Option<Filter>> {
    let mut ands = Vec::new();
    for value in arr {
        match value {
            Value::String(s) => ands.push(Either::Right(s.as_str())),
            Value::Array(arr) => {
                let mut ors = Vec::new();
                for value in arr {
                    match value {
                        Value::String(s) => ors.push(s.as_str()),
                        v => {
                            return Err(FacetError::InvalidExpression(&["String"], v.clone()).into())
                        }
                    }
                }
                ands.push(Either::Left(ors));
            }
            v => {
                return Err(
                    FacetError::InvalidExpression(&["String", "[String]"], v.clone()).into(),
                )
            }
        }
    }

    Ok(Filter::from_array(ands)?)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn no_ids_no_formatted() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        fields.insert("test").unwrap();

        let document: serde_json::Value = json!({
            "test": "hello",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let formatted_options = BTreeMap::new();

        let matching_words = MatchingWords::default();

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert!(value.is_empty());
    }

    #[test]
    fn formatted_with_highlight_in_word() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "The Hobbit",
            "author": "J. R. R. Tolkien",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: true,
                crop: None,
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("hobbit", Some(3));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "The <em>Hob</em>bit");
        assert_eq!(value["author"], "J. R. R. Tolkien");
    }

    #[test]
    fn formatted_with_highlight_in_number() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();
        let publication_year = fields.insert("publication_year").unwrap();

        let document: serde_json::Value = json!({
            "title": "The Hobbit",
            "author": "J. R. R. Tolkien",
            "publication_year": 1937,
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );
        formatted_options.insert(
            publication_year,
            FormatOptions {
                highlight: true,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("1937", Some(4));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "The Hobbit");
        assert_eq!(value["author"], "J. R. R. Tolkien");
        assert_eq!(value["publication_year"], "<em>1937</em>");
    }

    /// https://github.com/meilisearch/meilisearch/issues/1368
    #[test]
    fn formatted_with_highlight_emoji() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Go💼od luck.",
            "author": "JacobLey",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: true,
                crop: None,
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        // emojis are deunicoded during tokenization
        // TODO Tokenizer should remove spaces after deunicode
        matching_words.insert("gobriefcase od", Some(11));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "<em>Go💼od</em> luck.");
        assert_eq!(value["author"], "JacobLey");
    }

    #[test]
    fn formatted_with_highlight_in_unicode_word() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "étoile",
            "author": "J. R. R. Tolkien",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: true,
                crop: None,
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("etoile", Some(1));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "<em>étoile</em>");
        assert_eq!(value["author"], "J. R. R. Tolkien");
    }

    #[test]
    fn formatted_with_crop_2() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Harry Potter and the Half-Blood Prince",
            "author": "J. K. Rowling",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: false,
                crop: Some(2),
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("potter", Some(3));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "Harry Potter…");
        assert_eq!(value["author"], "J. K. Rowling");
    }

    #[test]
    fn formatted_with_crop_5() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Harry Potter and the Half-Blood Prince",
            "author": "J. K. Rowling",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: false,
                crop: Some(5),
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("potter", Some(5));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "Harry Potter and the Half…");
        assert_eq!(value["author"], "J. K. Rowling");
    }

    #[test]
    fn formatted_with_crop_0() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Harry Potter and the Half-Blood Prince",
            "author": "J. K. Rowling",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: false,
                crop: Some(0),
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("potter", Some(6));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "Harry Potter and the Half-Blood Prince");
        assert_eq!(value["author"], "J. K. Rowling");
    }

    #[test]
    fn formatted_with_crop_and_no_match() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Harry Potter and the Half-Blood Prince",
            "author": "J. K. Rowling",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: false,
                crop: Some(1),
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: Some(20),
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("rowling", Some(3));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "Harry…");
        assert_eq!(value["author"], "J. K. Rowling");
    }

    #[test]
    fn formatted_with_crop_and_highlight() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Harry Potter and the Half-Blood Prince",
            "author": "J. K. Rowling",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: true,
                crop: Some(1),
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("and", Some(3));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "…<em>and</em>…");
        assert_eq!(value["author"], "J. K. Rowling");
    }

    #[test]
    fn formatted_with_crop_and_highlight_in_word() {
        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);
        let formatter = Formatter::new(
            &analyzer,
            (String::from("<em>"), String::from("</em>")),
            String::from("…"),
        );

        let mut fields = FieldsIdsMap::new();
        let title = fields.insert("title").unwrap();
        let author = fields.insert("author").unwrap();

        let document: serde_json::Value = json!({
            "title": "Harry Potter and the Half-Blood Prince",
            "author": "J. K. Rowling",
        });

        // we need to convert the `serde_json::Map` into an `IndexMap`.
        let document = document
            .as_object()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut formatted_options = BTreeMap::new();
        formatted_options.insert(
            title,
            FormatOptions {
                highlight: true,
                crop: Some(4),
            },
        );
        formatted_options.insert(
            author,
            FormatOptions {
                highlight: false,
                crop: None,
            },
        );

        let mut matching_words = BTreeMap::new();
        matching_words.insert("blood", Some(3));

        let value = format_fields(
            &document,
            &fields,
            &formatter,
            &matching_words,
            &formatted_options,
        )
        .unwrap();

        assert_eq!(value["title"], "…the Half-<em>Blo</em>od Prince");
        assert_eq!(value["author"], "J. K. Rowling");
    }

    #[test]
    fn test_compute_value_matches() {
        let text = "Call me Ishmael. Some years ago—never mind how long precisely—having little or no money in my purse, and nothing particular to interest me on shore, I thought I would sail about a little and see the watery part of the world.";
        let value = serde_json::json!(text);

        let mut matcher = BTreeMap::new();
        matcher.insert("ishmael", Some(3));
        matcher.insert("little", Some(6));
        matcher.insert("particular", Some(1));

        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);

        let mut infos = Vec::new();

        compute_value_matches(&mut infos, &value, &matcher, &analyzer);

        let mut infos = infos.into_iter();
        let crop = |info: MatchInfo| &text[info.start..info.start + info.length];

        assert_eq!(crop(infos.next().unwrap()), "Ish");
        assert_eq!(crop(infos.next().unwrap()), "little");
        assert_eq!(crop(infos.next().unwrap()), "p");
        assert_eq!(crop(infos.next().unwrap()), "little");
        assert!(infos.next().is_none());
    }

    #[test]
    fn test_compute_match() {
        let value = serde_json::from_str(r#"{
            "color": "Green",
            "name": "Lucas Hess",
            "gender": "male",
            "price": 3.5,
            "address": "412 Losee Terrace, Blairstown, Georgia, 2825",
            "about": "Mollit ad in exercitation quis Laboris . Anim est ut consequat fugiat duis magna aliquip velit nisi. Commodo eiusmod est consequat proident consectetur aliqua enim fugiat. Aliqua adipisicing laboris elit proident enim veniam laboris mollit. Incididunt fugiat minim ad nostrud deserunt tempor in. Id irure officia labore qui est labore nulla nisi. Magna sit quis tempor esse consectetur amet labore duis aliqua consequat.\r\n"
  }"#).unwrap();
        let mut matcher = BTreeMap::new();
        matcher.insert("green", Some(5));
        matcher.insert("mollit", Some(6));
        matcher.insert("laboris", Some(7));
        matcher.insert("3", Some(1));

        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);

        let matches = compute_matches(&matcher, &value, &analyzer);
        assert_eq!(
            format!("{:?}", matches),
            r##"{"about": [MatchInfo { start: 0, length: 6 }, MatchInfo { start: 31, length: 7 }, MatchInfo { start: 191, length: 7 }, MatchInfo { start: 225, length: 7 }, MatchInfo { start: 233, length: 6 }], "color": [MatchInfo { start: 0, length: 5 }], "price": [MatchInfo { start: 0, length: 1 }]}"##
        );
    }

    #[test]
    fn test_insert_geo_distance() {
        let value: Document = serde_json::from_str(
            r#"{
      "_geo": {
        "lat": 50.629973371633746,
        "lng": 3.0569447399419567
      },
      "city": "Lille",
      "id": "1"
    }"#,
        )
        .unwrap();

        let sorters = &["_geoPoint(50.629973371633746,3.0569447399419567):desc".to_string()];
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        let sorters = &["_geoPoint(50.629973371633746, 3.0569447399419567):asc".to_string()];
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        let sorters =
            &["_geoPoint(   50.629973371633746   ,  3.0569447399419567   ):desc".to_string()];
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        let sorters = &[
            "prix:asc",
            "villeneuve:desc",
            "_geoPoint(50.629973371633746, 3.0569447399419567):asc",
            "ubu:asc",
        ]
        .map(|s| s.to_string());
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        // only the first geoPoint is used to compute the distance
        let sorters = &[
            "chien:desc",
            "_geoPoint(50.629973371633746, 3.0569447399419567):asc",
            "pangolin:desc",
            "_geoPoint(100.0, -80.0):asc",
            "chat:asc",
        ]
        .map(|s| s.to_string());
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        // there was no _geoPoint so nothing is inserted in the document
        let sorters = &["chien:asc".to_string()];
        let mut document = value;
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), None);
    }
}
