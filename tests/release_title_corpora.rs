//! Corpora for `services::release_title`, the release-title and file-name
//! parser that replaces the old per-module parsers.
//!
//! - `tests/fixtures/nyaa-titles/`: real Nyaa release titles in buckets, each
//!   with a hand-verified expectation (see the README there).
//! - `tests/fixtures/sonarr-parser/`: Sonarr's parser test cases, ported with
//!   Sonarr's own expected values.
//! - `tests/fixtures/release-title-regressions/`: every title the old
//!   parsers' inline tests pinned, with the outcome each asserted.
//! - `tests/fixtures/animetosho-corpus/`: 29,393 titles and 3,007 torrent
//!   file lists from AnimeTosho's feed, and 1,000 of the titles labeled
//!   blind (see the README there). Its tests hold today's numbers as floors.
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

/// The rows of a gzipped JSON-lines file in the AnimeTosho corpus.
fn animetosho(name: &str) -> Vec<Value> {
    use std::io::BufRead;
    let path = fixture_dir("animetosho-corpus").join(name);
    let file =
        std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    std::io::BufReader::new(flate2::read::GzDecoder::new(file))
        .lines()
        .map(|line| serde_json::from_str(&line.expect("read line")).expect("JSON line"))
        .collect()
}

fn animetosho_labels() -> Vec<Value> {
    let path = fixture_dir("animetosho-corpus").join("labels/labels.jsonl");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON line"))
        .collect()
}

/// Every title in every corpus, including the unverified listing sample
/// and the AnimeTosho corpus.
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
    titles.extend(
        animetosho("titles.jsonl.gz")
            .iter()
            .filter_map(|r| r["title"].as_str().map(String::from)),
    );
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

// ── The AnimeTosho corpus ────────────────────────────────────────────
//
// Each test below holds a number measured on 2026-10-28 as a floor (or a
// ceiling). A parser change that moves one prints what moved; if the move
// is an improvement, raise the floor; if it is a deliberate trade, say so
// in the commit and lower it.

