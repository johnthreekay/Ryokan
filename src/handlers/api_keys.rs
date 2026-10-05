//! API-key CRUD endpoints (issue #114).
//!
//! Surfaced through the Settings → API Keys tab. Cookie-auth gated
//! (the same `require_auth` middleware that wraps every other web-UI
//! endpoint). The plaintext comes back on `create` (the modal shows it
//! for copying) and again from `reveal`, which backs the keys list's
//! Show and Copy buttons. Both carry the site-wide `Cache-Control:
//! no-store` from `handlers::security_headers`, so the key never lands
//! in a browser's disk cache; don't give either a cache policy of its
//! own. The list itself never includes the plaintext.
//!
//! ## Wire shapes
//!
//! - `GET /api/api-keys` → list of `ApiKeyView` (no plaintext, no hash).
//! - `POST /api/api-keys` (form: name + scopes) → `CreatedKey { plaintext, view }`.
//! - `GET /api/api-keys/{id}/reveal` → `{ plaintext }`.
//! - `POST /api/api-keys/{id}/toggle` (form: enabled) → updated `ApiKeyView`.
//! - `POST /api/api-keys/{id}/delete` → 204.

use axum::{
    Form, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::api_key::{self, ApiKey};
use crate::models::log::LogCategory;
use crate::services::logger;

/// Shape returned from list / create / toggle endpoints. Never carries
/// the hash (internal). Pre-formatted display strings live alongside
/// the raw Unix timestamps so the JS render path doesn't have to
/// duplicate the chrono format and the cards don't flash a
/// `"Loading..."` placeholder while waiting for JS hydration.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ApiKeyView {
    pub id: i64,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub enabled: bool,
    /// `MMM DD, YYYY HH:MM` UTC. Mirror of `ApiKey::created_at_display`
    /// — the same string the server-side template renders, so the
    /// JS-rendered cards (post-create, post-toggle) match the
    /// server-rendered ones.
    pub created_display: String,
    /// Same shape, with a `"never"` fallback when the key has never
    /// been used.
    pub last_used_display: String,
}

impl From<ApiKey> for ApiKeyView {
    fn from(k: ApiKey) -> Self {
        ApiKeyView {
            id: k.id,
            name: k.name.clone(),
            scopes: k.scopes.clone(),
            created_at: k.created_at,
            last_used_at: k.last_used_at,
            enabled: k.enabled,
            created_display: k.created_at_display(),
            last_used_display: k.last_used_display(),
        }
    }
}

/// Response from `POST /api/api-keys`. The `plaintext` field is the
/// only place the unhashed key is ever surfaced; subsequent reads
/// only get `view`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreatedKey {
    pub plaintext: String,
    pub view: ApiKeyView,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateApiKeyForm {
    pub name: String,
    /// Comma-separated scope strings (matches what the create-key
    /// modal's checkbox group serializes — checkboxes with the same
    /// `name` attribute are joined with `,` by the form helper). An
    /// empty `scopes` field returns an error rather than silently
    /// creating a useless key.
    #[serde(default)]
    pub scopes: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ToggleApiKeyForm {
    pub enabled: bool,
}

/// `GET /api/api-keys` — list every key in newest-first order.
#[utoipa::path(
    get,
    path = "/api/api-keys",
    tag = "API Keys",
    summary = "List API keys",
    responses((status = 200, description = "Newest-first list", body = Vec<ApiKeyView>)),
)]
pub async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<ApiKeyView>>, (StatusCode, String)> {
    let rows = api_key::list(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(rows.into_iter().map(ApiKeyView::from).collect()))
}

