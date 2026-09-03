//! Nyaa HTML scraping + title classification.
//!
//! Everything that turns a page of HTML into typed `SearchResult`s:
//! DOM selectors, regex patterns for season/batch markers, the
//! `parse_results` + `parse_view_page` entry points, plus the batch-
//! detection + title-classification helpers that run on each row.

use std::sync::LazyLock;

// `::scraper` disambiguates the crate from the containing module of the
// same name. Without the leading ::, `scraper::Html` resolves to
// `self::scraper` (this file) instead of the external crate.
use ::scraper::{Html, Selector};

use super::{SearchOptions, SearchResult, extract_hash, nyaa_base};

static SEL_ROW: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("table.torrent-list tbody tr").expect("SEL_ROW parses"));
static SEL_TD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("td").expect("SEL_TD parses"));
static SEL_A: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").expect("SEL_A parses"));
static SEL_NEXT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("ul.pagination li.next:not(.disabled)").expect("SEL_NEXT parses")
});

/// Pre-compiled selectors for the single-torrent view page
/// (`/view/<id>`). Used by [`fetch_view_result`] for the SeaDex-bypass
/// path that ingests curated torrents directly from their view URLs
/// instead of going through the text search.
static SEL_VIEW_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.panel h3.panel-title").expect("SEL_VIEW_TITLE parses"));
static SEL_VIEW_ROW: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.panel-body div.row").expect("SEL_VIEW_ROW parses"));
// Target Nyaa's actual Bootstrap grid columns (`col-md-1`, `col-md-5`, etc.)
// rather than every `<div>` in the row. The broader `div` selector also
// descended into nested `<div>`s like embedded MediaInfo blocks, which
// made the label/value pair-up (`while i + 1 < cols.len()` in
// `parse_view_page`) drift and silently zero out seeder/leecher counts
// on view pages that had any extra inner markup.
static SEL_VIEW_COL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div[class*='col-md-']").expect("SEL_VIEW_COL parses"));
static SEL_VIEW_MAGNET: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a.card-footer-item[href^='magnet:']").expect("SEL_VIEW_MAGNET parses")
});
static SEL_VIEW_TORRENT: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a.card-footer-item[href$='.torrent']").expect("SEL_VIEW_TORRENT parses")
});

/// Episode range like "01-12", "01~24", "1 - 24". Broader than the old
/// `01[-~]\d{2,3}` hard-coded form so releases that start at a non-01
/// episode (sequels, cour splits) still register as batches.
static BATCH_RANGE_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"(?i)\b\d{1,3}\s*[-~]\s*\d{2,3}\b").expect("BATCH_RANGE_RE parses")
});

/// Bare season marker: `S1`, `S01`, `Season 1`, etc. A season marker on
/// its own — without a paired single-episode indicator — means the
/// release covers the whole season. This is how most BD packs from
/// high-quality groups (MTBB, Okay-Subs, Sephirotic, YURASUKA, neoDESU)
/// are titled: `[Group] Show S1 (BD 1080p)` or `[Group] Show [Season 1]`.
static SEASON_MARKER_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"(?i)\b(s\d{1,2}|season\s*\d+)\b").expect("SEASON_MARKER_RE parses")
});

/// Bare Roman-numeral season marker: `II`, `III`, `IV`, `VI`, `VII`,
/// `VIII`, `IX`. Common in anime sequel titles that spell the season
/// out (`Mob Psycho 100 III`, `Overlord IV`, `KanColle II`) — SeaDex
/// and many BD groups use this form, so without it the batch heuristic
/// misses entire season packs.
///
/// Multi-character only: bare `I`, `V`, `X` are excluded. `I` alone is
/// too noisy (pronoun, initialisms). Bare `V` collides with `Volume V`,
/// `Vol V` and similar; bare `X` collides with franchise names (`Show
/// X`, `X-Files`-style titles) and volume numbering. Anime rarely go
/// past season IX, so the coverage loss is negligible and the false-
/// positive floor drops meaningfully.
///
/// Case-sensitive (uppercase only) to avoid matching lowercase letter
/// sequences like `ix` or `vi` that could appear inside words. Applied
/// to the raw title, not the lowercased form used by the other batch
/// checks.
static ROMAN_SEASON_MARKER_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"\b(II|III|IV|VI|VII|VIII|IX)\b")
        .expect("ROMAN_SEASON_MARKER_RE parses")
});

/// Single-episode indicator. If any of these hit, the release is
/// scoped to one episode (or a very small multi-ep span) and should
/// NOT be flagged as a batch even if a season marker is present.
/// Patterns covered:
///   - `S01E12`, `S1E05` — Western-style
///   - ` - 12`, ` - 24.5` — classic fansub single-ep suffix
///   - `Ep 12`, `Ep. 12`, `Episode 12`
///   - `#12`
static SINGLE_EP_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(
        r"(?i)(s\d{1,2}e\d{1,3}|\s-\s*\d{1,3}(?:\.\d+)?\b|\bep\.?\s*\d{1,3}\b|\bepisode\s*\d{1,3}\b|#\d{1,3})",
    )
    .expect("SINGLE_EP_RE parses")
});

