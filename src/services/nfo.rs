use std::path::Path;

use crate::models::series::Series;
use crate::services::anilist::AnimeDetail;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Strip basic HTML tags out of an AniList description for use inside a
/// `<plot>` tag. AniList descriptions arrive with `<br>`, `<i>`, etc.; the
/// rich-description sanitizer leaves them in but the NFO consumer (Jellyfin)
/// renders the `<plot>` body verbatim, so the tags would show up in the UI.
///
/// Removed tags are replaced with a single space so structural tags like
/// `<br>` between sentences keep acting as word separators. Adjacent runs of
/// whitespace are then collapsed.
///
/// If the input ends mid-tag (an unmatched trailing `<`), the buffered chars
/// that came after that `<` are flushed as literal text — they weren't really
/// a tag, the `<` was just a stray bracket, and dropping the rest would
/// silently eat content from malformed descriptions. The leading `<` itself
/// is not re-emitted, matching the "strip markup, keep text" spirit of the
/// function.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Buffer for characters that appeared after a `<` — discarded when a
    // matching `>` closes the tag, or flushed as literal content when the
    // input ends before `>` arrives (or a second `<` restarts tag-mode).
    let mut tag_buf = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => {
                // Nested/unmatched `<` — flush the prior false-start as
                // literal before entering the new tag.
                if in_tag && !tag_buf.is_empty() {
                    out.push_str(&tag_buf);
                    tag_buf.clear();
                }
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => {
                in_tag = false;
                tag_buf.clear();
            }
            _ if !in_tag => out.push(ch),
            _ => tag_buf.push(ch),
        }
    }
    // Input ended with an unmatched `<foo` — treat the remainder as literal.
    if in_tag && !tag_buf.is_empty() {
        out.push_str(&tag_buf);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Best display title for a series: English → Romaji → title. Used as
/// the one-time default for `series.folder_name` generation and for
/// anywhere else a stable title is needed regardless of the user's
/// current preference. NFO writes should use
/// [`title_for_preference`] instead so `<title>` in tvshow.nfo /
/// season.nfo / episode.nfo matches the language setting.
pub fn best_title(series: &Series) -> String {
    if !series.title_english.is_empty() {
        series.title_english.clone()
    } else if !series.title_romaji.is_empty() {
        series.title_romaji.clone()
    } else {
        series.title.clone()
    }
}

/// Display title chosen according to the user's `title_language`
/// setting (`english` / `romaji` / `native`). Falls back through the
/// other fields when the preferred one is empty so the NFO is never
/// blank.
///
/// **Fallback order** (load-bearing — don't "fix" to symmetric
/// fallbacks later):
/// - `romaji` → english → native → title
/// - `native` → english → romaji → title
/// - `english` / unknown → romaji → native → title
///
/// English is the first fallback for non-english preferences because
/// most non-native viewers can read it when the preferred-language
/// field is missing. Going `romaji → native → english` in that case
/// would hand a katakana-heavy romaji title to a user who explicitly
/// said "I want native" and has no native string available, which is
/// worse than just giving them the English fallback.
pub fn title_for_preference(series: &Series, preference: &str) -> String {
    let e = series.title_english.as_str();
    let r = series.title_romaji.as_str();
    let n = series.title_native.as_str();
    let t = series.title.as_str();
    let pick = |primary: &str, fallbacks: [&str; 3]| -> String {
        if !primary.is_empty() {
            return primary.to_string();
        }
        for f in fallbacks {
            if !f.is_empty() {
                return f.to_string();
            }
        }
        String::new()
    };
    match preference {
        "romaji" => pick(r, [e, n, t]),
        "native" => pick(n, [e, r, t]),
        _ => pick(e, [r, n, t]),
    }
}

/// Write a `tvshow.nfo` file to `path` using data already stored in the DB.
/// Jellyfin reads this instead of querying any external metadata provider.
///
/// When `detail` is provided (the cached `AnimeDetail` from the metadata
/// cache), the NFO is enriched with plot, year, premiered, rating, genres,
/// and runtime. Without it the output is the minimal series-row-only form
/// used as a fallback when the metadata cache is empty.
pub async fn write_series_nfo(
    path: &Path,
    series: &Series,
    detail: Option<&AnimeDetail>,
    title_language: &str,
    has_poster: bool,
    has_banner: bool,
    has_backdrop: bool,
) -> std::io::Result<()> {
    let title = xml_escape(&title_for_preference(series, title_language));
    let orig = xml_escape(&series.title_native);
    let status = match series.status.as_str() {
        "FINISHED" | "FINISHED_AIRING" => "Ended",
        "RELEASING" | "CURRENTLY_AIRING" => "Continuing",
        _ => "Unknown",
    };

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <tvshow>\n\
         \x20\x20<title>{title}</title>\n\
         \x20\x20<originaltitle>{orig}</originaltitle>\n\
         \x20\x20<status>{status}</status>\n",
        title = title,
        orig = orig,
        status = status,
    );

    // Plot: prefer AniList description (HTML-stripped). Falls through silently
    // when the cache is unavailable.
    if let Some(d) = detail {
        let plot = strip_html_tags(&d.description);
        if !plot.trim().is_empty() {
            xml.push_str(&format!("  <plot>{}</plot>\n", xml_escape(plot.trim())));
        }

        // <year> and <premiered>: Jellyfin uses these to sort and to display
        // the year on cards. season_year is the only year field we have, so
        // we synthesize a January 1 premiered date — Jellyfin tolerates the
        // imprecision and uses just the year for display anyway.
        if let Some(year) = d.season_year {
            xml.push_str(&format!("  <year>{}</year>\n", year));
            xml.push_str(&format!("  <premiered>{}-01-01</premiered>\n", year));
        }

        // <rating>: AniList averageScore is 0-100. Convert to /10 so it
        // matches the convention Jellyfin expects from TVDB-sourced ratings.
        if let Some(score) = d.average_score {
            xml.push_str(&format!(
                "  <rating>{:.1}</rating>\n",
                (score as f32) / 10.0
            ));
        }

        // <runtime>: detail.duration is per-episode minutes from AniList.
        if let Some(duration) = d.duration
            && duration > 0
        {
            xml.push_str(&format!("  <runtime>{}</runtime>\n", duration));
        }

        // Real genre tags. Always include "Animation" as a fallback so the
        // category filter still groups it correctly even when AniList genres
        // are sparse.
        let mut emitted_animation = false;
        for genre in &d.genres {
            let trimmed = genre.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.eq_ignore_ascii_case("animation") {
                emitted_animation = true;
            }
            xml.push_str(&format!("  <genre>{}</genre>\n", xml_escape(trimmed)));
        }
        if !emitted_animation {
            xml.push_str("  <genre>Animation</genre>\n");
        }
    } else {
        // Minimal fallback when the cache is missing.
        xml.push_str("  <genre>Animation</genre>\n");
    }

    xml.push_str(&format!(
        "  <uniqueid type=\"anilist\" default=\"true\">{}</uniqueid>\n",
        series.anilist_id
    ));

    if let Some(mal_id) = series.mal_id {
        xml.push_str(&format!(
            "  <uniqueid type=\"myanimelist\">{}</uniqueid>\n",
            mal_id
        ));
    }

    // <art> block points Jellyfin at the sibling image files we
    // actually wrote (`poster.jpg`, `banner.jpg`). Jellyfin already
    // auto-discovers these by filename at the series root, but the
    // explicit reference belts-and-suspenders against scanner
    // variations (third-party NFO plugins, non-default image
    // discovery settings) where auto-discovery doesn't fire and the
    // metadata manager falls back to a TVDB/TMDB scrape for the slot.
    //
    // Each tag is gated on the caller confirming the file actually
    // landed — a hard-coded `<banner>banner.jpg</banner>` with no
    // banner on disk would surface as a missing-file error on every
    // Jellyfin scan and still not prevent the external fallback.
    // Skip the whole `<art>` block when both are missing so the NFO
    // doesn't carry an empty container.
    if has_poster || has_banner {
        xml.push_str("  <art>\n");
        if has_poster {
            xml.push_str("    <poster>poster.jpg</poster>\n");
        }
        if has_banner {
            xml.push_str("    <banner>banner.jpg</banner>\n");
        }
        xml.push_str("  </art>\n");
    }

    // <fanart> is a sibling of <art> in the standard Kodi/Jellyfin
    // tvshow.nfo schema, not a child. Jellyfin's NFO reader maps
    // <fanart><thumb> into `ImageType::Backdrop` (2), which is the
    // slot the series detail page reads for the hero image behind
    // the header. AniList's `bannerImage` is semantically a backdrop
    // (wide hero, 1900×400 typical), not a Kodi-style banner, so
    // copying the same blob into `backdrop.jpg` and pointing the NFO
    // there is what gets it actually displayed — the `banner.jpg`
    // reference above only surfaces in the "Banner" library layout.
    if has_backdrop {
        xml.push_str("  <fanart>\n");
        xml.push_str("    <thumb>backdrop.jpg</thumb>\n");
        xml.push_str("  </fanart>\n");
    }

    xml.push_str("</tvshow>\n");

    tokio::fs::write(path, xml).await
}

