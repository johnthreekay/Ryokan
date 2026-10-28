//! anitomy against `release_title` (phase 3 of the parser plan): cost per
//! title, and agreement on the fields Ryokan reads from anitomy, over the
//! corpora. Run in release mode:
//!
//! `cargo nextest run --release --features test-support --lib
//! release_title::bench --run-ignored only --no-capture`
//!
//! Each field is compared the way its consumer reads it today: group and
//! resolution through `source_filename::classify_filename` (anitomy plus
//! its fallbacks), the extras gate through `file_mode::is_non_episodic_extra`,
//! title and year straight from anitomy as `library_link` and manual
//! import take them.

use std::hint::black_box;
use std::time::Instant;

use anitomy::ElementCategory;

use super::differential::{fixtures, json_files, load, sample_titles, verified_titles};
use super::file_mode::is_non_episodic_extra;
use super::{legacy, parse};
use crate::services::media::anitomy_parse;
use crate::services::source::Resolution;
use crate::services::source_filename::classify_filename;

const ROUNDS: usize = 30;

type Parser = Box<dyn Fn(&str)>;

fn all_titles() -> Vec<String> {
    let mut titles: Vec<String> = verified_titles().into_iter().collect();
    titles.extend(sample_titles());
    titles.sort();
    titles.dedup();
    titles
}

/// Mean, median, p99 and max per title in microseconds.
fn per_title_cost(titles: &[String], f: impl Fn(&str)) -> (f64, f64, f64, f64) {
    for t in titles {
        f(t);
    }
    let mut per: Vec<f64> = titles
        .iter()
        .map(|t| {
            let start = Instant::now();
            for _ in 0..ROUNDS {
                f(black_box(t));
            }
            start.elapsed().as_nanos() as f64 / ROUNDS as f64 / 1000.0
        })
        .collect();
    per.sort_by(f64::total_cmp);
    let mean = per.iter().sum::<f64>() / per.len() as f64;
    let at = |q: f64| per[((per.len() - 1) as f64 * q) as usize];
    (mean, at(0.5), at(0.99), *per.last().unwrap())
}

fn timing() {
    let titles = all_titles();
    eprintln!(
        "## Cost per title ({} titles, {ROUNDS} rounds each, release build)\n",
        titles.len()
    );
    eprintln!("| Parser | mean µs | median µs | p99 µs | max µs |");
    eprintln!("|---|---|---|---|---|");
    let rows: [(&str, Parser); 4] = [
        (
            "anitomy (`media::anitomy_parse`)",
            Box::new(|t| {
                black_box(anitomy_parse(t));
            }),
        ),
        (
            "`source_filename::classify_filename` (anitomy + fallbacks)",
            Box::new(|t| {
                black_box(classify_filename(t));
            }),
        ),
        (
            "`release_title::parse` + `is_batch` + `release_episodes`",
            Box::new(|t| {
                let p = parse(t);
                black_box((p.is_batch(), p.release_episodes()));
            }),
        ),
        (
            "today's title parsers (3 batch detectors, `parse_release_numbers`, season, RSS `parse_release`, 2 group, 1 resolution)",
            Box::new(|t| {
                black_box(legacy::scraper::detect_batch(t));
                black_box(legacy::feed::detect_batch(t));
                black_box(legacy::indexers::detect_batch_from_title(t));
                black_box(legacy::release_parse::parse_release_numbers(t));
                black_box(legacy::release_parse::parse_release_season(t));
                black_box(legacy::rss::parse_release(t));
                black_box(legacy::feed::extract_group(t));
                black_box(legacy::indexers::extract_group_from_title(t));
                black_box(legacy::feed::extract_resolution(t));
            }),
        ),
    ];
    for (name, f) in rows {
        let (mean, median, p99, max) = per_title_cost(&titles, f);
        eprintln!("| {name} | {mean:.2} | {median:.2} | {p99:.2} | {max:.2} |");
    }
    eprintln!();
}

fn height(r: Resolution) -> Option<u32> {
    match r {
        Resolution::Unknown => None,
        Resolution::R480p => Some(480),
        Resolution::R576p => Some(576),
        Resolution::R720p => Some(720),
        Resolution::R1080p => Some(1080),
        Resolution::R2160p => Some(2160),
    }
}

/// Words of a title, lowercased, for comparing two title readings.
fn words(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Default)]
struct Tally {
    both: usize,
    anitomy_only: usize,
    parser_only: usize,
    neither: usize,
    misses: Vec<String>,
}

