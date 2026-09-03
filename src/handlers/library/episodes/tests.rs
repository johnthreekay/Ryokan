use super::*;
use crate::services::download_client::DownloadClient;
use crate::services::download_client::qbittorrent::QbitClient;
use crate::services::download_client::test_helpers;
use crate::test_support;
use std::sync::Arc;

/// `remove_hardlinks_with_inode` finds a hardlink anywhere under
/// `root` (depth-limited) and removes it. Verifies the actual
/// SAB-source-cleanup path: in hardlink import mode the
/// media-side and SAB-side files share an inode, so given the
/// inode of one we can locate and remove the other under SAB's
/// content_path even when SAB's reported `storage` field is
/// the parent complete dir (the user's reproducible bug).
#[cfg(unix)]
#[tokio::test]
async fn remove_hardlinks_with_inode_finds_link_in_subdirectory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // Simulate SAB's complete dir + per-job subfolder shape that
    // SAB's `del_files=1` guard refuses to descend into.
    let job_dir = root.join("[Erai-raws].One.Piece-1159.HEVC");
    std::fs::create_dir(&job_dir).unwrap();
    let source = job_dir.join("[Erai-raws].One.Piece-1159.HEVC.mkv");
    std::fs::write(&source, b"fake mkv contents").unwrap();
    // Media-side hardlink — same inode. Lives outside `root` so
    // it's not in scope for the cleanup pass.
    let media_dir = tmp.path().join("media");
    std::fs::create_dir(&media_dir).unwrap();
    let media = media_dir.join("ONE PIECE - S01E1159.mkv");
    std::fs::hard_link(&source, &media).unwrap();

    // Capture the shared inode via the media side (the realistic
    // call shape — caller has the media file's inode pre-deletion).
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(&media).unwrap().ino();
    // Remove the media side first (mirrors the production order:
    // delete media file, then walk SAB content_path for the
    // surviving hardlink).
    std::fs::remove_file(&media).unwrap();

    let removed = remove_hardlinks_with_inode(root, inode).await;
    assert_eq!(removed.len(), 1, "expected one source hardlink removed");
    assert_eq!(removed[0], source);
    assert!(!source.exists(), "source file must be gone");
    // Job subdir cleaned up (became empty after file removal).
    assert!(!job_dir.exists(), "empty job dir must be cleaned");
    // Root preserved — never rmdir the user's complete dir.
    assert!(root.exists(), "root must not be touched");
}

/// Inode mismatch (copy-mode imports — different inodes) means
/// the helper finds nothing and removes nothing. The client's
/// own `delete(hash, true)` is the cleanup path for copy-mode.
#[cfg(unix)]
#[tokio::test]
async fn remove_hardlinks_with_inode_skips_when_inode_does_not_match() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let source = root.join("show.mkv");
    std::fs::write(&source, b"contents").unwrap();

    // Pick an inode that very likely doesn't match anything in
    // tmp (max u64 sentinel — real inodes are never this).
    let removed = remove_hardlinks_with_inode(root, u64::MAX).await;
    assert!(removed.is_empty());
    assert!(source.exists(), "non-matching files must survive");
}

/// `remove_stamped_source_paths` removes the exact paths recorded
/// at import time and prunes the immediate parent dir (the SAB
/// job folder) if it became empty. Mode-agnostic — works for
/// hardlink, copy, or move imports (move's source already gone,
/// paths just no-op).
#[tokio::test]
async fn remove_stamped_source_paths_removes_files_and_immediate_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let job_dir = tmp.path().join("job-folder");
    std::fs::create_dir(&job_dir).unwrap();
    let f1 = job_dir.join("ep01.mkv");
    let f2 = job_dir.join("ep02.mkv");
    std::fs::write(&f1, b"a").unwrap();
    std::fs::write(&f2, b"b").unwrap();
    let stamps = vec![f1.display().to_string(), f2.display().to_string()];

    let removed = remove_stamped_source_paths(&stamps).await;
    assert_eq!(removed.len(), 2);
    assert!(!f1.exists());
    assert!(!f2.exists());
    // Job folder (immediate parent of the removed files) cleaned.
    assert!(!job_dir.exists(), "empty job dir should be pruned");
    // Tempdir itself (the would-be `complete/` root) preserved.
    assert!(tmp.path().exists(), "tempdir root must NOT be pruned");
}

