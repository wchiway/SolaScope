//! Annotated query files: a JSON array of
//! `{category, query, path, symbol, start_line?, end_line?}` objects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use solascope_core::{Error, Result};

use crate::metrics::{Expectation, LineRange};

/// One annotated query: what to search for and where the answer is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnnotatedQuery {
    pub category: String,
    pub query: String,
    pub expected: Expectation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySet {
    pub queries: Vec<AnnotatedQuery>,
    /// Field names in the file that the harness does not know, sorted. They are
    /// ignored; callers report them so that a misspelt field cannot hide.
    pub unknown_fields: Vec<String>,
}

impl QuerySet {
    /// Categories in order of first appearance.
    pub fn categories(&self) -> Vec<String> {
        let mut categories: Vec<String> = Vec::new();
        for item in &self.queries {
            if !categories.contains(&item.category) {
                categories.push(item.category.clone());
            }
        }
        categories
    }
}

/// The distinct query strings of a file, for latency measurements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryStrings {
    pub queries: Vec<String>,
    /// How many repeated query strings were dropped.
    pub duplicates: usize,
}

#[derive(Deserialize)]
struct RawAnnotated {
    category: String,
    query: String,
    path: String,
    symbol: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawQuery {
    Text(String),
    Object { query: String },
}

fn check_query(query: &str) -> std::result::Result<(), String> {
    solascope_core::validate_query(query, 1)
        .map_err(|_| "`query` must contain 1 to 4096 bytes of text".to_owned())
}

/// Parses and validates an annotated query file.
pub fn parse_annotated(text: &str) -> std::result::Result<QuerySet, String> {
    let raw: Vec<RawAnnotated> = serde_json::from_str(text)
        .map_err(|error| format!("not an array of annotated queries: {error}"))?;
    if raw.is_empty() {
        return Err("the file contains no queries".into());
    }
    let mut unknown = BTreeSet::new();
    let mut queries = Vec::with_capacity(raw.len());
    for (index, item) in raw.into_iter().enumerate() {
        let entry = |message: &str| format!("entry {}: {message}", index + 1);
        check_query(&item.query).map_err(|message| entry(&message))?;
        for (name, value) in [
            ("category", &item.category),
            ("path", &item.path),
            ("symbol", &item.symbol),
        ] {
            if value.trim().is_empty() {
                return Err(entry(&format!("`{name}` must not be empty")));
            }
        }
        let lines = match (item.start_line, item.end_line) {
            (None, None) => None,
            (Some(start), Some(end)) if start >= 1 && start <= end => {
                Some(LineRange { start, end })
            }
            (Some(_), Some(_)) => return Err(entry(
                "`start_line` and `end_line` are 1-based and start_line must not exceed end_line",
            )),
            _ => return Err(entry("`start_line` and `end_line` must be given together")),
        };
        unknown.extend(item.unknown.into_keys());
        queries.push(AnnotatedQuery {
            category: item.category,
            query: item.query,
            expected: Expectation {
                path: item.path,
                symbol: item.symbol,
                lines,
            },
        });
    }
    Ok(QuerySet {
        queries,
        unknown_fields: unknown.into_iter().collect(),
    })
}

/// Extracts distinct queries in file order from strings or annotated objects.
/// Fields other than `query` in an object are ignored for latency measurement.
pub fn parse_query_strings(text: &str) -> std::result::Result<QueryStrings, String> {
    let raw: Vec<RawQuery> = serde_json::from_str(text).map_err(|error| {
        format!("not an array of strings or objects with a `query` string: {error}")
    })?;
    if raw.is_empty() {
        return Err("the file contains no queries".into());
    }
    let total = raw.len();
    let mut queries: Vec<String> = Vec::with_capacity(total);
    for (index, item) in raw.into_iter().enumerate() {
        let query = match item {
            RawQuery::Text(query) | RawQuery::Object { query } => query,
        };
        check_query(&query).map_err(|message| format!("entry {}: {message}", index + 1))?;
        if !queries.contains(&query) {
            queries.push(query);
        }
    }
    Ok(QueryStrings {
        duplicates: total - queries.len(),
        queries,
    })
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|error| {
        Error::InvalidInput(format!(
            "cannot read query file {}: {error}",
            path.display()
        ))
    })
}

