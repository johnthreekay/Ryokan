use super::*;
use crate::models::series;

/// Round-trip a GrabSeriesRoute through record_grab_series_routes
/// + get_series_routes to verify the new episode_offset column is
/// written and read correctly. Covers Commit 3's schema plumbing
/// (ALTER TABLE ADD COLUMN + INSERT bind + SELECT with COALESCE).
#[tokio::test]
async fn grab_series_route_round_trip_preserves_episode_offset() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (parent_id, _) = series::upsert(
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
            episodes: Some(13),
            season_year: Some(2015),
            end_year: Some(2015),
        },
    )
    .await
    .expect("parent upsert");

    let (sibling_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 21860,
            mal_id: None,
            title: "Owarimonogatari Second Season",
            title_romaji: "Owarimonogatari Second Season",
            title_english: "Owarimonogatari Second Season",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(7),
            season_year: Some(2017),
            end_year: Some(2017),
        },
    )
    .await
    .expect("sibling upsert");

    let grab_id = record_grab(
        &db,
        "roundtriphash0000000000000000000000000000",
        "[smol] Monogatari S07 (Owarimonogatari) [BD 1080p]",
        parent_id,
        &[],
        true,
    )
    .await
    .expect("record_grab")
    .expect("grab inserted");

    let routes = vec![
        // Parent route: no offset.
        GrabSeriesRoute {
            grab_id,
            series_id: parent_id,
            file_indices: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            episode_numbers: (1..=13).collect(),
            matched_subtitle: String::new(),
            episode_offset: 0,
        },
        // Sibling route: absolute-numbered, offset = parent cap.
        GrabSeriesRoute {
            grab_id,
            series_id: sibling_id,
            file_indices: vec![13, 14, 15, 16, 17, 18, 19],
            episode_numbers: (14..=20).collect(),
            matched_subtitle: "episode-range fallback (14..=20)".to_string(),
            episode_offset: 13,
        },
    ];

    record_grab_series_routes(&db, &routes)
        .await
        .expect("record routes");

    let read_back = get_series_routes(&db, grab_id)
        .await
        .expect("get_series_routes");
    assert_eq!(read_back.len(), 2);

    let parent_route = read_back
        .iter()
        .find(|r| r.series_id == parent_id)
        .expect("parent route");
    assert_eq!(parent_route.episode_offset, 0);

    let sibling_route = read_back
        .iter()
        .find(|r| r.series_id == sibling_id)
        .expect("sibling route");
    assert_eq!(sibling_route.episode_offset, 13);
    assert_eq!(
        sibling_route.matched_subtitle,
        "episode-range fallback (14..=20)"
    );
    assert_eq!(sibling_route.file_indices.len(), 7);
}

/// Regression: find_imported_for_episode previously hard-coded
/// `is_batch: false`, so callers (handlers/library.rs and
/// post_processing) treated batch torrents as single-episode grabs and
/// `delete_torrent(..., delete_files=true)` would wipe the entire pack
/// off disk during an upgrade-replace.
#[tokio::test]
async fn find_imported_for_episode_preserves_is_batch() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 21202,
            mal_id: None,
            title: "Show",
            title_romaji: "Show",
            title_english: "Show",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(24),
            season_year: Some(2015),
            end_year: Some(2015),
        },
    )
    .await
    .expect("series upsert");

    let batch_eps: Vec<i32> = (1..=24).collect();
    let batch_grab_id = record_grab(
        &db,
        "batchhash00000000000000000000000000000000",
        "[Group] Show 01-24 [BD 1080p]",
        series_id,
        &batch_eps,
        true,
    )
    .await
    .expect("record batch grab")
    .expect("batch grab inserted");
    mark_imported(&db, batch_grab_id)
        .await
        .expect("mark batch imported");

    let single_grab_id = record_grab(
        &db,
        "singlehash0000000000000000000000000000000",
        "[Group] Show - 07 [WEB-DL 1080p]",
        series_id,
        &[7],
        false,
    )
    .await
    .expect("record single grab")
    .expect("single grab inserted");
    mark_imported(&db, single_grab_id)
        .await
        .expect("mark single imported");

    // Episode 5 is only covered by the batch grab — its is_batch must
    // round-trip as true.
    let ep5 = find_imported_for_episode(&db, series_id, 5)
        .await
        .expect("find ep5");
    assert_eq!(ep5.len(), 1, "expected one grab covering episode 5");
    assert!(
        ep5[0].is_batch,
        "batch grab for episode 5 must report is_batch=true"
    );

    // Episode 7 is covered by both grabs. The single-episode grab was
    // recorded second so it sorts first (ORDER BY grabbed_at DESC), but
    // both rows must report their true is_batch value.
    let ep7 = find_imported_for_episode(&db, series_id, 7)
        .await
        .expect("find ep7");
    assert_eq!(ep7.len(), 2, "expected both grabs covering episode 7");
    let single = ep7
        .iter()
        .find(|g| g.id == single_grab_id)
        .expect("single grab present");
    assert!(!single.is_batch, "single-episode grab is_batch=false");
    let batch = ep7
        .iter()
        .find(|g| g.id == batch_grab_id)
        .expect("batch grab present");
    assert!(batch.is_batch, "batch grab is_batch=true");
}

