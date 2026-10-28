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

/// Ryokan's default episode name, rendered for every corpus title's show
/// name (an extras release's name is no show's), reads back as written,
/// whatever the episode is called: the library scanner reads these names
/// with no series knowledge. `2.5 Dimensional Seduction - S01E24 - 2.5
/// Dimensional Ririsa.mkv` once read as a half episode and nothing else.
#[test]
fn default_episode_names_read_back() {
    use crate::services::naming::{self, NameContext, TemplateKind};
    const EPISODE_TITLES: &[&str] = &[
        "",
        "2.5 Dimensional Ririsa",
        "Episode 10.5",
        "The Final Battle",
        "Final",
        "Part 2",
        "100 Years Later",
        "Opening Night",
        "Preview",
        "Ending",
        "Special Training",
        "Season 2",
        "(2023)",
    ];
    let mut names: Vec<String> = verified_titles()
        .into_iter()
        .chain(sample_titles())
        .map(|t| parse(&t))
        .filter(|p| !p.extra)
        .filter_map(|p| p.series_title)
        .collect();
    names.sort();
    names.dedup();
    let mut failures = Vec::new();
    for series in &names {
        for title in EPISODE_TITLES {
            for (season, first, last) in [(1, 5, 5), (2, 12, 13), (0, 3, 3)] {
                let ctx = NameContext {
                    series_title: series.clone(),
                    season_number: season,
                    episode_number: first,
                    episode_last: last,
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
                let span =
                    crate::services::media::parse_episode_span(&rendered.name.to_lowercase());
                if !span
                    .is_some_and(|s| s.season == Some(season) && s.first == first && s.last == last)
                {
                    failures.push(format!("{} -> {span:?}", rendered.name));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} show names write a file name that does not read back:\n{}",
        failures.len(),
        names.len(),
        failures[..failures.len().min(30)].join("\n")
    );
}

/// `RT_TITLES=$'title one\ntitle two' cargo nextest run --features
/// test-support --lib release_title::corpus::dump --run-ignored only
/// --no-capture` prints each title's parse; a line `title<TAB>alias|alias`
/// adds the series-aware reading (`for_series`).
#[test]
#[ignore = "debugging aid: RT_TITLES"]
fn dump() {
    let titles = std::env::var("RT_TITLES").unwrap_or_default();
    for line in titles.lines().filter(|t| !t.trim().is_empty()) {
        let (title, aliases) = line.split_once('\t').unwrap_or((line, ""));
        eprintln!(
            "== {}\n   {:?}",
            snapshot_line(title),
            parse(title).episodes
        );
        if std::env::var_os("RT_TOKENS").is_some() {
            let lexed = super::lexer::lex(&super::normalize::normalize(title).text);
            for tok in &lexed.toks {
                eprintln!(
                    "   tok {:?} {:?} sep={:?} depth={} class={:?}",
                    tok.kind,
                    tok.text,
                    tok.sep,
                    tok.depth,
                    super::vocab::classify(tok)
                );
            }
        }
        if !aliases.is_empty() {
            let aliases: Vec<&str> = aliases.split('|').collect();
            let mut eps: Vec<i32> = parse(title)
                .for_series(&super::SeriesAnchors::new(&aliases))
                .episodes
                .into_iter()
                .collect();
            eps.sort_unstable();
            eprintln!("   for_series={eps:?}");
        }
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

/// The rows of a gzipped JSON-lines file in the AnimeTosho corpus.
fn animetosho(name: &str) -> Vec<Value> {
    use std::io::BufRead;
    let path = fixtures().join("animetosho-corpus").join(name);
    let file =
        std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    std::io::BufReader::new(flate2::read::GzDecoder::new(file))
        .lines()
        .map(|line| serde_json::from_str(&line.expect("read line")).expect("JSON line"))
        .collect()
}

/// Every reading of the AnimeTosho corpus (`tests/fixtures/animetosho-corpus`)
/// as JSON lines in `RT_OUT`, for diffing two parser versions: one line per
/// title, then one per file of each file list, read plainly and with its
/// torrent title's show name as the series' name. The corpus tests in
/// `tests/release_title_corpora.rs` hold the totals; this shows which
/// readings a change moved.
#[test]
#[ignore = "corpus tool: RT_OUT"]
fn big_corpus_dump() {
    use std::io::Write;
    use std::time::Instant;

    let out_path = std::env::var("RT_OUT").expect("RT_OUT");
    let mut out = std::io::BufWriter::new(std::fs::File::create(out_path).expect("RT_OUT"));
    for row in animetosho("titles.jsonl.gz") {
        let title = row["title"].as_str().unwrap_or_default();
        let started = Instant::now();
        let p = parse(title);
        let micros = started.elapsed().as_micros() as u64;
        let mut eps: Vec<i32> = p.release_episodes().into_iter().collect();
        eps.sort_unstable();
        let fields = super::release_fields(title);
        let rec = serde_json::json!({
            "title": title,
            "micros": micros,
            "batch": p.is_batch(),
            "eps": eps,
            "claim": p.episodes.as_ref().map(|c| format!("{:?}", c.rule)),
            "season": p.season_hint(),
            "seasons": p.seasons.numbers,
            "group": p.group,
            "res": p.resolution,
            "res_tier": fields.resolution,
            "special": p.special,
            "extra": p.extra,
            "half": p.half_episode,
            "file": p.single_file,
            "amb": p.ambiguous_number,
            "title_text": p.title,
            "series_title": p.series_title,
            "file_span": p.file_span().map(|s| (s.season, s.first, s.last, s.special)),
        });
        writeln!(out, "{rec}").expect("write");
    }
    for d in animetosho("file-lists.jsonl.gz") {
        let torrent_title = d["title"].as_str().unwrap_or_default();
        let name = parse(torrent_title).series_title.unwrap_or_default();
        let anchors = super::SeriesAnchors::new(&[name.as_str()]);
        for f in d["files"].as_array().into_iter().flatten() {
            let file = f[0].as_str().unwrap_or_default();
            let span = crate::services::media::parse_file_path(file);
            let named = crate::services::media::parse_file_path_for(file, &anchors);
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "torrent": d["at_id"],
                    "file": file,
                    "span": span.map(|s| (s.season, s.first, s.last, s.special)),
                    "named": named.map(|s| (s.season, s.first, s.last, s.special)),
                })
            )
            .expect("write");
        }
    }
}
