use super::*;
use crate::models::series;

fn unique_media_root(suffix: &str) -> std::path::PathBuf {
    let nonce = format!(
        "ryokan_pages_test_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        suffix,
    );
    let root = std::env::temp_dir().join(nonce);
    std::fs::create_dir_all(&root).expect("create media root");
    root
}

fn empty_anime_detail(id: i64, title_english: &str, episodes: Option<i32>) -> anilist::AnimeDetail {
    anilist::AnimeDetail {
        is_adult: false,
        id,
        id_mal: None,
        title_romaji: title_english.to_string(),
        title_english: title_english.to_string(),
        title_native: String::new(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".to_string(),
        status: "FINISHED".to_string(),
        status_display: "Finished".to_string(),
        episodes,
        duration: Some(24),
        season: String::new(),
        season_year: Some(2015),
        end_year: Some(2015),
        description: String::new(),
        genres: Vec::new(),
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

/// Issue #45: a BD release can partition a series into more files
/// than AniList reports (Owarimonogatari S1 — AL says 12 eps, the
/// [smol] BD has 13 files because it splits the 48-min aired ep 1
/// back into two halves). Before the fix, `build_episodes` only
/// looped 1..=ep_count, so file 13 was routed to disk by
/// auto-expand but never rendered in the UI. The fix surfaces
/// any on-disk file with ep > ep_count as its own row.
#[tokio::test]
async fn build_episodes_surfaces_on_disk_files_beyond_anilist_episode_count() {
    let db = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 21320,
            mal_id: None,
            title: "Owarimonogatari",
            title_romaji: "Owarimonogatari",
            title_english: "Owarimonogatari",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2015),
            end_year: Some(2015),
        },
    )
    .await
    .expect("series upsert");

    // Write 13 synthetic episode files — ep 13 exceeds AL's count.
    let media_root = unique_media_root("surface_beyond_count");
    let series_folder = media_root.join("Owarimonogatari");
    std::fs::create_dir_all(&series_folder).expect("create series dir");
    for ep in 1..=13 {
        let fname = format!("Owarimonogatari - S01E{:02} - Episode.mkv", ep);
        std::fs::write(series_folder.join(&fname), b"x").expect("write ep file");
    }

    // AL reports 12 eps (the on-air ep 1 was a 48-min merged episode).
    let detail = empty_anime_detail(21320, "Owarimonogatari", Some(12));

    let (episodes, on_disk_count, downloaded_count, _size, _monitored) = build_episodes(
        &db,
        &detail,
        Some(series_id),
        "Owarimonogatari",
        media_root.to_str().expect("media root str"),
    )
    .await;

    // Sorted desc by number, so ep 13 is first.
    assert_eq!(
        episodes.len(),
        13,
        "expected 13 rows (1..=12 from AL count + 13 from disk overflow), got {}",
        episodes.len()
    );
    let ep13 = episodes
        .iter()
        .find(|e| e.number == 13)
        .expect("ep 13 row present");
    assert!(ep13.on_disk, "ep 13 must render as on_disk");
    assert_eq!(on_disk_count, 13, "on_disk_count must include the overflow");
    assert_eq!(downloaded_count, 13, "downloaded_count same");

    // Cleanup (best effort).
    std::fs::remove_dir_all(&media_root).ok();
}

/// Regression guard: when every on-disk file falls within AL's
/// ep_count, the surface-beyond-count pass must not duplicate rows
/// the main loop already rendered.
#[tokio::test]
async fn build_episodes_does_not_duplicate_rows_when_disk_matches_count() {
    let db = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 999,
            mal_id: None,
            title: "Test Series",
            title_romaji: "Test Series",
            title_english: "Test Series",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2020),
            end_year: Some(2020),
        },
    )
    .await
    .expect("series upsert");

    let media_root = unique_media_root("no_duplicates");
    let series_folder = media_root.join("Test Series");
    std::fs::create_dir_all(&series_folder).expect("create series dir");
    for ep in 1..=12 {
        let fname = format!("Test Series - S01E{:02} - Episode.mkv", ep);
        std::fs::write(series_folder.join(&fname), b"x").expect("write ep file");
    }

    let detail = empty_anime_detail(999, "Test Series", Some(12));

    let (episodes, _, _, _, _) = build_episodes(
        &db,
        &detail,
        Some(series_id),
        "Test Series",
        media_root.to_str().expect("media root str"),
    )
    .await;

    assert_eq!(episodes.len(), 12, "no duplicates: exactly 12 rows");

    std::fs::remove_dir_all(&media_root).ok();
}