/// record_grab's atomic dedup uses a partial UNIQUE index on
/// `(hash) WHERE hash != '' AND state IN ('pending', 'imported')`.
/// A dedup hit is no longer a silent no-op — the existing row is
/// reactivated so post-processing picks it up again (see the
/// drift-cause story on the `record_grab` fn). This test pins both
/// the dedup-and-reactivate behavior and the empty-hash bypass.
#[tokio::test]
async fn record_grab_dedups_and_reactivates_same_hash() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 12345,
            mal_id: None,
            title: "Show",
            title_romaji: "Show",
            title_english: "Show",
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

    // First active grab inserts.
    let id1 = record_grab(&db, "racehash", "release a", series_id, &[1], false)
        .await
        .expect("first record_grab")
        .expect("first must insert");

    // Second grab with same hash against a PENDING row dedups
    // silently — reactivation only runs on 'imported' rows to
    // avoid null-clobbering an in-flight import's
    // client_content_path / imported_at. Returns Ok(None) and the
    // existing row's fields are left alone.
    let id2 = record_grab(&db, "racehash", "release b", series_id, &[2], false)
        .await
        .expect("second record_grab");
    assert!(
        id2.is_none(),
        "pending-row dedup must not reactivate: {:?}",
        id2
    );

    // Confirm only one row exists and the pending fields are
    // intact (no silent rewrite).
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = 'racehash'")
            .fetch_one(&db)
            .await
            .expect("count");
    assert_eq!(count, 1);

    let row: (String, String, String) = sqlx::query_as(
        "SELECT torrent_name, episode_numbers, state FROM grabbed_torrents WHERE id = ?",
    )
    .bind(id1)
    .fetch_one(&db)
    .await
    .expect("fetch row");
    assert_eq!(row.0, "release a", "pending row's fields must be untouched");
    assert_eq!(
        row.1, "[1]",
        "pending row's episode_numbers must be untouched"
    );
    assert_eq!(row.2, "pending", "pending row stays pending");

    // The original drift case: mark the row 'imported' (as
    // post-processing would have), then re-grab the same hash.
    // Reactivation must flip it back to 'pending' and null out
    // imported_at so the next post-processing tick picks it up.
    mark_imported(&db, id1).await.expect("mark imported");
    let imported_at_before: Option<String> =
        sqlx::query_scalar("SELECT imported_at FROM grabbed_torrents WHERE id = ?")
            .bind(id1)
            .fetch_one(&db)
            .await
            .expect("imported_at before");
    assert!(
        imported_at_before.is_some(),
        "mark_imported stamps imported_at"
    );

    let id3 = record_grab(&db, "racehash", "release c", series_id, &[1], false)
        .await
        .expect("third record_grab")
        .expect("re-grab of imported hash must yield an id");
    assert_eq!(id3, id1, "reactivation preserves the row id");

    let (state_after, imported_at_after): (String, Option<String>) =
        sqlx::query_as("SELECT state, imported_at FROM grabbed_torrents WHERE id = ?")
            .bind(id1)
            .fetch_one(&db)
            .await
            .expect("state after");
    assert_eq!(state_after, "pending", "imported→pending flip on re-grab");
    assert!(
        imported_at_after.is_none(),
        "imported_at must be cleared on reactivation"
    );

    // Failed grabs with the same hash are NOT covered by the
    // partial index, so a re-grab goes through the fresh-insert
    // path and writes a new row. This preserves blocklist
    // semantics (user marked the grab failed on purpose — the
    // re-grab is a genuinely new attempt, not a reactivation).
    mark_failed(&db, id1).await.expect("mark failed");
    let id4 = record_grab(&db, "racehash", "release d", series_id, &[1], false)
        .await
        .expect("fourth record_grab")
        .expect("re-record after failed must insert");
    assert_ne!(id4, id1, "post-failed re-grab inserts a new row");

    // Empty-hash rows aren't covered by the partial index and are
    // never deduped.
    let id5 = record_grab(&db, "", "no hash a", series_id, &[1], false)
        .await
        .expect("empty-hash a");
    let id6 = record_grab(&db, "", "no hash b", series_id, &[1], false)
        .await
        .expect("empty-hash b");
    assert!(id5.is_some() && id6.is_some(), "empty-hash never dedups");
    assert_ne!(id5, id6, "empty-hash inserts are distinct rows");
}

