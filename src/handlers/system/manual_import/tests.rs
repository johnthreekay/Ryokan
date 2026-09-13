//! Tests for the manual-import wizard handlers, split out of `mod.rs`
//! per `tests/AGENTS.md` (the inline modules pushed the file past
//! 1500 lines). Three topic modules: start-form validation, the
//! preview routes, and the confirm / cancel / report routes.

mod validate_tests {
    use super::super::*;

    fn form(path: &str, mode: &str) -> StartForm {
        StartForm {
            path: path.to_string(),
            mode: mode.to_string(),
            follow_symlinks: Some("1".into()),
            include_hidden: None,
        }
    }

    #[test]
    fn validate_requires_media_root_then_path() {
        let err = validate_start(&form("/tmp", "hardlink"), "").unwrap_err();
        assert!(err.contains("media root"), "{err}");
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let err = validate_start(&form("  ", "hardlink"), media.to_str().unwrap()).unwrap_err();
        assert!(err.contains("Enter the folder"), "{err}");
        let err = validate_start(&form("relative/path", "hardlink"), media.to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn validate_rejects_missing_dir_and_media_root_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let inside = media.join("Show");
        std::fs::create_dir_all(&inside).unwrap();
        let media_s = media.to_str().unwrap();

        let missing = tmp.path().join("missing");
        let err = validate_start(&form(missing.to_str().unwrap(), "copy"), media_s).unwrap_err();
        assert!(err.contains("not a folder"), "{err}");

        let err = validate_start(&form(inside.to_str().unwrap(), "copy"), media_s).unwrap_err();
        assert!(err.contains("inside your Ryokan media root"), "{err}");
        let err = validate_start(&form(media_s, "copy"), media_s).unwrap_err();
        assert!(err.contains("inside your Ryokan media root"), "{err}");
    }

    #[test]
    fn validate_builds_session_with_mode_and_toggles() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let s = validate_start(
            &form(src.to_str().unwrap(), "move"),
            media.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(s.mode, ImportMode::Move);
        assert!(s.follow_symlinks);
        assert!(!s.include_hidden);
        assert!(session::is_valid_id(&s.id));
        assert_eq!(s.status, SessionStatus::Scanning);

        // Empty mode defaults to hardlink; garbage is rejected.
        let s = validate_start(&form(src.to_str().unwrap(), ""), media.to_str().unwrap()).unwrap();
        assert_eq!(s.mode, ImportMode::Hardlink);
        let err = validate_start(
            &form(src.to_str().unwrap(), "symlink"),
            media.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("Unknown import mode"), "{err}");
    }

    #[test]
    fn format_display_maps_al_enums() {
        assert_eq!(format_display("TV"), "TV");
        assert_eq!(format_display("TV_SHORT"), "TV Short");
        assert_eq!(format_display("MOVIE"), "Movie");
        assert_eq!(format_display(""), "TBA");
    }
}

/// Handler-level coverage through a hand-built router (the shared
/// `handler_router` only mounts `/api/health`). No auth layer: these
/// exercise the handlers, not `require_auth`. The one background job
/// test uses a fixture with no series hint so the pipeline finishes
/// without touching AniList.
mod router_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::super::*;
    use crate::models::config::{Config, save_config};
    use crate::services::anilist::AnimeEntry;
    use crate::services::manual_import::parse::TitleSource;
    use crate::services::manual_import::{CandidateFile, ExistingSeries, SeriesGroup};
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/system/import", get(page).post(start))
            .route(
                "/system/import/{session_id}/group/{idx}",
                post(group_action),
            )
            .route(
                "/system/import/{session_id}/group/{idx}/candidates",
                get(picker_candidates),
            )
            .route("/system/import/{session_id}/discard", post(discard))
            .with_state(state)
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn get_page(app: &Router, uri: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_text(resp).await)
    }

    async fn post_form(
        app: &Router,
        uri: &str,
        form: &str,
        htmx: bool,
    ) -> axum::response::Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            req = req.header("HX-Request", "true");
        }
        app.clone()
            .oneshot(req.body(Body::from(form.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn seed_media_root(db: &sqlx::SqlitePool, media_root: &str) {
        let cfg = Config {
            media_root: media_root.to_string(),
            ..Config::default()
        };
        save_config(db, &cfg).await.unwrap();
    }

    fn entry(id: i64, english: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("{english} Romaji"),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    fn file(name: &str, episode: Option<i32>) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from(format!("/src/Show/{name}")),
            rel_path: format!("Show/{name}"),
            file_name: name.to_string(),
            size_bytes: 1024,
            parsed_title: Some("Show".into()),
            title_source: TitleSource::Filename,
            season: None,
            episode,
            year: None,
            group: None,
            quality_label: "WEB-1080p".into(),
            selected: true,
            episode_count: 1,
            source_episode: None,
        }
    }

    /// A Ready session with one matched group and two candidates.
    fn ready_session(state: &AppState) -> String {
        let mut s = ImportSession::new(
            session::mint_id(),
            PathBuf::from("/src"),
            ImportMode::Hardlink,
            false,
            false,
        );
        s.status = SessionStatus::Ready;
        s.stats.files = 2;
        s.groups.push(SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files: vec![
                file("Show - 01.mkv", Some(1)),
                file("Show - 02.mkv", Some(2)),
            ],
            candidates: vec![entry(100, "Show"), entry(101, "Show Alternative")],
            pick: Some(0),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        });
        let id = s.id.clone();
        session::insert(&state.import_sessions, s);
        id
    }

    #[tokio::test]
    async fn start_form_renders_and_warns_without_media_root() {
        let db = in_memory_pool().await;
        let app = router(build_test_app_state(db, None));
        let (status, body) = get_page(&app, "/system/import").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Import an existing library"));
        assert!(body.contains("Scan folder"));
        // Rendered inside the System shell with the sidebar entry active.
        assert!(body.contains("tabbed-sidebar"), "{body}");
        assert!(
            body.contains("href=\"/system/import\" class=\"tabbed-side-tab active\""),
            "{body}"
        );
        assert!(
            body.contains("No media root set"),
            "warns when media root is unset"
        );
        assert!(
            body.contains("disabled"),
            "scan button disabled without a media root"
        );
    }

    #[tokio::test]
    async fn start_page_lists_live_sessions() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let state_ref = state.clone();
        let app = router(state);
        let (_, body) = get_page(&app, "/system/import").await;
        assert!(body.contains("Recent scans"), "{body}");
        assert!(
            body.contains(&format!("/system/import?session={id}")),
            "{body}"
        );
        // A ready scan is listed without a status label; only a
        // running / failed one says so.
        assert!(!body.contains("Ready to review"), "{body}");
        session::update(&state_ref.import_sessions, &id, |s| {
            s.status = SessionStatus::Importing
        });
        let (_, body) = get_page(&app, "/system/import").await;
        assert!(body.contains("import-recent-meta\">Importing"), "{body}");
    }

    #[tokio::test]
    async fn unknown_or_malformed_session_shows_expired() {
        let db = in_memory_pool().await;
        let app = router(build_test_app_state(db, None));
        let (status, body) = get_page(&app, "/system/import?session=nope").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("That preview has expired"));
        let fresh = session::mint_id();
        let (_, body) = get_page(&app, &format!("/system/import?session={fresh}")).await;
        assert!(body.contains("That preview has expired"));
    }

    #[tokio::test]
    async fn start_with_bad_path_rerenders_form_with_error_and_echo() {
        let db = in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        seed_media_root(&db, media.to_str().unwrap()).await;
        let app = router(build_test_app_state(db, None));

        let resp = post_form(
            &app,
            "/system/import",
            "path=%2Fdefinitely%2Fmissing%2Fdir&mode=copy&include_hidden=1",
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("is not a folder Ryokan can read"), "{body}");
        assert!(
            body.contains("value=\"/definitely/missing/dir\""),
            "echoes the typed path"
        );
        assert!(
            body.contains("<option value=\"copy\" selected>"),
            "echoes the chosen mode"
        );
        assert!(
            body.contains("name=\"include_hidden\" value=\"1\" checked"),
            "echoes the hidden toggle"
        );
    }

    #[tokio::test]
    async fn start_runs_pipeline_and_page_reaches_ready() {
        let db = in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&media).unwrap();
        // No series hint anywhere, so the preview needs no AniList
        // call: one unmatched file plus a non-video sidecar.
        std::fs::create_dir_all(src.join("Season 01")).unwrap();
        std::fs::write(src.join("Season 01/01.mkv"), b"xx").unwrap();
        std::fs::write(src.join("Season 01/01.nfo"), b"x").unwrap();
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let app = router(state.clone());

        // HTMX caller gets 200 + HX-Redirect; plain caller gets 303.
        let form = format!(
            "path={}&mode=hardlink",
            urlencoding::encode(src.to_str().unwrap())
        );
        let resp = post_form(&app, "/system/import", &form, true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let location = resp
            .headers()
            .get("HX-Redirect")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            location.starts_with("/system/import?session="),
            "{location}"
        );
        let session_id = location
            .trim_start_matches("/system/import?session=")
            .to_string();
        assert!(session::is_valid_id(&session_id));

        let resp = post_form(&app, "/system/import", &form, false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);

        // The job runs in the background; poll the page until Ready.
        let mut body = String::new();
        for _ in 0..100 {
            let (status, b) = get_page(&app, &location).await;
            assert_eq!(status, StatusCode::OK);
            body = b;
            if !body.contains("import-scanning") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !body.contains("import-scanning"),
            "preview never left the scanning state"
        );
        assert!(body.contains("Files with no series hint"), "{body}");
        assert!(body.contains("Season 01/01.mkv"));
        assert!(body.contains(">E01<"));
        assert!(body.contains("Discard preview"));
        let s = session::get(&state.import_sessions, &session_id).unwrap();
        assert_eq!(s.status, SessionStatus::Ready);
        assert_eq!(s.stats.files, 1);
        assert_eq!(s.stats.skipped_non_video, 1);
        assert_eq!(s.unmatched_files.len(), 1);
        assert!(s.groups.is_empty());
    }

    #[tokio::test]
    async fn ready_page_renders_group_card_and_summary() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state);
        let (status, body) = get_page(&app, &format!("/system/import?session={id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("import-group-new"), "new-series card");
        assert!(body.contains("Show Alternative"), "alternative offered");
        assert!(
            body.contains("Show/Season 01/Show - 01.mkv"),
            "projected destination"
        );
        assert!(
            body.contains("<strong>1</strong> new"),
            "summary counts one new series"
        );
        assert!(body.contains("Import 2 files"), "{body}");
    }

    #[tokio::test]
    async fn group_actions_swap_card_via_htmx_and_redirect_otherwise() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state.clone());
        let uri = format!("/system/import/{id}/group/0");

        // Skip: card re-renders as skipped, files gone from the table,
        // and the summary strip + confirm bar come along out-of-band
        // with the new totals (nothing left to import).
        let resp = post_form(&app, &uri, "action=skip", true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("import-group-skipped"), "{body}");
        assert!(body.contains("Include"), "offers to include again");
        assert!(
            !body.contains("import-files"),
            "skipped card hides the file table"
        );
        assert!(
            body.contains("id=\"import-summary\" hx-swap-oob=\"true\""),
            "{body}"
        );
        assert!(
            body.contains("id=\"import-confirm\" hx-swap-oob=\"true\""),
            "{body}"
        );
        assert!(body.contains("<strong>1</strong> skipped"), "{body}");
        assert!(body.contains("Nothing to import"), "{body}");

        // Unskip via plain POST: 303 back to the card anchor.
        let resp = post_form(&app, &uri, "action=unskip", false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(loc, format!("/system/import?session={id}#import-group-0"));
        assert!(!session::get(&state.import_sessions, &id).unwrap().groups[0].skipped);

        // Toggle a file off: row shows Excluded and counts drop.
        let body = body_text(post_form(&app, &uri, "action=toggle_file&file=1", true).await).await;
        assert!(body.contains("import-file-deselected"), "{body}");
        assert!(
            body.contains("<strong>1</strong> to import, 1 excluded"),
            "{body}"
        );
        let body = body_text(post_form(&app, &uri, "action=select_all", true).await).await;
        assert!(!body.contains("import-file-deselected"));
        let body = body_text(post_form(&app, &uri, "action=select_none", true).await).await;
        assert!(
            body.contains("<strong>0</strong> to import, 2 excluded"),
            "{body}"
        );

        // Pick the alternative, then none.
        let body = body_text(post_form(&app, &uri, "action=pick&candidate=1", true).await).await;
        assert!(body.contains("anilist.co/anime/101"), "{body}");
        assert_eq!(
            session::get(&state.import_sessions, &id).unwrap().groups[0].pick,
            Some(1)
        );
        let body = body_text(post_form(&app, &uri, "action=pick&candidate=9", true).await).await;
        assert!(body.contains("Unknown candidate"), "inline error: {body}");
        let body = body_text(post_form(&app, &uri, "action=unpick", true).await).await;
        assert!(body.contains("No match found"), "{body}");
        assert!(body.contains("import-group-nomatch"));

        // Empty re-search is refused inline without touching AL.
        let body = body_text(post_form(&app, &uri, "action=research&query=+", true).await).await;
        assert!(body.contains("Type a title to search for"), "{body}");
    }

    #[tokio::test]
    async fn picker_lists_ranked_candidates_and_pick_id_switches() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state.clone());

        // The card embeds the picker with the current pick marked.
        let (_, body) = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("import-picker"), "{body}");
        assert!(body.contains("Change match"));
        assert!(
            body.contains("disabled aria-current=\"true\">Current<"),
            "{body}"
        );
        assert!(
            body.contains("name=\"id\" value=\"101\""),
            "alternative offered with a Use button"
        );

        // The candidates endpoint serves the same list on its own.
        let (status, body) =
            get_page(&app, &format!("/system/import/{id}/group/0/candidates")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Show Alternative"), "{body}");
        assert!(body.contains(">Use<"));
        assert!(!body.contains("tabbed-page"), "a fragment, not a page");

        // pick_id switches the card; an unknown id is refused inline.
        let uri = format!("/system/import/{id}/group/0");
        let body = body_text(post_form(&app, &uri, "action=pick_id&id=101", true).await).await;
        assert!(body.contains("anilist.co/anime/101"), "{body}");
        assert_eq!(
            session::get(&state.import_sessions, &id).unwrap().groups[0].pick,
            Some(1)
        );
        let body = body_text(post_form(&app, &uri, "action=pick_id&id=999", true).await).await;
        assert!(body.contains("Unknown candidate"), "{body}");

        // A no-match card opens the picker by itself.
        let body = body_text(post_form(&app, &uri, "action=unpick", true).await).await;
        assert!(
            body.contains("<details class=\"import-picker\" open>"),
            "{body}"
        );
        assert!(body.contains("Find a match"));
    }

    #[tokio::test]
    async fn merge_card_marks_existing_series_and_present_episode() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let series_id = seed_series(&db, 100, "Show On Disk").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        // Attach the library row the way the resolver would.
        let mut tags = HashMap::new();
        tags.insert(
            1,
            manual_import::ExistingTag {
                quality_label: "BD-1080p".into(),
                state: "completed".into(),
                manual_override: false,
                classification: crate::services::source::classify_release_sync(
                    "[G] Show - 01 [BD 1080p].mkv",
                    None,
                ),
            },
        );
        session::update(&state.import_sessions, &id, |s| {
            s.groups[0].existing = Some(ExistingSeries {
                id: series_id,
                anilist_id: 100,
                title: "Show On Disk".into(),
                title_romaji: String::new(),
                title_english: String::new(),
                title_native: String::new(),
                season_year: None,
                folder_name: "Show On Disk".into(),
                tags,
            });
        });
        let app = router(state);
        let (_, body) = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("import-group-merge"), "{body}");
        assert!(body.contains("In your library as"));
        assert!(body.contains("/series/100"));
        assert!(
            body.contains("import-status-present"),
            "episode 1 already have"
        );
        assert!(body.contains("have BD-1080p"));
        assert!(body.contains("Show On Disk/Season 01/Show - 02.mkv"));
        assert!(body.contains("<strong>1</strong> already in library"));
    }

    #[tokio::test]
    async fn expired_session_actions_redirect_and_discard_removes() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state.clone());

        let ghost = session::mint_id();
        let resp = post_form(
            &app,
            &format!("/system/import/{ghost}/group/0"),
            "action=skip",
            true,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key("HX-Redirect"));

        let resp = post_form(&app, &format!("/system/import/{id}/discard"), "", false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "/system/import"
        );
        assert!(session::get(&state.import_sessions, &id).is_none());
    }
}

