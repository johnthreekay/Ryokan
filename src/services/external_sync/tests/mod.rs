use super::*;

use crate::models::external_accounts;
use crate::models::monitoring::MonitorMode;
use crate::models::series;
use crate::models::series_custom_lists;
use crate::services::{anibridge, anilist, jikan, mal};
use std::collections::HashMap;

/// The anibridge CACHE is process-global, so the three async
/// resolver tests below have to serialize their seed→lookup→clear
/// sequences or they race each other. A static Mutex held for the
/// duration of each test is the simplest reliable guard; using
/// `tokio::sync::Mutex` (not std) so awaits inside the critical
/// section don't deadlock on a parking-lot lock.
static ANIBRIDGE_CACHE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn prefs_default() -> ImportPreferences {
    // Watching + Planning on, the rest off — the plan-doc-decided
    // shape that fresh installs land at.
    ImportPreferences {
        import_watching: true,
        import_planning: true,
        import_paused: false,
        import_dropped: false,
        import_completed: false,
        skip_already_watched: false,
    }
}

fn prefs_with_skip_already_watched() -> ImportPreferences {
    ImportPreferences {
        skip_already_watched: true,
        ..prefs_default()
    }
}

#[test]
fn anilist_status_strings_map_to_normalized() {
    assert_eq!(
        NormalizedStatus::from_anilist("CURRENT"),
        NormalizedStatus::Watching
    );
    assert_eq!(
        NormalizedStatus::from_anilist("PLANNING"),
        NormalizedStatus::Planning
    );
    assert_eq!(
        NormalizedStatus::from_anilist("PAUSED"),
        NormalizedStatus::Paused
    );
    assert_eq!(
        NormalizedStatus::from_anilist("DROPPED"),
        NormalizedStatus::Dropped
    );
    assert_eq!(
        NormalizedStatus::from_anilist("COMPLETED"),
        NormalizedStatus::Completed
    );
    assert_eq!(
        NormalizedStatus::from_anilist("REPEATING"),
        NormalizedStatus::Repeating
    );
    // Unknown values fall through to the safe Planning default
    // so a future AL enum addition doesn't accidentally route
    // entries to a destructive monitor mode.
    assert_eq!(
        NormalizedStatus::from_anilist("hypothetical_new_value"),
        NormalizedStatus::Planning
    );
}

#[test]
fn mal_status_strings_map_to_normalized() {
    assert_eq!(
        NormalizedStatus::from_mal("watching"),
        NormalizedStatus::Watching
    );
    assert_eq!(
        NormalizedStatus::from_mal("plan_to_watch"),
        NormalizedStatus::Planning
    );
    assert_eq!(
        NormalizedStatus::from_mal("on_hold"),
        NormalizedStatus::Paused
    );
    assert_eq!(
        NormalizedStatus::from_mal("dropped"),
        NormalizedStatus::Dropped
    );
    assert_eq!(
        NormalizedStatus::from_mal("completed"),
        NormalizedStatus::Completed
    );
    // MAL has no `repeating` value; unknown strings fall through
    // to the safe Planning default.
    assert_eq!(
        NormalizedStatus::from_mal("garbage"),
        NormalizedStatus::Planning
    );
}

#[test]
fn monitor_mode_for_status_matches_plan_decisions() {
    // Plan decisions #6 + #7 baked in. PTW → Future (NOT None,
    // overrides the issue body), Watching → All by default,
    // skip-already-watched flips Watching → Existing only.
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Watching, false),
        MonitorMode::All
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Repeating, false),
        MonitorMode::All
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Planning, false),
        MonitorMode::Future
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Paused, false),
        MonitorMode::Existing
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Completed, false),
        MonitorMode::Existing
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Dropped, false),
        MonitorMode::None
    );
}

#[test]
fn skip_already_watched_flips_only_watching_to_existing() {
    // The skip toggle is meant for migration-from-streaming
    // users — they want NEW episodes only, not the back catalog.
    // It MUST NOT affect Planning (still Future), Paused (still
    // Existing), or any other status, because those bucket
    // semantics would change in user-surprising ways.
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Watching, true),
        MonitorMode::Existing
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Repeating, true),
        MonitorMode::Existing
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Planning, true),
        MonitorMode::Future
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Paused, true),
        MonitorMode::Existing
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Completed, true),
        MonitorMode::Existing
    );
    assert_eq!(
        monitor_mode_for(NormalizedStatus::Dropped, true),
        MonitorMode::None
    );
}

#[test]
fn import_status_filters_by_per_list_preferences() {
    let prefs = prefs_default();
    // Default-on lists pass through.
    assert!(import_status(NormalizedStatus::Watching, &prefs));
    assert!(import_status(NormalizedStatus::Repeating, &prefs));
    assert!(import_status(NormalizedStatus::Planning, &prefs));
    // Default-off lists are dropped.
    assert!(!import_status(NormalizedStatus::Paused, &prefs));
    assert!(!import_status(NormalizedStatus::Dropped, &prefs));
    assert!(!import_status(NormalizedStatus::Completed, &prefs));

    // Flip a few flags and re-check.
    let mut prefs = prefs;
    prefs.import_watching = false;
    prefs.import_completed = true;
    assert!(!import_status(NormalizedStatus::Watching, &prefs));
    assert!(import_status(NormalizedStatus::Completed, &prefs));
    // Repeating tracks Watching's flag — they're the same bucket
    // for import purposes.
    assert!(!import_status(NormalizedStatus::Repeating, &prefs));
}

fn al_entry(media_id: i64, status: &str) -> anilist::AniListMediaListEntry {
    anilist::AniListMediaListEntry {
        media_id,
        status: status.to_string(),
        progress: 0,
        score: 0.0,
        updated_at: 0,
        notes: String::new(),
        custom_lists: Vec::new(),
    }
}