/// Issue #45 follow-up: during the download the overflow file isn't
/// on disk yet, but auto-expand has already written a grab-tag row
/// for it. `build_episodes` must surface that tag as a row (in
/// 'grabbed' state) so the user sees the extra episode's download
/// progress immediately — not just after post-processing runs.
#[tokio::test]
async fn build_episodes_surfaces_grab_tags_beyond_ep_count_without_disk_file() {
    use crate::services::source::ClassificationResult;

    let db = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 21262,
            mal_id: None,
            title: "Owarimonogatari",
            title_romaji: "Owarimonogatari",
            title_english: "Owarimonogatari",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2015),
            end_year: Some(2015),
        },
    )
    .await
    .expect("series upsert");

    // Write a grab tag for ep 13 (AL-overflow) — simulates what
    // auto_expand::expand_from_files does when it backfills a tag
    // for a parent file whose parsed ep exceeds AL's count.
    crate::models::episode_tags::record_grab(
        &db,
        series_id,
        13,
        &ClassificationResult::unknown(),
        "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
        "smol",
        0,
        true,
    )
    .await
    .expect("record_grab for ep 13");

    // Empty media root — torrent is still downloading, nothing
    // has landed in the library folder yet.
    let media_root = unique_media_root("surfaces_grab_tag_no_disk");
    let series_folder = media_root.join("Owarimonogatari");
    std::fs::create_dir_all(&series_folder).expect("create series dir");

    let detail = empty_anime_detail(21262, "Owarimonogatari", Some(12));

    let (episodes, on_disk_count, downloaded_count, _size, _monitored) = build_episodes(
        &db,
        &detail,
        Some(series_id),
        "Owarimonogatari",
        media_root.to_str().expect("media root str"),
    )
    .await;

    assert_eq!(
        episodes.len(),
        13,
        "expected 13 rows (1..=12 from AL + overflow E13 from grab tag), got {}",
        episodes.len()
    );
    let ep13 = episodes
        .iter()
        .find(|e| e.number == 13)
        .expect("ep 13 row present from grab tag");
    assert!(!ep13.on_disk, "no disk file yet, so on_disk must be false");
    assert!(!ep13.downloaded, "tag state is 'grabbed', not 'completed'");
    assert_eq!(ep13.quality_state, "grabbed");
    assert_eq!(on_disk_count, 0, "nothing on disk yet");
    assert_eq!(downloaded_count, 0, "nothing completed yet");

    std::fs::remove_dir_all(&media_root).ok();
}

// ── Pure-helper coverage ──────────────────────────────────────────
//
// The async/DB-bound `build_episodes` tests above pin the heaviest
// user-visible flows. The helpers in this section are the small
// pure functions the page renderers fan out to — they're the
// load-bearing invariants every relation card / episode list /
// size-display string passes through. None had unit tests before
// this commit.