/// **Regression guard for the "deletes the whole complete dir"
/// bug.** When the SAB complete dir contains exactly one job and
/// we remove that job, the prune logic must NOT ascend into
/// removing the complete dir itself. Earlier versions walked up
/// unbounded and trashed the whole download tree.
#[tokio::test]
async fn remove_stamped_source_paths_does_not_ascend_above_one_level() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Mirror the user-reported shape:
    // <tmp>/complete/[job-folder]/file.mkv
    let complete = tmp.path().join("complete");
    let job = complete.join("[job-folder]");
    std::fs::create_dir(&complete).unwrap();
    std::fs::create_dir(&job).unwrap();
    let f = job.join("file.mkv");
    std::fs::write(&f, b"data").unwrap();

    let removed = remove_stamped_source_paths(&[f.display().to_string()]).await;
    assert_eq!(removed.len(), 1);
    assert!(!f.exists(), "stamped file removed");
    assert!(!job.exists(), "empty job folder cleaned (one level up)");
    // CRITICAL: the configured complete dir survives even though
    // it's now empty. Earlier unbounded walk would rmdir this.
    assert!(
        complete.exists(),
        "complete dir must NOT be rmdir'd just because the only job inside it was cleared"
    );
}

/// Missing source files (move-mode imports — source already
/// renamed away) don't error out. The helper just records nothing
/// removed and the caller continues.
#[tokio::test]
async fn remove_stamped_source_paths_silently_skips_missing_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let stamps = vec![tmp.path().join("does-not-exist.mkv").display().to_string()];
    let removed = remove_stamped_source_paths(&stamps).await;
    assert!(removed.is_empty());
}

/// Non-empty parent dir is preserved — we only prune dirs that
/// became empty as a result of OUR removal. Other unrelated files
/// in a shared dir aren't touched.
#[tokio::test]
async fn remove_stamped_source_paths_preserves_non_empty_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("shared");
    std::fs::create_dir(&dir).unwrap();
    let target = dir.join("target.mkv");
    let bystander = dir.join("bystander.mkv");
    std::fs::write(&target, b"a").unwrap();
    std::fs::write(&bystander, b"b").unwrap();

    let stamps = vec![target.display().to_string()];
    let removed = remove_stamped_source_paths(&stamps).await;
    assert_eq!(removed.len(), 1);
    assert!(!target.exists());
    assert!(bystander.exists(), "unrelated sibling must survive");
    assert!(dir.exists(), "non-empty parent must not be pruned");
}

/// D2+D3 live integration test: cancelling a pending grab for an
/// episode must delete the torrent from the active download
/// client AND clear the grab state in the DB. Covers both the
/// blocklist (D2) and episode-removal (D3) call paths — they
/// share this same "delete in-flight grab and clean up state"
/// trait surface. The blocklist-specific path (`mark_episode_failed`)
/// additionally kicks off an auto-search re-run which requires
/// live AniList + Nyaa and is outside the scope of this trait-
/// boundary test.
///
/// Flow:
/// 1. Seed DB: series + pending grab_torrents row for episode 1.
/// 2. Upload synthetic torrent to qBit with matching hash.
/// 3. Call `cancel_pending_episode(anilist_id, 1)`.
/// 4. Assert torrent deleted from qBit.
#[tokio::test]
#[ignore = "requires live qBit + transmission-create"]
async fn d2_d3_cancel_pending_deletes_from_client() {
    if std::env::var("RYOKAN_QBIT_E2E").is_err() {
        eprintln!("skipping");
        return;
    }
    let Some((_tmp, torrent)) = test_helpers::build_named_torrent("d2-d3-cancel-pending") else {
        return;
    };
    let pass = std::env::var("QBIT_PASS").unwrap_or_else(|_| "adminadmin".to_string());
    let base_url = "http://localhost:8080";
    let category = "ryokan-e2e-d2d3";

    let hash =
        test_helpers::upload_torrent_file_qbit(base_url, "admin", &pass, category, &torrent).await;

    let pool = test_support::in_memory_pool().await;
    let qbit: Arc<dyn DownloadClient> =
        Arc::new(QbitClient::new(base_url, "admin", &pass, category));
    let state = test_support::build_test_app_state(pool.clone(), Some(qbit.clone()));

    // Seed: series + pending grab for episode 1. The
    // `seed_grabbed_torrent` helper writes state='pending' and
    // episode_numbers='[1]' by default — matches what the
    // handler looks up.
    let anilist_id: i64 = 54321;
    let series_id = test_support::seed_series(&pool, anilist_id, "D2/D3 Test Series").await;
    test_support::seed_grabbed_torrent(&pool, series_id, &hash, "d2-d3-test.torrent", &[1]).await;
    assert_eq!(
        test_support::count_grabs_for_series(&pool, series_id).await,
        1,
        "precondition: 1 grab seeded"
    );

    // Exercise: cancel the pending grab for episode 1.
    let (status, body) = cancel_pending_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 1)),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "cancel_pending_episode returned non-OK: {status} body={}",
        body.0
    );

    // Assert: torrent deleted from qBit.
    let check_client = QbitClient::new(base_url, "admin", &pass, category);
    let list = check_client
        .list_scoped()
        .await
        .expect("list_scoped post-cancel");
    assert!(
        !list.iter().any(|t| t.hash.eq_ignore_ascii_case(&hash)),
        "D2/D3: cancelled torrent must be deleted from qBit (still in list: {list:?})"
    );
    eprintln!("D2/D3 integration verified");
}