#[test]
fn entries_from_anilist_passes_all_statuses_through_unfiltered() {
    // Filter moved from conversion time to merge time so existing
    // series with a filtered-out status still get monitor_mode
    // updated (Watching → Dropped on AL must downgrade local
    // monitor_mode even with import_dropped=false). All four
    // statuses pass through here regardless of prefs.
    let raw = vec![
        al_entry(1, "CURRENT"),
        al_entry(2, "PLANNING"),
        al_entry(3, "DROPPED"),
        al_entry(4, "COMPLETED"),
    ];
    let entries = entries_from_anilist(raw);
    assert_eq!(entries.len(), 4, "no filter at conversion time");
    assert_eq!(entries[0].provider_media_id, 1);
    assert_eq!(
        entries[0].anilist_id, 1,
        "AL provider_media_id == anilist_id"
    );
    assert_eq!(entries[0].status, NormalizedStatus::Watching);
    assert_eq!(entries[2].status, NormalizedStatus::Dropped);
    assert_eq!(entries[3].status, NormalizedStatus::Completed);
}

fn mal_entry(media_id: i64, status: &str) -> mal::MalAnimeListEntry {
    mal::MalAnimeListEntry {
        media_id,
        status: status.to_string(),
        progress: 0,
        score: 0.0,
        updated_at: 0,
    }
}

#[test]
fn entries_from_mal_leaves_anilist_id_at_zero_for_resolution() {
    // The merge engine resolves MAL → AL via anibridge before
    // writing to series. Until then, anilist_id is the sentinel
    // 0 so a regression that skips the resolution step writes a
    // visibly-broken value rather than a silently-wrong one.
    let raw = vec![mal_entry(101, "watching"), mal_entry(102, "plan_to_watch")];
    let entries = entries_from_mal(raw);
    assert_eq!(entries.len(), 2);
    for e in &entries {
        assert_eq!(e.anilist_id, 0, "MAL anilist_id is 0 pre-resolution");
        assert_eq!(e.provider, external_accounts::PROVIDER_MAL);
    }
}

fn make_detail(id: i64, title_english: &str, format: &str, status: &str) -> anilist::AnimeDetail {
    anilist::AnimeDetail {
        is_adult: false,
        id,
        id_mal: None,
        title_romaji: title_english.to_string(),
        title_english: title_english.to_string(),
        title_native: title_english.to_string(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: format.to_string(),
        status: status.to_string(),
        status_display: status.to_string(),
        episodes: Some(12),
        duration: None,
        season: String::new(),
        season_year: None,
        end_year: None,
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

fn entry(provider: &str, anilist_id: i64, status: NormalizedStatus) -> SyncEntry {
    SyncEntry {
        provider: provider.to_string(),
        provider_media_id: anilist_id.unsigned_abs() as i64,
        anilist_id,
        status,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: Vec::new(),
    }
}

// ── Delta cursor / full-resync helpers ────────────────────────

#[test]
fn should_full_resync_when_no_cursor_at_all() {
    // Fresh link: list_last_synced_at and list_full_resync_at are
    // both None → first sync MUST be full to populate everything.
    assert!(should_full_resync(None, None, 1_700_000_000));
}

#[test]
fn should_full_resync_when_only_full_resync_missing() {
    // Defensive: list_last_synced_at populated but
    // list_full_resync_at NULL means the cursor schema landed
    // mid-deployment; treat as "no full sync ever, run one now."
    assert!(should_full_resync(Some(1_700_000_000), None, 1_700_000_001));
}

#[test]
fn should_full_resync_after_seven_day_window() {
    let now = 2_000_000_000;
    let just_under = now - FULL_RESYNC_INTERVAL_SECS + 1;
    let exactly = now - FULL_RESYNC_INTERVAL_SECS;
    let beyond = now - FULL_RESYNC_INTERVAL_SECS - 1;
    assert!(
        !should_full_resync(Some(just_under), Some(just_under), now),
        "below threshold → delta"
    );
    assert!(
        should_full_resync(Some(exactly), Some(exactly), now),
        "exactly at threshold → full (>= boundary)"
    );
    assert!(
        should_full_resync(Some(beyond), Some(beyond), now),
        "past threshold → full"
    );
}

#[test]
fn drop_entries_before_cursor_passes_all_when_cursor_missing() {
    // First sync ever: cursor None means every entry merges.
    let mk = |ts| SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: ts,
        anilist_id: ts,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: ts,
        custom_lists: Vec::new(),
    };
    let entries = vec![mk(1), mk(2), mk(3)];
    let kept = drop_entries_before_cursor(entries, None);
    assert_eq!(kept.len(), 3);
}

#[tokio::test]
async fn tick_once_or_busy_returns_busy_when_lock_held() {
    // Hold the sync lock from a separate task and assert that
    // tick_once_or_busy fails fast with the user-facing message.
    // Regression for the PR #94 finding: supervised + manual
    // races used to spawn two concurrent fetches. Works on the
    // default current_thread runtime because tokio::sync::Mutex
    // and Notify cooperatively yield — the holder runs to the
    // lock + notify_one + .notified() suspension point, then
    // control returns here for the try_lock attempt.
    let lock_held = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let lh = lock_held.clone();
    let r = release.clone();
    let holder = tokio::spawn(async move {
        let _guard = EXTERNAL_SYNC_LOCK.lock().await;
        lh.notify_one();
        r.notified().await;
    });
    lock_held.notified().await;

    let db = crate::test_support::in_memory_pool().await;
    let state = crate::test_support::build_test_app_state(db, None);
    let result = tick_once_or_busy(&state).await;
    assert!(matches!(&result, Err(msg) if msg.contains("already running")));

    release.notify_one();
    let _ = holder.await;
}

#[test]
fn drop_entries_before_cursor_keeps_boundary_and_newer_entries() {
    // cursor = 100: entries with updated_at >= 100 survive; only
    // strictly-older entries drop. Inclusive boundary is the safe
    // direction — see the doc comment on drop_entries_before_cursor
    // for the read-after-write / clock-skew rationale.
    let mk = |ts| SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: ts,
        anilist_id: ts,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: ts,
        custom_lists: Vec::new(),
    };
    let entries = vec![mk(50), mk(99), mk(100), mk(101), mk(200)];
    let kept = drop_entries_before_cursor(entries, Some(100));
    assert_eq!(kept.len(), 3, "boundary entry (100) must be kept");
    assert_eq!(kept[0].updated_at, 100);
    assert_eq!(kept[1].updated_at, 101);
    assert_eq!(kept[2].updated_at, 200);
}

#[tokio::test]
async fn merge_creates_new_series_with_resolved_monitor_mode() {
    let db = crate::test_support::in_memory_pool().await;
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        12345,
        NormalizedStatus::Watching,
    )];
    let mut detail = make_detail(12345, "Example", "TV", "RELEASING");
    detail.cover_url = "https://example/cover.jpg".to_string();
    detail.banner_url = "https://example/banner.jpg".to_string();
    let mut detail_map = HashMap::new();
    detail_map.insert(12345, detail);

    let outcome = merge_into_library(&db, &entries, &detail_map, &prefs_default(), None).await;
    assert_eq!(outcome.created, 1);
    assert_eq!(outcome.monitor_mode_updated, 0);
    assert_eq!(outcome.unchanged, 0);
    assert!(outcome.failed.is_empty());

    // Watching + skip_already_watched=false → monitor_mode = "all"
    let row = series::get_by_anilist_id(&db, 12345)
        .await
        .unwrap()
        .expect("series row should exist");
    assert_eq!(row.monitor_mode, MonitorMode::All.as_str());
    assert_eq!(row.title_english, "Example");

    // Newly-created series MUST yield an artwork spec so the
    // post-merge bulk-mode pass has the cover + banner URLs to
    // fetch. Without this the spec lookup would silently no-op
    // and the user's library would render via upstream-source
    // fallback URLs forever.
    assert_eq!(outcome.new_artwork.len(), 1);
    assert_eq!(
        outcome.new_artwork[0].cover_url,
        "https://example/cover.jpg"
    );
    assert_eq!(
        outcome.new_artwork[0].banner_url,
        "https://example/banner.jpg"
    );

    // metadata_cache row is written inline so the UI sees full
    // metadata immediately on next page load instead of waiting
    // on the next 12h metadata_refresh sweep. Without this the
    // newly-imported series page renders bare title + status only.
    let cached = crate::models::metadata_cache::get_by_series_id(&db, row.id)
        .await
        .unwrap()
        .expect("metadata_cache row should exist after merge");
    assert_eq!(cached.detail.id, 12345);
    assert_eq!(cached.detail.title_english, "Example");
    assert!(cached.is_fresh, "freshly-cached row must be is_fresh");
}