#[test]
fn animetosho_corpus_is_well_formed() {
    let titles = animetosho("titles.jsonl.gz");
    let names: HashSet<&str> = titles
        .iter()
        .map(|r| r["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles.len(), 29_393);
    assert_eq!(names.len(), titles.len(), "titles are distinct");
    let lists = animetosho("file-lists.jsonl.gz");
    assert_eq!(lists.len(), 3_007);
    for list in &lists {
        assert!(
            list["at_id"].as_i64().is_some() && list["title"].is_string(),
            "{list}"
        );
        // 570 torrents have no file list on AnimeTosho; their rows stay.
        assert!(list["files"].is_array(), "{list}");
    }
    let labels = animetosho_labels();
    assert_eq!(labels.len(), 1_000);
    for label in &labels {
        let title = label["title"].as_str().expect("title");
        assert!(
            names.contains(title),
            "label for a title not in the corpus: {title}"
        );
        for key in [
            "batch",
            "episodes",
            "ambiguous",
            "season",
            "special",
            "extra",
            "group",
            "resolution",
        ] {
            assert!(label.get(key).is_some(), "{key} missing: {title}");
        }
    }
}

/// The parser's reading of one title in the labels' terms.
fn label_view(parsed: &ParsedTitle) -> BTreeMap<&'static str, Value> {
    let mut eps: Vec<i32> = parsed.release_episodes().into_iter().collect();
    eps.sort_unstable();
    BTreeMap::from([
        ("batch", Value::from(parsed.is_batch())),
        ("episodes", Value::from(eps)),
        ("ambiguous", serde_json::json!(parsed.ambiguous_number)),
        ("season", serde_json::json!(parsed.season_hint())),
        ("special", Value::from(parsed.special)),
        ("extra", Value::from(parsed.extra)),
        (
            "group",
            Value::from(parsed.group.as_deref().unwrap_or("").trim().to_lowercase()),
        ),
        ("resolution", serde_json::json!(parsed.resolution)),
    ])
}

/// One label in the same terms: a long range written as its two ends
/// (`"unsure": "range"`, the guide's rule past 30 episodes) expanded.
fn label_answer(label: &Value) -> BTreeMap<&'static str, Value> {
    let mut eps: Vec<i64> = label["episodes"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if label["unsure"].as_str().unwrap_or("").contains("range")
        && let [a, b] = eps[..]
        && b - a > 1
    {
        eps = (a..=b).collect();
    }
    eps.sort_unstable();
    eps.dedup();
    let group = label["group"].as_str().unwrap_or("").trim().to_lowercase();
    BTreeMap::from([
        ("batch", label["batch"].clone()),
        ("episodes", Value::from(eps)),
        ("ambiguous", label["ambiguous"].clone()),
        ("season", label["season"].clone()),
        ("special", label["special"].clone()),
        ("extra", label["extra"].clone()),
        ("group", Value::from(group)),
        ("resolution", label["resolution"].clone()),
    ])
}

/// 1,000 corpus titles were labeled blind, by readers who never saw a
/// parser's output (`labels/GUIDE.md`). Each field must agree on at least
/// as many titles as it did when the floor was set.
#[test]
fn parser_agrees_with_the_blind_labels_at_least_as_before() {
    const FLOORS: &[(&str, usize)] = &[
        ("batch", 926),
        ("episodes", 983),
        ("ambiguous", 991),
        ("season", 993),
        ("special", 993),
        ("extra", 997),
        ("group", 981),
        ("resolution", 998),
    ];
    let labels = animetosho_labels();
    let mut agree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut misses: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for label in &labels {
        let title = label["title"].as_str().unwrap();
        let got = label_view(&release_title::parse(title));
        let want = label_answer(label);
        for (field, value) in &got {
            if want.get(field) == Some(value) {
                *agree.entry(field).or_default() += 1;
            } else {
                misses.entry(field).or_default().push(format!(
                    "    {title}\n      parser {value}, label {}",
                    want.get(field).cloned().unwrap_or(Value::Null)
                ));
            }
        }
    }
    eprintln!("agreement with {} labels: {agree:?}", labels.len());
    let mut failures = Vec::new();
    for (field, floor) in FLOORS {
        let count = agree.get(field).copied().unwrap_or(0);
        if count < *floor {
            failures.push(format!(
                "{field}: {count} titles agree, floor {floor}; disagreements:\n{}",
                misses.get(field).map(|m| m.join("\n")).unwrap_or_default()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// AnimeTosho's file count says what a title is: a one-file torrent is
/// not a batch, and most torrents of ten or more files are. Titles that
/// name no episodes (`[UTW] Angel Beats! [BD]`) are most of the packs the
/// flag misses; only the interactive batch list offers those.
#[test]
fn batch_flag_tracks_file_counts() {
    const ONE_FILE_BATCHES_MAX: usize = 67;
    const TEN_PLUS_BATCHES_MIN: usize = 4_484;
    let (mut one_file, mut ten_plus) = (Vec::new(), (0usize, 0usize));
    for row in animetosho("titles.jsonl.gz") {
        let Some(files) = row["num_files"].as_i64() else {
            continue;
        };
        let title = row["title"].as_str().unwrap();
        let batch = release_title::parse(title).is_batch();
        if files == 1 && batch {
            one_file.push(title.to_string());
        }
        if files >= 10 {
            ten_plus.0 += 1;
            ten_plus.1 += usize::from(batch);
        }
    }
    eprintln!(
        "one-file titles read as batches: {}; titles of 10+ files read as batches: {} of {}",
        one_file.len(),
        ten_plus.1,
        ten_plus.0
    );
    assert!(
        one_file.len() <= ONE_FILE_BATCHES_MAX,
        "{} one-file titles read as batches (at most {ONE_FILE_BATCHES_MAX}):\n{}",
        one_file.len(),
        one_file.join("\n")
    );
    assert!(
        ten_plus.1 >= TEN_PLUS_BATCHES_MIN,
        "{} of {} titles of 10+ files read as batches (at least {TEN_PLUS_BATCHES_MIN})",
        ten_plus.1,
        ten_plus.0
    );
}

/// The file lists, read the way the import reads a download: every video
/// that looks like an episode, plainly; and per pack, with the torrent
/// title's show name as the series' name (as `parse_file_path_for` gets
/// it from the series row), which files would claim one episode twice
/// (the import refuses such a pack) and whether an extra takes a slot.
#[test]
fn file_lists_read_as_before() {
    use ryokan::services::media::{parse_file_path, parse_file_path_for};
    const EPISODE_LIKE_READ_MIN: usize = 27_171;
    const PACKS_WITH_A_TIE_MAX: usize = 67;
    let video = regex_lite::Regex::new(
        r"(?i)\.(mkv|mp4|avi|ts|m2ts|webm|wmv|flv|ogm|mov|m4v|rmvb|mpg|mpeg)$",
    )
    .unwrap();
    // A name that might not be an episode; such files are not counted.
    let maybe_extra = regex_lite::Regex::new(
        r"(?i)(\bnc(op|ed)|creditless|textless|\bpv\d*\b|\bcm\d*\b|preview|trailer|teaser|\bmenu|\bop\d*\b|\bed\d*\b|opening|ending|bonus|extra|special|\bova\b|\boad\b|\bsp\d*\b|recap|movie|film|劇場版|特典|tokuten|\bmv\b|music|clip|promo|spot|/(extras?|specials?|bonus|sps?|menus?|scans?)/)",
    )
    .unwrap();
    // A name that says it is an extra (a CRC like `[ED509059]` is not `ED`).
    let crc = regex_lite::Regex::new(r"[\[(][0-9A-Fa-f]{8}[\])]").unwrap();
    let extra_word = regex_lite::Regex::new(
        r"(?i)\bnc(op|ed)\d*\b|creditless|textless|\bpv\d*\b|\bcm\d*\b|\bpreview|\btrailer|\bteaser|\bmenu\d*\b|\b(op|ed)\d*\b",
    )
    .unwrap();
    let (mut episode_like, mut read) = (0usize, 0usize);
    let mut tied = Vec::new();
    let mut extras_with_a_slot = Vec::new();
    for list in animetosho("file-lists.jsonl.gz") {
        let torrent = list["title"].as_str().unwrap();
        let name = release_title::parse(torrent)
            .series_title
            .unwrap_or_default();
        let anchors = SeriesAnchors::new(&[name.as_str()]);
        let mut slots: BTreeMap<(Option<i32>, i32), usize> = BTreeMap::new();
        let mut videos = 0;
        for file in list["files"].as_array().unwrap() {
            let path = file[0].as_str().unwrap();
            if !video.is_match(path) {
                continue;
            }
            videos += 1;
            let base = path.rsplit('/').next().unwrap_or(path);
            if !maybe_extra.is_match(path) {
                episode_like += 1;
                read += usize::from(parse_file_path(path).is_some());
            }
            let Some(span) = parse_file_path_for(path, &anchors).filter(|s| !s.special) else {
                continue;
            };
            // regex-lite's `\b` sees `é` as a non-letter (`Sautéed` would
            // hold a word `ed`), so other letters are written as `x`.
            let ascii: String = crc
                .replace_all(base, "")
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() && !c.is_ascii() {
                        'x'
                    } else {
                        c
                    }
                })
                .collect();
            if extra_word.is_match(&ascii) {
                extras_with_a_slot.push(format!("{path} -> {span:?}"));
            }
            for e in span.first..=span.last {
                *slots.entry((span.season, e)).or_default() += 1;
            }
        }
        if videos >= 2 && slots.values().any(|&n| n > 1) {
            tied.push(torrent.to_string());
        }
    }
    eprintln!(
        "episode-like videos read: {read} of {episode_like}; packs with a tie: {}; extras with a slot: {}",
        tied.len(),
        extras_with_a_slot.len()
    );
    assert!(
        extras_with_a_slot.is_empty(),
        "files whose names say they are extras take an episode slot:\n{}",
        extras_with_a_slot.join("\n")
    );
    assert!(
        read >= EPISODE_LIKE_READ_MIN,
        "{read} of {episode_like} episode-like videos read (at least {EPISODE_LIKE_READ_MIN})"
    );
    assert!(
        tied.len() <= PACKS_WITH_A_TIE_MAX,
        "{} packs claim one episode twice (at most {PACKS_WITH_A_TIE_MAX}):\n{}",
        tied.len(),
        tied.join("\n")
    );
}

/// Ryokan's default episode name, written for every show name in the
/// corpus (an extras release's name is no show's), reads back as written
/// through the library scanner's reader, whatever the episode is called.
#[test]
fn default_episode_names_read_back_for_every_corpus_show() {
    use ryokan::services::media::parse_episode_span;
    use ryokan::services::naming::{self, NameContext, TemplateKind};
    const EPISODE_TITLES: &[&str] = &[
        "",
        "2.5 Dimensional Ririsa",
        "Episode 10.5",
        "Final",
        "Preview",
    ];
    let mut shows: Vec<String> = animetosho("titles.jsonl.gz")
        .iter()
        .map(|r| release_title::parse(r["title"].as_str().unwrap()))
        .filter(|p| !p.extra)
        .filter_map(|p| p.series_title)
        // `Lucky Star OVA OP` reads as an extras release itself.
        .filter(|name| !release_title::parse(name).extra)
        .collect();
    shows.sort();
    shows.dedup();
    let mut failures = Vec::new();
    for show in &shows {
        for title in EPISODE_TITLES {
            let ctx = NameContext {
                series_title: show.clone(),
                season_number: 1,
                episode_number: 5,
                episode_title: title.to_string(),
                ext: "mkv".to_string(),
                ..Default::default()
            };
            let Ok(rendered) = naming::render(
                TemplateKind::EpisodeFile,
                naming::DEFAULT_EPISODE_FILE_FORMAT,
                &ctx,
            ) else {
                continue;
            };
            let span = parse_episode_span(&rendered.name.to_lowercase());
            if !span.is_some_and(|s| s.season == Some(1) && s.first == 5 && s.last == 5) {
                failures.push(format!("{} -> {span:?}", rendered.name));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} show names write a name that does not read back:\n{}",
        failures.len(),
        shows.len(),
        failures[..failures.len().min(40)].join("\n")
    );
}

// ── Building the AnimeTosho corpus ───────────────────────────────────

mod scrape {
    //! Scrapes the AnimeTosho corpus (`scrape_animetosho_corpus`, below).

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    const AT_FEED: &str = "https://feed.animetosho.net/json";
    const NYAA_RSS: &str = "https://nyaa.si/?page=rss&";
    const USER_AGENT: &str =
        "Ryokan/0.1 (release-title corpus; contact via github.com/johnthreekay/Ryokan)";
    const DELAY: Duration = Duration::from_millis(1500);

    /// AnimeTosho searches beside its recent pages: batch and BD groups,
    /// CJK groups, title shapes, long-runners and numeric titles.
    const AT_QUERIES: &[&str] = &[
        "VCB-Studio",
        "MTBB",
        "Kaizoku",
        "Judas",
        "Anime Time",
        "[DB]",
        "Beatrice-Raws",
        "Moozzi2",
        "ReinForce",
        "Coalgirls",
        "Commie",
        "HorribleSubs Batch",
        "EMBER",
        "smol",
        "Okay-Subs",
        "Erai-raws BATCH",
        "SubsPlease Batch",
        "Exiled-Destiny",
        "DKB",
        "ASW",
        "Yameii",
        "ToonsHub",
        "VARYG",
        "Tsundere-Raws",
        "Sokudo",
        "Arid",
        "Kametsu",
        "Doki",
        "gg",
        "Underwater",
        "UTW",
        "Chihiro",
        "FFF",
        "Mezashite",
        "THORA",
        "Cleo",
        "iAHD",
        "CyC",
        "Vodes",
        "LostYears",
        "Anime Chap",
        "Breeze",
        "Moxie",
        "Yousei-raws",
        "Snow-Raws",
        "LowPower-Raws",
        "ANK-Raws",
        "Ohys-Raws",
        "Leopard-Raws",
        "NanakoRaws",
        "Prof",
        "BlurayDesuYo",
        "Hi10",
        "neoHEVC",
        "Cerberus",
        "AkihitoSubs",
        "Nep_Blanc",
        "GJM",
        "SallySubs",
        "Asenshi",
        "Golumpa",
        "Ember batch",
        "NC-Raws",
        "Lilith-Raws",
        "ANi",
        "EternalAnimes",
        "LoliHouse",
        "喵萌奶茶屋",
        "桜都字幕组",
        "千夏字幕组",
        "北宇治字幕组",
        "DBD-Raws",
        "Nekomoe kissaten",
        "Sakurato",
        "SweetSub",
        "豌豆字幕组",
        "极影字幕",
        "幻樱字幕组",
        "风车字幕组",
        "银色子弹字幕组",
        "动漫国字幕组",
        "澄空学园",
        "华盟字幕社",
        "MingY",
        "Airota",
        "FLsnow",
        "GM-Team",
        "jibaketa",
        "Batch 1080p",
        "Complete BD",
        "BDMV",
        "BDRip",
        "OVA",
        "OAD",
        "Movie BD",
        "Special",
        "S01-S03",
        "S01-S04",
        "Season 1-3",
        "DVD",
        "DVDRip",
        "v2",
        "NCOP",
        "NCED",
        "PV",
        "Preview",
        "Recap",
        "13.5",
        "Part 2",
        "Cour 2",
        "2nd Season",
        "3rd Season",
        "Final Season",
        "II",
        "III",
        "第二季",
        "合集",
        "Fin",
        "END",
        "Vol",
        "Remux",
        "Dual Audio",
        "Multi-Subs",
        "x265",
        "Hi10P",
        "720p",
        "480p",
        "SD",
        "Dub",
        "VOSTFR",
        "Legendado",
        "subita",
        "One Piece",
        "Detective Conan",
        "Gintama",
        "Naruto",
        "Bleach",
        "Doraemon",
        "Pokemon",
        "Dragon Ball",
        "Crayon Shin-chan",
        "Yu-Gi-Oh",
        "Precure",
        "Mob Psycho 100",
        "86",
        "Kaiju No. 8",
        "Chihayafuru",
        "Cyborg 009",
        "Steins Gate 0",
        "Zom 100",
        "Lupin III",
        "Monogatari",
        "Fate",
        "Gundam",
        "Re Zero",
        "Mushoku Tensei",
        "Overlord",
        "Danmachi",
        "Kaguya-sama",
        "Spy x Family",
        "Jujutsu Kaisen",
        "Attack on Titan",
        "Frieren",
        "Oshi no Ko",
        "Dandadan",
        "Ranma",
        "Hunter x Hunter",
        "Fullmetal Alchemist",
    ];

    /// Nyaa RSS (category, query) for the raw and non-English categories
    /// AnimeTosho does not mirror as fully.
    const NYAA_QUERIES: &[(&str, &str)] = &[
        ("1_3", ""),
        ("1_4", ""),
        ("1_3", "1080p"),
        ("1_4", "1080p"),
        ("1_3", "VOSTFR"),
        ("1_3", "Legendado"),
        ("1_3", "Rus"),
        ("1_3", "Sub Ita"),
        ("1_3", "Batch"),
        ("1_4", "BDMV"),
        ("1_4", "Batch"),
        ("1_4", "Ohys-Raws"),
        ("1_4", "Leopard-Raws"),
        ("1_3", "简繁"),
        ("1_3", "繁體"),
        ("1_3", "BIG5"),
        ("1_3", "GB"),
        ("1_3", "合集"),
        ("1_1", ""),
        ("1_1", "AMV"),
        ("1_2", "DVD"),
        ("1_2", "TV-Rip"),
    ];

    /// File lists per AnimeTosho source: (article, packs, single files).
    const QUOTAS: &[(&str, usize, usize)] = &[("Nyaa", 1800, 500), ("nBT", 1000, 300)];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(super) struct Scraper {
        client: reqwest::Client,
        cache: PathBuf,
        failures: u32,
    }

    impl Scraper {
        pub(super) fn new(cache: PathBuf) -> Self {
            std::fs::create_dir_all(cache.join("listing")).expect("create cache");
            std::fs::create_dir_all(cache.join("details")).expect("create cache");
            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(60))
                .build()
                .expect("HTTP client");
            Scraper {
                client,
                cache,
                failures: 0,
            }
        }

        /// One polite GET: a pause before every request; a 403 / 429 / 5xx
        /// or a network error waits and retries once; three failed URLs in
        /// a row stop the run. `None` for a page that is not there.
        async fn get(&mut self, url: &str) -> Option<String> {
            for _ in 0..2 {
                tokio::time::sleep(DELAY).await;
                match self.client.get(url).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        if status.is_success() {
                            self.failures = 0;
                            return Some(body);
                        }
                        // AnimeTosho's feed stops paging past its history.
                        if status.as_u16() == 403 && body.contains("history_limit") {
                            return None;
                        }
                        let excerpt: String = body.chars().take(200).collect();
                        eprintln!("  HTTP {status} {url}: {excerpt}");
                        if matches!(status.as_u16(), 403 | 429) || status.is_server_error() {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            continue;
                        }
                        return None;
                    }
                    Err(e) => {
                        eprintln!("  error {url}: {e}");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            }
            self.failures += 1;
            assert!(self.failures < 3, "three failures in a row; stopping");
            None
        }

        fn listing_path(&self, url: &str) -> PathBuf {
            let digest = Sha256::digest(url.as_bytes());
            self.cache
                .join("listing")
                .join(format!("{}.json", &hex(&digest)[..16]))
        }

        /// One AnimeTosho feed page, cached. A failed fetch is not cached.
        async fn at_page(&mut self, url: &str, query: &str) -> Vec<Value> {
            let path = self.listing_path(url);
            if let Ok(raw) = std::fs::read_to_string(&path) {
                return serde_json::from_str(&raw).expect("cached listing");
            }
            let Some(body) = self.get(url).await else {
                return Vec::new();
            };
            let entries: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
            let rows: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "src": "at", "query": query, "id": e["id"], "title": e["title"],
                        "num_files": e["num_files"], "nyaa_id": e["nyaa_id"],
                        "anidb_aid": e["anidb_aid"], "anidb_eid": e["anidb_eid"],
                        "size": e["total_size"], "ts": e["timestamp"],
                        "article": e["article_title"],
                    })
                })
                .collect();
            std::fs::write(&path, serde_json::to_string(&rows).unwrap()).expect("write cache");
            eprintln!("  {:3} {url}", rows.len());
            rows
        }

        /// One Nyaa RSS query through Ryokan's own feed reader, cached.
        async fn nyaa_feed(&mut self, category: &str, query: &str) {
            let url = format!(
                "{NYAA_RSS}c={}&q={}&f=0",
                urlencoding::encode(category),
                urlencoding::encode(query)
            );
            let path = self.listing_path(&url);
            if path.exists() {
                return;
            }
            tokio::time::sleep(DELAY).await;
            let items = match ryokan::services::rss::feed::fetch_user_feed(
                &url,
                ryokan::services::rss::RssSource::Nyaa,
            )
            .await
            {
                Ok(items) => items,
                Err(e) => {
                    eprintln!("  error {url}: {e}");
                    return;
                }
            };
            let rows: Vec<Value> = items
                .iter()
                .map(|item| {
                    json!({
                        "src": "nyaa", "query": format!("nyaa {category} {query}"),
                        "title": item.title,
                        "url": if item.guid.is_empty() { &item.link } else { &item.guid },
                        "category": category,
                    })
                })
                .collect();
            std::fs::write(&path, serde_json::to_string(&rows).unwrap()).expect("write cache");
            eprintln!("  {:3} {url}", rows.len());
        }

        /// AnimeTosho's recent pages and searches, then Nyaa RSS, merged
        /// into `titles.json` (distinct titles; Ryokan never searches
        /// Sukebei, so its entries are left out).
        pub(super) async fn listing(&mut self) {
            for page in 1..=100 {
                if self
                    .at_page(&format!("{AT_FEED}?page={page}"), "recent")
                    .await
                    .is_empty()
                {
                    break;
                }
            }
            for query in AT_QUERIES {
                for page in 1..=3 {
                    let url = format!("{AT_FEED}?q={}&page={page}", urlencoding::encode(query));
                    if self.at_page(&url, query).await.len() < 75 {
                        break;
                    }
                }
            }
            for (category, query) in NYAA_QUERIES {
                self.nyaa_feed(category, query).await;
            }
            let mut paths: Vec<_> = std::fs::read_dir(self.cache.join("listing"))
                .expect("listing dir")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            paths.sort();
            let mut seen = std::collections::HashSet::new();
            let mut rows = Vec::new();
            for path in paths {
                let page: Vec<Value> =
                    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                for row in page {
                    if row["article"] == "Sukebei" {
                        continue;
                    }
                    if seen.insert(row["title"].as_str().unwrap_or_default().to_string()) {
                        rows.push(row);
                    }
                }
            }
            std::fs::write(
                self.cache.join("titles.json"),
                serde_json::to_string(&rows).unwrap(),
            )
            .expect("write titles.json");
            eprintln!("{} distinct titles", rows.len());
        }

        /// File lists for a fixed pick of AnimeTosho entries: per source,
        /// packs of 2 to 400 files (at most six per AniDB series) and
        /// single files, in an order seeded by each entry's id.
        pub(super) async fn details(&mut self) {
            let raw = std::fs::read_to_string(self.cache.join("titles.json")).expect("titles.json");
            let mut rows: Vec<Value> = serde_json::from_str::<Vec<Value>>(&raw)
                .unwrap()
                .into_iter()
                .filter(|r| r["src"] == "at")
                .collect();
            rows.sort_by_cached_key(|r| Sha256::digest(format!("296:{}", r["id"]).as_bytes()));
            let files = |r: &Value| r["num_files"].as_i64().unwrap_or(0);
            let mut per_series: std::collections::HashMap<String, usize> = Default::default();
            let mut chosen: Vec<&Value> = Vec::new();
            for (source, packs, singles) in QUOTAS {
                let mine: Vec<&Value> = rows.iter().filter(|r| r["article"] == *source).collect();
                let mut taken = 0;
                for r in &mine {
                    if taken == *packs {
                        break;
                    }
                    if !(2..=400).contains(&files(r)) {
                        continue;
                    }
                    let series = match r["anidb_aid"].as_i64() {
                        Some(aid) => aid.to_string(),
                        None => r["title"].as_str().unwrap_or("").chars().take(20).collect(),
                    };
                    let n = per_series.entry(series).or_default();
                    if *n >= 6 {
                        continue;
                    }
                    *n += 1;
                    chosen.push(r);
                    taken += 1;
                }
                chosen.extend(mine.iter().filter(|r| files(r) == 1).take(*singles));
            }
            eprintln!("file lists for {} torrents", chosen.len());
            for (n, r) in chosen.iter().enumerate() {
                let path = self.cache.join("details").join(format!("{}.json", r["id"]));
                if path.exists() {
                    continue;
                }
                let url = format!("{AT_FEED}?show=torrent&id={}", r["id"]);
                let Some(body) = self.get(&url).await else {
                    continue;
                };
                let d: Value = serde_json::from_str(&body).unwrap_or_default();
                let files: Vec<Value> = d["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| {
                        let name = if f["filename"].is_string() {
                            &f["filename"]
                        } else {
                            &f["name"]
                        };
                        json!({ "name": name, "size": f["size"] })
                    })
                    .collect();
                let keep = json!({
                    "id": d["id"], "title": d["title"], "is_batch": d["is_batch"],
                    "anidb_aid": d["anidb_aid"], "anidb_eid": d["anidb_eid"],
                    "tvdb_season": d["tvdb_season"], "num_files": d["num_files"], "files": files,
                });
                std::fs::write(&path, keep.to_string()).expect("write details");
                if (n + 1) % 50 == 0 {
                    eprintln!("  {}/{}", n + 1, chosen.len());
                }
            }
        }
    }

    fn without_nulls(mut row: Value) -> Value {
        if let Some(map) = row.as_object_mut() {
            map.retain(|_, v| !v.is_null());
        }
        row
    }

    fn write_jsonl_gz(path: &Path, rows: &[Value]) {
        use std::io::Write;
        let file = std::fs::File::create(path).expect("create fixture");
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
        for row in rows {
            writeln!(gz, "{row}").expect("write fixture");
        }
        gz.finish().expect("finish fixture");
    }

    /// Writes `titles.jsonl.gz` and `file-lists.jsonl.gz` from the cache,
    /// sorted, keeping only what the corpus tests read.
    pub(super) fn pack(cache: &Path, fixture: &Path) {
        let raw = std::fs::read_to_string(cache.join("titles.json")).expect("titles.json");
        let mut titles: Vec<Value> = serde_json::from_str::<Vec<Value>>(&raw)
            .unwrap()
            .into_iter()
            .map(|r| {
                without_nulls(if r["src"] == "nyaa" {
                    let nyaa_id = r["url"]
                        .as_str()
                        .and_then(|u| u.rsplit("/view/").next())
                        .and_then(|id| id.parse::<i64>().ok());
                    json!({
                        "title": r["title"], "source": "nyaa-rss", "nyaa_id": nyaa_id,
                        "nyaa_category": r["category"],
                    })
                } else {
                    json!({
                        "title": r["title"],
                        "source": r["article"].as_str().unwrap_or("AnimeTosho"),
                        "at_id": r["id"], "nyaa_id": r["nyaa_id"], "num_files": r["num_files"],
                        "size": r["size"], "anidb_aid": r["anidb_aid"], "anidb_eid": r["anidb_eid"],
                    })
                })
            })
            .collect();
        titles.sort_by(|a, b| a["title"].as_str().cmp(&b["title"].as_str()));
        let mut lists: Vec<Value> = std::fs::read_dir(cache.join("details"))
            .expect("details dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .map(|p| {
                let d: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
                let files: Vec<Value> = d["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| json!([f["name"], f["size"]]))
                    .collect();
                without_nulls(json!({
                    "at_id": d["id"], "title": d["title"], "at_is_batch": d["is_batch"],
                    "anidb_aid": d["anidb_aid"], "tvdb_season": d["tvdb_season"], "files": files,
                }))
            })
            .collect();
        lists.sort_by_key(|r| r["at_id"].as_i64());
        write_jsonl_gz(&fixture.join("titles.jsonl.gz"), &titles);
        write_jsonl_gz(&fixture.join("file-lists.jsonl.gz"), &lists);
        eprintln!(
            "{} titles, {} file lists -> {}",
            titles.len(),
            lists.len(),
            fixture.display()
        );
    }
}

