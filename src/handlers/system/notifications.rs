//! System → Notifications CRUD handlers (issue gh-121).
//!
//! Form-POST + server-side render style matching the other System
//! tabs (Logs, RSS, Scheduled Tasks). No JSON API, no modals —
//! provider CRUD goes through `<form method="post">` + redirect-back
//! patterns.
//!
//! The "Send test" button at `/api/notifications/{id}/test` already
//! lives in `handlers::notifications` and stays JSON because its
//! response is rendered inline by a small fetch() call from the page.
//! Everything else lands here.
//!
//! ## Sensitive-field handling
//!
//! Discord webhook URLs and webhook HMAC secrets are tokens. Render
//! masked (`<input type="password">`); never echo back the stored
//! value in the page response. On save:
//! - Empty submitted value → preserve the stored value (so a user
//!   editing the provider name doesn't accidentally clear the
//!   secret).
//! - Sentinel `__CLEAR__` submitted → wipe the stored value (the
//!   explicit "Clear secret" button submits this).
//! - Any other value → replace the stored value.

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::models::log::LogCategory;
use crate::services::logger;
use crate::services::notifications::{
    self, ALL_EVENT_KINDS, DEFAULT_ON_EVENT_KINDS, discord, store, webhook,
};

/// Sentinel submitted from the explicit "Clear secret" button on the
/// edit form. Distinguishes "I cleared this on purpose" from "I left
/// the field blank because I'm just renaming the provider."
const CLEAR_SENTINEL: &str = "__CLEAR__";

/// Per-event row rendered in the matrix checkbox group on the edit
/// form. `enabled` reflects the persisted `notification_settings.enabled`
/// value; an event_kind without a row in the matrix renders unchecked
/// (matches the dispatch path's default-deny behavior).
#[derive(Debug, Clone)]
pub struct EventToggleView {
    pub kind: String,
    pub label: &'static str,
    pub description: &'static str,
    pub enabled: bool,
}

/// Per-provider view for the list table. `config_json` is parsed into
/// shape-specific projections so the inline edit form can render the
/// right fields without re-deserializing per template branch.
#[derive(Debug, Clone)]
pub struct ProviderView {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    /// Webhook-only — the configured URL. Always rendered (URLs are
    /// not secret on the webhook surface; the HMAC secret is the
    /// secret). `None` for non-webhook rows.
    pub webhook_url: Option<String>,
    /// Webhook-only — has-secret flag. Renders a "[set]" placeholder
    /// next to the masked input so the user knows a value exists
    /// without seeing it.
    pub webhook_has_secret: bool,
    /// Webhook-only — custom headers serialized back to the textarea
    /// shape (`"Header-Name: value"` per line) so the edit form
    /// round-trips cleanly. Empty when no headers are configured.
    pub webhook_headers_text: String,
    /// Discord-only — the configured webhook URL. The URL itself is
    /// the secret (token in the path); the edit form pre-fills a
    /// `type=password` input with this value and exposes Show / Copy
    /// buttons (same pattern as the Jellyfin / Sonarr / Radarr API
    /// key fields). Empty when no URL is configured. The card view
    /// renders "[set]" / "(not configured)" without echoing the value;
    /// the secret only surfaces inside the edit modal.
    pub discord_webhook_url: Option<String>,
}

/// Form payload for `POST /system/notifications/upsert`. `id`
/// distinguishes create (None) from update (Some). Per-kind fields
/// are all `#[serde(default)]` so a webhook submission silently
/// drops the discord field group and vice versa.
#[derive(Debug, Deserialize)]
pub struct UpsertForm {
    #[serde(default)]
    pub id: Option<i64>,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub enabled: Option<String>,
    // Webhook fields
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default)]
    pub webhook_headers: String,
    // Discord fields
    #[serde(default)]
    pub discord_webhook_url: String,
    // Per-event matrix — one form key per kind, value `"on"` if checked.
    // Names: `event_Grabbed`, `event_Imported`, etc.
    #[serde(flatten)]
    pub events: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct EditQuery {
    #[serde(default)]
    pub edit_id: Option<i64>,
}