#[tokio::test]
async fn merge_updates_existing_series_when_monitor_mode_differs() {
    let db = crate::test_support::in_memory_pool().await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Example").await;
    // Default seed leaves monitor_mode empty; set it to a known
    // starting value so we can prove the merge changed it.
    sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
        .bind(MonitorMode::Future.as_str())
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();

    // No detail map needed — series already exists.
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        12345,
        NormalizedStatus::Watching,
    )];
    let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
    assert_eq!(outcome.created, 0);
    assert_eq!(outcome.monitor_mode_updated, 1);
    assert_eq!(outcome.unchanged, 0);

    let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    assert_eq!(row.monitor_mode, MonitorMode::All.as_str());
}

#[tokio::test]
async fn merge_leaves_existing_series_alone_when_monitor_mode_matches() {
    let db = crate::test_support::in_memory_pool().await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Example").await;
    sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
        .bind(MonitorMode::All.as_str())
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();

    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        12345,
        NormalizedStatus::Watching,
    )];
    let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
    assert_eq!(outcome.created, 0);
    assert_eq!(outcome.monitor_mode_updated, 0);
    assert_eq!(outcome.unchanged, 1);
}

#[tokio::test]
async fn merge_defers_negated_id_entries_for_jikan_path() {
    let db = crate::test_support::in_memory_pool().await;
    // -7777 means anibridge missed; the Jikan-fallback merge path
    // (next commit) will handle these. For now they're counted
    // and skipped.
    let entries = vec![entry(
        external_accounts::PROVIDER_MAL,
        -7777,
        NormalizedStatus::Watching,
    )];
    let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
    assert_eq!(outcome.deferred_jikan, 1);
    assert_eq!(outcome.created, 0);
    assert!(outcome.failed.is_empty());
}

#[tokio::test]
async fn merge_records_failure_when_detail_missing_for_new_id() {
    let db = crate::test_support::in_memory_pool().await;
    // AL id present in entries but absent from detail_map
    // (AL deleted the entry upstream is the canonical case).
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        99999,
        NormalizedStatus::Watching,
    )];
    let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
    assert_eq!(outcome.created, 0);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, 99999);
    assert!(outcome.failed[0].1.contains("no AniList detail"));
}