/// Confirm / cancel / report coverage. The confirm test runs the real
/// job: AniList is pointed at a closed local port so the metadata
/// hydration path executes and fails fast without the network.
mod import_router_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::{get, post};
    use std::sync::LazyLock;
    use tower::ServiceExt;

    use super::super::*;
    use crate::models::config::{Config, save_config};
    use crate::services::anilist::{self, AnimeEntry};
    use crate::services::manual_import::{CandidateFile, SeriesGroup, parse::TitleSource};
    use crate::test_support::{build_test_app_state, in_memory_pool};

    /// Serializes the env-var flip across tests in this module (nextest
    /// runs one process per test, so this only matters under plain
    /// `cargo test`).
    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/system/import", get(page).post(start))
            .route("/system/import/{session_id}/confirm", post(confirm))
            .route("/system/import/{session_id}/cancel", post(cancel))
            .route("/system/import/{session_id}/discard", post(discard))
            .with_state(state)
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn get_page(app: &Router, uri: &str) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_text(resp).await
    }

    async fn post_empty(app: &Router, uri: &str, htmx: bool) -> axum::response::Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            req = req.header("HX-Request", "true");
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn entry(id: i64, english: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("{english} Romaji"),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    /// Real files under a tempdir so the job has something to link.
    fn session_with_files(state: &AppState, root: &std::path::Path, selected: bool) -> String {
        let mut files = Vec::new();
        for (i, name) in ["Show - 01.mkv", "Show - 02.mkv"].iter().enumerate() {
            let path = root.join("Show").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"xx").unwrap();
            files.push(CandidateFile {
                path,
                rel_path: format!("Show/{name}"),
                file_name: name.to_string(),
                size_bytes: 2,
                parsed_title: Some("Show".into()),
                title_source: TitleSource::Filename,
                season: None,
                episode: Some(i as i32 + 1),
                year: None,
                group: None,
                quality_label: "Unknown".into(),
                selected,
                episode_count: 1,
                source_episode: None,
            });
        }
        let mut s = ImportSession::new(
            session::mint_id(),
            root.to_path_buf(),
            ImportMode::Hardlink,
            false,
            false,
        );
        s.status = SessionStatus::Ready;
        s.stats.files = 2;
        s.groups.push(SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files,
            candidates: vec![entry(100, "Show")],
            pick: Some(0),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        });
        let id = s.id.clone();
        session::insert(&state.import_sessions, s);
        id
    }

    async fn seed_media_root(db: &sqlx::SqlitePool, media_root: &str) {
        let cfg = Config {
            media_root: media_root.to_string(),
            ..Config::default()
        };
        save_config(db, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn confirm_runs_the_job_and_page_shows_the_report() {
        let _gate = ENV_LOCK.lock().await;
        // Closed port for every metadata provider: the hydration path
        // runs, fails fast (AL, then the Jikan and Kitsu fallbacks),
        // and the import carries on the way it would through an
        // outage. Nothing here reaches the network.
        anilist::reset_state_for_tests();
        unsafe {
            std::env::set_var("RYOKAN_ANILIST_API_BASE", "http://127.0.0.1:9");
            std::env::set_var("JIKAN_API_BASE", "http://127.0.0.1:9");
            std::env::set_var("RYOKAN_KITSU_API_BASE", "http://127.0.0.1:9");
        }
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let db = in_memory_pool().await;
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());

        // Ready page carries the confirm bar.
        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("Import 2 files"), "{body}");
        assert!(body.contains("data-ryokan-confirm-title"));

        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("HX-Redirect").unwrap().to_str().unwrap(),
            format!("/system/import?session={id}")
        );

        let mut body = String::new();
        for _ in 0..1500 {
            body = get_page(&app, &format!("/system/import?session={id}")).await;
            if !body.contains("import-importing") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(body.contains("Import complete"), "{body}");
        assert!(body.contains("import-status-import\">Created"), "{body}");
        assert!(body.contains("<strong>2</strong> imported"), "{body}");
        assert!(media.join("Show/Season 01/Show - 01.mkv").exists());
        let row = series::get_by_anilist_id(&state.db, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.folder_name, "Show");

        // A second confirm on the finished session is refused (not
        // Ready) and just routes back to the page.
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        unsafe {
            std::env::remove_var("RYOKAN_ANILIST_API_BASE");
            std::env::remove_var("JIKAN_API_BASE");
            std::env::remove_var("RYOKAN_KITSU_API_BASE");
        }
        anilist::reset_state_for_tests();
    }

    #[tokio::test]
    async fn confirm_refuses_when_nothing_would_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let db = in_memory_pool().await;
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), false);
        let app = router(state.clone());

        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("Nothing to import"), "{body}");
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("HX-Redirect").is_none());
        let body = body_text(resp).await;
        assert!(
            body.contains("Nothing to import: every file is excluded"),
            "{body}"
        );
        assert_eq!(
            session::get(&state.import_sessions, &id).unwrap().status,
            SessionStatus::Ready
        );
    }

    #[tokio::test]
    async fn confirm_refuses_without_media_root() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        let body = body_text(resp).await;
        assert!(
            body.contains("Set a media root under Settings before importing"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn malformed_session_ids_redirect_to_the_start_page() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let _id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state);
        // A percent-encoded newline decodes to a control character;
        // interpolated into a redirect it would panic the handler.
        for uri in [
            "/system/import/%0A/confirm",
            "/system/import/%0A/cancel",
            "/system/import/%0A/discard",
        ] {
            let resp = post_empty(&app, uri, false).await;
            assert_eq!(resp.status(), StatusCode::SEE_OTHER, "{uri}");
            assert_eq!(
                resp.headers()
                    .get(header::LOCATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "/system/import",
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn refused_confirm_renders_into_the_bar_under_htmx() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let db = in_memory_pool().await;
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());
        // Hold the import lock so the confirm is refused with
        // "already running"; the HTMX reply is the confirm bar itself
        // (the form targets it), carrying the reason.
        let guard = manual_import::import::IMPORT_LOCK.lock().await;
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        drop(guard);
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("HX-Redirect").is_none());
        let body = body_text(resp).await;
        assert!(body.contains("id=\"import-confirm\""), "{body}");
        assert!(body.contains("already running"), "{body}");
        assert!(!body.contains("tabbed-page"), "a fragment, not the page");
    }

    #[tokio::test]
    async fn cancel_flags_only_a_running_import() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());

        let resp = post_empty(&app, &format!("/system/import/{id}/cancel"), false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(
            !session::get(&state.import_sessions, &id)
                .unwrap()
                .cancel
                .load(std::sync::atomic::Ordering::Relaxed),
            "Ready session: no-op"
        );

        session::update(&state.import_sessions, &id, |s| {
            s.status = SessionStatus::Importing
        });
        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("import-importing"), "{body}");
        assert!(
            body.contains(&format!("data-import-progress=\"{id}-import\"")),
            "{body}"
        );
        let resp = post_empty(&app, &format!("/system/import/{id}/cancel"), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            session::get(&state.import_sessions, &id)
                .unwrap()
                .cancel
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[tokio::test]
    async fn done_page_renders_the_report() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let report = ImportReport {
            series_created: 1,
            files_written: 1,
            files_failed: 1,
            bytes_written: 2048,
            groups: vec![manual_import::GroupReport {
                parsed_title: "Show".into(),
                series_title: "Show".into(),
                anilist_id: 100,
                series_id: Some(7),
                folder_name: "Show".into(),
                created: true,
                written: 1,
                errors: vec!["Show/Show - 02.mkv: permission denied".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        session::update(&state.import_sessions, &id, |s| {
            s.status = SessionStatus::Done(Box::new(report));
        });
        let app = router(state);
        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("Import finished with errors"), "{body}");
        assert!(body.contains("2.0 KiB"), "{body}");
        assert!(body.contains("/series/100"));
        assert!(body.contains("permission denied"));
        assert!(body.contains("Import another folder"));
    }
}