/// `find_pending_for_episode` backs the cancel-pending handler.
/// It must only return 'pending' rows (not 'imported' / 'failed' /
/// 'removed'), and must find both direct single-series grabs and
/// grabs that reach the series via a route row.
#[tokio::test]
async fn find_pending_filters_by_state_and_series() {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 99999,
            mal_id: None,
            title: "Show",
            title_romaji: "Show",
            title_english: "Show",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2024),
            end_year: Some(2024),
        },
    )
    .await
    .expect("series upsert");

    // Pending grab for ep 5 — should be found.
    let pending_id = record_grab(
        &db,
        "pending0000000000000000000000000000000001",
        "[Group] Show - 05",
        series_id,
        &[5],
        false,
    )
    .await
    .expect("pending grab")
    .expect("id");

    // Imported grab for ep 6 — must NOT be returned for ep 6 pending
    // lookup (different state).
    let imported_id = record_grab(
        &db,
        "imported000000000000000000000000000000002",
        "[Group] Show - 06",
        series_id,
        &[6],
        false,
    )
    .await
    .expect("imported grab")
    .expect("id");
    mark_imported(&db, imported_id)
        .await
        .expect("mark imported");

    let hits_ep5 = find_pending_for_episode(&db, series_id, 5)
        .await
        .expect("query");
    assert_eq!(
        hits_ep5.len(),
        1,
        "should find the one pending grab for ep 5"
    );
    assert_eq!(hits_ep5[0].id, pending_id);
    assert_eq!(hits_ep5[0].state, "pending");

    let hits_ep6 = find_pending_for_episode(&db, series_id, 6)
        .await
        .expect("query");
    assert!(
        hits_ep6.is_empty(),
        "imported grabs must not leak into pending lookup"
    );

    // Cancel path: mark_removed flips the state; a second lookup
    // should no longer return the row.
    mark_removed(&db, pending_id).await.expect("mark removed");
    let hits_after_remove = find_pending_for_episode(&db, series_id, 5)
        .await
        .expect("query");
    assert!(
        hits_after_remove.is_empty(),
        "removed grabs must not reappear in pending lookup"
    );
}