/// Load every provider row + its per-event matrix and project into
/// the shape the template renders. Called from the system page
/// builder when `tab == "notifications"`.
pub async fn load_provider_views(db: &sqlx::SqlitePool) -> Vec<ProviderView> {
    let rows: Vec<store::ProviderRow> = sqlx::query_as::<_, store::ProviderRow>(
        "SELECT id, name, kind, enabled, config_json
         FROM notification_providers ORDER BY id ASC",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|r| {
            let mut webhook_url = None;
            let mut webhook_has_secret = false;
            let mut webhook_headers_text = String::new();
            let mut discord_webhook_url: Option<String> = None;
            match r.kind.as_str() {
                "webhook" => {
                    if let Ok(cfg) = serde_json::from_str::<webhook::WebhookConfig>(&r.config_json)
                    {
                        webhook_url = Some(cfg.url);
                        webhook_has_secret = cfg.secret.as_deref().is_some_and(|s| !s.is_empty());
                        webhook_headers_text = cfg
                            .headers
                            .iter()
                            .map(|(k, v)| format!("{k}: {v}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                }
                "discord" => {
                    if let Ok(cfg) = serde_json::from_str::<discord::DiscordConfig>(&r.config_json)
                        && !cfg.webhook_url.is_empty()
                    {
                        discord_webhook_url = Some(cfg.webhook_url);
                    }
                }
                _ => {}
            }
            ProviderView {
                id: r.id,
                name: r.name,
                kind: r.kind,
                enabled: r.enabled,
                webhook_url,
                webhook_has_secret,
                webhook_headers_text,
                discord_webhook_url,
            }
        })
        .collect()
}

/// Build the per-event matrix view for a given provider id.
/// Returns the canonical event order from `ALL_EVENT_KINDS` so the
/// rendered list is stable across saves; an event_kind without a
/// row defaults to unchecked. For a fresh "create" form (no provider
/// id yet), seeds the default-on policy so the user starts with the
/// conservative defaults pre-checked.
pub async fn matrix_view(db: &sqlx::SqlitePool, provider_id: Option<i64>) -> Vec<EventToggleView> {
    let configured: std::collections::HashMap<String, bool> = match provider_id {
        Some(id) => store::matrix_for_provider(db, id).await.unwrap_or_default(),
        None => std::collections::HashMap::new(),
    };
    ALL_EVENT_KINDS
        .iter()
        .map(|k| {
            let enabled = match provider_id {
                Some(_) => configured.get(*k).copied().unwrap_or(false),
                // Fresh create form — default-on the conservative set.
                None => DEFAULT_ON_EVENT_KINDS.contains(k),
            };
            EventToggleView {
                kind: (*k).to_string(),
                label: event_label(k),
                description: event_description(k),
                enabled,
            }
        })
        .collect()
}

/// Human-friendly label for the matrix checkbox row.
fn event_label(kind: &str) -> &'static str {
    match kind {
        "Grabbed" => "Grabbed",
        "Imported" => "Imported",
        "ImportFailed" => "Import failed",
        "Misgrabbed" => "Misgrab detected",
        "ClassifierNeedsReview" => "Classifier needs review",
        "IndexerDown" => "Indexer down",
        "DownloadClientUnreachable" => "Download client unreachable",
        "ExternalSyncReLinkRequired" => "Re-link required",
        "Health" => "Health (test)",
        _ => "Unknown",
    }
}

/// One-liner describing when each event fires; rendered as the
/// small-text hint under the checkbox label. Calibrated against the
/// gh-118 issue body to set the user's expectation for noise level.
fn event_description(kind: &str) -> &'static str {
    match kind {
        "Grabbed" => "A release was sent to the download client.",
        "Imported" => "A file was hardlinked / copied / moved into the library.",
        "ImportFailed" => "Post-processing couldn't import a file.",
        "Misgrabbed" => {
            "The files inside a grab named a different series. Ryokan removed and blocklisted it, or flagged it when auto-remove is off."
        }
        "ClassifierNeedsReview" => {
            "Quality classifier flagged a low-confidence verdict. Can be noisy during reclassify sweeps."
        }
        "IndexerDown" => {
            "An indexer's RSS poll returned an error (rate-limit cooldowns are suppressed). Per-indexer 1h dedup so a broken indexer doesn't spam every tick."
        }
        "DownloadClientUnreachable" => {
            "The Settings status probe couldn't reach the download client. Per-client 1h dedup so a refreshed Connections page doesn't fire repeatedly."
        }
        "ExternalSyncReLinkRequired" => {
            "Your AniList or MyAnimeList token expired and a re-link is needed."
        }
        "Health" => {
            "Synthetic event used by the Send test button. Default-off; opt in if you want the test to fire through the per-event filter too."
        }
        _ => "",
    }
}

/// Parse the `webhook_headers` textarea into the persisted tuple-list
/// shape. Skips empty lines and `#`-prefixed comment lines. Returns a
/// per-line error message on the first malformed line — the caller
/// surfaces this back through the settings render path so the user
/// sees the specific line that broke.
pub fn parse_webhook_headers(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            format!(
                "header line {} missing ':' separator: {:?}",
                lineno + 1,
                line
            )
        })?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return Err(format!("header line {} has empty name", lineno + 1));
        }
        out.push((name.to_string(), value.to_string()));
    }
    Ok(out)
}