/// Zero-init `RelatedEntry`. Tests mutate only the fields that
/// matter to the case so the assertion focus is on what's being
/// pinned, not on a wall of empty positional args.
fn default_relation() -> anilist::RelatedEntry {
    anilist::RelatedEntry {
        id: 1,
        id_mal: None,
        title_romaji: String::new(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: String::new(),
        status: String::new(),
        status_display: String::new(),
        episodes: None,
        relation_type: String::new(),
        season_year: None,
        media_type: String::new(),
    }
}

// ── format_relation_label ────────────────────────────────────────

#[test]
fn format_relation_label_known_types_get_friendly_names() {
    assert_eq!(format_relation_label("PREQUEL"), "Prequel");
    assert_eq!(format_relation_label("SEQUEL"), "Sequel");
    assert_eq!(format_relation_label("SIDE_STORY"), "Side Story");
    assert_eq!(format_relation_label("SPIN_OFF"), "Spin Off");
}

#[test]
fn format_relation_label_unknown_type_replaces_underscores() {
    // AL adds new relation_type variants periodically (most
    // recently `CONTAINS`). The fallback turns underscores into
    // spaces so the new variant renders readably without a code
    // change.
    assert_eq!(format_relation_label("BRAND_NEW_TYPE"), "BRAND NEW TYPE");
    assert_eq!(format_relation_label(""), "");
}

// ── non_empty_or ──────────────────────────────────────────────────

#[test]
fn non_empty_or_uses_value_when_non_empty() {
    assert_eq!(non_empty_or("real", "fallback"), "real");
}

#[test]
fn non_empty_or_falls_back_on_whitespace() {
    // Trim before the empty-check — `"   "` is a fallback case,
    // not a value the user intended to display.
    assert_eq!(non_empty_or("", "fallback"), "fallback");
    assert_eq!(non_empty_or("   ", "fallback"), "fallback");
    assert_eq!(non_empty_or("\t\n", "fallback"), "fallback");
}

// ── preferred_title ──────────────────────────────────────────────

#[test]
fn preferred_title_prefers_english_then_romaji_then_native() {
    // Order is fixed: english > romaji > native. A regression in
    // the priority would break every "Show: <title>" label across
    // the UI.
    assert_eq!(preferred_title("Eng", "Rom", "Nat"), "Eng");
    assert_eq!(preferred_title("", "Rom", "Nat"), "Rom");
    assert_eq!(preferred_title("", "", "Nat"), "Nat");
    assert_eq!(preferred_title("", "", ""), "");
}

// ── format_size ──────────────────────────────────────────────────

#[test]
fn format_size_zero_returns_empty_string() {
    // The "size unknown / not yet measured" sentinel renders blank
    // rather than `"0 MiB"` which would clutter every ungrabbed
    // episode row.
    assert_eq!(format_size(0), "");
}

#[test]
fn format_size_uses_mib_under_1_gib() {
    // 100 MiB → "100 MiB"; integer-rounded.
    assert_eq!(format_size(100 * 1024 * 1024), "100 MiB");
    // 500 MiB.
    assert_eq!(format_size(500 * 1024 * 1024), "500 MiB");
}

#[test]
fn format_size_uses_gib_at_or_above_1_gib() {
    // Boundary: exactly 1 GiB → "1.0 GiB". One decimal precision.
    assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
    // Typical BD episode (~1.5 GiB).
    assert_eq!(
        format_size((1.5_f64 * 1024.0 * 1024.0 * 1024.0) as u64),
        "1.5 GiB"
    );
    // Full season pack.
    assert_eq!(format_size(20 * 1024 * 1024 * 1024), "20.0 GiB");
}

// ── relation_identity_key ────────────────────────────────────────

#[test]
fn relation_identity_key_prefers_mal_over_provider() {
    // MAL ID is more stable across provider rebinds — when both
    // are available, key on MAL so a re-link doesn't re-key the
    // relation cache.
    assert_eq!(relation_identity_key(42, Some(99)), "mal:99");
}

#[test]
fn relation_identity_key_falls_back_to_provider() {
    assert_eq!(relation_identity_key(42, None), "provider:42");
}

#[test]
fn relation_identity_key_handles_negative_provider_id() {
    // Negative-provider sentinel for MAL-fallback rows should
    // still produce a stable key, not panic on formatting.
    assert_eq!(relation_identity_key(-99, None), "provider:-99");
}

// ── relation_richness ────────────────────────────────────────────

#[test]
fn relation_richness_zero_for_empty_relation() {
    assert_eq!(relation_richness(&default_relation()), 0);
}

#[test]
fn relation_richness_scores_each_field() {
    // The formula awards: cover=4, format=2, status=2, episodes=1,
    // title=1. Pin each via a relation that has only that field.
    let mut cover_only = default_relation();
    cover_only.cover_url = "url".to_string();
    assert_eq!(relation_richness(&cover_only), 4);

    let mut format_only = default_relation();
    format_only.format = "TV".to_string();
    assert_eq!(relation_richness(&format_only), 2);

    let mut status_only = default_relation();
    status_only.status = "FINISHED".to_string();
    assert_eq!(relation_richness(&status_only), 2);

    let mut episodes_only = default_relation();
    episodes_only.episodes = Some(12);
    assert_eq!(relation_richness(&episodes_only), 1);

    let mut title_only = default_relation();
    title_only.title_english = "Eng".to_string();
    assert_eq!(relation_richness(&title_only), 1);
}

#[test]
fn relation_richness_treats_tba_as_missing() {
    // AL emits "TBA" for unscheduled metadata; the richness
    // function discounts it as "no signal" rather than counting
    // it as present.
    let mut tba = default_relation();
    tba.title_english = "Eng".to_string();
    tba.format = "TBA".to_string();
    tba.status = "TBA".to_string();
    // Only the title contributes (1).
    assert_eq!(relation_richness(&tba), 1);
}

#[test]
fn relation_richness_zero_episodes_does_not_count() {
    // `episodes: Some(0)` (e.g., an unscheduled cour) contributes
    // nothing — the gate is `> 0`.
    let mut zero_eps = default_relation();
    zero_eps.title_english = "Eng".to_string();
    zero_eps.episodes = Some(0);
    assert_eq!(relation_richness(&zero_eps), 1);
}

// ── merge_relation_metadata ──────────────────────────────────────

#[test]
fn merge_relation_metadata_keeps_primary_when_complete() {
    // Primary has every field — fallback's data is never read.
    let mut primary = default_relation();
    primary.title_english = "Eng".to_string();
    primary.title_romaji = "Rom".to_string();
    primary.cover_url = "url".to_string();
    primary.format = "TV".to_string();
    primary.status = "FINISHED".to_string();
    primary.episodes = Some(12);
    primary.season_year = Some(2024);
    primary.id_mal = Some(99);
    primary.media_type = "ANIME".to_string();

    let mut fallback = default_relation();
    fallback.title_english = "Other".to_string();
    fallback.format = "OVA".to_string();
    fallback.episodes = Some(1);
    fallback.id_mal = Some(1);

    let merged = merge_relation_metadata(&primary, &fallback);
    assert_eq!(merged.title_english, "Eng");
    assert_eq!(merged.format, "TV");
    assert_eq!(merged.episodes, Some(12));
    assert_eq!(merged.id_mal, Some(99));
}

#[test]
fn merge_relation_metadata_fills_empty_fields_from_fallback() {
    // Primary missing every field — every fallback value lands.
    let primary = default_relation();
    let mut fallback = default_relation();
    fallback.title_english = "Eng".to_string();
    fallback.title_romaji = "Rom".to_string();
    fallback.cover_url = "url".to_string();
    fallback.format = "TV".to_string();
    fallback.status = "FINISHED".to_string();
    fallback.episodes = Some(12);
    fallback.season_year = Some(2024);
    fallback.id_mal = Some(99);
    fallback.media_type = "ANIME".to_string();

    let merged = merge_relation_metadata(&primary, &fallback);
    assert_eq!(merged.title_english, "Eng");
    assert_eq!(merged.title_romaji, "Rom");
    assert_eq!(merged.cover_url, "url");
    assert_eq!(merged.format, "TV");
    assert_eq!(merged.status, "FINISHED");
    assert_eq!(merged.episodes, Some(12));
    assert_eq!(merged.season_year, Some(2024));
    assert_eq!(merged.id_mal, Some(99));
    assert_eq!(merged.media_type, "ANIME");
}

#[test]
fn merge_relation_metadata_treats_tba_as_replaceable() {
    // TBA is a placeholder, not data. Both the format and status
    // arms have a `|| field == "TBA"` clause so a fallback with
    // real metadata wins over a TBA primary.
    let mut primary = default_relation();
    primary.title_english = "Eng".to_string();
    primary.format = "TBA".to_string();
    primary.status = "TBA".to_string();

    let mut fallback = default_relation();
    fallback.format = "TV".to_string();
    fallback.status = "FINISHED".to_string();

    let merged = merge_relation_metadata(&primary, &fallback);
    assert_eq!(merged.format, "TV");
    assert_eq!(merged.status, "FINISHED");
}

#[test]
fn merge_relation_metadata_replaces_zero_episodes_with_fallback() {
    // `episodes == Some(0)` is treated as "unknown" for merge
    // purposes — same as `None`. A fallback with real data wins.
    let mut primary = default_relation();
    primary.title_english = "Eng".to_string();
    primary.format = "TV".to_string();
    primary.episodes = Some(0);

    let mut fallback = default_relation();
    fallback.episodes = Some(12);

    let merged = merge_relation_metadata(&primary, &fallback);
    assert_eq!(merged.episodes, Some(12));
}

// ── episode_needs_kitsu_backfill ─────────────────────────────────

#[test]
fn episode_needs_kitsu_backfill_short_series_never_backfills() {
    // 1-episode series (movies / OVAs) never trigger the Kitsu
    // round-trip, even if Jikan returned nothing — the backfill
    // overhead isn't worth it for one missing title.
    for ep_count in [0, 1] {
        assert!(!episode_needs_kitsu_backfill(ep_count, |_| false));
    }
}

#[test]
fn episode_needs_kitsu_backfill_under_tolerance_skips() {
    // 12-episode series, 5 missing — under the 10-ep tolerance.
    // Skip the backfill: Jikan/MAL is allowed to lag a handful of
    // recent episodes on a still-airing show without forcing the
    // Kitsu HTTP round-trip.
    let missing_eps: std::collections::HashSet<i32> = (1..=5).collect();
    assert!(!episode_needs_kitsu_backfill(12, |ep| {
        !missing_eps.contains(&ep)
    }));
}

#[test]
fn episode_needs_kitsu_backfill_over_tolerance_triggers() {
    // 24-episode series, 11 missing — over the 10-ep tolerance.
    // Backfill fires.
    let missing_eps: std::collections::HashSet<i32> = (1..=11).collect();
    assert!(episode_needs_kitsu_backfill(24, |ep| {
        !missing_eps.contains(&ep)
    }));
}

#[test]
fn episode_needs_kitsu_backfill_complete_jikan_skips() {
    // All titles present — no backfill needed.
    assert!(!episode_needs_kitsu_backfill(24, |_| true));
}

// ── should_persist_detail_cache (handlers/library/reconcile) ─────
//
// Tested here rather than in reconcile.rs because the helper is
// private — keeping the test next to other library-handler
// helpers means the suite has one obvious home.

#[test]
fn should_persist_detail_cache_sentinel_anilist_id_always_persists() {
    // Negative anilist_id is the Jikan-fallback sentinel
    // (-mal_id). For these rows the AL detail can never match
    // (the row has no AL identity yet); the cache write is the
    // only way the metadata-cache → relations chain ever
    // populates. Always persist.
    let detail = empty_anime_detail(123, "Show", None);
    assert!(super::super::reconcile::should_persist_detail_cache_for_test(-999, &detail));
    // Zero anilist_id (theoretically impossible after the
    // negative-ID-sentinel sweep, but defensive) also persists.
    assert!(super::super::reconcile::should_persist_detail_cache_for_test(0, &detail));
}

#[test]
fn should_persist_detail_cache_real_anilist_id_requires_match() {
    let detail = empty_anime_detail(42, "Show", None);
    // Match → persist.
    assert!(super::super::reconcile::should_persist_detail_cache_for_test(42, &detail));
    // Mismatch → don't persist (the detail is for a different AL entry).
    assert!(
        !super::super::reconcile::should_persist_detail_cache_for_test(
            42,
            &empty_anime_detail(99, "Show", None)
        )
    );
    // Detail.id = 0 → don't persist (defensive, matches the
    // `id > 0` guard the AL parse path enforces).
    assert!(
        !super::super::reconcile::should_persist_detail_cache_for_test(
            42,
            &empty_anime_detail(0, "Show", None)
        )
    );
}

// ── normalize_system_tab (handlers/system) ───────────────────────

#[test]
fn normalize_system_tab_known_tabs_pass_through() {
    for tab in ["scoring", "debug", "rss", "tasks", "review", "credits"] {
        assert_eq!(
            crate::handlers::system::normalize_system_tab_for_test(Some(tab.into())),
            tab
        );
    }
}

#[test]
fn normalize_system_tab_help_alias_resolves_to_scoring() {
    // Legacy alias from when scoring rules used to live on a
    // dedicated /system?tab=help page. Pinning the aliasing so
    // the redirect-via-tab convention can't drift away from
    // existing bookmarks.
    assert_eq!(
        crate::handlers::system::normalize_system_tab_for_test(Some("help".into())),
        "scoring"
    );
}

#[test]
fn series_template_title_cell_is_always_clickable() {
    // Pins that the title-cell fallback (every row without a title)
    // renders as a clickable `ep-title-btn` button, never an
    // unclickable `<span class="ep-missing-text">` standalone.
    //
    // History: the original tag-overflow fix (b959c0c) widened a guard
    // to include `quality_state` and `downloaded`, but missed
    // main-loop rows where empty-title placeholder rows in
    // `series_episode_metadata` had promoted ep_count past the real
    // aired count (the One Piece 1157-1160 case — Jikan / Kitsu
    // hadn't seen the eps yet, the metadata-sync wrote
    // `source="series"` placeholders, which inflated cached_eps.len()
    // and pushed those rows into the main 1..=ep_count loop with no
    // tag, no disk file, no title → branch 3 → unclickable).
    //
    // Collapsing branches 2 and 3 makes this whole class of bug
    // impossible: every title-less row goes through the same
    // button. The grab-history modal renders fine on empty data
    // (the JS at series.js:145 covers the missing bits).
    let template = include_str!("../../../../templates/series.html");
    // Pin: the title-less span lives inside the click handler row of
    // the title-cell button. `data-on-disk="{{ ep.on_disk }}">` is
    // the LAST attribute on the open tag, so the very next line
    // being our span proves the span is wrapped by a button.
    let inside_button = "data-on-disk=\"{{ ep.on_disk }}\">\n                            <span class=\"ep-missing-text\">Episode {{ ep.number }}</span>";
    assert!(
        template.contains(inside_button),
        "series.html must keep the title-less row's `<span class=\"ep-missing-text\">Episode N</span>` immediately inside an `ep-title-btn` button (after the `data-on-disk=...` attribute) so the row is clickable for grab history"
    );
    // Pin: there's no second `<span class="ep-missing-text">Episode {{ ep.number }}</span>`
    // outside the button — we expect exactly one occurrence in the
    // whole template, and it's the one inside the button. A
    // standalone duplicate would be the unclickable branch we
    // removed.
    let occurrences = template
        .matches("<span class=\"ep-missing-text\">Episode {{ ep.number }}</span>")
        .count();
    assert_eq!(
        occurrences, 1,
        "series.html must contain exactly one `<span class=\"ep-missing-text\">Episode N</span>` (inside the title-cell button); a second occurrence would be the unclickable standalone branch the title-cell collapse removed"
    );
}

#[test]
fn normalize_system_tab_unknown_or_missing_defaults_to_logs() {
    // Logs is the safest landing — the user can always see what's
    // going on from the logs tab.
    assert_eq!(
        crate::handlers::system::normalize_system_tab_for_test(None),
        "logs"
    );
    assert_eq!(
        crate::handlers::system::normalize_system_tab_for_test(Some("garbage".into())),
        "logs"
    );
    assert_eq!(
        crate::handlers::system::normalize_system_tab_for_test(Some("".into())),
        "logs"
    );
}