#[tokio::test]
async fn mark_replaced_flips_state_and_stamps_back_pointer() {
    use sqlx::Row;
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("memory db");
    crate::models::migrate(&db).await.expect("migrate");

    let (series_id, _) = series::upsert(
        &db,
        series::SeriesCore {
            anilist_id: 424242,
            mal_id: None,
            title: "Show",
            title_romaji: "Show",
            title_english: "Show",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2024),
            end_year: Some(2024),
        },
    )
    .await
    .expect("series upsert");

    let old_id = record_grab(
        &db,
        "old00000000000000000000000000000000000001",
        "[OldGroup] Show - 01",
        series_id,
        &[1],
        false,
    )
    .await
    .expect("old")
    .expect("id");
    mark_imported(&db, old_id).await.expect("mark imported");

    let new_id = record_grab(
        &db,
        "new00000000000000000000000000000000000001",
        "[BetterGroup] Show - Batch [BD]",
        series_id,
        &[1, 2, 3],
        true,
    )
    .await
    .expect("new")
    .expect("id");

    mark_replaced(&db, old_id, new_id)
        .await
        .expect("mark replaced");

    let row = sqlx::query("SELECT state, replaced_by_grab_id FROM grabbed_torrents WHERE id = ?")
        .bind(old_id)
        .fetch_one(&db)
        .await
        .expect("lookup");
    let state: String = row.get("state");
    let replaced_by: Option<i64> = row.get("replaced_by_grab_id");
    assert_eq!(state, "replaced");
    assert_eq!(replaced_by, Some(new_id));

    // The replacing grab's row surfaces via replaces_count in the
    // with_series query — verify end-to-end.
    let history = get_all_with_series(&db, 10, "english")
        .await
        .expect("history");
    let new_row = history
        .iter()
        .find(|r| r.id == new_id)
        .expect("new grab present");
    assert_eq!(new_row.replaces_count, 1);
    let old_row = history
        .iter()
        .find(|r| r.id == old_id)
        .expect("old grab present");
    assert_eq!(old_row.state, "replaced");
    assert_eq!(old_row.replaced_by_grab_id, Some(new_id));
    assert_eq!(
        old_row.replaced_by_torrent_name,
        "[BetterGroup] Show - Batch [BD]"
    );
}

// ── Issue #28: indexer attribution + respect_seed_rules ────

async fn pr_c_seed_a_grab(db: &SqlitePool) -> (i64, String) {
    let (series_id, _) = series::upsert(
        db,
        series::SeriesCore {
            anilist_id: 1,
            mal_id: None,
            title: "Show",
            title_romaji: "Show",
            title_english: "Show",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2024),
            end_year: Some(2024),
        },
    )
    .await
    .expect("series upsert");
    let hash = "abc123def456".to_string();
    let id = record_grab(db, &hash, "[Group] Show - 01.mkv", series_id, &[1], false)
        .await
        .expect("record")
        .expect("inserted");
    (id, hash)
}

#[tokio::test]
async fn set_indexer_attribution_writes_indexer_id_and_flag() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    let (grab_id, _) = pr_c_seed_a_grab(&db).await;

    set_indexer_attribution(&db, grab_id, Some(42), true)
        .await
        .expect("attribution");

    let row: (Option<i64>, i64) =
        sqlx::query_as("SELECT indexer_id, respect_seed_rules FROM grabbed_torrents WHERE id = ?")
            .bind(grab_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(row.0, Some(42));
    assert_eq!(row.1, 1);
}

#[tokio::test]
async fn set_indexer_attribution_with_none_clears_indexer_id() {
    // Nyaa grabs (None) leave indexer_id NULL. The flag also
    // takes False since there are no per-indexer rules to
    // respect. Pin both behaviors.
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    let (grab_id, _) = pr_c_seed_a_grab(&db).await;

    set_indexer_attribution(&db, grab_id, None, false)
        .await
        .expect("attribution");

    let row: (Option<i64>, i64) =
        sqlx::query_as("SELECT indexer_id, respect_seed_rules FROM grabbed_torrents WHERE id = ?")
            .bind(grab_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(row.0, None);
    assert_eq!(row.1, 0);
}

#[tokio::test]
async fn respects_seed_rules_returns_true_when_flag_set() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    let (grab_id, hash) = pr_c_seed_a_grab(&db).await;

    // Default: flag clear, no rules to respect.
    assert!(!respects_seed_rules(&db, &hash).await);

    // After stamping: flag set, delete-path skip should fire.
    set_indexer_attribution(&db, grab_id, Some(7), true)
        .await
        .expect("attribution");
    assert!(respects_seed_rules(&db, &hash).await);
}