fn invalid(path: &Path, message: &str) -> Error {
    Error::InvalidInput(format!("query file {}: {message}", path.display()))
}

pub fn load_annotated(path: &Path) -> Result<QuerySet> {
    parse_annotated(&read(path)?).map_err(|message| invalid(path, &message))
}

pub fn load_query_strings(path: &Path) -> Result<QueryStrings> {
    parse_query_strings(&read(path)?).map_err(|message| invalid(path, &message))
}

/// The query file is evaluation data, never source to retrieve. If it lies
/// inside the corpus, exclude that exact path without changing its files.
pub fn exclude_query_file(config: &mut solascope_config::Config, file: &Path) -> Option<String> {
    let file = file.canonicalize().ok()?;
    let root = config.project.root.canonicalize().ok()?;
    let relative = file.strip_prefix(&root).ok()?;
    let path = relative.to_string_lossy().replace('\\', "/");
    let pattern = crate::perf::escape_glob(&path);
    if !config.project.exclude.contains(&pattern) {
        config.project.exclude.push(pattern);
    }
    Some(format!(
        "the query file is inside the project root ({path}); excluded from source scanning and results. \
         With --no-index, use an index built without these annotations"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_annotation_format() {
        let set = parse_annotated(
            r#"[
              {"category": "symbol", "query": "find_user", "path": "repository.py",
               "symbol": "find_user", "start_line": 5, "end_line": 9},
              {"category": "error", "query": "USER_NOT_FOUND", "path": "repository.py",
               "symbol": "find_user"},
              {"category": "symbol", "query": "Reserve", "path": "inventory.go",
               "symbol": "Reserve", "start_line": null, "end_line": null}
            ]"#,
        )
        .unwrap();
        assert_eq!(set.queries.len(), 3);
        assert_eq!(set.categories(), ["symbol", "error"]);
        assert!(set.unknown_fields.is_empty());
        assert_eq!(
            set.queries[0],
            AnnotatedQuery {
                category: "symbol".into(),
                query: "find_user".into(),
                expected: Expectation {
                    path: "repository.py".into(),
                    symbol: "find_user".into(),
                    lines: Some(LineRange { start: 5, end: 9 }),
                },
            }
        );
        assert_eq!(set.queries[1].expected.lines, None);
        assert_eq!(set.queries[2].expected.lines, None);
    }

    #[test]
    fn unknown_fields_are_collected_rather_than_silently_dropped() {
        let set = parse_annotated(
            r#"[
              {"category": "c", "query": "q1", "path": "p", "symbol": "s", "note": "x", "start_lin": 3},
              {"category": "c", "query": "q2", "path": "p", "symbol": "s", "note": "y"}
            ]"#,
        )
        .unwrap();
        assert_eq!(set.unknown_fields, ["note", "start_lin"]);
        assert_eq!(set.queries[0].expected.lines, None);
    }

    #[test]
    fn rejects_malformed_annotations_with_the_entry_number() {
        let entry = |fields: &str| {
            format!(
                r#"[{{"category": "c", "query": "ok", "path": "p", "symbol": "s"}}, {{{fields}}}]"#
            )
        };
        for (fields, expected) in [
            (
                r#""category": "c", "query": "q", "path": "p""#,
                "missing field `symbol`",
            ),
            (
                r#""category": "c", "query": "  ", "path": "p", "symbol": "s""#,
                "entry 2: `query` must contain",
            ),
            (
                r#""category": "c", "query": "q", "path": "", "symbol": "s""#,
                "entry 2: `path` must not be empty",
            ),
            (
                r#""category": "c", "query": "q", "path": "p", "symbol": " ""#,
                "entry 2: `symbol` must not be empty",
            ),
            (
                r#""category": "", "query": "q", "path": "p", "symbol": "s""#,
                "entry 2: `category` must not be empty",
            ),
            (
                r#""category": "c", "query": "q", "path": "p", "symbol": "s", "start_line": 3"#,
                "entry 2: `start_line` and `end_line` must be given together",
            ),
            (
                r#""category": "c", "query": "q", "path": "p", "symbol": "s", "end_line": 3"#,
                "entry 2: `start_line` and `end_line` must be given together",
            ),
            (
                r#""category": "c", "query": "q", "path": "p", "symbol": "s", "start_line": 4, "end_line": 3"#,
                "start_line must not exceed end_line",
            ),
            (
                r#""category": "c", "query": "q", "path": "p", "symbol": "s", "start_line": 0, "end_line": 3"#,
                "1-based",
            ),
            (
                r#""category": "c", "query": "q", "path": "p", "symbol": "s", "start_line": "3", "end_line": 3"#,
                "not an array of annotated queries",
            ),
        ] {
            let error = parse_annotated(&entry(fields)).unwrap_err();
            assert!(error.contains(expected), "{fields}: {error}");
        }
        for text in ["[]", "{}", "", "[1, 2]"] {
            assert!(parse_annotated(text).is_err(), "{text:?}");
        }
        assert_eq!(
            parse_annotated("[]").unwrap_err(),
            "the file contains no queries"
        );
    }

    #[test]
    fn query_strings_are_distinct_and_keep_file_order() {
        let parsed = parse_query_strings(
            r#"[
              {"category": "symbol", "query": "b", "path": "p", "symbol": "s"},
              {"query": "a"},
              {"query": "b", "anything": [1, 2]}
            ]"#,
        )
        .unwrap();
        assert_eq!(parsed.queries, ["b", "a"]);
        assert_eq!(parsed.duplicates, 1);
        assert!(parse_query_strings(r#"[{"query": ""}]"#)
            .unwrap_err()
            .starts_with("entry 1:"));
        assert!(parse_query_strings(r#"[{"symbol": "s"}]"#).is_err());
        assert!(parse_query_strings("[]").is_err());
    }

    #[test]
    fn latency_queries_accept_strings_without_weakening_annotation_validation() {
        let parsed = parse_query_strings(r#"["b", "a", "b", {"query": "c"}]"#).unwrap();
        assert_eq!(parsed.queries, ["b", "a", "c"]);
        assert_eq!(parsed.duplicates, 1);
        assert!(parse_query_strings(r#"[""]"#)
            .unwrap_err()
            .starts_with("entry 1:"));
        for invalid in ["[1]", "[null]", "[true]", "[[]]", "[{}]"] {
            assert!(parse_query_strings(invalid).is_err(), "{invalid}");
        }
        assert!(parse_annotated(r#"["b"]"#).is_err());
    }

    #[test]
    fn load_errors_name_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.json");
        let error = load_annotated(&missing).unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)));
        assert!(error.to_string().contains("missing.json"));

        let broken = directory.path().join("broken.json");
        std::fs::write(&broken, "[]").unwrap();
        let error = load_query_strings(&broken).unwrap_err().to_string();
        assert!(
            error.contains("broken.json") && error.contains("no queries"),
            "{error}"
        );

        let valid = directory.path().join("valid.json");
        std::fs::write(
            &valid,
            r#"[{"category": "c", "query": "q", "path": "p", "symbol": "s"}]"#,
        )
        .unwrap();
        assert_eq!(load_annotated(&valid).unwrap().queries.len(), 1);
        assert_eq!(load_query_strings(&valid).unwrap().queries, ["q"]);
    }

    #[test]
    fn the_repository_fixture_is_a_valid_annotation_file() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/multilang/queries.json");
        let set = load_annotated(&fixture).unwrap();
        assert_eq!(set.queries.len(), 18);
        assert_eq!(
            set.categories(),
            ["symbol", "error", "natural", "distractor"]
        );
        assert!(set.unknown_fields.is_empty());
        assert_eq!(
            set.queries
                .iter()
                .filter(|item| item.expected.lines.is_some())
                .count(),
            5
        );
    }
}