/// Write a `Season NN/season.nfo` so Jellyfin treats the season as
/// already-matched and doesn't fall back to TVDB/TMDB for the season
/// description, poster, or banner.
///
/// Without this file Jellyfin's metadata-match cascade runs on every
/// season folder — for an anime that's on TVDB as a different cour,
/// the scraped season data can belong to a different show entirely
/// (TVDB collapses cours differently than AniList). Writing season.nfo
/// pins the season to the same AniList entry as the parent series.
///
/// Ryokan hardcodes `season = 1` per AniList entry (one cour = one
/// entry), so there is exactly one season folder per series and it
/// carries the same plot/premiered/rating as the parent `tvshow.nfo`.
/// The season-level NFO just needs to exist in a shape Jellyfin
/// accepts — the metadata is semantically redundant with tvshow.nfo,
/// but omitting it re-opens the external-scrape path.
pub async fn write_season_nfo(
    path: &Path,
    season_number: i32,
    series: &Series,
    detail: Option<&AnimeDetail>,
    title_language: &str,
    has_folder_poster: bool,
) -> std::io::Result<()> {
    let title = xml_escape(&title_for_preference(series, title_language));

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <season>\n\
         \x20\x20<title>{title}</title>\n\
         \x20\x20<seasonnumber>{season}</seasonnumber>\n",
        title = title,
        season = season_number,
    );

    if let Some(d) = detail {
        let plot = strip_html_tags(&d.description);
        if !plot.trim().is_empty() {
            xml.push_str(&format!("  <plot>{}</plot>\n", xml_escape(plot.trim())));
        }
        if let Some(year) = d.season_year {
            xml.push_str(&format!("  <premiered>{}-01-01</premiered>\n", year));
            xml.push_str(&format!("  <year>{}</year>\n", year));
        }
        if let Some(score) = d.average_score {
            xml.push_str(&format!(
                "  <rating>{:.1}</rating>\n",
                (score as f32) / 10.0
            ));
        }
    }

    xml.push_str(&format!(
        "  <uniqueid type=\"anilist\" default=\"true\">{}</uniqueid>\n",
        series.anilist_id
    ));

    // Season-scoped art: `folder.jpg` next to this season.nfo is the
    // per-season poster that Jellyfin's season-card UI reads. Without
    // the explicit pointer, some Jellyfin scanner configurations fall
    // back to the series-root poster for the season card, which
    // defeats the purpose of writing a season folder image at all.
    // Only emit the reference when the file actually landed (mirrors
    // the gating in `write_series_nfo`'s <art> block).
    if has_folder_poster {
        xml.push_str("  <art>\n");
        xml.push_str("    <poster>folder.jpg</poster>\n");
        xml.push_str("  </art>\n");
    }

    xml.push_str("</season>\n");

    tokio::fs::write(path, xml).await
}