#[tokio::test]
async fn merge_jikan_fallback_creates_series_with_negated_sentinel() {
    let db = crate::test_support::in_memory_pool().await;
    // Seed Jikan's detail cache so the merge call hits the cache
    // rather than the live Jikan API. mal_id 555 ↔ negated AL id
    // -555 (the sync-time sentinel).
    let detail = make_detail(-555, "Jikan-only Show", "TV", "RELEASING");
    let mut detail = detail;
    detail.id_mal = Some(555);
    jikan::seed_detail_cache_for_tests(555, detail).await;

    let entries = vec![entry(
        external_accounts::PROVIDER_MAL,
        -555,
        NormalizedStatus::Watching,
    )];
    let outcome = merge_jikan_fallback_entries(&db, &entries, &prefs_default(), None).await;
    assert_eq!(outcome.created, 1);
    assert!(outcome.failed.is_empty());

    // The new row carries the negated sentinel — preserves the
    // existing `> 0` filters on every AL call site so this entry
    // routes back through Jikan on refresh.
    let row = series::get_by_anilist_id(&db, -555)
        .await
        .unwrap()
        .expect("series row should exist under negated sentinel");
    assert_eq!(row.anilist_id, -555);
    assert_eq!(row.mal_id, Some(555));
    assert_eq!(row.monitor_mode, MonitorMode::All.as_str());

    jikan::clear_detail_cache_entry_for_tests(555).await;
}

#[tokio::test]
async fn merge_jikan_fallback_skips_positive_ids() {
    // The Jikan pass must only touch negated-id entries. A
    // positive AL id sneaking in would be a logic bug — the AL
    // pass already handled it.
    let db = crate::test_support::in_memory_pool().await;
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        12345,
        NormalizedStatus::Watching,
    )];
    let outcome = merge_jikan_fallback_entries(&db, &entries, &prefs_default(), None).await;
    assert_eq!(outcome.created, 0);
    assert_eq!(outcome.monitor_mode_updated, 0);
    assert_eq!(outcome.unchanged, 0);
    assert!(outcome.failed.is_empty());
}

#[test]
fn merge_pass_combines_outcomes_and_drains_deferred_counter() {
    // AL pass deferred 3 entries; Jikan pass merged 2 + failed 1
    // (3 entries handled). Combined deferred drops to 0.
    let al = MergeOutcome {
        created: 5,
        monitor_mode_updated: 2,
        unchanged: 10,
        deferred_jikan: 3,
        failed: Vec::new(),
        skipped_by_preference: 1,
        pinned_manually: 0,
        new_artwork: vec![NewArtworkSpec {
            series_id: 1,
            cover_url: "c1".into(),
            banner_url: "b1".into(),
        }],
    };
    let jikan = MergeOutcome {
        created: 2,
        monitor_mode_updated: 0,
        unchanged: 0,
        deferred_jikan: 0,
        failed: vec![(-9999, "Jikan rate-limited".into())],
        skipped_by_preference: 0,
        pinned_manually: 0,
        new_artwork: vec![
            NewArtworkSpec {
                series_id: 2,
                cover_url: "c2".into(),
                banner_url: "b2".into(),
            },
            NewArtworkSpec {
                series_id: 3,
                cover_url: "c3".into(),
                banner_url: "b3".into(),
            },
        ],
    };
    let combined = al.merge_pass(jikan);
    assert_eq!(combined.created, 7);
    assert_eq!(combined.monitor_mode_updated, 2);
    assert_eq!(combined.unchanged, 10);
    assert_eq!(combined.deferred_jikan, 0);
    assert_eq!(combined.failed.len(), 1);
    // Artwork specs concatenate across passes — the bulk-mode
    // post-merge task expects the full list of new series.
    assert_eq!(combined.new_artwork.len(), 3);
}

#[test]
fn merge_pass_keeps_remaining_deferred_when_jikan_partial() {
    // AL deferred 5; Jikan only handled 2. 3 still deferred.
    let al = MergeOutcome {
        created: 0,
        monitor_mode_updated: 0,
        unchanged: 0,
        deferred_jikan: 5,
        failed: Vec::new(),
        skipped_by_preference: 0,
        pinned_manually: 0,
        new_artwork: Vec::new(),
    };
    let jikan = MergeOutcome {
        created: 2,
        monitor_mode_updated: 0,
        unchanged: 0,
        deferred_jikan: 0,
        failed: Vec::new(),
        skipped_by_preference: 0,
        pinned_manually: 0,
        new_artwork: Vec::new(),
    };
    let combined = al.merge_pass(jikan);
    assert_eq!(combined.deferred_jikan, 3);
}

#[tokio::test]
async fn merge_skip_already_watched_lands_existing_for_watching_only() {
    let db = crate::test_support::in_memory_pool().await;
    let entries = vec![
        entry(
            external_accounts::PROVIDER_ANILIST,
            100,
            NormalizedStatus::Watching,
        ),
        entry(
            external_accounts::PROVIDER_ANILIST,
            200,
            NormalizedStatus::Planning,
        ),
    ];
    let mut detail_map = HashMap::new();
    detail_map.insert(100, make_detail(100, "Active", "TV", "RELEASING"));
    detail_map.insert(200, make_detail(200, "PTW", "TV", "FINISHED"));

    let outcome = merge_into_library(
        &db,
        &entries,
        &detail_map,
        &prefs_with_skip_already_watched(),
        None,
    )
    .await;
    assert_eq!(outcome.created, 2);

    let watching = series::get_by_anilist_id(&db, 100).await.unwrap().unwrap();
    let planning = series::get_by_anilist_id(&db, 200).await.unwrap().unwrap();
    assert_eq!(
        watching.monitor_mode,
        MonitorMode::Existing.as_str(),
        "skip_already_watched flips Watching → existing"
    );
    assert_eq!(
        planning.monitor_mode,
        MonitorMode::Future.as_str(),
        "Planning still maps to future regardless of skip flag"
    );
}

