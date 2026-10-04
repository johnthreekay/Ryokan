//! First-run library step at `/setup/library`.
//!
//! `/setup` creates the account and sends the new user here. The step
//! asks where finished episodes go and, optionally, for a Jellyfin
//! server, and saving turns post-processing on. Post-processing is off
//! by default, so a user who followed setup and the quick start used
//! to get downloads that never reached the library. The step can be
//! skipped (no media library yet); Settings → General holds the same
//! options afterwards, and the page keeps working for a later visit.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::handlers::secret_field;
use crate::models::config;
use crate::models::log::LogCategory;
use crate::services::jellyfin::JellyfinClient;
use crate::services::logger;

use super::CONFIG_WRITE_LOCK;

#[derive(Template)]
#[template(path = "setup_library.html")]
struct SetupLibraryTemplate {
    page: &'static str,
    title_language: String,
    media_root: String,
    post_processing_mode: String,
    jellyfin_url: String,
    jellyfin_key_saved: bool,
    error: Option<String>,
    /// The settings are stored (only Jellyfin failed), so the way out is
    /// Continue rather than Skip.
    saved: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetupLibraryForm {
    #[serde(default)]
    pub media_root: String,
    #[serde(default)]
    pub post_processing_mode: String,
    #[serde(default)]
    pub jellyfin_url: String,
    #[serde(default)]
    pub jellyfin_api_key: String,
}

fn render(cfg: &config::Config, error: Option<String>, saved: bool) -> Response {
    let template = SetupLibraryTemplate {
        // A setup step, not a nav section: no nav item lights up.
        page: "setup",
        title_language: cfg.title_language.clone(),
        media_root: cfg.media_root.clone(),
        post_processing_mode: cfg.post_processing_mode.clone(),
        jellyfin_url: cfg.jellyfin_url.clone(),
        jellyfin_key_saved: !cfg.jellyfin_api_key.is_empty(),
        error,
        saved,
    };
    Html(template.render().unwrap_or_default()).into_response()
}

pub async fn setup_library_page(State(state): State<AppState>) -> Response {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    render(&cfg, None, false)
}

/// The media root must be a folder Ryokan can write to. Settings →
/// General only warns, but here the user is following setup and a path
/// that only exists on the host (the usual Docker mistake) would turn
/// on post-processing that fails on every import.
async fn check_media_root(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(
            "Enter the folder Ryokan should place finished episodes in, or skip this step."
                .to_string(),
        );
    }
    let root = std::path::Path::new(path);
    if !root.is_absolute() {
        return Err(format!(
            "Use a full path, such as /data/media/anime, not {path}."
        ));
    }
    if !tokio::fs::metadata(root)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(format!(
            "Ryokan can't find the folder {path}. In Docker this is a path inside the container, on a mounted volume."
        ));
    }
    // A random name: the PID is always 1 in Docker, so a probe a crash
    // left behind made `create_new` fail on every later attempt.
    let probe = root.join(format!(
        ".ryokan-write-test-{}",
        hex::encode(rand::random::<[u8; 8]>())
    ));
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .await
    {
        Ok(_) => {
            let _ = tokio::fs::remove_file(&probe).await;
            Ok(())
        }
        Err(e) => Err(format!(
            "Ryokan can't write to {path} ({e}). In Docker, the PUID and PGID the container runs as must own it."
        )),
    }
}

