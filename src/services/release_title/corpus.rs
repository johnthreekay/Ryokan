//! The corpora as data for the parser's own tests: loaders, a snapshot of
//! every title's reading (the long-term regression check that replaced the
//! differential against the old parsers), and two debugging aids.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::parse;

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

/// Titles whose answers other tests check field by field.
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

/// The unverified Nyaa listing sample.
pub(super) fn sample_titles() -> Vec<String> {
    load(&fixtures().join("nyaa-titles/sample/listing-sample.json"))
        .as_array()
        .expect("sample array")
        .iter()
        .filter_map(|r| r["title"].as_str().map(String::from))
        .collect()
}

/// `1-12` for a run, `1,3,5` otherwise.
fn episodes_text(title: &super::ParsedTitle) -> String {
    let mut eps: Vec<i32> = title.release_episodes().into_iter().collect();
    eps.sort_unstable();
    match eps.as_slice() {
        [] => "-".into(),
        [one] => one.to_string(),
        [first, .., last] if (last - first) as usize + 1 == eps.len() => format!("{first}-{last}"),
        _ => eps.iter().map(i32::to_string).collect::<Vec<_>>().join(","),
    }
}

/// One line per title: what every reader takes from it.
fn snapshot_line(title: &str) -> String {
    let p = parse(title);
    let mut flags = Vec::new();
    for (on, name) in [
        (p.is_batch(), "batch"),
        (p.special, "special"),
        (p.extra, "extra"),
        (p.half_episode, "half"),
        (p.single_file, "file"),
    ] {
        if on {
            flags.push(name);
        }
    }
    format!(
        "{title}\n    eps={} season={} group={} res={} amb={} [{}] series={}",
        episodes_text(&p),
        p.season_hint().map_or("-".into(), |s| s.to_string()),
        p.group.as_deref().unwrap_or("-"),
        p.resolution.map_or("-".into(), |r| r.to_string()),
        p.ambiguous_number.map_or("-".into(), |n| n.to_string()),
        flags.join(" "),
        p.series_title.as_deref().unwrap_or("-"),
    )
}

/// Every corpus title's reading. A change here is a behavior change:
/// review the diff (`INSTA_UPDATE=always cargo t release_title::corpus`
/// rewrites it) and pin anything deliberate with a corpus case.
#[test]
fn every_corpus_title_reads_as_snapshotted() {
    let mut titles: Vec<String> = verified_titles().into_iter().collect();
    titles.extend(sample_titles());
    titles.sort();
    titles.dedup();
    let lines: Vec<String> = titles.iter().map(|t| snapshot_line(t)).collect();
    insta::assert_snapshot!("release_title_corpus", lines.join("\n"));
}

/// `RT_TITLES=$'title one\ntitle two' cargo nextest run --features
/// test-support --lib release_title::corpus::dump --run-ignored only
/// --no-capture` prints each title's parse.
#[test]
#[ignore = "debugging aid: RT_TITLES"]
fn dump() {
    let titles = std::env::var("RT_TITLES").unwrap_or_default();
    for title in titles.lines().filter(|t| !t.trim().is_empty()) {
        eprintln!(
            "== {}\n   {:?}",
            snapshot_line(title),
            parse(title).episodes
        );
    }
}

/// Prints the file reading of each line of `RT_FILES` (`name` or
/// `name<TAB>alias|alias`, the aliases giving the series-aware reading
/// too).
#[test]
#[ignore = "debugging aid: RT_FILES"]
fn file_dump() {
    let files = std::env::var("RT_FILES").unwrap_or_default();
    for line in files.lines().filter(|t| !t.trim().is_empty()) {
        let (name, aliases) = line.split_once('\t').unwrap_or((line, ""));
        let lower = name.to_lowercase();
        let new = crate::services::media::parse_episode_span(&lower);
        eprintln!("== {name}\n   new={new:?}");
        if !aliases.is_empty() {
            let aliases: Vec<&str> = aliases.split('|').collect();
            let anchors = super::SeriesAnchors::new(&aliases);
            let named = crate::services::media::parse_episode_span_for(&lower, &anchors);
            eprintln!("   for={named:?}");
        }
    }
}