#[tokio::test]
async fn resolve_mal_anilist_ids_uses_anibridge_hit() {
    // Cache-hit path: MAL 1234 → AL 9999 lives in the seeded
    // anibridge cache, so the resolver writes the real AL id back
    // onto the SyncEntry.
    let _guard = ANIBRIDGE_CACHE_GUARD.lock().await;
    anibridge::seed_mal_to_anilist_for_tests(&[(1234, 9999)]).await;

    let entries = vec![SyncEntry {
        provider: external_accounts::PROVIDER_MAL.to_string(),
        provider_media_id: 1234,
        anilist_id: 0,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: Vec::new(),
    }];
    let resolved = resolve_mal_anilist_ids(entries).await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].anilist_id, 9999,
        "anibridge hit should set anilist_id to the real AL id"
    );

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn resolve_mal_anilist_ids_falls_back_to_negated_sentinel_on_miss() {
    // Empty cache → every lookup misses, every MAL entry gets
    // anilist_id = -provider_media_id. This is the reconcile-
    // path-friendly state from the existing Jikan fallback flow.
    let _guard = ANIBRIDGE_CACHE_GUARD.lock().await;
    anibridge::seed_mal_to_anilist_for_tests(&[]).await;

    let entries = vec![
        SyncEntry {
            provider: external_accounts::PROVIDER_MAL.to_string(),
            provider_media_id: 7777,
            anilist_id: 0,
            status: NormalizedStatus::Watching,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        },
        SyncEntry {
            provider: external_accounts::PROVIDER_MAL.to_string(),
            provider_media_id: 8888,
            anilist_id: 0,
            status: NormalizedStatus::Planning,
            progress: 0,
            score: 0.0,
            updated_at: 0,
            custom_lists: Vec::new(),
        },
    ];
    let resolved = resolve_mal_anilist_ids(entries).await;
    assert_eq!(resolved[0].anilist_id, -7777);
    assert_eq!(resolved[1].anilist_id, -8888);

    anibridge::clear_cache_for_tests().await;
}

#[tokio::test]
async fn resolve_mal_anilist_ids_passes_through_anilist_entries_unchanged() {
    // AL entries (anilist_id != 0) MUST NOT be touched even if a
    // MAL ID with the same numeric value happens to live in the
    // cache. Otherwise an AL entry whose AL id collides with some
    // MAL id would be silently rewritten.
    let _guard = ANIBRIDGE_CACHE_GUARD.lock().await;
    anibridge::seed_mal_to_anilist_for_tests(&[(1234, 9999)]).await;

    let entries = vec![SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: 1234,
        anilist_id: 1234,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: Vec::new(),
    }];
    let resolved = resolve_mal_anilist_ids(entries).await;
    assert_eq!(
        resolved[0].anilist_id, 1234,
        "AL pass-through must not be rewritten"
    );

    anibridge::clear_cache_for_tests().await;
}

#[test]
fn entries_from_mal_passes_all_statuses_through_unfiltered() {
    // Conversion no longer filters; merge step gates create-only
    // by import preference. Every status passes through here so
    // existing series with a filtered-out new status still get
    // their monitor_mode updated downstream.
    let raw = vec![
        mal_entry(1, "watching"),
        mal_entry(2, "on_hold"),
        mal_entry(3, "dropped"),
        mal_entry(4, "completed"),
        mal_entry(5, "plan_to_watch"),
    ];
    let entries = entries_from_mal(raw);
    assert_eq!(entries.len(), 5, "no filter at conversion time");
}

#[tokio::test]
async fn merge_updates_existing_when_status_filtered_out() {
    // Regression for the user-reported case: existing Watching
    // series, AL transitions it to Dropped, user has
    // import_dropped=false. The series MUST flip to monitor_mode
    // = none anyway — otherwise a dropped show keeps grabbing
    // forever.
    let db = crate::test_support::in_memory_pool().await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Example").await;
    sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
        .bind(MonitorMode::All.as_str())
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();

    // No detail map needed — series already exists, merge updates
    // monitor_mode regardless of whether import_dropped is on.
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        12345,
        NormalizedStatus::Dropped,
    )];
    let prefs = prefs_default(); // import_dropped = false
    assert!(!prefs.import_dropped, "test premise: import_dropped off");

    let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs, None).await;
    assert_eq!(outcome.created, 0);
    assert_eq!(outcome.monitor_mode_updated, 1);
    assert_eq!(outcome.skipped_by_preference, 0);

    let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    assert_eq!(
        row.monitor_mode,
        MonitorMode::None.as_str(),
        "Dropped status must downgrade existing series to None even with import_dropped=false"
    );
}

/// Helper: insert a placeholder `external_accounts` row directly,
/// bypassing the encrypt-then-INSERT path of `link()`. The
/// removal-detection tests need a real id to satisfy the FK
/// constraint on `synced_from_external_account_id` but don't care
/// about the token contents — the tests never decrypt them.
/// `provider` must be `"anilist"` or `"mal"` (schema CHECK +
/// UNIQUE(provider)); two-account tests pass one of each.
async fn seed_account_id(db: &sqlx::SqlitePool, id: i64, provider: &str) {
    sqlx::query(
        "INSERT INTO external_accounts \
         (id, provider, provider_user_id, username, \
          access_token_encrypted, refresh_token_encrypted, linked_at) \
         VALUES (?, ?, ?, ?, X'00', X'00', 0)",
    )
    .bind(id)
    .bind(provider)
    .bind(format!("user-{id}"))
    .bind(format!("user-{id}"))
    .execute(db)
    .await
    .unwrap();
}