pub(super) fn parse_results(html: &str, opts: &SearchOptions) -> (Vec<SearchResult>, bool) {
    let document = Html::parse_document(html);
    let base = nyaa_base();

    let mut results = Vec::new();

    for row in document.select(&SEL_ROW) {
        let tds: Vec<_> = row.select(&SEL_TD).collect();
        if tds.len() < 8 {
            continue;
        }

        // Category td is index 0, name td is index 1.
        let name_td = tds[1];
        let links: Vec<_> = name_td.select(&SEL_A).collect();

        // Find the last non-comment link as the title link.
        let title_link = links.iter().rev().find(|a| {
            a.value()
                .attr("href")
                .map(|h| h.starts_with("/view/"))
                .unwrap_or(false)
        });

        let (title, link) = match title_link {
            Some(a) => {
                let title = a.text().collect::<String>().trim().to_string();
                let href = a.value().attr("href").unwrap_or("");
                let link = format!("{}{}", base, href);
                (title, link)
            }
            None => continue,
        };

        // Torrent and magnet links (td index 2).
        let link_td = tds[2];
        let link_anchors: Vec<_> = link_td.select(&SEL_A).collect();
        let torrent = link_anchors
            .iter()
            .find_map(|a| {
                let href = a.value().attr("href").unwrap_or("");
                if href.ends_with(".torrent") {
                    Some(format!("{}{}", base, href))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let magnet = link_anchors
            .iter()
            .find_map(|a| {
                let href = a.value().attr("href").unwrap_or("");
                if href.starts_with("magnet:") {
                    Some(href.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Size (td index 3).
        let size = tds[3].text().collect::<String>().trim().to_string();
        let size_bytes = parse_size(&size);

        // Upload date (td index 4) — Nyaa renders "YYYY-MM-DD HH:MM"
        // in UTC. Some cells also carry a `data-timestamp` attribute
        // with the Unix epoch; we prefer the text since it's already
        // display-ready.
        let upload_date = tds[4].text().collect::<String>().trim().to_string();

        // Seeders, leechers, downloads (td indices 5, 6, 7).
        let seeders = parse_int(&tds[5].text().collect::<String>());
        let leechers = parse_int(&tds[6].text().collect::<String>());
        let downloads = parse_int(&tds[7].text().collect::<String>());

        // Trusted/remake detection from row class.
        let row_class = row.value().attr("class").unwrap_or("");
        let is_trusted = row_class.contains("success");

        // Filename-layer classification (anitomy + source-token scan).
        // Drops the old ad-hoc bracket/regex extract and mirrors what the
        // grab-side pipeline's Layer 1 produces, so the label the user
        // sees in interactive search equals the value persisted on grab.
        let classified = classify_search_result(&title);
        let is_batch = detect_batch(&title);
        let info_hash = extract_hash(&magnet);

        let mut result = SearchResult {
            match_provenance: None,
            title,
            link,
            magnet,
            torrent,
            size,
            size_bytes,
            seeders,
            leechers,
            downloads,
            group: classified.group,
            resolution: classified.resolution,
            quality_label: classified.quality_label,
            source: classified.source,
            web_kind: classified.web_kind,
            is_remux: classified.is_remux,
            is_bdmv: classified.is_bdmv,
            is_batch,
            is_trusted,
            score: 0,
            info_hash,
            score_breakdown: Vec::new(),
            upload_date: upload_date.clone(),
            indexer_id: None,
            indexer_name: String::new(),
        };

        let (total, breakdown) =
            crate::services::scoring::score_result_with_breakdown(&result, opts, opts.prefer_subs);
        result.score = total;
        result.score_breakdown = breakdown;
        results.push(result);
    }

    // Sort by score descending.
    results.sort_by_key(|r| std::cmp::Reverse(r.score));

    // Detect if there's a next page.
    let has_next = {
        let pagination_exists = document.select(&SEL_NEXT).next().is_some();
        // Fallback: if we got 75 results (full page), assume there might be more.
        pagination_exists || results.len() >= 75
    };

    (results, has_next)
}

/// Classification-derived fields for a single Nyaa row. Bundles the
/// values that used to come from three separate ad-hoc extractors with
/// the richer label the template now renders directly, so `parse_results`
/// only touches one helper per row.
struct ClassifiedFields {
    group: String,
    resolution: String,
    quality_label: String,
    source: String,
    web_kind: String,
    is_remux: bool,
    is_bdmv: bool,
}

/// Run the filename classifier over a release title and reshape the
/// output for [`SearchResult`]. Mirrors the backend's
/// [`crate::services::source::ClassificationResult::label`] so the UI
/// label in interactive search matches the value a grab would persist.
///
/// The group-map (Layer 3) lookup is not done here — it's async and the
/// parser is sync. Interactive paths that want Layer 3 enrichment call
/// [`enrich_results_with_group_map`] after parsing.
fn classify_search_result(title: &str) -> ClassifiedFields {
    use crate::services::source::{ClassificationResult, DecisionRule, Resolution, Source};
    use crate::services::source_filename::classify_filename;

    let fc = classify_filename(title);

    // Reduce the filename-layer evidence down to a winning source the
    // same way the multi-layer aggregator would if this were the only
    // layer's output. We don't need confidence/needs_review — we only
    // want a source token for the label — so take the highest-confidence
    // piece of evidence and use its source directly.
    let mut winning_source = Source::Unknown;
    let mut best_conf = 0.0_f32;
    for e in &fc.evidence {
        if e.confidence > best_conf {
            winning_source = e.source;
            best_conf = e.confidence;
        }
    }

    let cls = ClassificationResult {
        source: winning_source,
        resolution: fc.resolution,
        is_remux: fc.is_remux,
        web_kind: fc.web_kind,
        is_bdmv: fc.is_bdmv,
        confidence: best_conf,
        needs_review: false,
        evidence: Vec::new(),
        decision_rule: DecisionRule::default(),
    };

    let quality_label = match cls.label().as_str() {
        "Unknown" => String::new(),
        other => other.to_string(),
    };

    // Bare-digit resolution ("1080") for back-compat with existing
    // templates that render `{{ r.resolution }}p` tags.
    let resolution = match fc.resolution {
        Resolution::Unknown => String::new(),
        r => r.as_str().trim_end_matches('p').to_string(),
    };

    ClassifiedFields {
        group: fc.release_group.unwrap_or_default(),
        resolution,
        quality_label,
        source: match winning_source {
            Source::Unknown => String::new(),
            other => other.as_str().to_string(),
        },
        web_kind: fc.web_kind.as_str().to_string(),
        is_remux: fc.is_remux,
        is_bdmv: fc.is_bdmv,
    }
}

/// Strip exclusion-operator hyphens from a Nyaa search query. Nyaa
/// runs Sphinx full-text search, where a token starting with `-` is
/// interpreted as **NOT this token** — and Sphinx applies that even
/// inside double-quoted phrases (verified live 2026-04-20). AniList's
/// English titles routinely wrap subtitles in decorative hyphens —
/// `Solo Leveling Season 2 -Arise from the Shadow-`, `Re:Zero
/// -Starting Life in Another World-`, etc. — and shipping those raw
/// silently drops every release whose title contains the subtitled
/// word. The Solo Leveling S2 query that ought to surface the EMBER
/// batch (`q=Solo+Leveling+Season+2+-Arise+from+the+Shadow-+batch`)
/// returned zero hits because Sphinx excluded every result containing
/// "Arise".
pub(super) fn sanitize_query_for_nyaa(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut at_token_start = true;
    for ch in query.chars() {
        if at_token_start && ch == '-' {
            // Drop. Stay in token-start state so a run of `--`
            // collapses to nothing.
            continue;
        }
        out.push(ch);
        at_token_start = ch.is_whitespace() || ch == '"';
    }
    out
}

fn detect_batch(title: &str) -> bool {
    let lower = title.to_lowercase();

    // Explicit batch keywords.
    if lower.contains("batch") || lower.contains("complete") {
        return true;
    }

    // Numeric episode ranges like "01-12", "01~24", "1 - 24".
    if BATCH_RANGE_RE.is_match(&lower) {
        return true;
    }

    // Season marker with no single-episode indicator — the dominant
    // batch form for BD season packs: `Show S1 (BD 1080p)`, `Show
    // [Season 1] [BD 1080p]`, `Show.S01.1080p.BluRay...`,
    // `Mob Psycho 100 III (BD 1080p)`. The single-ep guard keeps
    // `Show S01E12` / `Show S1 - 12` off the batch path.
    //
    // The Roman-numeral check runs against the raw title (not `lower`)
    // because the regex is case-sensitive — see ROMAN_SEASON_MARKER_RE.
    let has_season_marker =
        SEASON_MARKER_RE.is_match(&lower) || ROMAN_SEASON_MARKER_RE.is_match(title);
    if has_season_marker && !SINGLE_EP_RE.is_match(&lower) {
        return true;
    }

    false
}

fn parse_size(s: &str) -> i64 {
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 2 {
        return 0;
    }
    let num: f64 = parts[0].parse().unwrap_or(0.0);
    match parts[1].to_uppercase().as_str() {
        "B" | "BYTES" => num as i64,
        "KIB" | "KB" => (num * 1024.0) as i64,
        "MIB" | "MB" => (num * 1024.0 * 1024.0) as i64,
        "GIB" | "GB" => (num * 1024.0 * 1024.0 * 1024.0) as i64,
        "TIB" | "TB" => (num * 1024.0 * 1024.0 * 1024.0 * 1024.0) as i64,
        _ => 0,
    }
}

fn parse_int(s: &str) -> i32 {
    s.trim().parse().unwrap_or(0)
}

/// Fetch a single Nyaa view page by URL and return a populated
/// [`SearchResult`]. Used by the SeaDex bypass path in auto-search:
/// SeaDex tells us the curated torrent's info hash and view URL for a
/// given AniList ID, but the torrent's title may not contain any of
/// the query tokens (smol's Kizumonogatari pack is titled
/// `[smol] Monogatari (Season 9) ...` so searches for "Kizumonogatari
/// II: Nekketsu-hen" never surface it). Going direct to the view page
/// sidesteps the whole text-match problem.
pub(super) fn parse_view_page(
    html: &str,
    view_url: &str,
    opts: &SearchOptions,
) -> Option<SearchResult> {
    let document = Html::parse_document(html);

    // Title is the first `<h3 class="panel-title">` under the first
    // `.panel` — the second instance is the "File list" header.
    let title = document
        .select(&SEL_VIEW_TITLE)
        .next()?
        .text()
        .collect::<String>()
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }

    // Scrape the labelled key/value rows in the header panel. Nyaa lays
    // them out as `<div class="row"><div class="col-md-1">Label:</div>
    // <div class="col-md-5">value</div> …</div>`, with a second
    // (label, value) pair on the same row for Leechers/Completed/etc.
    let mut seeders = 0i32;
    let mut leechers = 0i32;
    let mut downloads = 0i32;
    let mut size = String::new();
    for row in document.select(&SEL_VIEW_ROW) {
        let cols: Vec<_> = row
            .select(&SEL_VIEW_COL)
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // We want pairs: each "Label:" should be followed by its value.
        let mut i = 0;
        while i + 1 < cols.len() {
            let label = cols[i].trim_end_matches(':').trim().to_ascii_lowercase();
            let value = cols[i + 1].trim().to_string();
            match label.as_str() {
                "seeders" => seeders = parse_int(&value),
                "leechers" => leechers = parse_int(&value),
                "completed" => downloads = parse_int(&value),
                "file size" => size = value,
                _ => {}
            }
            i += 2;
        }
    }

    let size_bytes = parse_size(&size);

    // Magnet: first `a.card-footer-item` with a `magnet:` href. Info
    // hash comes from the same magnet via `extract_hash`.
    let magnet = document
        .select(&SEL_VIEW_MAGNET)
        .next()
        .and_then(|a| a.value().attr("href"))
        .unwrap_or("")
        .to_string();
    let info_hash = extract_hash(&magnet);

    // Torrent file URL: sibling `.card-footer-item` ending in .torrent.
    // Paths on nyaa.si are relative, so prefix the configured base URL
    // when needed.
    let torrent = document
        .select(&SEL_VIEW_TORRENT)
        .next()
        .and_then(|a| a.value().attr("href"))
        .map(|href| {
            if href.starts_with("http") {
                href.to_string()
            } else {
                format!("{}{}", nyaa_base(), href)
            }
        })
        .unwrap_or_default();

    let classified = classify_search_result(&title);
    let is_batch = detect_batch(&title);

    let mut result = SearchResult {
        match_provenance: None,
        title,
        link: view_url.to_string(),
        magnet,
        torrent,
        size,
        size_bytes,
        seeders,
        leechers,
        downloads,
        group: classified.group,
        resolution: classified.resolution,
        quality_label: classified.quality_label,
        source: classified.source,
        web_kind: classified.web_kind,
        is_remux: classified.is_remux,
        is_bdmv: classified.is_bdmv,
        is_batch,
        // We don't get the row-class `success` tag from a view page, so
        // the trusted flag stays false. Not a problem for the SeaDex
        // path because the SeaDex boost dominates any trusted bonus.
        is_trusted: false,
        score: 0,
        info_hash,
        score_breakdown: Vec::new(),
        // The view page doesn't render the listing-table date column.
        // Callers on this path (SeaDex-bypass) get an empty string.
        upload_date: String::new(),
        indexer_id: None,
        indexer_name: String::new(),
    };

    let (total, breakdown) =
        crate::services::scoring::score_result_with_breakdown(&result, opts, opts.prefer_subs);
    result.score = total;
    result.score_breakdown = breakdown;

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::super::enrich_results_with_group_map;
    use super::*;

    // ── sanitize_query_for_nyaa ──────────────────────────────────────────
    //
    // Boundary: a hyphen is stripped only when it sits at the start of a
    // token (start of string, immediately after whitespace, or
    // immediately after a `"`). Hyphens *inside* a token stay — they're
    // part of the word. Trailing hyphens (`Shadow-`) are also part of
    // the token and stay.

    #[test]
    fn sanitize_strips_decorative_subtitle_hyphens() {
        // The bug case: AniList's English title for Solo Leveling S2.
        let q = "Solo Leveling Season 2 -Arise from the Shadow- batch";
        assert_eq!(
            sanitize_query_for_nyaa(q),
            "Solo Leveling Season 2 Arise from the Shadow- batch"
        );
    }

    #[test]
    fn sanitize_handles_re_zero_subtitle_form() {
        // Same shape, different series — make sure the fix isn't an
        // ad-hoc Solo Leveling patch.
        let q = "Re:Zero -Starting Life in Another World-";
        assert_eq!(
            sanitize_query_for_nyaa(q),
            "Re:Zero Starting Life in Another World-"
        );
    }

    #[test]
    fn sanitize_preserves_internal_hyphens_in_release_groups() {
        // Erai-raws, X-Files, Web-DL, One-Punch — all tokens with an
        // internal hyphen should round-trip untouched. Sphinx only treats
        // a *leading* `-` as the NOT operator.
        let q = "Erai-raws Yu-Gi-Oh One-Punch Man Web-DL";
        assert_eq!(
            sanitize_query_for_nyaa(q),
            "Erai-raws Yu-Gi-Oh One-Punch Man Web-DL"
        );
    }

    #[test]
    fn sanitize_preserves_trailing_hyphens() {
        // Trailing `-` is part of the token — Sphinx doesn't treat it as
        // an operator. Decorative trailing dashes from titles like
        // `Shadow-` stay intact.
        let q = "Foo Shadow- batch";
        assert_eq!(sanitize_query_for_nyaa(q), "Foo Shadow- batch");
    }

    #[test]
    fn sanitize_collapses_runs_of_leading_hyphens() {
        // Defensive: `--foo` (Sphinx exclude double-NOT) collapses
        // entirely so we don't trip on Unicode dash variants getting
        // duplicated.
        let q = "Show --foo bar";
        assert_eq!(sanitize_query_for_nyaa(q), "Show foo bar");
    }

    #[test]
    fn sanitize_drops_token_start_hyphen_inside_quotes() {
        // The aliased exact-match form `"Solo Leveling -Arise..."` —
        // Sphinx still treats the `-` as exclusion even inside a
        // double-quoted phrase (verified live), so we strip it the
        // same way as outside quotes. Opening quote stays attached
        // because it isn't a `-`.
        let q = "\"Solo Leveling -Arise-\"";
        assert_eq!(sanitize_query_for_nyaa(q), "\"Solo Leveling Arise-\"");
    }

    #[test]
    fn sanitize_drops_query_starting_with_hyphen() {
        // Edge case: a leading `-` at the very start of the query.
        let q = "-foo bar";
        assert_eq!(sanitize_query_for_nyaa(q), "foo bar");
    }

    #[test]
    fn sanitize_leaves_episode_dash_separator_searchable() {
        // The `Show - 12` shape that `build_queries_from_aliases`
        // emits for per-episode queries collapses the standalone `-`
        // to whitespace — Nyaa tokenizes on whitespace so the search
        // semantics are unchanged.
        let q = "Show - 12";
        assert_eq!(sanitize_query_for_nyaa(q), "Show  12");
    }

    /// Minimal fixture mirroring the real Nyaa view page structure we
    /// saw for the smol Kizumonogatari megapack (`/view/1713886`). This
    /// keeps `parse_view_page` tied to the actual DOM shape rather than
    /// the assumptions the parser makes in isolation. If Nyaa ever
    /// renumbers the column layout, this test fails loudly.
    const SMOL_VIEW_FIXTURE: &str = r#"
<html><body>
<div class="panel panel-default">
  <div class="panel-heading">
    <h3 class="panel-title">
      [smol] Monogatari (Season 9) (BD 1080p 1920x816 HEVC Opus) | Kizumonogatari | Monogatari Series | Kizumonogatari: Tekketsu-hen | Kizumonogatari: Nekketsu-hen | Kizumonogatari: Reiketsu-hen
    </h3>
  </div>
  <div class="panel-body">
    <div class="row">
      <div class="col-md-1">Category:</div>
      <div class="col-md-5"><a href="/?c=1_0">Anime</a> - <a href="/?c=1_2">English-translated</a></div>
      <div class="col-md-1">Date:</div>
      <div class="col-md-5" data-timestamp="1694025140">2023-09-06 18:32 UTC</div>
    </div>
    <div class="row">
      <div class="col-md-1">Submitter:</div>
      <div class="col-md-5"><a class="text-default" href="/user/smol">smol</a></div>
      <div class="col-md-1">Seeders:</div>
      <div class="col-md-5"><span style="color: green;">51</span></div>
    </div>
    <div class="row">
      <div class="col-md-1">Information:</div>
      <div class="col-md-5"><a href="https://anidb.net/anime/8357">https://anidb.net/anime/8357</a></div>
      <div class="col-md-1">Leechers:</div>
      <div class="col-md-5"><span style="color: red;">0</span></div>
    </div>
    <div class="row">
      <div class="col-md-1">File size:</div>
      <div class="col-md-5">23.8 GiB</div>
      <div class="col-md-1">Completed:</div>
      <div class="col-md-5">2286</div>
    </div>
    <div class="row">
      <div class="col-md-offset-6 col-md-1">Info hash:</div>
      <div class="col-md-5"><kbd>0f8ee3286d768fb53ae593f10155a5077e38e893</kbd></div>
    </div>
  </div>
  <div class="panel-footer clearfix">
    <a href="/download/1713886.torrent" class="card-footer-item">Download Torrent</a>
    or
    <a href="magnet:?xt=urn:btih:0f8ee3286d768fb53ae593f10155a5077e38e893&amp;dn=smol+pack" class="card-footer-item">Magnet</a>
  </div>
</div>
</body></html>
"#;

    #[test]
    fn parse_view_page_extracts_smol_pack_metadata() {
        let opts = SearchOptions::default();
        let result = parse_view_page(SMOL_VIEW_FIXTURE, "https://nyaa.si/view/1713886", &opts)
            .expect("parser should succeed on a well-formed view page");

        assert!(
            result.title.contains("smol") && result.title.contains("Kizumonogatari"),
            "title should be scraped from the header panel, got {:?}",
            result.title
        );
        assert_eq!(result.seeders, 51);
        assert_eq!(result.leechers, 0);
        assert_eq!(result.downloads, 2286);
        assert_eq!(result.size, "23.8 GiB");
        assert!(
            result.size_bytes > 20 * 1024 * 1024 * 1024,
            "size_bytes should parse to GiB range"
        );
        assert_eq!(
            result.info_hash, "0f8ee3286d768fb53ae593f10155a5077e38e893",
            "info_hash should be extracted from the magnet link"
        );
        assert!(
            result.magnet.starts_with("magnet:?"),
            "magnet link should be captured, got {:?}",
            result.magnet
        );
        assert_eq!(result.torrent, "https://nyaa.si/download/1713886.torrent");
        assert_eq!(result.link, "https://nyaa.si/view/1713886");
        // `detect_batch` fires on the season marker in the title.
        assert!(
            result.is_batch,
            "smol pack titled with Season N should be flagged as batch"
        );
        assert_eq!(result.resolution, "1080");
        assert_eq!(result.group, "smol");
    }

    // ── detect_batch — Roman-numeral season markers ──────────────────────
    //
    // SeaDex's curated picks for anime sequels frequently use Roman-numeral
    // season markers in the title (`Mob Psycho 100 III`, `Overlord IV`,
    // `KanColle II`). Before these tests were added, detect_batch missed
    // those entirely because SEASON_MARKER_RE only recognised `S\d+` /
    // `Season \d+` forms, so the curated pack got silently dropped at the
    // `candidates.retain(|c| c.is_batch)` filter in
    // `find_best_batch_for_target`.

    #[test]
    fn detect_batch_roman_numeral_season_marker_iii() {
        // The regression case from the PR #47 session: the DIY full-season
        // BD pack for Mob Psycho 100 III.
        assert!(detect_batch(
            "[DIY] Mob Psycho 100 III (BD 1080p HEVC FLAC) [Dual-Audio]"
        ));
    }

    #[test]
    fn detect_batch_roman_numeral_season_marker_ii_and_iv() {
        assert!(detect_batch("[MTBB] KanColle II (BD 1080p)"));
        assert!(detect_batch("[smol] Overlord IV (BD 1080p)"));
    }

    #[test]
    fn detect_batch_roman_numeral_with_single_ep_is_not_batch() {
        // Single-ep guard must still fire: a Roman-numeral season marker
        // paired with a per-episode indicator is an individual episode
        // release, not a batch.
        assert!(
            !detect_batch("[Group] Mob Psycho 100 III - 05 (1080p)"),
            "Roman season marker + single-ep suffix must not be a batch"
        );
        assert!(
            !detect_batch("[Group] Overlord IV Ep 12 (1080p)"),
            "Roman season marker + Ep N must not be a batch"
        );
    }

    #[test]
    fn detect_batch_lowercase_roman_numerals_are_not_matched() {
        // Case-sensitive on purpose: lowercase "ii"/"iii"/"ix" etc. could
        // appear inside words and would false-positive if we accepted them.
        // Torrent titles conventionally use uppercase Roman numerals, so
        // we don't pay for that false-positive risk.
        assert!(
            !detect_batch("[Group] some title iii (1080p)"),
            "lowercase roman numerals must not trigger the batch heuristic"
        );
    }

    #[test]
    fn detect_batch_single_i_does_not_fire() {
        // `I` alone is excluded from the Roman regex — too ambiguous
        // (pronoun, initial, etc.). A title with a bare `I` and no other
        // batch signal must stay off the batch path.
        assert!(
            !detect_batch("[Group] Show I vs Y (some subtitle)"),
            "bare `I` must not be treated as a season marker"
        );
    }

    #[test]
    fn detect_batch_single_letter_roman_v_and_x_do_not_fire() {
        // Bare `V` and `X` are excluded to avoid colliding with `Volume
        // V` / `Vol V` volume markers, `X-Files`-style franchise names,
        // and miscellaneous single-letter tokens. Multi-character
        // Roman numerals (II/III/IV/VI/VII/VIII/IX) remain supported.
        assert!(
            !detect_batch("[Group] Volume V (1080p)"),
            "`Volume V` with no other batch signal must not fire"
        );
        assert!(
            !detect_batch("[Group] Show V (1080p)"),
            "bare `V` must not be treated as a season marker"
        );
        assert!(
            !detect_batch("[Group] The X Movie (1080p)"),
            "bare `X` must not be treated as a season marker"
        );
    }

    #[test]
    fn extract_hash_lowercases_hex() {
        let magnet = "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=thing";
        assert_eq!(
            extract_hash(magnet),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn extract_hash_accepts_uppercase_prefix() {
        // Real-world magnets occasionally use `urn:BTIH:`; prior impl
        // returned "" for this and broke downstream lookups.
        let magnet = "magnet:?xt=urn:BTIH:ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        assert_eq!(
            extract_hash(magnet),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    /// Test-only base32 encoder used for round-trip verification of
    /// `extract_hash`'s canonicalization. RFC 4648 alphabet, no padding
    /// (20 bytes → 32 chars exactly, no padding needed).
    fn base32_encode_for_test(bytes: &[u8]) -> String {
        const ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = String::new();
        let mut buf: u32 = 0;
        let mut bits: u32 = 0;
        for &b in bytes {
            buf = (buf << 8) | b as u32;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHA[((buf >> bits) & 0x1f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHA[((buf << (5 - bits)) & 0x1f) as usize] as char);
        }
        out
    }

    #[test]
    fn extract_hash_canonicalizes_base32_to_hex() {
        // Base32-encoded magnets must produce the same dedup key as
        // their hex-encoded siblings. Non-qBit clients (Deluge,
        // Transmission) normalize to hex internally — storing base32
        // would break dedupe silently. #63.
        let hex_hash = "abcdef0123456789abcdef0123456789abcdef01";
        let bytes = hex::decode(hex_hash).expect("known-good hex");
        let base32 = base32_encode_for_test(&bytes);
        assert_eq!(base32.len(), 32, "20 bytes → 32 base32 chars");
        let magnet = format!("magnet:?xt=urn:btih:{base32}&dn=thing");
        assert_eq!(extract_hash(&magnet), hex_hash);
    }

    #[test]
    fn extract_hash_base32_accepts_lowercase() {
        // RFC 4648 base32 is case-insensitive; some magnet generators
        // emit lowercase. Decoder must accept both shapes.
        let hex_hash = "abcdef0123456789abcdef0123456789abcdef01";
        let bytes = hex::decode(hex_hash).expect("known-good hex");
        let base32_lower = base32_encode_for_test(&bytes).to_ascii_lowercase();
        let magnet = format!("magnet:?xt=urn:btih:{base32_lower}&dn=thing");
        assert_eq!(extract_hash(&magnet), hex_hash);
    }

    #[test]
    fn extract_hash_base32_hex_round_trip_agree() {
        // The hex and base32 forms of the same info-hash must produce
        // identical `extract_hash` output — this is the invariant that
        // makes dedupe work across magnet encodings.
        let hex_hash = "0f8ee3286d768fb53ae593f10155a5077e38e893";
        let bytes = hex::decode(hex_hash).expect("known-good hex");
        let base32 = base32_encode_for_test(&bytes);
        let hex_magnet = format!("magnet:?xt=urn:btih:{hex_hash}&dn=thing");
        let base32_magnet = format!("magnet:?xt=urn:btih:{base32}&dn=thing");
        let uc_hex_magnet = format!(
            "magnet:?xt=urn:btih:{}&dn=thing",
            hex_hash.to_ascii_uppercase()
        );
        assert_eq!(extract_hash(&hex_magnet), hex_hash);
        assert_eq!(extract_hash(&base32_magnet), hex_hash);
        assert_eq!(extract_hash(&uc_hex_magnet), hex_hash);
    }

    #[test]
    fn extract_hash_malformed_base32_falls_back() {
        // 32-char payload that fails RFC 4648 decoding (contains '1',
        // which is not in the base32 alphabet). Callers get a
        // lowercased string rather than "", matching the defensive
        // fallthrough behavior for other malformed inputs.
        let magnet = "magnet:?xt=urn:btih:11111111111111111111111111111111&dn=thing";
        assert_eq!(extract_hash(magnet), "11111111111111111111111111111111");
    }

    #[test]
    fn extract_hash_no_prefix_returns_empty() {
        assert_eq!(extract_hash("https://example.com/t.torrent"), "");
        assert_eq!(extract_hash(""), "");
    }

    // ── #24 — anitomy-derived classification on SearchResult ──────────────

    #[test]
    fn classify_search_result_subsplease_1080p_webdl_from_filename_tokens() {
        // SubsPlease's own filename is silent on source — just "1080p",
        // no WEB/WEBDL token. Layer 1 (filename) should still surface
        // 1080p + empty source; the group-map enricher fills in Web.
        let c = classify_search_result("[SubsPlease] Frieren - 01 (1080p) [A1B2C3D4].mkv");
        assert_eq!(c.group, "SubsPlease");
        assert_eq!(c.resolution, "1080");
        // Without Layer 3 (group map), source is unknown here.
        assert!(c.source.is_empty() || c.source == "Web");
    }

    #[test]
    fn classify_search_result_bdmv_label_matches_grab_path() {
        // BDMV releases must produce `BD-1080p RAW` — the same label the
        // grab-side ClassificationResult::label() emits, so UI and DB agree.
        let c =
            classify_search_result("[smol] Monogatari S1 (BDMV 1080p x264 FLAC) [f00ba211].mkv");
        assert_eq!(c.resolution, "1080");
        assert_eq!(c.source, "BluRay");
        assert!(c.is_bdmv);
        assert_eq!(c.quality_label, "BD-1080p RAW");
    }

    #[test]
    fn classify_search_result_remux_gets_suffix() {
        let c = classify_search_result("[Tenrai-Sensei] Frieren - 01 (BD Remux 1080p).mkv");
        assert_eq!(c.source, "BluRay");
        assert!(c.is_remux);
        assert_eq!(c.quality_label, "BD-1080p Remux");
    }

    #[test]
    fn classify_search_result_web_dl_produces_full_label() {
        let c = classify_search_result("Show Name - 01 (1080p) [WEB-DL].mkv");
        assert_eq!(c.resolution, "1080");
        assert_eq!(c.source, "Web");
        // web_kind still tracks WebDl internally so CF value-3 specs
        // match releases with explicit WEB-DL tokens. The user-facing
        // label collapses WebDl and bare-WEB into "WEB" (issue #48).
        assert_eq!(c.web_kind, "WEB-DL");
        assert_eq!(c.quality_label, "WEB-1080p");
    }

    #[test]
    fn classify_search_result_empty_label_when_nothing_parses() {
        // No source, no resolution → empty label so the UI shows a dash
        // instead of a stray "Unknown" string.
        let c = classify_search_result("garbage title with no tokens");
        assert!(c.resolution.is_empty());
        assert!(c.source.is_empty());
        assert!(c.quality_label.is_empty());
    }

    #[tokio::test]
    async fn enrich_with_group_map_fills_source_for_known_group() {
        use crate::models::group_source_map;
        use crate::services::source::Source;

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&pool).await.unwrap();
        // Seeded map already ships SubsPlease=Web; rely on that rather
        // than re-inserting here so the test also exercises the
        // real seed data round-trip.
        assert_eq!(
            group_source_map::get(&pool, "SubsPlease")
                .await
                .unwrap()
                .map(|e| e.source),
            Some(Source::Web),
        );

        let mut results = vec![SearchResult {
            match_provenance: None,
            title: "[SubsPlease] Show - 01 (1080p) [abc].mkv".to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders: 0,
            leechers: 0,
            downloads: 0,
            group: "SubsPlease".to_string(),
            resolution: "1080".to_string(),
            quality_label: "1080p".to_string(),
            source: String::new(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: String::new(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        }];

        enrich_results_with_group_map(&pool, &mut results).await;

        assert_eq!(results[0].source, "Web");
        // Issue #48: the SubsPlease WebDl seed was dropped — the
        // distinction between "WEBDL" and "WEB" labels was more
        // confusing than useful and nothing in the file list said
        // the release was a stream remux vs a re-encode. The group
        // map still pins Source::Web; the label unifies to WEB.
        assert_eq!(results[0].quality_label, "WEB-1080p");
    }

    #[tokio::test]
    async fn enrich_with_group_map_does_not_overwrite_filename_source() {
        // Filename said BluRay explicitly; even if the group map would
        // claim Web, the filename's specificity wins.
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&pool).await.unwrap();

        let mut results = vec![SearchResult {
            match_provenance: None,
            title: "[SubsPlease] Show - 01 (BD 1080p) [abc].mkv".to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes: 0,
            seeders: 0,
            leechers: 0,
            downloads: 0,
            group: "SubsPlease".to_string(),
            resolution: "1080".to_string(),
            quality_label: "BD-1080p".to_string(),
            source: "BluRay".to_string(),
            web_kind: String::new(),
            is_remux: false,
            is_bdmv: false,
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: String::new(),
            score_breakdown: Vec::new(),
            upload_date: String::new(),
            indexer_id: None,
            indexer_name: String::new(),
        }];

        enrich_results_with_group_map(&pool, &mut results).await;

        assert_eq!(results[0].source, "BluRay");
        assert_eq!(results[0].quality_label, "BD-1080p");
    }
}
