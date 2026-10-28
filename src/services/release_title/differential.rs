//! Old parsers against the new one, title by title, while call sites move.
//!
//! Every title in every corpus goes through each "lens": an old parser's
//! answer (from the frozen copies in `legacy.rs`) beside the new parser's
//! equivalent. On a hand-verified title a difference is already settled,
//! since the corpus tests check the new answer. On a title from the
//! unverified listing sample, every difference needs a reviewed entry in
//! `tests/fixtures/release-title-differential/triage.json`, naming one of
//! the file's `reasons`. Each reason carries a note and a verdict:
//!
//! - `fix`: the old parser was wrong.
//! - `accepted`: a deliberate change (the note says which).
//! - `regression`: the new parser is wrong. Not allowed on a lens listed
//!   in [`MIGRATED_LENSES`], so no call site moves onto a known regression.
//!
//! The test fails on an unlisted difference (printing a ready-to-paste
//! entry), on a stale entry whose difference is gone or changed, and on an
//! unknown or unused reason.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::legacy;
use super::parse;

/// Lenses whose call sites already read the new parser.
///
/// - `numbers.loose`: the search gates (`classify_match`, the interactive
///   filter, scoring, the Wanted episode-set selection) read episodes
///   through `for_series`, which is this reading plus the series' name.
/// - `season`: the season check (`conflicts_with_season`) in the same
///   gates, and the misgrab verdict's season note.
/// - `batch.nyaa`, `batch.rss`, `batch.torznab`: every source's batch
///   flag is `is_batch`.
const MIGRATED_LENSES: &[&str] = &[
    "numbers.loose",
    "season",
    "batch.nyaa",
    "batch.rss",
    "batch.torznab",
];

pub(super) fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub(super) fn load(path: &Path) -> Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

pub(super) fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    paths.retain(|p| p.extension().is_some_and(|e| e == "json"));
    paths.sort();
    paths
}

/// Titles whose new answers other tests already check.
pub(super) fn verified_titles() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for dir in ["nyaa-titles", "sonarr-parser", "release-title-regressions"] {
        for path in json_files(&fixtures().join(dir)) {
            if let Some(cases) = load(&path)["cases"].as_array() {
                out.extend(
                    cases
                        .iter()
                        .filter_map(|c| c["title"].as_str().map(String::from)),
                );
            }
        }
    }
    out
}

pub(super) fn sample_titles() -> Vec<String> {
    load(&fixtures().join("nyaa-titles/sample/listing-sample.json"))
        .as_array()
        .expect("sample array")
        .iter()
        .filter_map(|r| r["title"].as_str().map(String::from))
        .collect()
}

fn sorted(set: impl IntoIterator<Item = i32>) -> Value {
    let mut v: Vec<i32> = set.into_iter().collect();
    v.sort_unstable();
    v.dedup();
    json!(v)
}