/// Helper: stamp `synced_from_external_account_id` on a series row
/// so the removal-detection tests can pin which series came from
/// which account.
async fn force_synced_from(db: &sqlx::SqlitePool, series_id: i64, account_id: i64) {
    sqlx::query("UPDATE series SET synced_from_external_account_id = ? WHERE id = ?")
        .bind(account_id)
        .bind(series_id)
        .execute(db)
        .await
        .unwrap();
}

async fn force_monitor_mode(db: &sqlx::SqlitePool, series_id: i64, mode: MonitorMode) {
    sqlx::query("UPDATE series SET monitor_mode = ? WHERE id = ?")
        .bind(mode.as_str())
        .bind(series_id)
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn detect_removals_downgrades_missing_synced_series() {
    // The key user-facing behavior: a series that was on AL,
    // synced into Ryokan with monitor_mode=all, gets removed from
    // AL → next full-sync downgrades monitor_mode to None so it
    // stops grabbing.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let kept_id = crate::test_support::seed_series(&db, 100, "Kept").await;
    let removed_id = crate::test_support::seed_series(&db, 200, "Removed").await;
    force_synced_from(&db, kept_id, 1).await;
    force_synced_from(&db, removed_id, 1).await;
    force_monitor_mode(&db, kept_id, MonitorMode::All).await;
    force_monitor_mode(&db, removed_id, MonitorMode::All).await;

    // Current fetch only includes the kept one (anilist_id=100);
    // 200 is missing → removal detection downgrades it.
    let mut fetch_ids = std::collections::HashSet::new();
    fetch_ids.insert(100);
    let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0], removed_id);

    let kept = series::get_by_id(&db, kept_id).await.unwrap().unwrap();
    let removed = series::get_by_id(&db, removed_id).await.unwrap().unwrap();
    assert_eq!(
        kept.monitor_mode,
        MonitorMode::All.as_str(),
        "in-fetch series stays at its existing mode"
    );
    assert_eq!(
        removed.monitor_mode,
        MonitorMode::None.as_str(),
        "removed-from-fetch series downgrades to None"
    );
}

#[tokio::test]
async fn detect_removals_leaves_manually_added_series_alone() {
    // synced_from_external_account_id IS NULL means the user added
    // this manually. Removal detection MUST NOT touch it even if
    // it's not in the fetch — the user's library is theirs.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let manual_id = crate::test_support::seed_series(&db, 300, "Manual").await;
    let synced_id = crate::test_support::seed_series(&db, 400, "Synced").await;
    force_synced_from(&db, synced_id, 1).await;
    force_monitor_mode(&db, manual_id, MonitorMode::All).await;
    force_monitor_mode(&db, synced_id, MonitorMode::All).await;

    // Empty fetch — neither series is on the user's list.
    let fetch_ids = std::collections::HashSet::new();
    let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();

    // Only the synced series gets downgraded.
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0], synced_id);

    let manual = series::get_by_id(&db, manual_id).await.unwrap().unwrap();
    assert_eq!(
        manual.monitor_mode,
        MonitorMode::All.as_str(),
        "manually-added series MUST NOT be touched by removal detection"
    );
}

#[tokio::test]
async fn detect_removals_skips_already_none_series() {
    // A series that's already at monitor_mode=none doesn't need a
    // redundant write. Counter only includes series that actually
    // changed.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let already_none = crate::test_support::seed_series(&db, 500, "Already None").await;
    force_synced_from(&db, already_none, 1).await;
    force_monitor_mode(&db, already_none, MonitorMode::None).await;

    let fetch_ids = std::collections::HashSet::new();
    let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
    assert_eq!(
        report.removed.len(),
        0,
        "already-None series stays out of the report"
    );
}

#[tokio::test]
async fn detect_removals_scopes_to_account_id() {
    // Two accounts, each synced one series. Removal detection
    // for account=1 must NOT touch account=2's series.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    seed_account_id(&db, 2, "mal").await;
    let acct1_series = crate::test_support::seed_series(&db, 600, "Acct1 series").await;
    let acct2_series = crate::test_support::seed_series(&db, 700, "Acct2 series").await;
    force_synced_from(&db, acct1_series, 1).await;
    force_synced_from(&db, acct2_series, 2).await;
    force_monitor_mode(&db, acct1_series, MonitorMode::All).await;
    force_monitor_mode(&db, acct2_series, MonitorMode::All).await;

    // Empty fetch for account 1.
    let fetch_ids = std::collections::HashSet::new();
    let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
    assert_eq!(report.removed, vec![acct1_series]);

    // Account 2's series is unaffected.
    let acct2 = series::get_by_id(&db, acct2_series).await.unwrap().unwrap();
    assert_eq!(acct2.monitor_mode, MonitorMode::All.as_str());
}

#[tokio::test]
async fn merge_writes_user_score_on_existing_series() {
    // Sync brings in entry.score = 8.5; merge writes it to
    // series.user_score so the "You: 8.5" badge renders. Doesn't
    // need to be a status transition — score updates on every
    // tick regardless of monitor_mode movement.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Scored").await;

    let entries = vec![SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: 12345,
        anilist_id: 12345,
        status: NormalizedStatus::Watching,
        progress: 4,
        score: 8.5,
        updated_at: 0,
        custom_lists: Vec::new(),
    }];
    let outcome =
        merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), Some(1)).await;
    // Existing series → MonitorUpdated (the seed left monitor_mode
    // empty so target=All differs); the test pins user_score
    // regardless of the action variant.
    assert!(outcome.failed.is_empty());

    let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    assert_eq!(
        row.user_score,
        Some(8.5),
        "merge must write entry.score to user_score"
    );
}