/// `POST /system/notifications/upsert` — create or update a
/// provider. `id` field present → update; missing → create. Calls
/// `rebuild_notification_providers_cache` after a successful write
/// so the next dispatch sees the new shape immediately. Per-event
/// matrix rows are inserted/updated based on the form's
/// `event_<kind>=on` checkboxes.
pub async fn notifications_upsert(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<UpsertForm>,
) -> Response {
    let is_htmx = headers.contains_key("HX-Request");
    let name = form.name.trim();
    if name.is_empty() {
        return redirect_with_err(is_htmx, "Name is required");
    }
    if name.len() > 100 {
        return redirect_with_err(is_htmx, "Name must be 100 characters or fewer");
    }
    let enabled = form.enabled.is_some();

    // Build the kind-specific config_json. Resolve the existing row's
    // sensitive fields (URL, secret) for the empty-means-no-change
    // path before constructing the new config blob.
    let existing_row = match form.id {
        Some(id) => store::get_provider(&state.db, id).await.ok().flatten(),
        None => None,
    };
    let config_json = match form.kind.as_str() {
        "webhook" => match build_webhook_config(&form, existing_row.as_ref()) {
            Ok(s) => s,
            Err(e) => return redirect_with_err(is_htmx, &e),
        },
        "discord" => match build_discord_config(&form, existing_row.as_ref()) {
            Ok(s) => s,
            Err(e) => return redirect_with_err(is_htmx, &e),
        },
        other => {
            return redirect_with_err(is_htmx, &format!("Unknown notification kind: {other}"));
        }
    };

    let row_id = match form.id {
        Some(id) => {
            let res = sqlx::query(
                "UPDATE notification_providers
                 SET name = ?, kind = ?, enabled = ?, config_json = ?,
                     updated_at = strftime('%s','now')
                 WHERE id = ?",
            )
            .bind(name)
            .bind(&form.kind)
            .bind(enabled as i64)
            .bind(&config_json)
            .bind(id)
            .execute(&state.db)
            .await;
            if let Err(e) = res {
                return redirect_with_err(is_htmx, &format!("DB write failed: {e}"));
            }
            id
        }
        None => {
            let res: Result<(i64,), sqlx::Error> = sqlx::query_as(
                "INSERT INTO notification_providers (name, kind, enabled, config_json)
                 VALUES (?, ?, ?, ?) RETURNING id",
            )
            .bind(name)
            .bind(&form.kind)
            .bind(enabled as i64)
            .bind(&config_json)
            .fetch_one(&state.db)
            .await;
            match res {
                Ok((id,)) => id,
                Err(e) => return redirect_with_err(is_htmx, &format!("DB insert failed: {e}")),
            }
        }
    };

    // Per-event matrix. Read every checkbox from `form.events` (key
    // shape `event_<Kind>`) and upsert the per-kind row. Default-deny
    // is preserved by deleting un-checked rows so a freshly-unchecked
    // event short-circuits at the dispatcher's matrix lookup.
    if let Err(e) = persist_matrix(&state.db, row_id, &form.events).await {
        return redirect_with_err(is_htmx, &format!("Matrix write failed: {e}"));
    }

    // Rebuild the live cache so the next dispatch sees the new shape.
    notifications::rebuild_notification_providers_cache(&state.notification_providers, &state.db)
        .await;

    logger::info(
        &state.db,
        LogCategory::Notifications,
        &format!("Notification provider saved: {name} ({})", form.kind),
        &format!("id={row_id}"),
    )
    .await;

    // HTMX path: render the section partial in place. The
    // `outerHTML` swap on `#notif-section` closes the modal (lives
    // inside the swapped fragment) and refreshes the card grid in
    // one shot — no manual `closeNotificationModal()` call needed.
    // Plain form-POST path: redirect back to the tab with a toast.
    if is_htmx {
        let providers = load_provider_views(&state.db).await;
        return NotificationSectionPartial {
            notification_providers: providers,
            notification_event_toggles: matrix_view(&state.db, None).await,
        }
        .into_html_ok();
    }
    htmx_aware_redirect(is_htmx, "/system?tab=notifications&message=Provider+saved").into_response()
}