/// Builds the AnimeTosho corpus: `RT_STEP=listing` reads AnimeTosho's
/// feed and Nyaa RSS into `RT_CACHE/titles.json`, `details` fetches the
/// file lists, `pack` writes the fixture files; `all` (the default) runs
/// the three. It reads live sites at one request per 1.5 s, about two
/// hours from an empty cache; every response is cached under `RT_CACHE`,
/// so a rerun fetches only what is missing. Keep the cache out of a
/// RAM-backed `/tmp`.
#[tokio::test]
#[ignore = "corpus tool: scrapes AnimeTosho and Nyaa; RT_CACHE, RT_STEP"]
async fn scrape_animetosho_corpus() {
    let cache = PathBuf::from(std::env::var("RT_CACHE").expect("RT_CACHE"));
    let step = std::env::var("RT_STEP").unwrap_or_else(|_| "all".to_string());
    if matches!(step.as_str(), "listing" | "details" | "all") {
        let mut scraper = scrape::Scraper::new(cache.clone());
        if step != "details" {
            scraper.listing().await;
        }
        if step != "listing" {
            scraper.details().await;
        }
    }
    if matches!(step.as_str(), "pack" | "all") {
        scrape::pack(&cache, &fixture_dir("animetosho-corpus"));
    }
}