#[tokio::test]
async fn respects_seed_rules_false_for_empty_hash_or_missing_row() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    // Empty hash short-circuits without a query.
    assert!(!respects_seed_rules(&db, "").await);
    // Hash that doesn't exist.
    assert!(!respects_seed_rules(&db, "no-such-hash").await);
}

// ── Multi-client refactor: download_client_id round-trip ────────
//
// Pre-PR-F regression: post_processing fanning `list_scoped` over
// a single (default) client meant pinned grabs got marked stale.
// The fan-out fix depends on `get_all_pending` faithfully reading
// back the stamped `download_client_id`. These tests pin that
// round-trip so a future SELECT-list refactor can't silently drop
// the column and re-introduce the bug.

#[tokio::test]
async fn set_download_client_round_trips_through_get_all_pending() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    let (grab_id, _) = pr_c_seed_a_grab(&db).await;

    // Pre-stamp: a fresh row reads back as None.
    let pending = get_all_pending(&db).await.expect("pending");
    let row = pending.iter().find(|g| g.id == grab_id).expect("present");
    assert_eq!(row.download_client_id, None);

    // After stamp: round-trip yields the stored id.
    set_download_client(&db, grab_id, Some(42))
        .await
        .expect("stamp");
    let pending = get_all_pending(&db).await.expect("pending");
    let row = pending.iter().find(|g| g.id == grab_id).expect("present");
    assert_eq!(row.download_client_id, Some(42));

    // Clearing back to NULL works too — a deleted/disabled client
    // unlinking would NULL the column via `download_clients::delete`.
    set_download_client(&db, grab_id, None)
        .await
        .expect("clear");
    let pending = get_all_pending(&db).await.expect("pending");
    let row = pending.iter().find(|g| g.id == grab_id).expect("present");
    assert_eq!(row.download_client_id, None);
}

/// `find_pending_for_episode` must read the `download_client_id`
/// column. Without it, the cancel-pending handler routes every
/// SAB grab to the (torrent) default client and the SAB queue
/// entry never gets removed — the user clicks Cancel, the row
/// vanishes from Ryokan's UI, and the SAB job downloads to
/// completion in the background as if nothing happened.
#[tokio::test]
async fn find_pending_for_episode_round_trips_download_client_id() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    let (grab_id, _) = pr_c_seed_a_grab(&db).await;
    set_download_client(&db, grab_id, Some(99))
        .await
        .expect("stamp");

    let series_id: i64 = sqlx::query_scalar("SELECT series_id FROM grabbed_torrents WHERE id = ?")
        .bind(grab_id)
        .fetch_one(&db)
        .await
        .unwrap();
    let pending = find_pending_for_episode(&db, series_id, 1)
        .await
        .expect("pending");
    let row = pending.iter().find(|g| g.id == grab_id).expect("present");
    assert_eq!(row.download_client_id, Some(99));
}

/// Same shape for `find_imported_for_episode` — used by the
/// delete-from-disk path. SAB grabs that completed and got
/// imported need the per-grab client routing to clean up the
/// SAB history entry's storage dir on user delete.
#[tokio::test]
async fn find_imported_for_episode_round_trips_download_client_id() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    crate::models::migrate(&db).await.unwrap();
    let (grab_id, _) = pr_c_seed_a_grab(&db).await;
    set_download_client(&db, grab_id, Some(123))
        .await
        .expect("stamp");
    mark_imported(&db, grab_id).await.expect("imported");

    let series_id: i64 = sqlx::query_scalar("SELECT series_id FROM grabbed_torrents WHERE id = ?")
        .bind(grab_id)
        .fetch_one(&db)
        .await
        .unwrap();
    let imported = find_imported_for_episode(&db, series_id, 1)
        .await
        .expect("imported");
    let row = imported.iter().find(|g| g.id == grab_id).expect("present");
    assert_eq!(row.download_client_id, Some(123));
}