/// `(lens, old, new)` for every lens where the answers differ.
pub(super) fn differences(title: &str) -> Vec<(&'static str, Value, Value)> {
    let new = parse(title);
    let new_batch = json!(new.is_batch());
    let new_eps = sorted(new.release_episodes());
    let new_season = json!(new.season_hint());
    let new_group = json!(new.group.clone().unwrap_or_default());
    let new_res = json!(new.resolution.map(|r| r.to_string()).unwrap_or_default());
    let rss = legacy::rss::parse_release(title);
    let lenses: [(&'static str, Value, Value); 10] = [
        (
            "batch.nyaa",
            json!(legacy::scraper::detect_batch(title)),
            new_batch.clone(),
        ),
        (
            "batch.rss",
            json!(legacy::feed::detect_batch(title)),
            new_batch.clone(),
        ),
        (
            "batch.torznab",
            json!(legacy::indexers::detect_batch_from_title(title)),
            new_batch,
        ),
        (
            "numbers.loose",
            sorted(legacy::release_parse::parse_release_numbers(title)),
            new_eps.clone(),
        ),
        (
            "season",
            json!(Some(legacy::release_parse::parse_release_season(title)).filter(|s| *s > 0)),
            new_season.clone(),
        ),
        ("rss.season", json!(rss.season_hint), new_season),
        (
            "rss.episodes",
            sorted(
                rss.season_relative_eps
                    .iter()
                    .chain(rss.absolute_eps.iter())
                    .copied(),
            ),
            new_eps,
        ),
        (
            "group.feed",
            json!(legacy::feed::extract_group(title)),
            new_group.clone(),
        ),
        (
            "group.torznab",
            json!(legacy::indexers::extract_group_from_title(title)),
            new_group,
        ),
        (
            "resolution.feed",
            json!(legacy::feed::extract_resolution(title)),
            new_res,
        ),
    ];
    lenses
        .into_iter()
        .filter(|(_, old, new)| old != new)
        .collect()
}

fn triage_path() -> PathBuf {
    fixtures().join("release-title-differential/triage.json")
}

#[test]
fn every_sample_difference_has_a_verdict() {
    let verified = verified_titles();
    let triage = load(&triage_path());
    let reasons = triage["reasons"].as_object().expect("reasons");
    for (id, reason) in reasons {
        assert!(
            matches!(
                reason["verdict"].as_str(),
                Some("fix" | "accepted" | "regression")
            ),
            "bad verdict on reason {id}"
        );
        assert!(
            reason["note"].as_str().is_some_and(|n| !n.is_empty()),
            "reason {id} needs a note"
        );
    }
    let mut entries: BTreeMap<(String, String), Value> = BTreeMap::new();
    let mut used = BTreeSet::new();
    for e in triage["entries"].as_array().expect("entries") {
        let key = (
            e["lens"].as_str().unwrap().to_string(),
            e["title"].as_str().unwrap().to_string(),
        );
        let reason = e["reason"].as_str().unwrap_or_default();
        assert!(
            reasons.contains_key(reason),
            "unknown reason {reason:?} on {key:?}"
        );
        used.insert(reason.to_string());
        entries.insert(key, e.clone());
    }
    for id in reasons.keys() {
        assert!(used.contains(id), "reason {id} is used by no entry");
    }
    let mut missing = Vec::new();
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for title in sample_titles() {
        if verified.contains(&title) {
            continue;
        }
        for (lens, old, new) in differences(&title) {
            let key = (lens.to_string(), title.clone());
            match entries.get(&key) {
                None => missing.push(
                    json!({"lens": lens, "title": title, "old": old, "new": new, "reason": ""}),
                ),
                Some(e) if e["old"] != old || e["new"] != new => problems.push(format!(
                    "{lens} {title:?}: answers changed (old {old}, new {new}); re-review"
                )),
                Some(e) => {
                    let reason = &reasons[e["reason"].as_str().unwrap_or_default()];
                    if reason["verdict"] == "regression" && MIGRATED_LENSES.contains(&lens) {
                        problems.push(format!("{lens} {title:?}: regression on a migrated lens"));
                    }
                }
            }
            seen.insert(key);
        }
    }
    for key in entries.keys() {
        if !seen.contains(key) {
            problems.push(format!(
                "{} {:?}: stale entry, the difference is gone",
                key.0, key.1
            ));
        }
    }
    let lines: Vec<String> = missing.iter().map(|m| m.to_string()).collect();
    assert!(
        missing.is_empty() && problems.is_empty(),
        "{} unreviewed differences:\n{}\n{}",
        missing.len(),
        lines.join(",\n"),
        problems.join("\n")
    );
}

/// `cargo t release_title::differential::report -- --ignored` prints the
/// difference counts per lens over every corpus.
#[test]
#[ignore]
fn report() {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let titles: Vec<String> = verified_titles()
        .into_iter()
        .chain(sample_titles())
        .collect();
    for title in &titles {
        for (lens, _, _) in differences(title) {
            *counts.entry(lens).or_default() += 1;
        }
    }
    eprintln!("{} titles", titles.len());
    for (lens, n) in counts {
        eprintln!("{lens:16} {n}");
    }
}

/// `RT_TITLES=$'title one\ntitle two' cargo t release_title::differential::dump
/// --run-ignored only --no-capture` prints each title's parse and differences.
#[test]
#[ignore]
fn dump() {
    let titles = std::env::var("RT_TITLES").unwrap_or_default();
    for title in titles.lines().filter(|t| !t.trim().is_empty()) {
        let p = parse(title);
        eprintln!(
            "== {title}\n   group={:?} title={:?} res={:?} year={:?} seasons={:?} part={:?} hint_only={} \
             part_pack={}\n   claim={:?} ambiguous={:?} special={} extra={} half={} batch_kw={} end={} \
             movie={} vol={} single={} is_batch={}",
            p.group,
            p.title,
            p.resolution,
            p.year,
            p.seasons.numbers,
            p.seasons.part,
            p.seasons.hint_only,
            p.seasons.part_pack,
            p.episodes,
            p.ambiguous_number,
            p.special,
            p.extra,
            p.half_episode,
            p.batch_keyword,
            p.end_marker,
            p.movie,
            p.volume_range,
            p.single_file,
            p.is_batch(),
        );
        for (lens, old, new) in differences(title) {
            eprintln!("   {lens}: {old} -> {new}");
        }
    }
}
