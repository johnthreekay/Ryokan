//! Corpora for `services::release_title`, the release-title and file-name
//! parser that replaces the old per-module parsers.
//!
//! - `tests/fixtures/nyaa-titles/`: real Nyaa release titles in buckets, each
//!   with a hand-verified expectation (see the README there).
//! - `tests/fixtures/sonarr-parser/`: Sonarr's parser test cases, ported with
//!   Sonarr's own expected values.
//! - `tests/fixtures/release-title-regressions/`: every title the old
//!   parsers' inline tests pinned, with the outcome each asserted.
//!
//! These tests check that every corpus file loads and is well formed. The
//! parser assertions over the same files live with the parser.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
}

fn load(path: &Path) -> Value {
    let raw = std::fs::read_to_string(path).expect("read fixture");
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("bad fixture {}: {e}", path.display()))
}

fn cases(doc: &Value, path: &Path) -> Vec<Value> {
    doc["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("{} has no cases array", path.display()))
        .clone()
}

const EXPECT_KEYS: &[&str] = &[
    "group",
    "batch",
    "seasons",
    "season_part",
    "episodes",
    "ambiguous",
    "special",
    "includes_specials",
    "extra",
    "half_episode",
    "version",
    "resolution",
    "year",
];

fn check_episodes(value: &Value, at: &str) {
    match value {
        Value::Null => {}
        Value::Array(items) => assert!(
            items.iter().all(|v| v.as_i64().is_some()),
            "{at}: episode list must hold integers"
        ),
        Value::Object(range) => {
            let first = range["first"].as_i64().expect("range first");
            let last = range["last"].as_i64().expect("range last");
            assert!(first <= last, "{at}: range must ascend");
        }
        other => panic!("{at}: bad episodes value {other}"),
    }
}

#[test]
fn nyaa_title_corpus_is_well_formed() {
    let dir = fixture_dir("nyaa-titles");
    let mut titles = BTreeSet::new();
    let mut per_bucket = BTreeMap::new();
    for path in json_files(&dir) {
        let doc = load(&path);
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            doc["bucket"].as_str(),
            Some(stem.as_str()),
            "{}",
            path.display()
        );
        let cases = cases(&doc, &path);
        assert!(!cases.is_empty(), "{} is empty", path.display());
        for case in &cases {
            let title = case["title"].as_str().expect("title");
            let at = format!("{stem}: {title}");
            assert!(titles.insert(title.to_string()), "duplicate title {at}");
            assert_eq!(case["verified"].as_str(), Some("hand"), "{at}");
            let expect = case["expect"].as_object().expect("expect object");
            assert!(expect["batch"].is_boolean(), "{at}: batch is required");
            for key in expect.keys() {
                assert!(
                    EXPECT_KEYS.contains(&key.as_str()),
                    "{at}: unknown key {key}"
                );
            }
            if let Some(eps) = expect.get("episodes") {
                check_episodes(eps, &at);
            }
            if let Some(series) = case.get("series") {
                let aliases = series["aliases"].as_array().expect("series aliases");
                assert!(!aliases.is_empty(), "{at}: series needs aliases");
                check_episodes(&series["episodes"], &at);
            }
        }
        per_bucket.insert(stem, cases.len());
    }
    assert!(per_bucket.len() >= 20, "buckets: {per_bucket:?}");
    assert!(
        titles.len() >= 500,
        "corpus unexpectedly small: {}",
        titles.len()
    );
}

#[test]
fn nyaa_title_sample_is_well_formed() {
    let path = fixture_dir("nyaa-titles").join("sample/listing-sample.json");
    let doc = load(&path);
    let rows = doc.as_array().expect("sample is an array");
    assert!(
        rows.len() >= 1500,
        "sample unexpectedly small: {}",
        rows.len()
    );
    let mut seen = BTreeSet::new();
    for row in rows {
        let title = row["title"].as_str().expect("title");
        assert!(seen.insert(title), "duplicate sample title {title}");
    }
}

#[test]
fn sonarr_parser_corpus_is_well_formed() {
    let dir = fixture_dir("sonarr-parser");
    let mut total = 0;
    for path in json_files(&dir) {
        let doc = load(&path);
        assert_eq!(
            doc["license"].as_str(),
            Some("GPL-3.0-only"),
            "{}",
            path.display()
        );
        assert!(doc["commit"].as_str().is_some_and(|c| c.len() == 40));
        for case in cases(&doc, &path) {
            let title = case["title"].as_str().expect("title");
            assert!(
                matches!(case["kind"].as_str(), Some("release" | "file")),
                "{title}: kind"
            );
            assert!(
                matches!(case["scope"].as_str(), Some("anime" | "scene")),
                "{title}: scope"
            );
            let sonarr = case["sonarr"].as_object().expect("sonarr object");
            assert!(!sonarr.is_empty(), "{title}: empty expectation");
            total += 1;
        }
    }
    assert!(total >= 700, "Sonarr corpus unexpectedly small: {total}");
}

#[test]
fn regression_literals_are_well_formed() {
    let dir = fixture_dir("release-title-regressions");
    let mut total = 0;
    for path in json_files(&dir) {
        let doc = load(&path);
        assert!(doc["about"].as_str().is_some(), "{}", path.display());
        for case in cases(&doc, &path) {
            let func = case["fn"].as_str().expect("fn");
            let title = case["title"].as_str().expect("title");
            assert!(
                !title.is_empty() || func.ends_with("extract_group"),
                "{func}: empty title"
            );
            assert!(case.get("expect").is_some(), "{func} {title}: expect");
            assert!(case["from"].as_str().is_some(), "{func} {title}: from");
            total += 1;
        }
    }
    assert!(
        total >= 150,
        "regression literals unexpectedly few: {total}"
    );
}