// ── Misgrab guardrails ───────────────────────────────────────────────

async fn misgrab_pool() -> SqlitePool {
    let db = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    crate::models::migrate(&db).await.expect("migrate");
    db
}

async fn misgrab_series(db: &SqlitePool, anilist_id: i64, title: &str) -> i64 {
    let (id, _) = series::upsert(
        db,
        series::SeriesCore {
            anilist_id,
            mal_id: None,
            title,
            title_romaji: title,
            title_english: "",
            title_native: "",
            cover_url: "",
            format: "TV",
            status: "FINISHED",
            episodes: Some(12),
            season_year: Some(2024),
            end_year: None,
        },
    )
    .await
    .expect("series upsert");
    id
}

#[tokio::test]
async fn misgrab_columns_exist_after_migrate() {
    let db = misgrab_pool().await;
    for col in [
        "verification",
        "verified_at",
        "verification_detail",
        "failure_reason",
        "misgrab_action",
        "reviewed_at",
        "source_url",
    ] {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('grabbed_torrents') WHERE name = ?",
        )
        .bind(col)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1, "column {col} missing");
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('config') WHERE name = 'misgrab_auto_remove'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(n, 1);
    let cfg = crate::models::config::get_config(&db).await.unwrap();
    assert!(cfg.is_none_or(|c| c.misgrab_auto_remove), "default is on");
}

#[tokio::test]
async fn stamp_verification_only_writes_when_null() {
    let db = misgrab_pool().await;
    let sid = misgrab_series(&db, 1, "Show").await;
    let id = record_grab(&db, "aaaa", "[G] Show - 01", sid, &[1], false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(get_verification(&db, id).await, None);
    assert!(
        stamp_verification(&db, id, "misgrab", "{\"files\":[\"x.mkv\"]}")
            .await
            .unwrap()
    );
    assert!(
        !stamp_verification(&db, id, "verified", "{}").await.unwrap(),
        "second stamp is a no-op"
    );
    assert_eq!(get_verification(&db, id).await.as_deref(), Some("misgrab"));
    let row = get_by_id(&db, id).await.unwrap().expect("row");
    assert_eq!(row.verification.as_deref(), Some("misgrab"));
    assert!(row.misgrab_action.is_none());
}

#[tokio::test]
async fn is_blocklisted_release_matches_hash_or_series_title() {
    let db = misgrab_pool().await;
    let sid = misgrab_series(&db, 1, "Show").await;
    let other = misgrab_series(&db, 2, "Other").await;
    let id = record_grab(&db, "bbbb", "[G] Show - 02", sid, &[2], false)
        .await
        .unwrap()
        .unwrap();
    assert!(!is_blocklisted_release(&db, sid, "bbbb", "[G] Show - 02").await);
    assert_eq!(
        mark_failed_by_hash_with_reason(&db, "bbbb", "misgrab")
            .await
            .unwrap(),
        1
    );
    assert!(
        is_blocklisted_release(&db, other, "bbbb", "anything").await,
        "hash blocks globally"
    );
    assert!(
        is_blocklisted_release(&db, sid, "", "[G] Show - 02").await,
        "title blocks per series"
    );
    assert!(
        !is_blocklisted_release(&db, other, "", "[G] Show - 02").await,
        "title does not block another series"
    );
    let reason: String =
        sqlx::query_scalar("SELECT failure_reason FROM grabbed_torrents WHERE id = ?")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(reason, "misgrab");
    let snap = blocklist_snapshot(&db, 1).await;
    assert!(
        snap.rejects("BBBB", "unrelated title"),
        "hash match is case-insensitive"
    );
    assert!(
        snap.rejects("", "[g] show - 02"),
        "title match is case-insensitive"
    );
    assert!(
        !blocklist_snapshot(&db, 2)
            .await
            .rejects("", "[G] Show - 02")
    );
}

#[tokio::test]
async fn whitelist_by_hash_marks_all_rows_for_hash() {
    let db = misgrab_pool().await;
    let sid = misgrab_series(&db, 1, "Show").await;
    let old = record_grab(&db, "cccc", "[G] Show - 03", sid, &[3], false)
        .await
        .unwrap()
        .unwrap();
    stamp_verification(&db, old, "misgrab", "{}").await.unwrap();
    mark_failed_by_hash_with_reason(&db, "cccc", "misgrab")
        .await
        .unwrap();
    // A restored grab is a new pending row with the same hash.
    let fresh = record_grab(&db, "cccc", "[G] Show - 03", sid, &[3], false)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old, fresh);
    assert!(!is_whitelisted_hash(&db, "cccc").await);
    assert_eq!(whitelist_by_hash(&db, "cccc").await.unwrap(), 2);
    assert!(is_whitelisted_hash(&db, "cccc").await);
    assert_eq!(
        get_verification(&db, fresh).await.as_deref(),
        Some("whitelisted")
    );
    assert!(
        list_misgrabs(&db, "romaji").await.unwrap().is_empty(),
        "reviewed_at hides it"
    );
}

