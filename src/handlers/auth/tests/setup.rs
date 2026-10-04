//! First-run gate coverage — the `/setup` page and `login_page`'s
//! redirect-to-setup behavior before any user exists. Runs against
//! the actual handlers through `handler_router` so the middleware
//! ordering (setup-vs-login redirect chain) is exercised end-to-end.
//!
//! Design invariant: Ryokan never deletes the admin account. Once
//! `users_exist` flips true the login page renders the form; until
//! then every authed path redirects to `/setup` and `/login` itself
//! forwards to `/setup` so a clean-install browser lands on the
//! account-creation form instead of a login form it can't submit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use crate::test_support::{
    build_test_app_state, handler_router, in_memory_pool, logged_in_session,
};

/// Override the `users_exist` cache to false so the test reflects a
/// fresh-install state. The production cache flips true on the first
/// successful `has_users()` read; tests need to force it back to
/// false after calling `build_test_app_state` (which defaults it to
/// true so most tests don't have to invent an authenticated session).
fn fresh_install_state(db: sqlx::SqlitePool) -> crate::AppState {
    let mut state = build_test_app_state(db, None);
    state.users_exist = Arc::new(AtomicBool::new(false));
    state
}

// ─── /setup GET ────────────────────────────────────────────────────

#[tokio::test]
async fn setup_page_renders_before_any_user_exists() {
    let db = in_memory_pool().await;
    let state = fresh_install_state(db);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/html"),
        "setup page should render HTML"
    );
}

#[tokio::test]
async fn setup_page_redirects_to_login_when_users_exist() {
    let db = in_memory_pool().await;
    // Seed a user so `has_users` returns true.
    let (state, _cookie) = logged_in_session(&db).await;
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::SEE_OTHER | StatusCode::FOUND
    ));
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/login");
}

// ─── /login GET's pre-user redirect ───────────────────────────────

#[tokio::test]
async fn login_page_redirects_to_setup_before_any_user_exists() {
    let db = in_memory_pool().await;
    let state = fresh_install_state(db);
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::SEE_OTHER | StatusCode::FOUND
    ));
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/setup");
}

#[tokio::test]
async fn login_page_renders_when_users_exist() {
    let db = in_memory_pool().await;
    let (state, _cookie) = logged_in_session(&db).await;
    let app = handler_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// ─── users_exist atomic cache ─────────────────────────────────────

#[tokio::test]
async fn users_exist_atomic_promotes_on_first_protected_request() {
    // Fresh-install (users_exist=false) state, but seed a user
    // behind the cache so has_users() returns true when the
    // middleware consults the DB. The middleware should promote the
    // cache to true so subsequent calls skip the SELECT.
    let db = in_memory_pool().await;
    let state = fresh_install_state(db.clone());
    // Pre-create a user (bypassing `logged_in_session` because we
    // want the cache in its stale=false state and no cookie).
    crate::models::user::create_user(&db, "cache-test", "hunter2-cache-test")
        .await
        .unwrap();
    assert!(
        !state.users_exist.load(Ordering::Relaxed),
        "cache should start false"
    );

    // Hit a protected endpoint. The middleware sees users_exist=false,
    // falls through to the DB query which reports true, promotes the
    // cache, then falls through to the session check which redirects
    // the unauthenticated caller to /login (not /setup).
    let app = handler_router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/login", "should redirect to /login, not /setup");
    assert!(
        state.users_exist.load(Ordering::Relaxed),
        "cache should be promoted to true after successful has_users read"
    );
}

/// Regression: `/setup` POST must seed a default `config` row so the
/// per-tab subform handlers (settings_general_submit /
/// settings_quality_submit / settings_integrations_submit) don't bail
/// with "No config row found — run /setup first." the very first time
/// the user opens Settings → Connections after creating their account.
///
/// The pre-fix shape created the user + session and called it done;
/// the user landed on Settings → Connections, edited Jellyfin, hit
/// Save, and got an error message that contradicted the action they
/// had just completed. The seed runs from `setup_submit` directly
/// after `create_user` and is upsert-shaped so a re-run can't
/// clobber an already-saved config (defense in depth — `has_users`
/// gates the path above this anyway).
#[tokio::test]
async fn setup_submit_seeds_default_config_row() {
    let db = in_memory_pool().await;
    let state = fresh_install_state(db.clone());

    // Sanity check: no config row exists pre-setup.
    let pre = crate::models::config::get_config(&db).await.unwrap();
    assert!(
        pre.is_none(),
        "no config row should exist on a fresh install before /setup runs"
    );

    let app = handler_router(state);
    let body = "username=admin&password=hunter2-test-password&confirm=hunter2-test-password";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/setup")
                .header(header::HOST, "ryokan.local")
                .header(header::ORIGIN, "http://ryokan.local")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "setup POST should redirect after success"
    );

    // Post-condition: the config row exists. We don't pin specific
    // field values — `Config::default()` is the contract; this test
    // is the gate that enforces "row exists at all" so the subform
    // handlers can read it.
    let post = crate::models::config::get_config(&db).await.unwrap();
    assert!(
        post.is_some(),
        "config row should be seeded by setup_submit so subform saves don't bail"
    );
}

// ─── concurrent /setup submissions ───────────────────────────────

/// Regression: two `/setup` POSTs in flight at once used to both pass
/// the `has_users` gate during the bcrypt hash and both insert, so a
/// second admin with a different username slipped in next to the
/// first. `create_first_user` makes the emptiness check part of the
/// insert; exactly one submission wins and the other is sent to
/// `/login`, whichever way the two interleave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_setup_posts_create_exactly_one_account() {
    let db = in_memory_pool().await;
    let state = fresh_install_state(db.clone());
    let submit = |username: &'static str| {
        let app = handler_router(state.clone());
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/setup")
                        .header(header::HOST, "ryokan.local")
                        .header(header::ORIGIN, "http://ryokan.local")
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .body(Body::from(format!(
                            "username={username}&password=race-pw-1&confirm=race-pw-1"
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        }
    };

    let (a, b) = tokio::join!(submit("admin"), submit("intruder"));

    let users: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        users.0, 1,
        "only one setup submission may create an account"
    );
    let mut locations = [a, b];
    locations.sort();
    assert_eq!(
        locations,
        ["/".to_string(), "/login".to_string()],
        "the winner lands on the app, the loser on the login page"
    );
}

#[tokio::test]
async fn setup_refuses_passwords_bcrypt_would_mangle_or_that_are_trivial() {
    for (password, needle) in [("x", "at least"), (&*"p".repeat(73), "at most")] {
        let db = in_memory_pool().await;
        let app = handler_router(fresh_install_state(db.clone()));
        let body = format!("username=admin&password={password}&confirm={password}");
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/setup")
                    .header(header::HOST, "ryokan.local")
                    .header(header::ORIGIN, "http://ryokan.local")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "the form comes back");
        let html = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&html).contains(needle), "{needle}");
        let users: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(users.0, 0);
    }
}
