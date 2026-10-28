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
//! The first tests check that every corpus file loads and is well formed;
//! the rest hold `release_title::parse` to each corpus.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use ryokan::services::release_title::{self, ParsedTitle, SeriesAnchors};
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
        if path.ends_with("ryokan-divergences.json") {
            continue;
        }
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
        if path.ends_with("divergences.json") {
            continue;
        }
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

// ── Parser assertions ──────────────────────────────────────────────────

fn episode_set(value: &Value) -> HashSet<i32> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_i64())
            .map(|v| v as i32)
            .collect(),
        Value::Object(range) => {
            let first = range["first"].as_i64().unwrap_or(0) as i32;
            let last = range["last"].as_i64().unwrap_or(0) as i32;
            (first..=last).collect()
        }
        _ => HashSet::new(),
    }
}

fn describe_claim(parsed: &ParsedTitle) -> String {
    match &parsed.episodes {
        None => "none".into(),
        Some(c) if !c.list.is_empty() => format!("{:?}", c.list),
        Some(c) if c.first == c.last => format!("{}", c.first),
        Some(c) => format!("{}-{}", c.first, c.last),
    }
}

fn describe_expected_claim(value: Option<&Value>) -> String {
    match value {
        None => "none".into(),
        Some(Value::Array(items)) if items.len() == 1 => items[0].to_string(),
        Some(Value::Array(items)) => format!(
            "{:?}",
            items.iter().filter_map(Value::as_i64).collect::<Vec<_>>()
        ),
        Some(Value::Object(r)) if r["first"] == r["last"] => r["first"].to_string(),
        Some(Value::Object(r)) => format!("{}-{}", r["first"], r["last"]),
        Some(other) => other.to_string(),
    }
}