/// Write an episode `.nfo` alongside the renamed video file.
/// `path` should have a `.nfo` extension.
/// Falls back gracefully when title or air date are unavailable.
///
/// `runtime_minutes` is the per-episode runtime from the cached series
/// detail, or `None` when unknown. Jellyfin shows "Unknown" duration on
/// episode cards until it has scanned the file once, so emitting the
/// AniList runtime up-front is a meaningful UX improvement.
pub async fn write_episode_nfo(
    path: &Path,
    showtitle: &str,
    season: i32,
    episode: i32,
    ep_title: &str,
    aired: &str,
    runtime_minutes: Option<i32>,
) -> std::io::Result<()> {
    let display_title = if ep_title.trim().is_empty() {
        format!("Episode {}", episode)
    } else {
        ep_title.to_string()
    };

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <episodedetails>\n\
         \x20\x20<title>{title}</title>\n\
         \x20\x20<showtitle>{show}</showtitle>\n\
         \x20\x20<season>{season}</season>\n\
         \x20\x20<episode>{episode}</episode>\n\
         \x20\x20<aired>{aired}</aired>\n",
        title = xml_escape(&display_title),
        show = xml_escape(showtitle),
        season = season,
        episode = episode,
        aired = aired,
    );

    if let Some(runtime) = runtime_minutes
        && runtime > 0
    {
        xml.push_str(&format!("  <runtime>{}</runtime>\n", runtime));
    }

    xml.push_str("</episodedetails>\n");

    tokio::fs::write(path, xml).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_tags_removes_anilist_markup() {
        let input = "First sentence.<br><br>Second sentence with <i>emphasis</i> and a <a href=\"x\">link</a>";
        let out = strip_html_tags(input);
        // Tags gone, structural tags act as word separators, runs of
        // whitespace collapsed. (A trailing tag before punctuation can
        // leave a stray space — see strip_html_tags doc comment — so this
        // test deliberately ends without trailing punctuation.)
        assert_eq!(
            out,
            "First sentence. Second sentence with emphasis and a link"
        );
    }

    #[test]
    fn strip_html_tags_handles_unbalanced_input() {
        // Malformed AniList descriptions shouldn't crash or eat characters
        // outside the broken tag. When the input ends mid-tag, the buffered
        // chars are flushed as literal text (without the leading `<`).
        assert_eq!(strip_html_tags("hello <not closed"), "hello not closed");
        assert_eq!(strip_html_tags("plain text"), "plain text");
        // Two consecutive unclosed `<` — the first one's contents are
        // flushed when the second `<` arrives.
        assert_eq!(strip_html_tags("a <bc<d"), "a bc d");
    }

    fn detail_with_everything() -> AnimeDetail {
        AnimeDetail {
            is_adult: false,
            id: 12345,
            id_mal: Some(67890),
            title_romaji: "Romaji Title".to_string(),
            title_english: "English Title".to_string(),
            title_native: "原題".to_string(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(12),
            duration: Some(24),
            season: "WINTER".to_string(),
            season_year: Some(2024),
            end_year: Some(2024),
            description: "A <i>brilliant</i> story.<br>About things.".to_string(),
            genres: vec!["Action".to_string(), "Drama".to_string()],
            average_score: Some(85),
            average_score_display: Some("85%".to_string()),
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn series_stub() -> Series {
        Series {
            is_adult: false,
            id: 1,
            anilist_id: 12345,
            mal_id: Some(67890),
            title: "English Title".to_string(),
            title_romaji: "Romaji Title".to_string(),
            title_english: "English Title".to_string(),
            title_native: "原題".to_string(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            episodes: Some(12),
            season_year: Some(2024),
            end_year: Some(2024),
            folder_name: "english-title".to_string(),
            monitor_mode: "future".to_string(),
            allow_upgrades: true,
            allow_pt_upgrades: false,
            custom_query_tokens: String::new(),
            restrict_to_uploader: String::new(),
            cumulative_prior_episodes: 0,
            monitor_mode_manual_override: false,
            user_score: None,
            added_at: String::new(),
        }
    }

    fn unique_temp_path(suffix: &str) -> std::path::PathBuf {
        let nonce = format!(
            "ryokan_nfo_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            suffix,
        );
        let dir = std::env::temp_dir().join(nonce);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir.join(suffix)
    }

    async fn render_series_nfo(detail: Option<&AnimeDetail>) -> String {
        // Default to all-landed so the existing enrichment tests
        // (which don't care about <art>/<fanart> presence) don't have
        // to care about the flag plumbing.
        render_series_nfo_full(detail, "english", true, true, true).await
    }

    async fn render_series_nfo_with_lang(detail: Option<&AnimeDetail>, lang: &str) -> String {
        render_series_nfo_full(detail, lang, true, true, true).await
    }

    async fn render_series_nfo_full(
        detail: Option<&AnimeDetail>,
        lang: &str,
        has_poster: bool,
        has_banner: bool,
        has_backdrop: bool,
    ) -> String {
        let path = unique_temp_path("tvshow.nfo");
        write_series_nfo(
            &path,
            &series_stub(),
            detail,
            lang,
            has_poster,
            has_banner,
            has_backdrop,
        )
        .await
        .expect("write nfo");
        let xml = std::fs::read_to_string(&path).expect("read nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        xml
    }

    #[tokio::test]
    async fn series_nfo_with_detail_emits_plot_year_rating_genres() {
        let detail = detail_with_everything();
        let xml = render_series_nfo(Some(&detail)).await;

        // Plot is HTML-stripped.
        assert!(xml.contains("<plot>A brilliant story. About things.</plot>"));
        // Year + premiered both emitted from season_year.
        assert!(xml.contains("<year>2024</year>"));
        assert!(xml.contains("<premiered>2024-01-01</premiered>"));
        // 85/100 → 8.5/10.
        assert!(xml.contains("<rating>8.5</rating>"));
        // Per-episode runtime in minutes.
        assert!(xml.contains("<runtime>24</runtime>"));
        // Real genres + the always-on Animation tag.
        assert!(xml.contains("<genre>Action</genre>"));
        assert!(xml.contains("<genre>Drama</genre>"));
        assert!(xml.contains("<genre>Animation</genre>"));
        // Status maps to Jellyfin's vocabulary.
        assert!(xml.contains("<status>Ended</status>"));
        // Identifiers preserved.
        assert!(xml.contains("<uniqueid type=\"anilist\" default=\"true\">12345</uniqueid>"));
        assert!(xml.contains("<uniqueid type=\"myanimelist\">67890</uniqueid>"));
    }

    #[tokio::test]
    async fn series_nfo_without_detail_falls_back_to_minimal() {
        let xml = render_series_nfo(None).await;
        // No enrichment fields — just title/originaltitle/status/genre/ids.
        assert!(!xml.contains("<plot>"));
        assert!(!xml.contains("<year>"));
        assert!(!xml.contains("<rating>"));
        assert!(!xml.contains("<runtime>"));
        // Animation fallback still emitted so the category filter behaves.
        assert!(xml.contains("<genre>Animation</genre>"));
        assert!(xml.contains("<title>English Title</title>"));
    }

    #[tokio::test]
    async fn series_nfo_does_not_double_emit_animation_when_anilist_lists_it() {
        let mut detail = detail_with_everything();
        detail.genres = vec!["Animation".to_string(), "Adventure".to_string()];
        let xml = render_series_nfo(Some(&detail)).await;
        // Animation should appear exactly once (from AniList) — the fallback
        // must not double-emit it.
        assert_eq!(xml.matches("<genre>Animation</genre>").count(), 1);
        assert!(xml.contains("<genre>Adventure</genre>"));
    }

    #[tokio::test]
    async fn episode_nfo_emits_runtime_when_provided() {
        let path = unique_temp_path("ep_with_runtime.nfo");
        write_episode_nfo(&path, "Show", 1, 5, "The Title", "2024-03-01", Some(24))
            .await
            .expect("write nfo");
        let xml = std::fs::read_to_string(&path).expect("read nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        assert!(xml.contains("<runtime>24</runtime>"));
        assert!(xml.contains("<title>The Title</title>"));
        assert!(xml.contains("<aired>2024-03-01</aired>"));
    }

    #[tokio::test]
    async fn episode_nfo_omits_runtime_when_unknown() {
        let path = unique_temp_path("ep_no_runtime.nfo");
        write_episode_nfo(&path, "Show", 1, 5, "", "", None)
            .await
            .expect("write nfo");
        let xml = std::fs::read_to_string(&path).expect("read nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        assert!(!xml.contains("<runtime>"));
        // Empty title falls back to "Episode N".
        assert!(xml.contains("<title>Episode 5</title>"));
    }

    // ── title_for_preference ─────────────────────────────────────────────

    #[test]
    fn title_for_preference_picks_english_by_default_and_falls_back_in_order() {
        let s = series_stub();
        // English available → english preference returns English.
        assert_eq!(title_for_preference(&s, "english"), "English Title");
        // Unknown preference defaults to english-first.
        assert_eq!(title_for_preference(&s, "bogus"), "English Title");
        // Empty english falls through to romaji → native → title.
        let mut s2 = s.clone();
        s2.title_english.clear();
        assert_eq!(title_for_preference(&s2, "english"), "Romaji Title");
        s2.title_romaji.clear();
        assert_eq!(title_for_preference(&s2, "english"), "原題");
    }

    #[test]
    fn title_for_preference_romaji_and_native_respect_preference() {
        let s = series_stub();
        assert_eq!(title_for_preference(&s, "romaji"), "Romaji Title");
        assert_eq!(title_for_preference(&s, "native"), "原題");
    }

    #[test]
    fn title_for_preference_empty_preferred_field_falls_back() {
        // Romaji preference but only english is populated — should fall
        // back to english rather than emit the empty string.
        let mut s = series_stub();
        s.title_romaji.clear();
        s.title_native.clear();
        s.title.clear();
        assert_eq!(title_for_preference(&s, "romaji"), "English Title");
    }

    // ── preference propagates into series NFO <title> ────────────────────

    #[tokio::test]
    async fn series_nfo_title_respects_romaji_preference() {
        let xml = render_series_nfo_with_lang(None, "romaji").await;
        assert!(xml.contains("<title>Romaji Title</title>"));
        assert!(!xml.contains("<title>English Title</title>"));
    }

    #[tokio::test]
    async fn series_nfo_title_respects_native_preference() {
        let xml = render_series_nfo_with_lang(None, "native").await;
        assert!(xml.contains("<title>原題</title>"));
    }

    // ── season NFO ────────────────────────────────────────────────────────

    async fn render_season_nfo(
        detail: Option<&AnimeDetail>,
        lang: &str,
        has_folder_poster: bool,
    ) -> String {
        let path = unique_temp_path("season.nfo");
        write_season_nfo(&path, 1, &series_stub(), detail, lang, has_folder_poster)
            .await
            .expect("write season nfo");
        let xml = std::fs::read_to_string(&path).expect("read season nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        xml
    }

    #[tokio::test]
    async fn season_nfo_emits_seasonnumber_and_anilist_uniqueid() {
        let detail = detail_with_everything();
        let xml = render_season_nfo(Some(&detail), "english", true).await;

        // Root is <season>, not <tvshow>. Jellyfin keys on this.
        assert!(xml.contains("<season>"));
        assert!(xml.contains("</season>"));
        // Season number is the load-bearing field — without it
        // Jellyfin re-scrapes.
        assert!(xml.contains("<seasonnumber>1</seasonnumber>"));
        // AniList unique id so Jellyfin matches the season to the
        // series, not to an unrelated TVDB entry.
        assert!(xml.contains("<uniqueid type=\"anilist\" default=\"true\">12345</uniqueid>"));
        // Plot/year/rating enrichment when detail is provided.
        assert!(xml.contains("<plot>A brilliant story. About things.</plot>"));
        assert!(xml.contains("<year>2024</year>"));
        assert!(xml.contains("<rating>8.5</rating>"));
    }

    #[tokio::test]
    async fn season_nfo_without_detail_still_has_seasonnumber_and_id() {
        let xml = render_season_nfo(None, "english", true).await;
        assert!(xml.contains("<seasonnumber>1</seasonnumber>"));
        assert!(xml.contains("<uniqueid type=\"anilist\" default=\"true\">12345</uniqueid>"));
        assert!(!xml.contains("<plot>"));
    }

    #[tokio::test]
    async fn season_nfo_title_respects_preference() {
        let xml = render_season_nfo(None, "native", true).await;
        assert!(xml.contains("<title>原題</title>"));
    }

    // ── <art> tag emission ────────────────────────────────────────────────

    #[tokio::test]
    async fn series_nfo_emits_art_block_referencing_poster_and_banner_when_both_present() {
        let xml = render_series_nfo_full(None, "english", true, true, false).await;
        assert!(xml.contains("<art>"));
        assert!(xml.contains("<poster>poster.jpg</poster>"));
        assert!(xml.contains("<banner>banner.jpg</banner>"));
        assert!(xml.contains("</art>"));
    }

    #[tokio::test]
    async fn series_nfo_omits_banner_tag_when_banner_missing() {
        // Regression: the PR-51 review flagged that a hard-coded
        // <banner>banner.jpg</banner> would leave Jellyfin staring at
        // a missing file on every scan. Gate must hold.
        let xml = render_series_nfo_full(None, "english", true, false, false).await;
        assert!(xml.contains("<poster>poster.jpg</poster>"));
        assert!(!xml.contains("<banner>"));
    }

    #[tokio::test]
    async fn series_nfo_omits_all_art_blocks_when_nothing_landed() {
        let xml = render_series_nfo_full(None, "english", false, false, false).await;
        assert!(!xml.contains("<art>"));
        assert!(!xml.contains("<poster>"));
        assert!(!xml.contains("<banner>"));
        assert!(!xml.contains("<fanart>"));
    }

    #[tokio::test]
    async fn series_nfo_emits_fanart_block_when_backdrop_landed() {
        // <fanart><thumb>backdrop.jpg</thumb></fanart> is a sibling of
        // <art>, not a child — that's what maps to Jellyfin's
        // ImageType::Backdrop slot, which is what the series detail
        // page actually renders behind the header.
        let xml = render_series_nfo_full(None, "english", false, false, true).await;
        assert!(xml.contains("<fanart>"));
        assert!(xml.contains("<thumb>backdrop.jpg</thumb>"));
        assert!(xml.contains("</fanart>"));
        // Sibling, not child: the <fanart> block must not appear
        // inside <art>.
        assert!(
            !xml.contains("<art>"),
            "no poster or banner landed, so <art> should be skipped",
        );
    }

    #[tokio::test]
    async fn series_nfo_omits_fanart_when_backdrop_missing() {
        let xml = render_series_nfo_full(None, "english", true, true, false).await;
        assert!(!xml.contains("<fanart>"));
        assert!(!xml.contains("backdrop.jpg"));
    }

    #[tokio::test]
    async fn season_nfo_emits_art_block_with_folder_poster_when_present() {
        let xml = render_season_nfo(None, "english", true).await;
        assert!(xml.contains("<art>"));
        assert!(xml.contains("<poster>folder.jpg</poster>"));
        assert!(xml.contains("</art>"));
    }

    #[tokio::test]
    async fn season_nfo_omits_art_block_when_folder_poster_missing() {
        let xml = render_season_nfo(None, "english", false).await;
        assert!(!xml.contains("<art>"));
        assert!(!xml.contains("<poster>"));
    }

    // ─── xml_escape + strip_html_tags (pure-helper gap coverage) ───────

    #[test]
    fn xml_escape_replaces_the_four_canonical_entities() {
        assert_eq!(super::xml_escape("a & b"), "a &amp; b");
        assert_eq!(super::xml_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(
            super::xml_escape("she said \"hi\""),
            "she said &quot;hi&quot;"
        );
    }

    #[test]
    fn xml_escape_escapes_ampersand_before_tag_chars() {
        // Canonical "escape & first" ordering — if `<` were escaped
        // before `&`, the subsequent `&` pass would turn the emitted
        // `&lt;` into `&amp;lt;`, double-escaping every tag char.
        assert_eq!(super::xml_escape("<a&b>"), "&lt;a&amp;b&gt;");
    }

    #[test]
    fn xml_escape_is_idempotent_on_already_escaped_text() {
        // An already-escaped string re-escapes the `&amp;` prefix —
        // that's expected behavior (xml_escape doesn't claim
        // idempotence, it claims "output is valid XML"), so pin it so
        // a future "optimize" that swaps to a one-shot scan catches
        // the change.
        assert_eq!(super::xml_escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn xml_escape_preserves_plain_ascii_and_unicode() {
        assert_eq!(super::xml_escape("plain text"), "plain text");
        assert_eq!(super::xml_escape("日本語"), "日本語");
        assert_eq!(super::xml_escape(""), "");
    }

    #[test]
    fn xml_escape_preserves_apostrophe_intentionally() {
        // Apostrophe (single quote) is not in the escape list.
        // The encoder's output goes into element text and double-
        // quoted attribute values — HTML/XML doesn't require
        // escaping `'` in either context, and leaving it raw keeps
        // NFO titles like "Don't Look" readable rather than rendering
        // as "Don&apos;t Look" in Jellyfin's UI.
        assert_eq!(super::xml_escape("Don't"), "Don't");
    }

    #[test]
    fn strip_html_tags_removes_inline_tags_and_collapses_whitespace() {
        let input = "A <b>bold</b> claim<br>and another.";
        assert_eq!(super::strip_html_tags(input), "A bold claim and another.");
    }

    #[test]
    fn strip_html_tags_handles_unmatched_trailing_open_bracket_as_literal() {
        // Input ends mid-tag — the buffered chars should flush as
        // literal content rather than disappear silently. Matches
        // the docstring invariant on `strip_html_tags`.
        let input = "broken <open-tag-never-closed";
        let out = super::strip_html_tags(input);
        assert!(
            out.contains("open-tag-never-closed"),
            "unmatched trailing tag content should survive as literal: got {out}"
        );
    }

    #[test]
    fn strip_html_tags_on_plain_text_is_identity_modulo_whitespace() {
        // No tags — function collapses runs of whitespace into single
        // spaces but keeps the words themselves intact.
        assert_eq!(super::strip_html_tags("hello   world"), "hello world");
    }
}