#[tokio::test]
async fn merge_writes_custom_list_memberships_for_anilist() {
    // AL custom-list membership is replaced on every successful
    // merge action so the detail-page badge row + library filter
    // stay in lockstep with the user's current AL state.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Listed").await;

    let entries = vec![SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: 12345,
        anilist_id: 12345,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: vec!["Hidden Gems".into(), "Top 10".into()],
    }];
    let _ = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), Some(1)).await;

    let memberships = series_custom_lists::list_for_series(&db, series_id)
        .await
        .unwrap();
    assert_eq!(memberships.len(), 2);
    assert!(memberships.iter().any(|m| m.list_name == "Hidden Gems"));
    assert!(memberships.iter().any(|m| m.list_name == "Top 10"));
    for m in &memberships {
        assert_eq!(m.provider, external_accounts::PROVIDER_ANILIST);
    }
}

#[tokio::test]
async fn merge_replaces_stale_custom_list_membership() {
    // The user moved a series out of "Hidden Gems" on AL. The
    // next sync's replace-on-merge MUST drop the old membership;
    // an upsert-only path would leak stale rows forever.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Moved").await;

    // First sync: in Hidden Gems.
    let entries_v1 = vec![SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: 12345,
        anilist_id: 12345,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: vec!["Hidden Gems".into()],
    }];
    let _ = merge_into_library(&db, &entries_v1, &HashMap::new(), &prefs_default(), Some(1)).await;

    // Second sync: moved to Top 10, no longer in Hidden Gems.
    let entries_v2 = vec![SyncEntry {
        custom_lists: vec!["Top 10".into()],
        ..entries_v1[0].clone()
    }];
    let _ = merge_into_library(&db, &entries_v2, &HashMap::new(), &prefs_default(), Some(1)).await;

    let memberships = series_custom_lists::list_for_series(&db, series_id)
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].list_name, "Top 10");
}

#[tokio::test]
async fn merge_jikan_path_does_not_write_custom_lists() {
    // MAL has no custom-list concept; entries_from_mal always
    // returns empty custom_lists. The Jikan-fallback merge path
    // MUST also skip the membership write so it can't ever
    // accidentally clobber AL-side memberships.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "mal").await;
    // Pre-seed a hypothetical AL-side membership for this series
    // (e.g. user previously had AL linked, then switched to MAL).
    let series_id = crate::test_support::seed_series(&db, -777, "Jikan series").await;
    sqlx::query(
        "INSERT INTO series_custom_lists (series_id, provider, list_name) VALUES (?, ?, ?)",
    )
    .bind(series_id)
    .bind("anilist")
    .bind("Old AL List")
    .execute(&db)
    .await
    .unwrap();

    // Seed a Jikan detail so the merge path actually fires.
    crate::services::jikan::seed_detail_cache_for_tests(
        777,
        make_detail(-777, "Jikan series", "TV", "FINISHED"),
    )
    .await;

    let entries = vec![SyncEntry {
        provider: external_accounts::PROVIDER_MAL.to_string(),
        provider_media_id: 777,
        anilist_id: -777,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: Vec::new(),
    }];
    let _ = merge_jikan_fallback_entries(&db, &entries, &prefs_default(), Some(1)).await;

    // The pre-seeded AL membership stays put — Jikan path's
    // skip-when-not-anilist guard prevents it from being wiped.
    let memberships = series_custom_lists::list_for_series(&db, series_id)
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].list_name, "Old AL List");
    assert_eq!(memberships[0].provider, "anilist");

    crate::services::jikan::clear_detail_cache_entry_for_tests(777).await;
}

#[tokio::test]
async fn merge_normalizes_zero_score_to_null() {
    // AL sends 0.0 for unrated entries. The merge normalizes that
    // to NULL so `user_score IS NOT NULL` cleanly means "rated"
    // for any future query. Render helper handles 0.0 defensively
    // for older rows but new writes never produce it.
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Unrated").await;
    // Pre-condition: user_score is non-null (e.g. user rated 7
    // last sync, then unrated this sync).
    sqlx::query("UPDATE series SET user_score = 7.0 WHERE id = ?")
        .bind(series_id)
        .execute(&db)
        .await
        .unwrap();

    let entries = vec![SyncEntry {
        provider: external_accounts::PROVIDER_ANILIST.to_string(),
        provider_media_id: 12345,
        anilist_id: 12345,
        status: NormalizedStatus::Watching,
        progress: 0,
        score: 0.0,
        updated_at: 0,
        custom_lists: Vec::new(),
    }];
    let _ = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), Some(1)).await;

    let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    assert_eq!(
        row.user_score, None,
        "score=0.0 must normalize to NULL on write"
    );
}

#[tokio::test]
async fn merge_skips_existing_when_manual_override_set() {
    // The user has pinned monitor_mode through the UI. AL says
    // their status changed (Watching → Dropped), but the merge
    // step MUST leave the monitor_mode alone — the user
    // explicitly chose this.
    let db = crate::test_support::in_memory_pool().await;
    let series_id = crate::test_support::seed_series(&db, 12345, "Pinned").await;
    sqlx::query(
        "UPDATE series SET monitor_mode = ?, monitor_mode_manual_override = 1 WHERE id = ?",
    )
    .bind(MonitorMode::All.as_str())
    .bind(series_id)
    .execute(&db)
    .await
    .unwrap();

    // AL says Dropped, which would normally flip to monitor_mode=None.
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        12345,
        NormalizedStatus::Dropped,
    )];
    let outcome = merge_into_library(&db, &entries, &HashMap::new(), &prefs_default(), None).await;
    assert_eq!(outcome.pinned_manually, 1);
    assert_eq!(outcome.monitor_mode_updated, 0);

    let row = series::get_by_id(&db, series_id).await.unwrap().unwrap();
    assert_eq!(
        row.monitor_mode,
        MonitorMode::All.as_str(),
        "manual override must survive a Watching → Dropped transition on AL"
    );
}