#[tokio::test]
async fn list_misgrabs_hides_reviewed_rows_and_parses_files_sample() {
    let db = misgrab_pool().await;
    let sid = misgrab_series(&db, 21521, "Kowaremono").await;
    let id = record_grab(&db, "dddd", "[Xonline] Grisaia", sid, &[1], false)
        .await
        .unwrap()
        .unwrap();
    let detail = VerificationDetail {
        files: vec![
            "Grisaia - 01.mkv".to_string(),
            "Grisaia - 02.mkv".to_string(),
        ],
        matched: None,
        reason: "no file matched".to_string(),
        notes: vec!["season mismatch".to_string()],
    };
    stamp_verification(&db, id, "misgrab", &serde_json::to_string(&detail).unwrap())
        .await
        .unwrap();
    set_misgrab_action(&db, id, "removed").await.unwrap();
    let rows = list_misgrabs(&db, "romaji").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].series_title, "Kowaremono");
    assert_eq!(rows[0].anilist_id, 21521);
    assert_eq!(rows[0].files_sample, detail.files);
    assert_eq!(rows[0].notes, vec!["season mismatch".to_string()]);
    assert_eq!(rows[0].status_label(), "Removed and blocklisted");
    assert_eq!(
        list_unhandled_misgrabs(&db).await.unwrap().len(),
        0,
        "action recorded"
    );
    mark_misgrab_reviewed(&db, id).await.unwrap();
    assert!(list_misgrabs(&db, "romaji").await.unwrap().is_empty());
}

#[tokio::test]
async fn get_all_pending_excludes_misgrab_rows_and_unverified_listing_respects_age() {
    let db = misgrab_pool().await;
    let sid = misgrab_series(&db, 1, "Show").await;
    let a = record_grab(&db, "eeee", "[G] Show - 04", sid, &[4], false)
        .await
        .unwrap()
        .unwrap();
    let b = record_grab(&db, "ffff", "[G] Show - 05", sid, &[5], false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(list_unverified_pending(&db, 0).await.unwrap().len(), 2);
    assert!(
        list_unverified_pending(&db, 3600).await.unwrap().is_empty(),
        "too young"
    );
    stamp_verification(&db, a, "misgrab", "{}").await.unwrap();
    let pending = get_all_pending(&db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, b);
    assert_eq!(list_unhandled_misgrabs(&db).await.unwrap().len(), 1);
    assert_eq!(list_unverified_pending(&db, 0).await.unwrap().len(), 1);
    assert_eq!(count_recent_misgrabs(&db, sid, 24).await, 1);
    set_source_url(&db, b, "magnet:?xt=urn:btih:ffff")
        .await
        .unwrap();
    assert_eq!(
        get_by_id(&db, b).await.unwrap().unwrap().source_url,
        "magnet:?xt=urn:btih:ffff"
    );
}