impl Tally {
    fn add<T: PartialEq + std::fmt::Debug>(
        &mut self,
        title: &str,
        want: &T,
        anitomy: &T,
        parser: &T,
    ) {
        match (anitomy == want, parser == want) {
            (true, true) => self.both += 1,
            (true, false) => self.anitomy_only += 1,
            (false, true) => self.parser_only += 1,
            (false, false) => self.neither += 1,
        }
        if anitomy != want || parser != want {
            self.misses.push(format!(
                "want {want:?} | anitomy {anitomy:?} | release_title {parser:?} | {title}"
            ));
        }
    }

    fn row(&self, field: &str) {
        let n = self.both + self.anitomy_only + self.parser_only + self.neither;
        eprintln!(
            "| {field} | {n} | {} | {} | {} | {} | {:.1}% | {:.1}% |",
            self.both,
            self.anitomy_only,
            self.parser_only,
            self.neither,
            100.0 * (self.both + self.anitomy_only) as f64 / n as f64,
            100.0 * (self.both + self.parser_only) as f64 / n as f64,
        );
    }
}

fn agreement() {
    let mut group = Tally::default();
    let mut resolution = Tally::default();
    let mut extra = Tally::default();
    let mut year = Tally::default();
    let mut title_differs = Vec::new();
    let mut cases = 0;
    let (mut with_source, mut with_audio) = (0, 0);
    for path in json_files(&fixtures().join("nyaa-titles")) {
        for case in load(&path)["cases"].as_array().into_iter().flatten() {
            let Some(title) = case["title"].as_str() else {
                continue;
            };
            let expect = &case["expect"];
            cases += 1;
            let an = anitomy_parse(title);
            let fc = classify_filename(title);
            let rt = parse(title);
            if let Some(want) = expect.get("group") {
                let want = want.as_str().map(String::from);
                group.add(title, &want, &fc.release_group, &rt.group);
            }
            if let Some(want) = expect.get("resolution") {
                let want = want.as_u64().map(|v| v as u32);
                resolution.add(title, &want, &height(fc.resolution), &rt.resolution);
            }
            let want_extra = expect["extra"].as_bool().unwrap_or(false);
            extra.add(
                title,
                &want_extra,
                &is_non_episodic_extra(&title.to_lowercase()),
                &rt.extra,
            );
            if let Some(want) = expect.get("year") {
                let want = want.as_i64().map(|v| v as i32);
                let an_year = an
                    .get(ElementCategory::AnimeYear)
                    .and_then(|y| y.trim().parse().ok());
                year.add(title, &want, &an_year, &rt.year);
            }
            let an_title = an
                .get(ElementCategory::AnimeTitle)
                .map(words)
                .unwrap_or_default();
            let rt_title = rt.title.as_deref().map(words).unwrap_or_default();
            if an_title != rt_title {
                title_differs.push(format!(
                    "anitomy {an_title:?} | release_title {rt_title:?} | {title}"
                ));
            }
            with_source += usize::from(an.get(ElementCategory::Source).is_some());
            with_audio += usize::from(!an.get_all(ElementCategory::AudioTerm).is_empty());
        }
    }
    eprintln!("## Agreement on the hand-verified corpus ({cases} titles)\n");
    eprintln!(
        "| Field | checked | both right | only anitomy | only release_title | neither | anitomy | release_title |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|");
    group.row("Release group (`classify_filename`)");
    resolution.row("Resolution (`classify_filename`)");
    extra.row("Extras gate (`is_non_episodic_extra`)");
    year.row("Year (`AnimeYear`)");
    eprintln!();
    eprintln!(
        "Title: anitomy's `AnimeTitle` and `release_title`'s title text differ (as lowercase words) on {} of {cases} titles.",
        title_differs.len()
    );
    eprintln!(
        "anitomy finds a `Source` on {with_source} and an `AudioTerm` on {with_audio} of {cases} titles; release_title has no equivalent output.\n"
    );
    for (name, t) in [
        ("group", &group),
        ("resolution", &resolution),
        ("extra", &extra),
        ("year", &year),
    ] {
        eprintln!("### {name} misses ({})", t.misses.len());
        for m in &t.misses {
            eprintln!("- {m}");
        }
        eprintln!();
    }
    eprintln!("### title differences ({})", title_differs.len());
    for d in &title_differs {
        eprintln!("- {d}");
    }
}