/// `POST /system/notifications/delete` — drop the provider row.
/// `notification_settings` rows cascade out via the migration's FK.
/// Rebuilds the cache after the delete.
pub async fn notifications_delete(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Response {
    let is_htmx = headers.contains_key("HX-Request");
    if let Err(e) = sqlx::query("DELETE FROM notification_providers WHERE id = ?")
        .bind(form.id)
        .execute(&state.db)
        .await
    {
        return redirect_with_err(is_htmx, &format!("DB delete failed: {e}"));
    }
    notifications::rebuild_notification_providers_cache(&state.notification_providers, &state.db)
        .await;
    logger::info(
        &state.db,
        LogCategory::Notifications,
        "Notification provider deleted",
        &format!("id={}", form.id),
    )
    .await;
    // HTMX path mirrors upsert: re-render the section in place.
    if is_htmx {
        let providers = load_provider_views(&state.db).await;
        return NotificationSectionPartial {
            notification_providers: providers,
            notification_event_toggles: matrix_view(&state.db, None).await,
        }
        .into_html_ok();
    }
    htmx_aware_redirect(
        is_htmx,
        "/system?tab=notifications&message=Provider+deleted",
    )
    .into_response()
}

/// Build the webhook `config_json` blob from the form. Handles the
/// empty-means-no-change + `__CLEAR__`-means-wipe sentinel for the
/// secret field, validates the URL via `webhook::validate_url`, and
/// validates custom headers via `webhook::validate_headers`. Returns
/// the serialized JSON string ready for DB persistence.
fn build_webhook_config(
    form: &UpsertForm,
    existing: Option<&store::ProviderRow>,
) -> Result<String, String> {
    let url = form.webhook_url.trim();
    webhook::validate_url(url)?;

    // Headers: parse + validate.
    let headers = parse_webhook_headers(&form.webhook_headers)?;
    webhook::validate_headers(&headers)?;

    // Secret: three-way decode.
    let secret: Option<String> = match form.webhook_secret.as_str() {
        "" => existing
            .and_then(|r| serde_json::from_str::<webhook::WebhookConfig>(&r.config_json).ok())
            .and_then(|c| c.secret),
        CLEAR_SENTINEL => None,
        v => Some(v.to_string()),
    };

    // Use `serde_json::Map` (insertion-ordered when the `preserve_order`
    // feature is on, which serde_json 1.x has by default for string
    // keys) so the user's header order round-trips through save → DB →
    // edit-form re-render. `BTreeMap` would alphabetize, which silently
    // shuffles `Authorization: Bearer ...` past convention-prefix
    // headers and surprises the user when they reopen the modal.
    let mut headers_obj = serde_json::Map::with_capacity(headers.len());
    for (k, v) in &headers {
        headers_obj.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    let cfg = serde_json::json!({
        "url": url,
        "secret": secret,
        "headers": serde_json::Value::Object(headers_obj),
    });
    Ok(cfg.to_string())
}

/// Build the discord `config_json`. Webhook URL is itself the secret
/// (token in the path), so the empty-means-no-change semantics apply
/// to it directly.
fn build_discord_config(
    form: &UpsertForm,
    existing: Option<&store::ProviderRow>,
) -> Result<String, String> {
    let webhook_url: String = match form.discord_webhook_url.as_str() {
        "" => existing
            .and_then(|r| serde_json::from_str::<discord::DiscordConfig>(&r.config_json).ok())
            .map(|c| c.webhook_url)
            .unwrap_or_default(),
        CLEAR_SENTINEL => String::new(),
        v => v.trim().to_string(),
    };
    if webhook_url.is_empty() {
        return Err("Discord webhook URL is required".into());
    }
    discord::validate_url(&webhook_url)?;
    let cfg = serde_json::json!({"webhook_url": webhook_url});
    Ok(cfg.to_string())
}

/// Read every `event_<Kind>` form field and project to the matrix.
/// Form fields with `=on` (the HTML default for a checked checkbox)
/// produce `enabled=1` rows; absent / unchecked fields produce
/// `enabled=0` rows. Walks `ALL_EVENT_KINDS` to ensure every variant
/// has an explicit row, so the dispatcher's "missing row → deny" path
/// is never accidentally entered for a deliberately-off event.
async fn persist_matrix(
    db: &sqlx::SqlitePool,
    provider_id: i64,
    events: &std::collections::HashMap<String, String>,
) -> Result<(), sqlx::Error> {
    for kind in ALL_EVENT_KINDS {
        let key = format!("event_{kind}");
        let enabled = events.get(&key).map(|v| v == "on").unwrap_or(false);
        sqlx::query(
            "INSERT INTO notification_settings (provider_id, event_kind, enabled)
             VALUES (?, ?, ?)
             ON CONFLICT(provider_id, event_kind)
             DO UPDATE SET enabled = excluded.enabled",
        )
        .bind(provider_id)
        .bind(*kind)
        .bind(enabled as i64)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// `GET /system/notifications/{id}/edit-form` — minimal stub for
/// Section partial for the card+modal frontend (gh-121). Returned
/// directly from upsert / delete on HTMX requests so a save closes
/// the modal + re-renders the card grid in one swap. Also fetched
/// directly via `GET /system/notifications/section` if a future
/// caller wants to refresh just this surface without a full page
/// reload.
#[derive(Template)]
#[template(path = "partials/system/notifications/list.html")]
struct NotificationSectionPartial {
    notification_providers: Vec<ProviderView>,
    notification_event_toggles: Vec<EventToggleView>,
}

impl NotificationSectionPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Modal body for the Add flow. Returned by
/// `GET /system/notifications/add-form`. Fresh form, no row id, no
/// pre-filled values; per-event toggles default to the
/// DEFAULT_ON_EVENT_KINDS conservative seed so a brand-new provider
/// receives Grabbed / Imported / ImportFailed / ExternalSyncReLinkRequired
/// out of the box.
#[derive(Template)]
#[template(path = "partials/system/notifications/add_form_body.html")]
struct NotificationAddFormPartial {
    notification_event_toggles: Vec<EventToggleView>,
}

impl NotificationAddFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Modal body for the Edit flow. Returned by
/// `GET /system/notifications/{id}/edit-form`. Pre-filled with the
/// row's name / kind / enabled state and the persisted matrix.
/// Sensitive fields (HMAC secret, Discord webhook URL) render
/// masked with placeholder hints — the on-save logic preserves
/// stored values when the field is left blank, and the explicit
/// "Clear" button submits the `__CLEAR__` sentinel for explicit
/// wipe.
#[derive(Template)]
#[template(path = "partials/system/notifications/edit_form_body.html")]
struct NotificationEditFormPartial {
    row: ProviderView,
    notification_event_toggles: Vec<EventToggleView>,
}

impl NotificationEditFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// `GET /system/notifications/section` — re-render just the
/// notifications section. Used by HTMX after upsert / delete; the
/// `outerHTML` swap on `#notif-section` closes the modal (which
/// lives inside the swapped fragment) and refreshes the cards in
/// one shot. Also reachable by direct GET if any future caller
/// wants to refresh only this surface.
pub async fn notifications_section(State(state): State<AppState>) -> Response {
    let providers = load_provider_views(&state.db).await;
    NotificationSectionPartial {
        notification_providers: providers,
        notification_event_toggles: matrix_view(&state.db, None).await,
    }
    .into_html_ok()
}

/// `GET /system/notifications/add-form` — modal body for the Add
/// flow. Fetched via `htmx.ajax()` from the JS open-modal helper
/// so the modal stays at display:none with the previous Add form
/// in place across saves; only the open click triggers the fetch.
pub async fn notifications_add_form(State(state): State<AppState>) -> Response {
    NotificationAddFormPartial {
        notification_event_toggles: matrix_view(&state.db, None).await,
    }
    .into_html_ok()
}

/// `GET /system/notifications/{id}/edit-form` — modal body for the
/// Edit flow. Loads the row + its matrix, projects to the partial
/// view shape, renders. 404s when the row id doesn't match any
/// provider (e.g. the user opened a stale tab and someone else
/// deleted the row in the meantime).
pub async fn notifications_edit_form(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let providers = load_provider_views(&state.db).await;
    let Some(row) = providers.iter().find(|p| p.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<div class=\"modal-body\"><p class=\"form-hint\">\
                 Provider #{id} no longer exists. Close this modal and refresh the page.\
                 </p></div>"
            )),
        )
            .into_response();
    };
    let toggles = matrix_view(&state.db, Some(id)).await;
    NotificationEditFormPartial {
        row,
        notification_event_toggles: toggles,
    }
    .into_html_ok()
}

fn redirect_with_err(is_htmx: bool, err: &str) -> Response {
    let encoded = urlencoding_encode(err);
    htmx_aware_redirect(
        is_htmx,
        &format!("/system?tab=notifications&error={encoded}"),
    )
    .into_response()
}

/// Tiny in-line URL encoder for the redirect-with-err path. Avoids
/// pulling in another dependency for a single use.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Dispatch helper exposed to `EditQuery` extraction so the settings
/// page builder doesn't need to know the parameter shape.
pub fn parse_edit_id(query: &Query<EditQuery>) -> Option<i64> {
    query.edit_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;

    #[test]
    fn parse_webhook_headers_skips_empty_and_comment_lines() {
        let input = "# comment\n\nX-A: 1\n  \n# another\nX-B:  spaces around\n";
        let parsed = parse_webhook_headers(input).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("X-A".into(), "1".into()),
                ("X-B".into(), "spaces around".into()),
            ]
        );
    }

    #[test]
    fn parse_webhook_headers_rejects_lines_without_colon() {
        let r = parse_webhook_headers("Authorization Bearer token");
        let err = r.expect_err("must reject");
        assert!(err.contains("missing ':'"), "got: {err}");
    }

    #[test]
    fn parse_webhook_headers_rejects_empty_name() {
        let r = parse_webhook_headers(": bare-value");
        let err = r.expect_err("must reject");
        assert!(err.contains("empty name"), "got: {err}");
    }

    #[tokio::test]
    async fn persist_matrix_round_trips_form_to_db_and_back() {
        let db = in_memory_pool().await;
        // Insert a provider row so the matrix's FK has a target.
        sqlx::query(
            "INSERT INTO notification_providers (id, name, kind, enabled, config_json)
             VALUES (1, 'p', 'webhook', 1, '{}')",
        )
        .execute(&db)
        .await
        .unwrap();
        let mut form_events = std::collections::HashMap::new();
        form_events.insert("event_Grabbed".into(), "on".into());
        form_events.insert("event_Imported".into(), "on".into());
        // ImportFailed deliberately omitted — must produce enabled=0.
        persist_matrix(&db, 1, &form_events).await.unwrap();
        let m = store::matrix_for_provider(&db, 1).await.unwrap();
        assert_eq!(m.get("Grabbed"), Some(&true));
        assert_eq!(m.get("Imported"), Some(&true));
        assert_eq!(m.get("ImportFailed"), Some(&false));
        // Every event kind has a row regardless of form input.
        assert_eq!(m.len(), ALL_EVENT_KINDS.len());
    }

    #[test]
    fn build_webhook_config_preserves_secret_on_empty_submit() {
        // Existing row has a stored secret; user blanks the form
        // field while editing the name. Secret must survive.
        let existing = store::ProviderRow {
            id: 1,
            name: "p".into(),
            kind: "webhook".into(),
            enabled: true,
            config_json: r#"{"url":"https://example.com/x","secret":"shh"}"#.into(),
        };
        let form = UpsertForm {
            id: Some(1),
            name: "p".into(),
            kind: "webhook".into(),
            enabled: Some("on".into()),
            webhook_url: "https://example.com/x".into(),
            webhook_secret: "".into(),
            webhook_headers: "".into(),
            discord_webhook_url: "".into(),
            events: Default::default(),
        };
        let json = build_webhook_config(&form, Some(&existing)).unwrap();
        let cfg: webhook::WebhookConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.secret.as_deref(), Some("shh"));
    }

    #[test]
    fn build_webhook_config_clears_secret_on_sentinel() {
        let existing = store::ProviderRow {
            id: 1,
            name: "p".into(),
            kind: "webhook".into(),
            enabled: true,
            config_json: r#"{"url":"https://example.com/x","secret":"shh"}"#.into(),
        };
        let form = UpsertForm {
            id: Some(1),
            name: "p".into(),
            kind: "webhook".into(),
            enabled: Some("on".into()),
            webhook_url: "https://example.com/x".into(),
            webhook_secret: CLEAR_SENTINEL.into(),
            webhook_headers: "".into(),
            discord_webhook_url: "".into(),
            events: Default::default(),
        };
        let json = build_webhook_config(&form, Some(&existing)).unwrap();
        let cfg: webhook::WebhookConfig = serde_json::from_str(&json).unwrap();
        assert!(cfg.secret.is_none());
    }

    #[test]
    fn build_webhook_config_preserves_user_authored_header_order() {
        // Regression guard for the BTreeMap-shuffles-headers bug:
        // the user-typed sequence must round-trip through save → DB →
        // re-read in the same order. Pinning this requires inputs
        // where alphabetical and insertion order DISAGREE — pre-fix
        // (BTreeMap collect) would alphabetize, post-fix (preserve_order
        // serde_json) preserves insertion. Z-First typed first, A-Second
        // typed second: the user wants `[Z-First, A-Second]`; the
        // broken code would emit `[A-Second, Z-First]`.
        let form = UpsertForm {
            id: None,
            name: "p".into(),
            kind: "webhook".into(),
            enabled: Some("on".into()),
            webhook_url: "https://example.com/x".into(),
            webhook_secret: "".into(),
            webhook_headers: "Z-First: 1\nA-Second: 2".into(),
            discord_webhook_url: "".into(),
            events: Default::default(),
        };
        let json = build_webhook_config(&form, None).unwrap();
        let cfg: webhook::WebhookConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            cfg.headers
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["Z-First", "A-Second"],
            "header order must match user-authored input, not alphabetical"
        );
    }

    #[test]
    fn build_webhook_config_rejects_reserved_header() {
        let form = UpsertForm {
            id: None,
            name: "p".into(),
            kind: "webhook".into(),
            enabled: Some("on".into()),
            webhook_url: "https://example.com/x".into(),
            webhook_secret: "".into(),
            webhook_headers: "Content-Type: text/plain".into(),
            discord_webhook_url: "".into(),
            events: Default::default(),
        };
        let r = build_webhook_config(&form, None);
        assert!(r.is_err());
    }

    #[test]
    fn build_discord_config_preserves_url_on_empty_submit() {
        let existing = store::ProviderRow {
            id: 1,
            name: "p".into(),
            kind: "discord".into(),
            enabled: true,
            config_json: r#"{"webhook_url":"https://discord.com/api/webhooks/1/abc"}"#.into(),
        };
        let form = UpsertForm {
            id: Some(1),
            name: "p".into(),
            kind: "discord".into(),
            enabled: Some("on".into()),
            webhook_url: "".into(),
            webhook_secret: "".into(),
            webhook_headers: "".into(),
            discord_webhook_url: "".into(),
            events: Default::default(),
        };
        let json = build_discord_config(&form, Some(&existing)).unwrap();
        let cfg: discord::DiscordConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.webhook_url, "https://discord.com/api/webhooks/1/abc");
    }

    #[tokio::test]
    async fn matrix_view_seeds_default_on_for_fresh_create_form() {
        let db = sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy");
        let view = matrix_view(&db, None).await;
        // Default-on policy: Grabbed / Imported / ImportFailed /
        // ExternalSyncReLinkRequired pre-checked. Others off.
        let pre_checked: Vec<_> = view.iter().filter(|e| e.enabled).map(|e| &e.kind).collect();
        assert!(pre_checked.iter().any(|k| *k == "Grabbed"));
        assert!(pre_checked.iter().any(|k| *k == "Imported"));
        assert!(pre_checked.iter().any(|k| *k == "ImportFailed"));
        assert!(
            pre_checked
                .iter()
                .any(|k| *k == "ExternalSyncReLinkRequired")
        );
        assert!(!pre_checked.iter().any(|k| *k == "Health"));
    }

    #[test]
    fn urlencoding_encode_preserves_unreserved_and_escapes_others() {
        assert_eq!(urlencoding_encode("hello-world"), "hello-world");
        assert_eq!(urlencoding_encode("a b"), "a+b");
        assert_eq!(urlencoding_encode("a&b"), "a%26b");
        assert_eq!(urlencoding_encode("a/b?c"), "a%2Fb%3Fc");
    }
}