/// `POST /api/api-keys` — mint a new key. The response carries the
/// plaintext exactly once; callers must show it to the user
/// immediately (the plaintext can never be recovered after this
/// response).
#[utoipa::path(
    post,
    path = "/api/api-keys",
    tag = "API Keys",
    summary = "Create an API key",
    request_body = CreateApiKeyForm,
    responses(
        (status = 200, description = "Newly minted key (plaintext shown once)", body = CreatedKey),
        (status = 400, description = "Validation error (empty name, unknown scope)"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    Form(form): Form<CreateApiKeyForm>,
) -> Result<Json<CreatedKey>, (StatusCode, String)> {
    let scopes: Vec<String> = form
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if scopes.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Pick at least one scope".to_string(),
        ));
    }
    let (id, plaintext) = api_key::create(&state.db, &form.name, &scopes)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // Re-read the row so the response carries the canonical
    // created_at + enabled values (the SQL DEFAULT applied them on
    // insert; we don't have them in hand from `create`).
    let rows = api_key::list(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let row = rows.into_iter().find(|r| r.id == id).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Created row not found".to_string(),
        )
    })?;

    logger::info(
        &state.db,
        LogCategory::Auth,
        "API key created",
        &format!(
            "id={} name={} scopes={:?}",
            row.id,
            crate::handlers::auth::sanitize_for_log(&row.name),
            row.scopes,
        ),
    )
    .await;

    Ok(Json(CreatedKey {
        plaintext,
        view: ApiKeyView::from(row),
    }))
}