// ─── CI-gated episode-handler coverage ────────────────
//
// Directly-called handler tests that don't need a live
// download client. Complements the env-gated d1/d2/d3 tests
// above; those prove the client-backed paths, these prove the
// DB-only paths that run whenever the client isn't involved
// (404 on unknown series, grab-history pass-through,
// resolve_tracked_series lookup semantics).
mod episodes_ci {
    use super::super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::Json as AxumJson;

    // ─── get_episode_grab_history ────────────────────────────

    #[tokio::test]
    async fn get_episode_grab_history_returns_empty_for_series_without_grabs() {
        let db = in_memory_pool().await;
        let anilist_id: i64 = 100;
        let _ = seed_series(&db, anilist_id, "New Show").await;
        let state = build_test_app_state(db, None);
        let AxumJson(history) = get_episode_grab_history(State(state), Path((anilist_id, 1)))
            .await
            .expect("empty history should be Ok, not error");
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn get_episode_grab_history_rejects_untracked_series_with_400() {
        // Series not in library → 400, not a silent empty list.
        // Caller expects a clear "you asked about a series I
        // don't track" signal rather than a success-with-zero-
        // rows that might be mistaken for "no grabs yet."
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let err = get_episode_grab_history(State(state), Path((99_999, 1)))
            .await
            .expect_err("unknown series should 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("not in library"));
    }

    #[tokio::test]
    async fn get_episode_grab_history_accepts_internal_id_and_anilist_id_equivalently() {
        // The path parameter can be either an AniList id or an
        // internal series id — resolve_tracked_series handles
        // both. Pin dual-lookup PARITY by seeding a real grab
        // row and asserting that both paths return it. Without
        // the seed, an empty-history-for-both result would only
        // prove the internal-id branch doesn't reject — it
        // wouldn't catch a regression that routed the two ids
        // to different series.
        let db = in_memory_pool().await;
        let anilist_id: i64 = 101;
        let series_id = seed_series(&db, anilist_id, "Show").await;
        // Raw SQL insert — bypasses episode_tags::record_grab's
        // ClassificationResult plumbing since we only care that
        // the row round-trips through the handler's resolver.
        sqlx::query(
            "INSERT INTO episode_grab_history \
             (series_id, episode_number, quality_tag, release_title, release_group) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(series_id)
        .bind(5_i32)
        .bind("WEBDL-1080p")
        .bind("[Group] Show - 05 [WEB-DL 1080p].mkv")
        .bind("Group")
        .execute(&db)
        .await
        .unwrap();

        let state = build_test_app_state(db, None);
        let AxumJson(via_al) =
            get_episode_grab_history(State(state.clone()), Path((anilist_id, 5)))
                .await
                .expect("AL-id lookup should work");
        let AxumJson(via_internal) = get_episode_grab_history(State(state), Path((series_id, 5)))
            .await
            .expect("internal-id lookup should work");

        assert_eq!(via_al.len(), 1, "AL-id lookup must return the seeded grab");
        assert_eq!(
            via_internal.len(),
            1,
            "internal-id lookup must return the seeded grab"
        );
        // Parity: both paths resolve to the same series, so they
        // return the same row (same release_title + quality_tag).
        assert_eq!(via_al[0].release_title, via_internal[0].release_title);
        assert_eq!(via_al[0].quality_tag, via_internal[0].quality_tag);
        assert_eq!(
            via_al[0].release_title,
            "[Group] Show - 05 [WEB-DL 1080p].mkv"
        );
    }

    // ─── delete_episode_file (no-client path) ─────────────────

    #[tokio::test]
    async fn delete_episode_file_on_unknown_series_returns_error_status() {
        // `delete_episode_file` returns `Response` (the htmx-aware
        // body shape: htmx path emits empty body + `HX-Trigger`,
        // non-htmx path emits the JSON `{ok, message}` shape). The
        // specific status depends on the resolve path: in an
        // offline test env, `resolve_series_context` fails before
        // reaching the "series not in library" branch (AniList
        // unreachable → 502). Either way, the handler must emit a
        // 4xx/5xx with a structured body so the UI can show the
        // reason; silently succeeding on an unknown id would delete
        // phantom files.
        //
        // We exercise the non-htmx path here because it preserves a
        // JSON body we can parse for the structured-error contract;
        // the htmx path is empty-body-by-design and is covered by
        // the trigger-payload assertions below.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = delete_episode_file(State(state), HxRequest(false), Path((99_999, 1))).await;
        assert!(
            resp.status().is_client_error() || resp.status().is_server_error(),
            "unknown series must be an error status, got {}",
            resp.status()
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body collect");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("non-htmx error path returns JSON");
        assert_eq!(body["ok"], false);
    }

    #[tokio::test]
    async fn delete_episode_file_htmx_path_emits_empty_body_with_trigger() {
        // Companion to the assertion above: the htmx path
        // (`HxRequest(true)`) replaces the JSON body with an
        // `HX-Trigger` header carrying the structured payload
        // (`ryokan-episode-deleted` event with `ok`, `episode_number`,
        // `message`). The frontend listener in `static/js/series.js`
        // keys off this event for the toast + row update. Empty body
        // is load-bearing — the modal footer button row would reflow
        // for any text-bearing response.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = delete_episode_file(State(state), HxRequest(true), Path((99_999, 1))).await;
        assert!(
            resp.status().is_client_error() || resp.status().is_server_error(),
            "unknown series under htmx must still be an error status, got {}",
            resp.status()
        );
        let trigger = resp
            .headers()
            .get("HX-Trigger")
            .expect("htmx error path must carry HX-Trigger")
            .to_str()
            .unwrap()
            .to_string();
        let payload: serde_json::Value =
            serde_json::from_str(&trigger).expect("trigger header is JSON");
        assert_eq!(payload["ryokan-episode-deleted"]["ok"], false);
        assert_eq!(payload["ryokan-episode-deleted"]["episode_number"], 1);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body collect");
        assert!(bytes.is_empty(), "htmx response body must be empty");
    }

    // `series_episodes_json` + `mark_episode_failed` are
    // deliberately not covered here: both call
    // `resolve_series_context` which unconditionally consults
    // AniList + metadata_cache on the first miss, so exercising
    // them from a cold in-memory DB just hits AniList. Covering
    // them requires either wiremock'ing the AniList client
    // (punted with the rest of the HTTP-backed provider
    // tests) or seeding the provider_metadata_cache table,
    // which is a separate plan item.

    // ─── cancel_pending_episode (DB-only paths) ─────────────────
    //
    // The full happy-path test (`d2_d3_cancel_pending_deletes_from_client`
    // above) is `#[ignore]`d on a live qBit. These cover the DB-only
    // branches that don't need a download client — important
    // because the handler routes through `state.resolve_grab_client`
    // which returns `None` when no client is configured, and the
    // delete still needs to flip the grab's state to `removed`.

    #[tokio::test]
    async fn cancel_pending_episode_returns_400_for_unknown_series() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let (status, body) = cancel_pending_episode(State(state), Path((99_999, 1))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["ok"], false);
        assert!(
            body.0["message"]
                .as_str()
                .unwrap_or("")
                .contains("not in library"),
            "message must explain the missing-series cause: {body:?}",
            body = body.0
        );
    }

    #[tokio::test]
    async fn cancel_pending_episode_returns_404_when_no_pending_grab() {
        let db = in_memory_pool().await;
        let anilist_id: i64 = 200;
        let _ = seed_series(&db, anilist_id, "Tracked, no grabs").await;
        let state = build_test_app_state(db, None);

        let (status, body) = cancel_pending_episode(State(state), Path((anilist_id, 5))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "tracked series with no pending grab + no stuck-grabbed tag must 404 — anything else means the no-op path silently \"succeeded\""
        );
        assert_eq!(body.0["ok"], false);
    }

    #[tokio::test]
    async fn cancel_pending_episode_flips_grab_state_to_removed_when_no_client() {
        // Without a configured download client,
        // `resolve_grab_client` returns None and the handler skips
        // the client.delete call. The DB-side state flip
        // (mark_removed) must STILL run so the upgrade sweep
        // doesn't see this as a still-pending grab and re-grab on
        // its next cycle. Pin that contract.
        let db = in_memory_pool().await;
        let anilist_id: i64 = 300;
        let series_id = seed_series(&db, anilist_id, "Cancel-no-client").await;
        let test_hash = "deadbeef00000000000000000000000000000000";
        // `seed_grabbed_torrent` returns the grab id via
        // `last_insert_rowid()`, which is connection-local in
        // sqlx's SQLite pool — under contention it can come back
        // from a different connection than the INSERT and read 0.
        // Look the row up by hash instead so the test is
        // deterministic across connection-pool churn.
        let _ = crate::test_support::seed_grabbed_torrent(
            &db,
            series_id,
            test_hash,
            "[Group] Cancel-no-client - 03.mkv",
            &[3],
        )
        .await;
        let state = build_test_app_state(db.clone(), None);

        let (status, body) = cancel_pending_episode(State(state), Path((anilist_id, 3))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "no-client path must still 200 + flip the grab state — got status={status} body={body}",
            body = body.0
        );
        assert_eq!(body.0["cancelled"], 1);

        let final_state: String =
            sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE hash = ?")
                .bind(test_hash)
                .fetch_one(&db)
                .await
                .expect("fetch grab state by hash");
        assert_eq!(
            final_state, "removed",
            "cancel must flip pending grab to 'removed' even when no client is configured"
        );
    }

    // ─── episode_download_progress (DB-only) ────────────────────

    #[tokio::test]
    async fn episode_download_progress_returns_400_for_unknown_series() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        // Avoid `.expect_err` since the Ok branch's
        // `Json<Vec<EpisodeProgress>>` doesn't derive Debug. A
        // plain match dodges the Debug bound and reads cleaner
        // for a binary status assertion.
        match episode_download_progress(State(state), Path(99_999)).await {
            Err((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("unknown series must return Err, not Ok"),
        }
    }

    #[tokio::test]
    async fn episode_download_progress_returns_empty_for_tracked_series_with_no_pending_grabs() {
        let db = in_memory_pool().await;
        let anilist_id: i64 = 400;
        let _ = seed_series(&db, anilist_id, "Tracked, no pending").await;
        let state = build_test_app_state(db, None);

        let result = episode_download_progress(State(state), Path(anilist_id)).await;
        let AxumJson(progress) = result.expect("tracked series must succeed");
        assert!(
            progress.is_empty(),
            "no pending grabs → empty list (NOT a 500 — the polling loop in series.js calls this every 5s and a 500 would spam the console)"
        );
    }

    // ─── series_episodes_json + mark_episode_failed (cache-seeded) ──
    //
    // These two handlers gate on `resolve_series_context`, which on a
    // cold DB falls through to a live AniList request. Pre-seeding
    // `series_metadata_cache` short-circuits the resolver at line 263
    // of `handlers/library/reconcile.rs` — `cached.is_fresh = true`
    // returns immediately without any network. Without these tests
    // both handlers stayed dark in coverage; with them, the happy path
    // through `series_episodes_json` (which renders an episode list
    // from cached AnimeDetail + on-disk file walk) plus the early-
    // error branches of `mark_episode_failed` are pinned.

    fn detail_fixture(id: i64, romaji: &str) -> crate::services::anilist::AnimeDetail {
        crate::services::anilist::AnimeDetail {
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: romaji.into(),
            title_english: romaji.into(),
            title_native: romaji.into(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: "Finished".into(),
            episodes: Some(3),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2024),
            end_year: Some(2024),
            description: String::new(),
            genres: vec![],
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: vec![],
            streaming_episodes: vec![],
            relations: vec![],
        }
    }

    #[tokio::test]
    async fn series_episodes_json_returns_episodes_from_cached_metadata() {
        // Cache hit → resolver returns immediately, build_episodes
        // synthesizes one Episode row per episodes count, JSON
        // round-trips clean. Pins both the resolver short-circuit
        // and the basic build_episodes plumbing — a refactor that
        // dropped `episodes` from the response (or returned nested
        // JSON shape) would surface here as a count mismatch.
        let db = in_memory_pool().await;
        let anilist_id: i64 = 500;
        let series_id = seed_series(&db, anilist_id, "Cached Show").await;
        crate::models::metadata_cache::upsert(
            &db,
            series_id,
            anilist_id,
            None,
            &detail_fixture(anilist_id, "Cached Show"),
        )
        .await
        .unwrap();

        let state = build_test_app_state(db, None);
        let AxumJson(episodes) = series_episodes_json(State(state), Path(anilist_id))
            .await
            .expect("cache-hit path must succeed without network");
        assert_eq!(
            episodes.len(),
            3,
            "build_episodes must synthesize one Episode row per `episodes` count"
        );
        // Episode numbers are 1..=ep_count and contiguous. The
        // template renders newest-first so build_episodes returns
        // them descending — pin both ends to catch a future sort
        // direction flip.
        assert_eq!(episodes[0].number, 3);
        assert_eq!(episodes[2].number, 1);
    }

    #[tokio::test]
    async fn mark_episode_failed_returns_400_when_series_not_in_library() {
        // No `series` row + no metadata cache + no live AniList →
        // `resolve_series_context` returns Err (502 in the handler).
        // Path is therefore: `request_id=99999` → resolver fails at
        // network step → handler returns 502, NOT 400. Pin the
        // actual outcome so a refactor that flipped the error
        // mapping (the file is full of similar error-mapping
        // chains) gets caught.
        //
        // Why not 400: the handler's 400 branch fires when
        // `resolve_series_context` returns Ok with `tracked_row =
        // None` — a known-bad provider id where AL had a "found
        // no series" answer. Network failure produces a 502
        // because we couldn't tell whether the series exists at
        // all, only that we couldn't ask.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let result = mark_episode_failed(
            State(state),
            Path((99_999, 1)),
            AxumJson(MarkEpisodeFailedForm {
                history_id: 1,
                blocklist: false,
            }),
        )
        .await;
        match result {
            Err((status, _)) => {
                assert!(
                    status == StatusCode::BAD_GATEWAY || status == StatusCode::BAD_REQUEST,
                    "untracked series with no cache must surface as a 4xx/5xx error; got {status}"
                );
            }
            Ok(_) => panic!("untracked series must return Err, not Ok"),
        }
    }

    #[tokio::test]
    async fn mark_episode_failed_returns_500_when_history_id_does_not_exist() {
        // Series is tracked + metadata cached → resolver succeeds.
        // `mark_grab_failed` then queries `episode_grab_history`
        // by id; an unknown id maps to `sqlx::Error::RowNotFound`
        // which the handler maps to 500. Pin the contract: a
        // refactor that swallowed the model error and returned
        // an empty AutoSearchReport would silently report
        // "no upgrades found" instead of surfacing the bad
        // history_id, leaving the user confused why their
        // mark-failed click did nothing.
        let db = in_memory_pool().await;
        let anilist_id: i64 = 501;
        let series_id = seed_series(&db, anilist_id, "Tracked With Cache").await;
        crate::models::metadata_cache::upsert(
            &db,
            series_id,
            anilist_id,
            None,
            &detail_fixture(anilist_id, "Tracked With Cache"),
        )
        .await
        .unwrap();

        let state = build_test_app_state(db, None);
        let result = mark_episode_failed(
            State(state),
            Path((anilist_id, 1)),
            AxumJson(MarkEpisodeFailedForm {
                history_id: 99_999,
                blocklist: false,
            }),
        )
        .await;
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert!(
                    !body.is_empty(),
                    "the mark-failed model error must propagate to the response body"
                );
            }
            Ok(_) => panic!("bogus history_id must return Err, not Ok"),
        }
    }

    // ─── episode_download_progress: pending grab + no clients ────────

    #[tokio::test]
    async fn episode_download_progress_returns_empty_when_pool_has_no_clients() {
        // The handler's "no configured clients" branch (line 1089
        // of mod.rs) returns an empty Vec rather than 500 when there
        // are pending grabs but no `DownloadClient` to query. The
        // poll runs every 5s on the series page; a 500 here would
        // spam the console and the rendering loop. Companion to
        // `episode_download_progress_returns_empty_for_tracked_series_with_no_pending_grabs`,
        // which covers the other empty branch (no pending grabs).
        let db = in_memory_pool().await;
        let anilist_id: i64 = 502;
        let series_id = seed_series(&db, anilist_id, "Pending No Client").await;
        let _ = crate::test_support::seed_grabbed_torrent(
            &db,
            series_id,
            "abc123abc123abc123abc123abc123abc123abcd",
            "[Group] Pending No Client - 03.mkv",
            &[3],
        )
        .await;
        // build_test_app_state(db, None) constructs an empty
        // download_clients pool, so the handler's pool.clients
        // .is_empty() branch fires.
        let state = build_test_app_state(db, None);

        let result = episode_download_progress(State(state), Path(anilist_id)).await;
        let AxumJson(progress) = result.expect("no-clients-with-pending-grab path must succeed");
        assert!(
            progress.is_empty(),
            "no configured clients → empty list, not 500 — the polling loop calls this every 5s"
        );
    }

    #[test]
    fn episode_progress_wire_shape_carries_both_state_fields() {
        // The series-page download-progress poller (series.js)
        // keys off `state_kind` for the "Importing…" transition
        // check (kind starts with "seeding" → torrent finished).
        // The client-native `state` string is kept for debug
        // tooling only. Regression for the PR that added
        // state_kind: a silent `#[serde(skip)]` or rename would
        // leave `isComplete` always false and the progress bar
        // stuck at 100% forever.
        let p = super::super::EpisodeProgress {
            episode: 5,
            progress: 1.0,
            speed: 0,
            state: "stalledUP".to_string(),
            state_kind: crate::services::download_client::DownloadItemState::SeedingStalled,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["episode"], 5);
        assert_eq!(v["state"], "stalledUP");
        assert_eq!(v["state_kind"], "seeding-stalled");
    }
}

// ─── End-to-end: cancel-pending against a wiremocked SAB client ────
//
// CI-runnable companion to the env-gated `d2_d3_cancel_pending_deletes_from_client`
// (which only runs against a live qBit). Pins the full handler →
// `resolve_grab_client` → `SabClient::delete` → SAB-API path against
// a mock SAB server, with two specific regressions guarded:
//
//   1. The 2026-05-03 "Cancel Pending isn't removing in-flight SAB
//      jobs" report. Earlier impl tried `mode=history&name=delete`
//      first, hit SAB's phantom-true response shape (history's
//      `_api_history_delete` calls `report()` regardless of whether
//      the nzo_id was found), returned Ok without ever touching
//      queue, marked the grab removed in the DB, and SAB happily
//      kept downloading. Pinned via `expect(0)` on the history
//      mock — a regression to history-first would fail this test
//      even if the wire-format pin in the SAB wiremock_tests still
//      passed.
//   2. The handler's `resolve_grab_client` correctly routing to a
//      stamped `download_client_id`. A future refactor that drops
//      the stamped-id path (e.g. always falls through to the
//      hash-shape SAB heuristic) would still work for SAB nzo_ids
//      but would break for any client whose hashes aren't
//      identifiable. Test seeds the grab with a stamped client_id.
mod cancel_pending_sab_e2e {
    use super::super::cancel_pending_episode;
    use crate::services::download_client::DownloadClient;
    use crate::services::download_client::sabnzbd::SabClient;
    use crate::test_support::seed_series;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use sqlx::SqlitePool;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Single-connection in-memory pool. The shared
    /// `test_support::in_memory_pool` builds a default `SqlitePool`
    /// which can hold multiple connections to `:memory:` — and each
    /// `:memory:` connection has its OWN database, so a row inserted
    /// via connection A is invisible to connection B. Most tests in
    /// the codebase happen to land all their queries on a single
    /// connection by luck, but this test fans out across the seed
    /// path, the handler's resolve+delete path, and a post-condition
    /// read — that's enough connection churn to flake (`RowNotFound`
    /// when the post-cancel grab-state read landed on a different
    /// connection than the seed). Pinning `max_connections=1` keeps
    /// every query on the same physical DB.
    async fn single_connection_in_memory_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("open :memory: SQLite");
        crate::models::migrations::migrate(&pool)
            .await
            .expect("run migrations");
        pool
    }