/// Whole-word match, as `source_filename`'s scanner reads the title.
fn has_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    haystack.match_indices(word).any(|(at, _)| {
        let end = at + word.len();
        (at == 0 || !bytes[at - 1].is_ascii_alphanumeric())
            && (end >= bytes.len() || !bytes[end].is_ascii_alphanumeric())
    })
}

/// Source evidence that only anitomy's `Source` / `AudioTerm` produced:
/// what `classify_filename` would lose without it, over every title.
fn anitomy_only_source_evidence() {
    let titles = all_titles();
    let mut source_only = Vec::new();
    let mut aac_only = 0;
    for title in &titles {
        let lower = title.to_ascii_lowercase();
        let fc = classify_filename(title);
        for e in &fc.evidence {
            let fallback: &[&str] = match e.detail.as_str() {
                "BD keyword" => &["bdrip", "bdremux", "bluray", "blu-ray"],
                "Remux keyword" => &["bdremux"],
                "DVD keyword" => &["dvdrip"],
                "Web keyword" => &["web-dl", "webrip", "webdl", "web"],
                "HDTV keyword" => &["hdtv"],
                "TV keyword" => &["pdtv", "sdtv", "tvrip"],
                _ => continue,
            };
            if !fallback.iter().any(|w| has_word(&lower, w)) {
                source_only.push(format!("{} | {title}", e.detail));
            }
        }
        if fc.evidence.iter().any(|e| e.detail == "web audio (aac)") {
            aac_only += 1;
        }
    }
    eprintln!(
        "## Source evidence only anitomy produces ({} titles)\n\n{} titles get a `Source` keyword record the raw-title fallback would not; {} get the weak `web audio (aac)` record, which only anitomy's `AudioTerm` supplies.\n",
        titles.len(),
        source_only.len(),
        aac_only
    );
    let mut kinds = std::collections::BTreeMap::new();
    for row in &source_only {
        *kinds
            .entry(row.split(" | ").next().unwrap_or_default().to_string())
            .or_insert(0) += 1;
    }
    eprintln!("By keyword: {kinds:?}\n");
    for row in source_only.iter().take(25) {
        eprintln!("- {row}");
    }
    eprintln!();
}

/// Every disagreement on the unverified sample titles (none of them in
/// the corpus either parser was checked against), for judging by hand.
fn sample_disagreements() {
    let verified = verified_titles();
    let titles: Vec<String> = sample_titles()
        .into_iter()
        .filter(|t| !verified.contains(t))
        .collect();
    let mut out: Vec<(&str, String)> = Vec::new();
    for title in &titles {
        let fc = classify_filename(title);
        let rt = parse(title);
        let an = anitomy_parse(title);
        if fc.release_group != rt.group {
            out.push((
                "group",
                format!(
                    "anitomy {:?} | release_title {:?} | {title}",
                    fc.release_group, rt.group
                ),
            ));
        }
        if height(fc.resolution) != rt.resolution {
            out.push((
                "resolution",
                format!(
                    "anitomy {:?} | release_title {:?} | {title}",
                    height(fc.resolution),
                    rt.resolution
                ),
            ));
        }
        let an_extra = is_non_episodic_extra(&title.to_lowercase());
        if an_extra != rt.extra {
            out.push((
                "extra",
                format!("anitomy {an_extra} | release_title {} | {title}", rt.extra),
            ));
        }
        let an_year: Option<i32> = an
            .get(ElementCategory::AnimeYear)
            .and_then(|y| y.trim().parse().ok());
        if an_year != rt.year {
            out.push((
                "year",
                format!(
                    "anitomy {an_year:?} | release_title {:?} | {title}",
                    rt.year
                ),
            ));
        }
    }
    eprintln!(
        "\n## Disagreements on {} unverified sample titles\n",
        titles.len()
    );
    for field in ["group", "resolution", "extra", "year"] {
        let rows: Vec<&String> = out
            .iter()
            .filter(|(f, _)| *f == field)
            .map(|(_, r)| r)
            .collect();
        eprintln!("### {field} ({})", rows.len());
        for r in rows {
            eprintln!("- {r}");
        }
        eprintln!();
    }
}

#[test]
#[ignore]
fn anitomy_report() {
    timing();
    agreement();
    anitomy_only_source_evidence();
    sample_disagreements();
}

/// Keeps the report compiling in the default (debug) test run.
#[test]
fn report_helpers_read_the_corpus() {
    assert!(!all_titles().is_empty());
    assert_eq!(words("[Group] Show-Name 2"), "group show name 2");
}