/// `POST /api/api-keys/{id}/toggle` — flip the `enabled` flag. Used by
/// the row-level toggle in the Settings tab. Returns the updated
/// view so the JS can re-render in place.
#[utoipa::path(
    post,
    path = "/api/api-keys/{id}/toggle",
    tag = "API Keys",
    summary = "Enable / disable an API key",
    request_body = ToggleApiKeyForm,
    params(("id" = i64, Path, description = "API key id")),
    responses(
        (status = 200, description = "Updated row", body = ApiKeyView),
        (status = 404, description = "Key not found"),
    ),
)]
pub async fn toggle(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<ToggleApiKeyForm>,
) -> Result<Json<ApiKeyView>, (StatusCode, String)> {
    api_key::set_enabled(&state.db, id, form.enabled)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows = api_key::list(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let row = rows
        .into_iter()
        .find(|r| r.id == id)
        .ok_or((StatusCode::NOT_FOUND, "Key not found".to_string()))?;
    logger::info(
        &state.db,
        LogCategory::Auth,
        if form.enabled {
            "API key enabled"
        } else {
            "API key disabled"
        },
        &format!(
            "id={} name={}",
            row.id,
            crate::handlers::auth::sanitize_for_log(&row.name)
        ),
    )
    .await;
    Ok(Json(ApiKeyView::from(row)))
}

/// `GET /api/api-keys/{id}/reveal` — return the plaintext for a
/// given key. Cookie-auth gated through the regular `require_auth`
/// middleware (Settings is web-UI only; scoped-key auth never
/// reaches this handler).
///
/// Three response shapes:
/// - 200 + `{plaintext}` — key found.
/// - 404 — key id doesn't exist OR the row was created under the
///   prior hash+encrypted schema and has no plaintext to surface
///   (user must rotate).
/// - 500 — DB error.
///
/// Not audit-logged. Reveal happens whenever the user clicks Show
/// or Copy on the keys list, which means a single Settings visit
/// can fire it many times — drowning the legitimate Auth-category
/// log filter (failed scoped-key attempts, key create / delete /
/// toggle events) in noise. The threat model doesn't pay back the
/// volume: the user is already cookie-authed and already has the
/// plaintext available client-side via the Show button. Other arr-
/// stack apps (Sonarr, Jellyfin) don't log key reads either.
#[utoipa::path(
    get,
    path = "/api/api-keys/{id}/reveal",
    tag = "API Keys",
    summary = "Reveal the plaintext for an API key",
    params(("id" = i64, Path, description = "API key id")),
    responses(
        (status = 200, description = "Plaintext key", body = serde_json::Value),
        (status = 404, description = "Key not found or has no recoverable plaintext"),
    ),
)]
pub async fn reveal(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let plaintext = api_key::get_plaintext(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let Some(plaintext) = plaintext else {
        return Err((
            StatusCode::NOT_FOUND,
            "Key not found, or its row predates the plaintext-storage rewrite. Delete and recreate the key to recover the plaintext.".to_string(),
        ));
    };
    Ok(Json(serde_json::json!({ "plaintext": plaintext })))
}

/// `POST /api/api-keys/{id}/delete` — drop a key. Idempotent. POST
/// (not DELETE) so the existing CSRF/auth middleware shape applies
/// uniformly — every state-mutating endpoint in this codebase is a
/// POST, and adding DELETE-method routing for one feature is more
/// churn than the REST purity is worth.
#[utoipa::path(
    post,
    path = "/api/api-keys/{id}/delete",
    tag = "API Keys",
    summary = "Delete an API key",
    params(("id" = i64, Path, description = "API key id")),
    responses((status = 204, description = "Deleted")),
)]
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Look up the name first so the audit log line is meaningful;
    // best-effort — a missing row still proceeds to the (idempotent)
    // delete below.
    let name_for_log = api_key::list(&state.db)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.id == id))
        .map(|r| r.name)
        .unwrap_or_else(|| format!("id={id}"));

    api_key::delete(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    logger::info(
        &state.db,
        LogCategory::Auth,
        "API key deleted",
        &format!(
            "id={} name={}",
            id,
            crate::handlers::auth::sanitize_for_log(&name_for_log)
        ),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    #[tokio::test]
    async fn create_returns_plaintext_and_view() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool.clone(), None);
        let form = CreateApiKeyForm {
            name: "Cal".into(),
            scopes: "calendar".into(),
        };
        let Json(created) = create(State(state), Form(form)).await.unwrap();
        assert_eq!(created.plaintext.len(), 64);
        assert_eq!(created.view.name, "Cal");
        assert_eq!(created.view.scopes, vec!["calendar".to_string()]);
        assert!(created.view.enabled);
    }

    #[tokio::test]
    async fn create_rejects_empty_scopes() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool, None);
        let form = CreateApiKeyForm {
            name: "X".into(),
            scopes: "".into(),
        };
        let err = create(State(state), Form(form)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("at least one scope"));
    }

    #[tokio::test]
    async fn create_parses_comma_separated_scopes() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool, None);
        let form = CreateApiKeyForm {
            name: "Multi".into(),
            scopes: "calendar,search".into(),
        };
        let Json(created) = create(State(state), Form(form)).await.unwrap();
        assert_eq!(created.view.scopes, vec!["calendar", "search"]);
    }

    #[tokio::test]
    async fn create_rejects_unknown_scope() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool, None);
        let form = CreateApiKeyForm {
            name: "Bad".into(),
            scopes: "does-not-exist".into(),
        };
        let err = create(State(state), Form(form)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_returns_created_keys() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool.clone(), None);
        let _ = create(
            State(state.clone()),
            Form(CreateApiKeyForm {
                name: "A".into(),
                scopes: "calendar".into(),
            }),
        )
        .await
        .unwrap();
        let Json(views) = list(State(state)).await.unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "A");
    }

    #[tokio::test]
    async fn toggle_flips_enabled_and_returns_view() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool.clone(), None);
        let Json(created) = create(
            State(state.clone()),
            Form(CreateApiKeyForm {
                name: "Cal".into(),
                scopes: "calendar".into(),
            }),
        )
        .await
        .unwrap();
        let Json(view) = toggle(
            State(state.clone()),
            Path(created.view.id),
            Form(ToggleApiKeyForm { enabled: false }),
        )
        .await
        .unwrap();
        assert!(!view.enabled);
        let Json(view) = toggle(
            State(state),
            Path(created.view.id),
            Form(ToggleApiKeyForm { enabled: true }),
        )
        .await
        .unwrap();
        assert!(view.enabled);
    }

    #[tokio::test]
    async fn toggle_returns_404_for_unknown_id() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool, None);
        let err = toggle(
            State(state),
            Path(99_999),
            Form(ToggleApiKeyForm { enabled: false }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_removes_row_and_returns_204() {
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool.clone(), None);
        let Json(created) = create(
            State(state.clone()),
            Form(CreateApiKeyForm {
                name: "Cal".into(),
                scopes: "calendar".into(),
            }),
        )
        .await
        .unwrap();
        let status = delete(State(state.clone()), Path(created.view.id))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let Json(views) = list(State(state)).await.unwrap();
        assert!(views.is_empty());
    }

    #[tokio::test]
    async fn delete_is_idempotent_for_unknown_id() {
        // No 404 — matches the model layer's "delete missing id is
        // ok" contract. A double-click on the delete button or a
        // stale list-row id shouldn't surface an error to the user.
        let pool = in_memory_pool().await;
        let state = build_test_app_state(pool, None);
        let status = delete(State(state), Path(99_999)).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}