/// Every field of one hand-verified expectation that the parse misses.
fn mismatches(parsed: &ParsedTitle, case: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let expect = &case["expect"];
    let mut check = |field: &str, got: String, want: String| {
        if got != want {
            out.push(format!("{field}: got {got}, want {want}"));
        }
    };
    check(
        "batch",
        parsed.is_batch().to_string(),
        expect["batch"].as_bool().unwrap_or(false).to_string(),
    );
    if let Some(group) = expect.get("group") {
        check(
            "group",
            format!("{:?}", parsed.group),
            format!("{:?}", group.as_str().map(String::from)),
        );
    }
    let want_seasons: Vec<i64> = expect
        .get("seasons")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let got_seasons: Vec<i64> = parsed.seasons.numbers.iter().map(|&n| n as i64).collect();
    check(
        "seasons",
        format!("{got_seasons:?}"),
        format!("{want_seasons:?}"),
    );
    if let Some(part) = expect.get("season_part") {
        check(
            "season_part",
            format!("{:?}", parsed.seasons.part),
            format!("{:?}", part.as_i64().map(|p| p as i32)),
        );
    }
    match expect.get("episodes") {
        Some(Value::Null) => {}
        claim => check(
            "episodes",
            describe_claim(parsed),
            describe_expected_claim(claim),
        ),
    }
    if let Some(amb) = expect.get("ambiguous") {
        check(
            "ambiguous",
            format!("{:?}", parsed.ambiguous_number),
            format!("{:?}", amb.as_i64().map(|v| v as i32)),
        );
    }
    for (field, got) in [
        ("special", parsed.special),
        ("includes_specials", parsed.includes_specials),
        ("extra", parsed.extra),
        ("half_episode", parsed.half_episode),
    ] {
        check(
            field,
            got.to_string(),
            expect[field].as_bool().unwrap_or(false).to_string(),
        );
    }
    check(
        "version",
        parsed.revision.version.to_string(),
        expect["version"].as_u64().unwrap_or(1).to_string(),
    );
    if let Some(res) = expect.get("resolution") {
        check(
            "resolution",
            format!("{:?}", parsed.resolution),
            format!("{:?}", res.as_u64().map(|v| v as u32)),
        );
    }
    if let Some(year) = expect.get("year") {
        check(
            "year",
            format!("{:?}", parsed.year),
            format!("{:?}", year.as_i64().map(|v| v as i32)),
        );
    }
    if let Some(series) = case.get("series") {
        let aliases: Vec<String> = series["aliases"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut got: Vec<i32> = parsed
            .for_series(&SeriesAnchors::new(&aliases))
            .episodes
            .into_iter()
            .collect();
        got.sort_unstable();
        let mut want: Vec<i32> = episode_set(&series["episodes"]).into_iter().collect();
        want.sort_unstable();
        check("for_series", format!("{got:?}"), format!("{want:?}"));
    }
    out
}

#[test]
fn parser_matches_the_hand_verified_corpus() {
    let dir = fixture_dir("nyaa-titles");
    let mut failures = Vec::new();
    let mut total = 0;
    for path in json_files(&dir) {
        let doc = load(&path);
        let bucket = doc["bucket"].as_str().unwrap_or_default().to_string();
        for case in cases(&doc, &path) {
            total += 1;
            let title = case["title"].as_str().expect("title");
            let parsed = release_title::parse(title);
            let missed = mismatches(&parsed, &case);
            if !missed.is_empty() {
                failures.push(format!("{bucket}: {title}\n    {}", missed.join("\n    ")));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} hand-verified titles parse wrong:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The episodes a multi-episode title holds around a clicked episode, the
/// rule `grab::held_episodes` pins: the claimed run when it holds the
/// episode and spans at most `MAX_FILE_SPAN`, else that episode alone.
fn held_episodes(parsed: &ParsedTitle, episode: i32) -> Vec<i32> {
    match &parsed.episodes {
        Some(c)
            if c.list.is_empty()
                && c.first <= episode
                && episode <= c.last
                && c.last - c.first < ryokan::services::media::MAX_FILE_SPAN =>
        {
            (c.first..=c.last).collect()
        }
        _ => vec![episode],
    }
}

/// What the new parser answers for one regression case, in the shape of
/// that case's `expect`.
fn regression_answer(case: &Value) -> Value {
    let func = case["fn"].as_str().unwrap_or_default();
    let title = case["title"].as_str().unwrap_or_default();
    let parsed = release_title::parse(title);
    let sorted = |set: HashSet<i32>| {
        let mut v: Vec<i32> = set.into_iter().collect();
        v.sort_unstable();
        v
    };
    if func.ends_with("detect_batch") || func.ends_with("detect_batch_from_title") {
        return Value::Bool(parsed.is_batch());
    }
    if func.ends_with("extract_group") || func.ends_with("extract_group_from_title") {
        return Value::String(parsed.group.unwrap_or_default());
    }
    if func.ends_with("extract_resolution") || func.ends_with("extract_resolution_from_title") {
        return Value::String(parsed.resolution.map(|r| r.to_string()).unwrap_or_default());
    }
    match func {
        "release_parse::parse_release_season" => Value::from(parsed.season_hint().unwrap_or(0)),
        "release_parse::season_mismatch" => Value::Bool(
            parsed.conflicts_with_season(case["expected_season"].as_i64().unwrap_or(1) as i32),
        ),
        "rss::parse_release.season_hint" => Value::from(parsed.season_hint()),
        "release_parse::parse_release_numbers.contains"
        | "rss::parse_release.episodes.contains" => {
            let got = parsed.release_episodes();
            let want: Vec<i32> = serde_json::from_value(case["expect"].clone()).unwrap_or_default();
            if want.iter().all(|e| got.contains(e)) {
                case["expect"].clone()
            } else {
                Value::from(sorted(got))
            }
        }
        "release_parse::parse_release_numbers.excludes" => {
            let got = parsed.release_episodes();
            let want: Vec<i32> = serde_json::from_value(case["expect"].clone()).unwrap_or_default();
            if want.iter().all(|e| !got.contains(e)) {
                case["expect"].clone()
            } else {
                Value::from(sorted(got))
            }
        }
        "rss::parse_release.episodes" => Value::from(sorted(parsed.release_episodes())),
        "grab::held_episodes" => Value::from(held_episodes(
            &parsed,
            case["episode"].as_i64().unwrap_or(0) as i32,
        )),
        "release_parse::episode_after_series_name"
        | "release_parse::parse_release_numbers_for_series"
        | "rss::best_series_match.resolved_eps" => {
            let aliases: Vec<String> =
                serde_json::from_value(case["aliases"].clone()).unwrap_or_default();
            let got = sorted(parsed.for_series(&SeriesAnchors::new(&aliases)).episodes);
            match &case["expect"] {
                Value::Array(_) => Value::from(got),
                Value::Null => match got.as_slice() {
                    [] => Value::Null,
                    _ => Value::from(got),
                },
                _ => match got.as_slice() {
                    [one] => Value::from(*one),
                    _ => Value::from(got),
                },
            }
        }
        other => panic!("no mapping for regression fn {other}"),
    }
}

#[test]
fn parser_keeps_the_regression_literals() {
    let dir = fixture_dir("release-title-regressions");
    let divergences: BTreeMap<(String, String), String> =
        load(&dir.join("divergences.json"))["cases"]
            .as_array()
            .expect("divergences cases")
            .iter()
            .map(|d| {
                (
                    (
                        d["fn"].as_str().unwrap().to_string(),
                        d["title"].as_str().unwrap().to_string(),
                    ),
                    d["reason"].as_str().unwrap().to_string(),
                )
            })
            .collect();
    let mut failures = Vec::new();
    let mut seen_divergences = BTreeSet::new();
    for path in json_files(&dir) {
        if path.file_name().is_some_and(|n| n == "divergences.json") {
            continue;
        }
        let doc = load(&path);
        for case in cases(&doc, &path) {
            let func = case["fn"].as_str().unwrap().to_string();
            let title = case["title"].as_str().unwrap().to_string();
            let got = regression_answer(&case);
            let key = (func.clone(), title.clone());
            let differs = got != case["expect"];
            match (differs, divergences.contains_key(&key)) {
                (true, false) => failures.push(format!(
                    "{func} {title:?}: got {got}, the old test asserted {}",
                    case["expect"]
                )),
                (false, true) => failures.push(format!(
                    "{func} {title:?}: listed as a divergence but now matches; drop the entry"
                )),
                (true, true) => {
                    seen_divergences.insert(key);
                }
                (false, false) => {}
            }
        }
    }
    for key in divergences.keys() {
        if !seen_divergences.contains(key) && !failures.iter().any(|f| f.contains(&key.1)) {
            failures.push(format!(
                "{} {:?}: divergence entry matches no case",
                key.0, key.1
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One Sonarr expectation field the parse misses, as `field: got, want`.
fn sonarr_mismatches(case: &Value) -> Vec<String> {
    let title = case["title"].as_str().unwrap_or_default();
    let sonarr = &case["sonarr"];
    let parsed = release_title::parse(title);
    let scene = case["scope"].as_str() == Some("scene");
    let mut out = Vec::new();
    let mut check = |field: &str, got: String, want: String| {
        if got != want {
            out.push(format!("{field}: got {got}, want {want}"));
        }
    };
    let mut seasons: Vec<i64> = sonarr
        .get("seasons")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let episodes: Vec<i64> = sonarr
        .get("episodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    // Sonarr reports season 0 for an absolute-only title.
    if seasons == [0] && episodes.is_empty() && sonarr["special"] != Value::Bool(true) {
        seasons.clear();
    }
    if sonarr.get("seasons").is_some() {
        let got: Vec<i64> = parsed.seasons.numbers.iter().map(|&n| n as i64).collect();
        check("seasons", format!("{got:?}"), format!("{seasons:?}"));
    }
    if let Some(full) = sonarr["full_season"].as_bool() {
        // A partial season (`S03 Part 2`) is a pack but not a full season.
        let got = parsed.is_batch() && parsed.episodes.is_none() && parsed.seasons.part.is_none();
        check("full_season", got.to_string(), full.to_string());
    }
    if let Some(multi) = sonarr["multi_season"].as_bool() {
        check(
            "multi_season",
            (parsed.seasons.numbers.len() > 1).to_string(),
            multi.to_string(),
        );
    }
    if let Some(part) = sonarr["partial_season"].as_i64() {
        check(
            "partial_season",
            format!("{:?}", parsed.seasons.part),
            format!("Some({part})"),
        );
    }
    if scene {
        return out;
    }
    let absolute: Vec<i64> = sonarr
        .get("absolute")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if sonarr.get("absolute").is_some() || sonarr.get("episodes").is_some() {
        let mut want: Vec<i64> = if episodes.is_empty() {
            absolute
        } else {
            episodes
        };
        want.sort_unstable();
        let aliases: Vec<String> = sonarr["series_title"]
            .as_str()
            .map(|t| vec![t.to_string()])
            .unwrap_or_default();
        let mut got: Vec<i64> = match &parsed.episodes {
            Some(claim) => claim.episodes().into_iter().map(i64::from).collect(),
            None => parsed
                .for_series(&SeriesAnchors::new(&aliases))
                .episodes
                .into_iter()
                .map(i64::from)
                .collect(),
        };
        got.sort_unstable();
        check("episodes", format!("{got:?}"), format!("{want:?}"));
    }
    if let Some(special) = sonarr["special"].as_bool() {
        check("special", parsed.special.to_string(), special.to_string());
    }
    if let Some(recap) = sonarr.get("special_absolute").and_then(Value::as_array) {
        check(
            "half_episode",
            parsed.half_episode.to_string(),
            (!recap.is_empty()).to_string(),
        );
    }
    if let Some(group) = sonarr.get("group") {
        check(
            "group",
            format!("{:?}", parsed.group),
            format!("{:?}", group.as_str().map(String::from)),
        );
    }
    if let Some(hash) = sonarr.get("hash") {
        check(
            "hash",
            format!("{:?}", parsed.hash),
            format!("{:?}", hash.as_str().map(str::to_uppercase)),
        );
    }
    if let Some(version) = sonarr["version"].as_u64() {
        check(
            "version",
            parsed.revision.version.to_string(),
            version.to_string(),
        );
    }
    out
}

#[test]
fn parser_agrees_with_sonarr_or_says_why_not() {
    let dir = fixture_dir("sonarr-parser");
    let divergences: BTreeMap<String, String> = load(&dir.join("ryokan-divergences.json"))["cases"]
        .as_array()
        .expect("divergence cases")
        .iter()
        .map(|d| {
            (
                d["title"].as_str().unwrap().to_string(),
                d["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut failures = Vec::new();
    let mut matched = BTreeSet::new();
    for path in json_files(&dir) {
        if path
            .file_name()
            .is_some_and(|n| n == "ryokan-divergences.json")
        {
            continue;
        }
        let doc = load(&path);
        for case in cases(&doc, &path) {
            let title = case["title"].as_str().unwrap().to_string();
            let missed = sonarr_mismatches(&case);
            match (missed.is_empty(), divergences.contains_key(&title)) {
                (false, false) => failures.push(format!("{title}\n    {}", missed.join("\n    "))),
                (true, true) => {
                    if !matched.contains(&title) {
                        failures.push(format!(
                            "{title}\n    listed as a divergence but agrees now"
                        ));
                    }
                }
                (false, true) => {
                    matched.insert(title);
                }
                (true, false) => {}
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} Sonarr cases disagree:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every title in every corpus, including the unverified listing sample.
fn all_titles() -> Vec<String> {
    let mut titles = Vec::new();
    for path in json_files(&fixture_dir("nyaa-titles")) {
        for case in cases(&load(&path), &path) {
            titles.push(case["title"].as_str().unwrap().to_string());
        }
    }
    let sample = load(&fixture_dir("nyaa-titles").join("sample/listing-sample.json"));
    for row in sample.as_array().unwrap() {
        titles.push(row["title"].as_str().unwrap().to_string());
    }
    for dir in ["sonarr-parser", "release-title-regressions"] {
        for path in json_files(&fixture_dir(dir)) {
            if let Some(list) = load(&path)["cases"].as_array() {
                titles.extend(
                    list.iter()
                        .filter_map(|c| c["title"].as_str().map(String::from)),
                );
            }
        }
    }
    titles
}

/// Readings agree with each other on every corpus title:
/// - a single-episode claim is never a batch unless a keyword says so;
/// - a special, extra or half episode names no slot;
/// - a bare trailing number recorded as ambiguous never appears in the
///   release reading;
/// - the year is never an episode.
#[test]
fn readings_are_consistent_on_every_corpus_title() {
    let mut failures = Vec::new();
    for title in all_titles() {
        let parsed = release_title::parse(&title);
        let episodes = parsed.release_episodes();
        if let Some(claim) = &parsed.episodes
            && !claim.spans_several()
            && !parsed.batch_keyword
            && parsed.is_batch()
        {
            failures.push(format!("single-episode claim reads as batch: {title}"));
        }
        if parsed.names_no_slot() && !episodes.is_empty() {
            failures.push(format!("no-slot release names episodes: {title}"));
        }
        if let Some(amb) = parsed.ambiguous_number
            && episodes.contains(&amb)
        {
            failures.push(format!("ambiguous number in release episodes: {title}"));
        }
        if let Some(year) = parsed.year
            && episodes.contains(&year)
        {
            failures.push(format!("year read as an episode: {title}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