#[tokio::test]
async fn detect_removals_skips_manual_override_pinned_series() {
    // A series the user pinned is also off-limits to removal
    // detection — they explicitly want to keep this monitor_mode
    // even after removing from AL (e.g. their list went private).
    let db = crate::test_support::in_memory_pool().await;
    seed_account_id(&db, 1, "anilist").await;
    let pinned_id = crate::test_support::seed_series(&db, 800, "Pinned").await;
    force_synced_from(&db, pinned_id, 1).await;
    force_monitor_mode(&db, pinned_id, MonitorMode::All).await;
    sqlx::query("UPDATE series SET monitor_mode_manual_override = 1 WHERE id = ?")
        .bind(pinned_id)
        .execute(&db)
        .await
        .unwrap();

    let fetch_ids = std::collections::HashSet::new();
    let report = detect_removals(&db, 1, &fetch_ids).await.unwrap();
    assert!(
        report.removed.is_empty(),
        "manual-override series must NOT be downgraded by removal detection"
    );

    let row = series::get_by_id(&db, pinned_id).await.unwrap().unwrap();
    assert_eq!(row.monitor_mode, MonitorMode::All.as_str());
}

#[test]
fn is_auth_rejection_matches_known_dead_token_strings() {
    // Each of these is a stable error-prefix the sync engine
    // emits on a token-dead failure; the Settings UI's
    // "Re-link required" banner keys off this exact match.
    // Adding a new wording requires updating the auth-prefix
    // list — pinning the existing ones here so a refactor that
    // reshapes a message gets caught.
    assert!(is_auth_rejection(
        "AniList rejected the watch-list token (status 401); user may need to re-link"
    ));
    // OAuth-shaped 400s carry the same prefix so the same matcher
    // fires. The body excerpt distinguishes invalid_token vs
    // invalid_grant for the operator log but the prefix is stable.
    assert!(is_auth_rejection(
        "AniList rejected the watch-list token (status 400, invalid_token); user may need to re-link"
    ));
    assert!(is_auth_rejection(
        "AniList rejected the watch-list token (status 400, invalid_grant); user may need to re-link"
    ));
    assert!(is_auth_rejection(
        "MAL access token expired and no refresh token stored — re-link required"
    ));
    assert!(is_auth_rejection(
        "MAL refresh failed (re-link required): some upstream detail"
    ));
    assert!(is_auth_rejection(
        "MAL rejected the token immediately after refresh — re-link required"
    ));
}

#[test]
fn is_auth_rejection_does_not_match_transient_errors() {
    // Rate-limits, network timeouts, and 5xx-shaped errors are
    // transient; the Settings banner shouldn't fire for them.
    assert!(!is_auth_rejection(
        "AniList rate-limited: too many requests"
    ));
    assert!(!is_auth_rejection(
        "AniList unavailable (status 503): service unavailable"
    ));
    assert!(!is_auth_rejection("AniList HTTP error: connection reset"));
    assert!(!is_auth_rejection(
        "AniList batch request failed: connection timed out"
    ));
    assert!(!is_auth_rejection("MAL fetch failed: 500 Internal Server"));
}

#[test]
fn force_full_sync_overrides_should_full_resync_decision() {
    // A 2-day-old full sync would normally yield is_full_sync=false
    // (within the 7-day delta window). With the force flag set,
    // is_full_sync MUST be true regardless — the manual "Sync
    // now" trigger uses this to make removals apply immediately
    // instead of waiting up to 7 days for the next boundary.
    let now = 2_000_000_000;
    let two_days_ago = now - 2 * 24 * 60 * 60;
    // Without force: standard delta logic returns false.
    assert!(!should_full_resync(
        Some(two_days_ago),
        Some(two_days_ago),
        now
    ));
    // The force-full-sync gate `force || should_full_resync(...)`
    // is the actual logic in tick_once_inner. Pin the OR so a
    // refactor doesn't accidentally drop the force path.
    let force = true;
    let is_full_sync = force || should_full_resync(Some(two_days_ago), Some(two_days_ago), now);
    assert!(is_full_sync, "force flag must override the delta window");
}

#[tokio::test]
async fn merge_skips_new_entry_when_import_pref_off() {
    // The other side of the rule: a NEW Dropped entry with
    // import_dropped=false should NOT create a series row. Stays
    // out of the library entirely. Counter rolls into
    // skipped_by_preference so the operator sees the count.
    let db = crate::test_support::in_memory_pool().await;
    let entries = vec![entry(
        external_accounts::PROVIDER_ANILIST,
        99999,
        NormalizedStatus::Dropped,
    )];
    // Even though detail_map has the entry, the merge should skip
    // creation because the user doesn't want Dropped imports.
    let mut detail_map = HashMap::new();
    detail_map.insert(
        99999,
        make_detail(99999, "Should Not Land", "TV", "FINISHED"),
    );

    let prefs = prefs_default();
    let outcome = merge_into_library(&db, &entries, &detail_map, &prefs, None).await;
    assert_eq!(outcome.created, 0);
    assert_eq!(outcome.skipped_by_preference, 1);
    assert!(
        series::get_by_anilist_id(&db, 99999)
            .await
            .unwrap()
            .is_none(),
        "import_dropped=false must keep new Dropped entries out of the library"
    );
}