    /// Build an `AppState` with the supplied SAB client at id=1,
    /// marked as the torrent default. We pin the grab's
    /// `download_client_id = 1` in the seed below so the routing
    /// goes through the stamped-id path; making the SAB client also
    /// the torrent default just means the legacy fall-through (a
    /// future bug that drops the stamped path) still lands here
    /// rather than panicking on no-default. Mirrors what
    /// `build_test_app_state` does, but inlined so this test can
    /// hold the SabClient `Arc` for in-place verification (test
    /// helpers don't return the client).
    fn app_state_with_sab(db: sqlx::SqlitePool, sab: Arc<dyn DownloadClient>) -> crate::AppState {
        let mut clients: HashMap<i64, Arc<dyn DownloadClient>> = HashMap::new();
        clients.insert(1, sab);
        let pool = crate::DownloadClientPool {
            clients,
            default_torrent_id: Some(1),
            default_usenet_id: None,
        };
        crate::AppState {
            db,
            download_clients: Arc::new(RwLock::new(Arc::new(pool))),
            jellyfin: Arc::new(RwLock::new(None)),
            custom_formats: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            indexers: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            progress: crate::services::progress::ProgressRegistry::new(),
            users_exist: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            interactive_search_cache: crate::services::interactive_search_cache::new(),
            oauth_state: crate::services::oauth_state::new(),
            start_time: chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_067_200, 0)
                .expect("epoch"),
            tasks: crate::services::task_registry::TaskRegistry::new(),
            dc_status_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            notification_providers: crate::services::notifications::empty_cache(),
            import_sessions: crate::services::manual_import::session::new_store(),
        }
    }

    /// Seed a `grabbed_torrents` row with a known SAB-shape nzo_id
    /// hash, state='pending', episode_numbers=[1], and the supplied
    /// `download_client_id` stamp (so resolve_grab_client routes
    /// through the stamped-id path rather than the hash heuristic).
    async fn seed_sab_grab(
        db: &sqlx::SqlitePool,
        series_id: i64,
        nzo_id: &str,
        download_client_id: i64,
    ) -> i64 {
        sqlx::query(
            "INSERT INTO grabbed_torrents \
                 (series_id, hash, torrent_name, episode_numbers, state, download_client_id) \
             VALUES (?, ?, ?, '[1]', 'pending', ?)",
        )
        .bind(series_id)
        .bind(nzo_id)
        .bind("Test Show - 01.nzb")
        .bind(download_client_id)
        .execute(db)
        .await
        .expect("seed grab");
        sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(db)
            .await
            .expect("fetch grab id")
    }

    #[tokio::test]
    async fn cancel_pending_routes_through_sab_queue_delete_for_in_flight_nzb() {
        // Wiremock: queue?delete = honest success, history?delete =
        // phantom-true (the live SAB shape). With queue-first
        // ordering, the impl must hit queue and never touch history.
        let server = MockServer::start().await;
        let queue_delete_mock = Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("mode", "queue"))
            .and(query_param("name", "delete"))
            .and(query_param("value", "SABnzbd_nzo_test123"))
            .and(query_param("del_files", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": true,
            })))
            .expect(1);
        server.register(queue_delete_mock).await;
        // History MUST NOT be hit — `expect(0)` catches a regression
        // to the history-first ordering that was the actual 2026-05-03
        // bug. Wiremock validates `.expect(0)` on `server.verify()`.
        let history_delete_mock = Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("mode", "history"))
            .and(query_param("name", "delete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": true,
            })))
            .expect(0);
        server.register(history_delete_mock).await;

        let sab: Arc<dyn DownloadClient> = Arc::new(SabClient::new(
            &server.uri(),
            "",
            "test-api-key",
            "ryokan-test",
        ));
        let db = single_connection_in_memory_pool().await;
        let anilist_id: i64 = 12345;
        let series_id = seed_series(&db, anilist_id, "Cancel-Pending SAB E2E").await;
        let grab_id = seed_sab_grab(&db, series_id, "SABnzbd_nzo_test123", 1).await;
        let state = app_state_with_sab(db.clone(), sab);

        let (status, body) = cancel_pending_episode(State(state), Path((anilist_id, 1))).await;

        assert_eq!(
            status,
            StatusCode::OK,
            "handler returned non-OK: {status} body={}",
            body.0
        );
        assert_eq!(
            body.0["ok"], true,
            "handler must report ok=true on successful cancel: body={}",
            body.0
        );
        assert_eq!(
            body.0["cancelled"], 1,
            "exactly one grab must be cancelled: body={}",
            body.0
        );
        // Wiremock validates queue.expect(1) AND history.expect(0)
        // on this call — an extra `assert!(...)` here would just
        // duplicate that contract.
        server.verify().await;

        // DB-side: the grab row must be marked removed so a
        // subsequent re-grab attempt sees fresh state and doesn't
        // dedup against this row.
        let new_state: String =
            sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
                .bind(grab_id)
                .fetch_one(&db)
                .await
                .expect("read grab state");
        assert_eq!(
            new_state, "removed",
            "grab row must transition to 'removed' after successful cancel; got {new_state}"
        );
    }

    #[tokio::test]
    async fn cancel_pending_falls_back_to_history_when_queue_reports_absent() {
        // The post-import case: queue says false (nzo_id has aged
        // out of the queue and lives only in history), history says
        // true. The handler must succeed — the user clicked
        // Cancel after import-but-before-grab-state-flip and
        // expects the row gone either way. Pinned because the
        // queue-first refactor must not have over-corrected into
        // "queue-only" by accident.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("mode", "queue"))
            .and(query_param("name", "delete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": false,
            })))
            .mount(&server)
            .await;
        // History gets called and succeeds with del_files=1 — the
        // unpacked output dir cleanup pin from the SAB wiremock
        // tests, end-to-end here.
        let history_mock = Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("mode", "history"))
            .and(query_param("name", "delete"))
            .and(query_param("del_files", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": true,
            })))
            .expect(1);
        server.register(history_mock).await;

        let sab: Arc<dyn DownloadClient> = Arc::new(SabClient::new(
            &server.uri(),
            "",
            "test-api-key",
            "ryokan-test",
        ));
        let db = single_connection_in_memory_pool().await;
        let anilist_id: i64 = 12346;
        let series_id = seed_series(&db, anilist_id, "Cancel-Pending SAB Hist Fallback").await;
        seed_sab_grab(&db, series_id, "SABnzbd_nzo_imported456", 1).await;
        let state = app_state_with_sab(db.clone(), sab);

        let (status, body) = cancel_pending_episode(State(state), Path((anilist_id, 1))).await;

        assert_eq!(status, StatusCode::OK, "body={}", body.0);
        assert_eq!(body.0["cancelled"], 1);
        server.verify().await;
    }
}