pub async fn setup_library_submit(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<SetupLibraryForm>,
) -> Response {
    let media_root = form.media_root.trim().trim_end_matches('/').to_string();
    let post_processing_mode = match form.post_processing_mode.as_str() {
        "copy" => "copy",
        "move" => "move",
        _ => "hardlink",
    }
    .to_string();
    let jellyfin_url = form.jellyfin_url.trim().to_string();

    let guard = CONFIG_WRITE_LOCK.lock().await;
    // The save below writes the whole row, so a read that failed must
    // stop here: defaults would replace every other setting.
    let existing = match config::get_config(&state.db).await {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(e) => {
            let typed = config::Config {
                media_root,
                post_processing_mode,
                jellyfin_url,
                ..config::Config::default()
            };
            return render(
                &typed,
                Some(format!(
                    "Couldn't read the saved settings, so nothing was saved ({e}). Try again in a moment."
                )),
                false,
            );
        }
    };
    // Re-render with what the user typed, never the stored key.
    let typed = config::Config {
        media_root: media_root.clone(),
        post_processing_mode: post_processing_mode.clone(),
        jellyfin_url: jellyfin_url.clone(),
        ..existing.clone()
    };
    if let Err(e) = check_media_root(&media_root).await {
        return render(&typed, Some(e), false);
    }
    let jellyfin_api_key = match secret_field::resolve(
        form.jellyfin_api_key.trim(),
        Some((
            existing.jellyfin_api_key.as_str(),
            existing.jellyfin_url.as_str(),
        )),
        &jellyfin_url,
    ) {
        Ok(key) => key,
        Err(e) => return render(&typed, Some(format!("Jellyfin: {e}")), false),
    };
    let cfg = config::Config {
        media_root,
        post_processing_enabled: true,
        post_processing_mode,
        jellyfin_url,
        jellyfin_api_key,
        ..existing
    };
    if let Err(e) = config::save_config(&state.db, &cfg).await {
        return render(
            &typed,
            Some(format!("Couldn't save the settings: {e}")),
            false,
        );
    }
    drop(guard);

    logger::info(
        &state.db,
        LogCategory::System,
        "Library set up from first-run setup",
        &format!(
            "media_root={}, mode={}, jellyfin={}",
            cfg.media_root,
            cfg.post_processing_mode,
            if cfg.jellyfin_url.is_empty() {
                "off"
            } else {
                "on"
            }
        ),
    )
    .await;

    // Same Jellyfin side effect as the Integrations save: the live
    // client is the tested one or none.
    if cfg.jellyfin_url.is_empty() || cfg.jellyfin_api_key.is_empty() {
        *state.jellyfin.write().await = None;
    } else {
        let client = JellyfinClient::new(&cfg.jellyfin_url, &cfg.jellyfin_api_key);
        match client.test_connection().await {
            Ok(_) => *state.jellyfin.write().await = Some(client),
            Err(e) => {
                logger::error(&state.db, LogCategory::Jellyfin, "Connection failed", &e).await;
                *state.jellyfin.write().await = None;
                return render(
                    &cfg,
                    Some(format!(
                        "Saved, but Jellyfin didn't connect ({e}). Fix it here, or continue and change it later in Settings → Connections."
                    )),
                    true,
                );
            }
        }
    }

    htmx_aware_redirect(is_htmx, "/")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::models::config;
    use crate::test_support::{handler_router, in_memory_pool, logged_in_session};

    async fn seeded() -> (sqlx::SqlitePool, crate::AppState, String) {
        let db = in_memory_pool().await;
        config::save_config(&db, &config::Config::default())
            .await
            .unwrap();
        let (state, cookie) = logged_in_session(&db).await;
        (db, state, cookie)
    }

    async fn post(
        state: crate::AppState,
        cookie: &str,
        body: String,
    ) -> (StatusCode, String, String) {
        let response = handler_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/setup/library")
                    .header(header::HOST, "ryokan.local")
                    .header(header::ORIGIN, "http://ryokan.local")
                    .header(header::COOKIE, cookie)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            location,
            String::from_utf8_lossy(&bytes).to_string(),
        )
    }

    fn form(fields: &[(&str, &str)]) -> String {
        fields
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    #[tokio::test]
    async fn saving_sets_the_media_root_and_turns_post_processing_on() {
        let (db, state, cookie) = seeded().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_str().unwrap().to_string();
        let (status, location, _) = post(
            state,
            &cookie,
            form(&[
                ("media_root", &format!("{path}/")),
                ("post_processing_mode", "copy"),
                ("jellyfin_url", ""),
                ("jellyfin_api_key", ""),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/");
        let cfg = config::get_config(&db).await.unwrap().unwrap();
        assert_eq!(cfg.media_root, path, "trailing slash trimmed");
        assert!(cfg.post_processing_enabled);
        assert_eq!(cfg.post_processing_mode, "copy");
        assert!(
            std::fs::read_dir(root.path()).unwrap().next().is_none(),
            "the write probe is cleaned up"
        );
    }

    #[tokio::test]
    async fn a_folder_ryokan_cannot_see_is_refused_and_nothing_is_saved() {
        // The usual Docker mistake: a host path the container doesn't have.
        for (path, says) in [
            ("/srv/no-such-folder-for-ryokan", "find the folder"),
            ("media/anime", "Use a full path"),
        ] {
            let (db, state, cookie) = seeded().await;
            let (status, _, page) = post(state, &cookie, form(&[("media_root", path)])).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(page.contains(says), "{path}: {page}");
            let cfg = config::get_config(&db).await.unwrap().unwrap();
            assert!(!cfg.post_processing_enabled, "{path}");
            assert!(cfg.media_root.is_empty(), "{path}");
        }
    }

    #[tokio::test]
    async fn a_write_probe_left_by_a_crash_does_not_block_the_step() {
        // The probe was named after the PID, which is always 1 in
        // Docker, so a stranded one refused every later attempt.
        let (_db, state, cookie) = seeded().await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path()
                .join(format!(".ryokan-write-test-{}", std::process::id())),
            b"",
        )
        .unwrap();
        let (status, location, _) = post(
            state,
            &cookie,
            form(&[("media_root", root.path().to_str().unwrap())]),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/");
    }

    #[tokio::test]
    async fn a_failed_settings_read_saves_nothing() {
        // The read fell back to defaults and the save wrote them over
        // the whole row.
        let (db, state, cookie) = seeded().await;
        sqlx::query(
            "UPDATE config SET jellyfin_url = 'http://kept:8096', rss_interval_minutes = 'unreadable'",
        )
        .execute(&db)
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let (status, _, page) = post(
            state,
            &cookie,
            form(&[("media_root", root.path().to_str().unwrap())]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("read the saved settings"), "{page}");
        let (jellyfin_url, enabled): (String, bool) =
            sqlx::query_as("SELECT jellyfin_url, post_processing_enabled FROM config")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(jellyfin_url, "http://kept:8096");
        assert!(!enabled);
    }

    #[tokio::test]
    async fn a_jellyfin_that_does_not_answer_still_saves_and_offers_continue() {
        let (db, state, cookie) = seeded().await;
        let root = tempfile::tempdir().unwrap();
        let (status, _, page) = post(
            state.clone(),
            &cookie,
            form(&[
                ("media_root", root.path().to_str().unwrap()),
                ("jellyfin_url", "http://127.0.0.1:1"),
                ("jellyfin_api_key", "typed-jellyfin-key"),
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Saved, but Jellyfin didn"), "{page}");
        assert!(page.contains(">Continue<"), "{page}");
        assert!(
            !page.contains("typed-jellyfin-key"),
            "the key never renders"
        );
        let cfg = config::get_config(&db).await.unwrap().unwrap();
        assert!(cfg.post_processing_enabled);
        assert_eq!(cfg.jellyfin_api_key, "typed-jellyfin-key");
        assert!(state.jellyfin.read().await.is_none(), "no untested client");
    }

    #[tokio::test]
    async fn the_page_renders_for_a_signed_in_user_without_the_stored_key() {
        let (db, state, cookie) = seeded().await;
        let cfg = config::Config {
            jellyfin_url: "http://jellyfin:8096".into(),
            jellyfin_api_key: "stored-jellyfin-key".into(),
            ..config::Config::default()
        };
        config::save_config(&db, &cfg).await.unwrap();
        let response = handler_router(state)
            .oneshot(
                Request::builder()
                    .uri("/setup/library")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let page = String::from_utf8_lossy(&bytes);
        assert!(page.contains("Set up your library"));
        assert!(page.contains("Skip for now"));
        assert!(page.contains("http://jellyfin:8096"));
        assert!(!page.contains("stored-jellyfin-key"));
    }
}
