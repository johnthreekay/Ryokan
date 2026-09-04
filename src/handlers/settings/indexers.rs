//! Settings → Indexers CRUD handlers (issue #28).
//!
//! Mirrors the shape of the groups + custom-formats settings
//! handlers: form-driven upsert + delete that redirect back to
//! the tab. The "test connection" path lands in a follow-up
//! commit since it needs the full search-pipeline integration to
//! be useful.

use askama::Template;
use axum::{
    Form, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::models::download_clients::DownloadClientRow;
use crate::models::indexers::{
    Indexer, IndexerForm, KIND_NEWZNAB, KIND_TORZNAB, delete, insert, list_all, update,
};
use crate::models::log::LogCategory;
use crate::services::indexer_catalog::{SEEDED, SeededIndexer, find_seed};
use crate::services::logger;

/// Per-kind hint text + URL placeholder, surfaced server-side so the
/// initial Add/Edit form render carries the right copy for the
/// selected kind. Pre-this-helper the templates hardcoded the torznab
/// hint and the JS `applyIndexerKindCopy` ran post-paint to swap it
/// for newznab rows — visible flash on every Edit-on-newznab open.
/// Keep these strings in lockstep with `INDEXER_KIND_COPY` in
/// `static/js/settings.js`; the JS path still owns the live
/// kind-flip case (user toggles the dropdown after the form is open).
pub const TORZNAB_API_KEY_HINT: &str = "Sent in the request URL per torznab spec; appears in Prowlarr / Jackett access logs and any reverse-proxy logs in front of them. Find this key in Prowlarr Settings \u{2192} General (or Jackett's UI).";
pub const NEWZNAB_API_KEY_HINT: &str = "Sent in the request URL per newznab spec; the same key Sonarr/Radarr/Prowlarr use against this indexer. For Prowlarr-fronted indexers, find it in Prowlarr Settings \u{2192} General; for direct-to-indexer setups, find it on the indexer's site (e.g. NZBGeek \u{2192} Profile \u{2192} API Key).";

/// Map a kind string to the matching API-key hint text. Falls back
/// to the torznab hint for unknown values (matches the JS
/// `INDEXER_KIND_COPY[kind] || INDEXER_KIND_COPY.torznab` shape).
pub fn api_key_hint_for_kind(kind: &str) -> &'static str {
    match kind {
        KIND_NEWZNAB => NEWZNAB_API_KEY_HINT,
        _ => TORZNAB_API_KEY_HINT,
    }
}

/// URL placeholder per-kind. Same pattern as
/// [`api_key_hint_for_kind`].
pub fn url_placeholder_for_kind(kind: &str) -> &'static str {
    match kind {
        KIND_NEWZNAB => "https://nzb.indexer.example/api",
        _ => "https://prowlarr.local/{N}/api",
    }
}

/// Section partial — the entire Indexers fieldset (catalog grid +
/// existing-row card grid + shared edit/add modal). Every successful
/// HTMX action (upsert / delete) returns this so a single swap
/// re-renders the whole section without a page reload, mirroring the
/// Download Clients section pattern.
#[derive(Template)]
#[template(path = "partials/settings/indexers/list.html")]
struct IndexerSectionPartial {
    indexers: Vec<Indexer>,
    download_clients: Vec<DownloadClientRow>,
    indexer_catalog: &'static [SeededIndexer],
}

impl IndexerSectionPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Edit form body — modal content for the Edit flow. Returned by
/// `GET /settings/indexers/{id}/edit-form` and swapped into
/// `#indexer-modal-body` (innerHTML) when the user clicks an
/// existing-indexer card.
#[derive(Template)]
#[template(path = "partials/settings/indexers/edit_form_body.html")]
struct IndexerEditFormPartial {
    row: Indexer,
    /// What the indexer's cached caps report, rendered as chips under
    /// the categories field so the user can pick ids by clicking.
    reported_categories: Vec<crate::services::indexers::ReportedCategory>,
    download_clients: Vec<DownloadClientRow>,
}

impl IndexerEditFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Inline error partial rendered into `#indexer-modal-body` when the
/// edit form fetch fails. Mirrors `ModalErrorPartial` in
/// `download_clients.rs` for the same reason — htmx 2.x skips the
/// swap on 4xx/5xx, leaving the user staring at the prior form
/// while the modal title says "Editing FooIndexer".
#[derive(Template)]
#[template(path = "partials/settings/modal_error.html")]
struct ModalErrorPartial {
    message: String,
}

impl ModalErrorPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Add form body — modal content for the Add flow. Returned by
/// `GET /settings/indexers/add-form?template=<slug>` and swapped
/// into `#indexer-modal-body` when the user clicks one of the
/// catalog seed cards. `seed = None` (no template, or unknown
/// slug) renders a blank Add form.
#[derive(Template)]
#[template(path = "partials/settings/indexers/add_form_body.html")]
struct IndexerAddFormPartial {
    seed: Option<&'static SeededIndexer>,
    download_clients: Vec<DownloadClientRow>,
}

impl IndexerAddFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Helper — load the current rows + download clients and render the
/// section partial. Used by the success path of upsert/delete and the
/// `/settings/indexers/section` cancel-edit / refresh route.
async fn render_section(state: &AppState) -> Response {
    let indexers = list_all(&state.db).await.unwrap_or_default();
    let download_clients = crate::models::download_clients::list_all(&state.db)
        .await
        .unwrap_or_default();
    IndexerSectionPartial {
        indexers,
        download_clients,
        indexer_catalog: SEEDED,
    }
    .into_html_ok()
}

/// Form for create/update — `id == None` creates, `id == Some(n)`
/// updates row `n`. Mirrors CustomFormatUpsertForm shape.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerUpsertForm {
    pub id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub api_key: String,
    /// Sonarr-convention priority. Range 1-50; out-of-range coerces
    /// to 25. Empty string also coerces to 25.
    pub priority: Option<String>,
    /// HTML form checkboxes only POST when checked, so the field
    /// is `Option<String>` and presence-equivalent to true.
    pub enabled: Option<String>,
    pub is_private_tracker: Option<String>,
    /// Empty string = NULL (use default seed rules).
    pub seed_ratio: Option<String>,
    pub seed_time_minutes: Option<String>,
    pub min_seeders: Option<String>,
    pub request_timeout_secs: Option<String>,
    /// Multi-client routing pin — id of the row in
    /// `download_clients` this indexer routes to. Empty
    /// string = NULL (use the default client at grab time).
    pub download_client_id: Option<String>,
    /// Comma-separated torznab category ids; blank means automatic.
    #[serde(default)]
    pub categories: Option<String>,
    /// Multi-RSS — opt this indexer into the RSS sync
    /// fan-out. Checkbox; presence-equivalent to true.
    pub rss_enabled: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerDeleteForm {
    pub id: i64,
}

#[utoipa::path(
    post,
    path = "/settings/indexers/upsert",
    tag = "Settings",
    summary = "Create or update an indexer",
    description = "Form-driven upsert for the Settings → Indexers tab. Creates a new row when `id` is omitted; updates the row identified by `id` otherwise. Validates kind ∈ {torznab, newznab}, priority ∈ [1, 50], min_seeders ≥ 0. Out-of-range numerics coerce to safe defaults rather than rejecting the submission. Redirects back to the indexers tab.",
    responses(
        (status = 303, description = "Redirect back to the indexers tab"),
    ),
)]
pub async fn settings_indexers_upsert(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<IndexerUpsertForm>,
) -> Response {
    let name = form.name.trim();
    if name.is_empty() {
        return error_redirect(is_htmx, "Name+required");
    }
    let kind = match form.kind.as_str() {
        KIND_TORZNAB | KIND_NEWZNAB => form.kind.as_str(),
        _ => {
            return error_redirect(is_htmx, "Invalid+kind");
        }
    };
    let url = form.url.trim();
    if url.is_empty() {
        return error_redirect(is_htmx, "URL+required");
    }
    // PR #107 review fix #12: catch typos at save time rather
    // than at the next search. reqwest::Url::parse is what the
    // client uses internally; round-tripping it here surfaces
    // missing scheme / malformed host immediately.
    if reqwest::Url::parse(url).is_err() {
        return error_redirect(is_htmx, "Invalid+URL+syntax");
    }
    let priority = parse_priority(&form.priority);
    let min_seeders = parse_optional_i32(&form.min_seeders, 1).max(0);
    let request_timeout_secs = parse_optional_secs(&form.request_timeout_secs);
    let api_key = form.api_key.trim();
    let download_client_id = parse_optional_i64(&form.download_client_id);

    // Protocol guard — torznab indexers route torrent magnets /
    // .torrent URLs; newznab indexers route NZB URLs. Pinning a
    // newznab indexer to a BT client (or vice versa, torznab → SAB)
    // surfaces at grab time as "client rejected URL" with no upfront
    // signal — better to refuse the save with a clear toast. Mirrors
    // Sonarr's per-indexer Protocol enum check.
    //
    // PR 112 review #1 (4th pass) — fail closed on transient DB
    // error. The earlier `if let Ok(Some(row))` shape silently
    // skipped the gate when get_by_id returned Err, which would
    // let a torznab→SAB pin slip through under a hiccup at save
    // time. Match Err explicitly with a "DB error: ...; please
    // retry" toast. Ok(None) still permits (row deleted between
    // page-load and submit is intentional).
    if let Some(client_id) = download_client_id {
        let row = match crate::models::download_clients::get_by_id(&state.db, client_id).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => None, // intentional: client deleted between page-load and submit
            Err(e) => {
                let msg = urlencoding::encode(&format!(
                    "Couldn't verify protocol pin (DB error: {e}); please retry."
                ))
                .into_owned();
                return error_redirect(is_htmx, &msg);
            }
        };
        if let Some(row) = row {
            let indexer_proto = crate::services::download_client::protocol_for_indexer_kind(kind);
            let client_proto =
                crate::services::download_client::protocol_for_client_kind(&row.kind);
            if let (Some(ip), Some(cp)) = (indexer_proto, client_proto)
                && ip != cp
            {
                let msg = urlencoding::encode(&format!(
                    "Can't pin a {kind} indexer to a {} client (protocol mismatch; \
                     {kind} returns {ip} URLs, {} accepts {cp})",
                    row.kind, row.kind
                ))
                .into_owned();
                return error_redirect(is_htmx, &msg);
            }
        }
    }

    let payload = IndexerForm {
        name,
        kind,
        url,
        api_key,
        priority,
        enabled: form.enabled.is_some(),
        is_private_tracker: form.is_private_tracker.is_some(),
        seed_ratio: parse_optional_f64(&form.seed_ratio),
        seed_time_minutes: parse_optional_i64(&form.seed_time_minutes),
        min_seeders,
        request_timeout_secs,
        download_client_id,
        rss_enabled: form.rss_enabled.is_some(),
        categories: form.categories.as_deref().unwrap_or("").trim(),
    };

    let result = match form.id {
        Some(id) => update(&state.db, id, payload).await.map(|_| id),
        None => insert(&state.db, payload).await,
    };

    match result {
        Ok(id) => {
            let verb = if form.id.is_some() {
                "updated"
            } else {
                "added"
            };
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Indexer {verb}: {name} ({kind})"),
                &format!("id={id}, priority={priority}"),
            )
            .await;
            // PR #107 review fix #4: rebuild the IndexerCache so
            // the next search picks up the new/edited row without
            // a process restart.
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            // HTMX redesign — re-render the whole #indexer-section so
            // the new/edited card surfaces and the modal goes back to
            // display:none with the default body, all in one swap.
            // Mirrors the DC upsert handler.
            if is_htmx {
                render_section(&state).await
            } else {
                let msg = urlencoding::encode(&format!("Indexer '{name}' {verb}")).into_owned();
                Redirect::to(&format!("/settings?tab=indexers&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Indexer upsert failed",
                &e.to_string(),
            )
            .await;
            error_redirect(is_htmx, "Save+failed")
        }
    }
}

/// Send the user back to the Indexers tab with `&err=<msg>` flashed.
/// Thin wrapper around [`crate::handlers::responses::htmx_aware_redirect`]
/// — kept for the prefix-encoded ergonomic at the existing call
/// sites. The shared helper handles the HX-Redirect-vs-303 split.
fn error_redirect(is_htmx: bool, encoded_msg: &str) -> Response {
    let url = format!("/settings?tab=indexers&err={encoded_msg}");
    crate::handlers::responses::htmx_aware_redirect(is_htmx, &url)
}

#[utoipa::path(
    post,
    path = "/settings/indexers/delete",
    tag = "Settings",
    summary = "Delete an indexer",
    description = "Removes the indexer row by id. Existing grabbed_torrents and pending_grabs rows referencing this indexer have their indexer_id NULLed in the same transaction, so grab history is preserved with the FK cleared. SQLite can't enforce a real ON DELETE SET NULL via ALTER TABLE so the model layer (`models::indexers::delete`) handles it explicitly.",
    responses(
        (status = 303, description = "Redirect back to the indexers tab"),
    ),
)]
pub async fn settings_indexers_delete(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<IndexerDeleteForm>,
) -> Response {
    // PR #107 round-3 review fixes #2+#3: the SET-NULL UPDATEs
    // for grabbed_torrents.indexer_id + pending_grabs.indexer_id
    // are now folded into models::indexers::delete as a transaction
    // so all three statements succeed or fail atomically; previously
    // the handler ran them with `let _ = …` and a partial-NULL-out
    // could ride out a transient I/O error silently.
    // Capture the name before delete so the success toast can name
    // the row that was removed. A failed lookup falls back to the
    // numeric id; the delete itself is the source of truth.
    let display_name = crate::models::indexers::get_by_id(&state.db, form.id)
        .await
        .ok()
        .flatten()
        .map(|r| r.name)
        .unwrap_or_else(|| format!("id={}", form.id));
    match delete(&state.db, form.id).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Indexer deleted: {display_name} (id={})", form.id),
                "",
            )
            .await;
            // PR #107 review fix #4: same cache refresh as upsert.
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            // Card-redesign — delete also re-renders the whole
            // #indexer-section so the empty-state path renders when
            // the last row goes away, and the modal stays at
            // display:none. Mirrors the DC delete handler.
            if is_htmx {
                render_section(&state).await
            } else {
                let msg =
                    urlencoding::encode(&format!("Indexer '{display_name}' deleted")).into_owned();
                Redirect::to(&format!("/settings?tab=indexers&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Indexer delete failed",
                &e.to_string(),
            )
            .await;
            // PR #107 round-4 review fix #3: surface the failure
            // via `&err=` so the user sees an inline banner instead
            // of a quiet success-looking redirect. Mirrors the
            // upsert handler's "Save failed" pattern. For HTMX,
            // return a 5xx so `htmx:responseError` fires and the
            // row stays put; the toast helper picks up the status.
            if is_htmx {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            } else {
                Redirect::to("/settings?tab=indexers&err=Delete+failed").into_response()
            }
        }
    }
}

// ── multi-rss commit G — Test RSS feed endpoint for indexers ────────

/// Body of the indexer-RSS Test request. Mirrors the direct-feed
/// shape — caller passes the indexer row id, handler runs a
/// single empty-`q` `?t=tvsearch` against it and returns the
/// item count + first title.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerRssTestForm {
    pub id: i64,
}

/// JSON envelope for the indexer-RSS Test response. Same shape
/// as the direct-feed Test response so the frontend toast can
/// share the rendering helper.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct IndexerRssTestResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_title: Option<String>,
}

#[utoipa::path(
    post,
    path = "/settings/indexers/test-rss",
    tag = "Settings",
    summary = "Test-fetch an indexer's RSS endpoint",
    description = "Fires a single `?t=tvsearch&cat=5070` (with empty `q`) request against the indexer identified by id and returns a JSON envelope describing the result: item count and first item's title. Used by the Settings → Indexers form's per-row Test RSS button. Indexer protocol kind is already known from the row (torznab/newznab → torrent/usenet) so no protocol detection step is needed here, unlike the direct-feed Test.",
    responses(
        (status = 200, description = "Test result envelope", body = IndexerRssTestResponse),
    ),
)]
pub async fn settings_indexers_test_rss(
    State(state): State<AppState>,
    Json(form): Json<IndexerRssTestForm>,
) -> Json<IndexerRssTestResponse> {
    // Look up the live `Arc<dyn Indexer>` from the in-memory
    // cache so the test fetch reuses the same reqwest client +
    // cooldown state the sync path uses.
    let snapshot = state.indexers.read().await.clone();
    let Some(indexer) = snapshot.iter().find(|i| i.id() == form.id).cloned() else {
        return Json(IndexerRssTestResponse {
            ok: false,
            error: Some(format!(
                "Indexer id={} not in cache (try Save before Test, or check Enabled)",
                form.id
            )),
            item_count: None,
            first_title: None,
        });
    };

    match crate::services::indexers::fetch_indexer_rss(&*indexer).await {
        Ok(items) => {
            let count = items.len() as i32;
            let first_title = items.first().map(|i| i.title.clone());
            Json(IndexerRssTestResponse {
                ok: true,
                error: None,
                item_count: Some(count),
                first_title,
            })
        }
        Err(err) => Json(IndexerRssTestResponse {
            ok: false,
            error: Some(err),
            item_count: None,
            first_title: None,
        }),
    }
}

// ── Modal-form GET endpoints ──────────────────────────────────────
//
// These three read-only endpoints back the card-redesign modal flow.
// `/settings/indexers/section` re-renders the whole section (used by
// any caller that wants to reset the UI without a full page reload —
// currently no callers, but symmetric with DC and useful for future
// Cancel-button wiring). The two form-body endpoints are what the
// modal opens fetch into `#indexer-modal-body`.

#[utoipa::path(
    get,
    path = "/settings/indexers/section",
    tag = "Settings",
    summary = "Render the Indexers section partial",
    description = "Returns the catalog grid + existing-indexer cards + shared modal fragment that lives at #indexer-section on the Indexers tab. Mirrors `/api/download-clients/section`.",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_indexers_section(State(state): State<AppState>) -> Response {
    render_section(&state).await
}

#[utoipa::path(
    get,
    path = "/settings/indexers/{id}/edit-form",
    tag = "Settings",
    summary = "Render the edit form body for one indexer",
    description = "Returns the edit_form_body.html fragment for the targeted row, prefilled with current values. Swapped into the shared modal body (`#indexer-modal-body`) when the user clicks an existing-indexer card. Returns 404 when the row no longer exists (e.g. concurrent delete from another tab).",
    responses(
        (status = 200, description = "HTML fragment"),
        (status = 404, description = "Row not found"),
    ),
)]
pub async fn settings_indexers_edit_form(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    match crate::models::indexers::get_by_id(&state.db, id).await {
        Ok(Some(row)) => {
            let download_clients = crate::models::download_clients::list_all(&state.db)
                .await
                .unwrap_or_default();
            IndexerEditFormPartial {
                reported_categories: crate::services::indexers::reported_categories(&row.caps_json),
                row,
                download_clients,
            }
            .into_html_ok()
        }
        Ok(None) => ModalErrorPartial {
            message: "This indexer no longer exists. It may have been deleted in another tab."
                .into(),
        }
        .into_html_ok(),
        Err(e) => ModalErrorPartial {
            message: format!("Failed to load indexer: {e}"),
        }
        .into_html_ok(),
    }
}

#[derive(Deserialize)]
pub struct AddFormQuery {
    /// Catalog seed slug. When set + recognized, the form pre-fills
    /// from the matched seed (name / kind / private flag / priority /
    /// min seeders / suggested seed-ratio). Empty / unrecognized →
    /// blank form.
    pub template: Option<String>,
}

#[utoipa::path(
    get,
    path = "/settings/indexers/add-form",
    tag = "Settings",
    summary = "Render the add form body",
    description = "Returns the add_form_body.html fragment swapped into the shared modal body (`#indexer-modal-body`) when the user clicks one of the catalog seed cards. The optional `template` query param resolves a seed and pre-fills the form; an unknown / missing template renders a blank Add form.",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_indexers_add_form(
    State(state): State<AppState>,
    Query(q): Query<AddFormQuery>,
) -> Response {
    let seed = q.template.as_deref().and_then(find_seed);
    let download_clients = crate::models::download_clients::list_all(&state.db)
        .await
        .unwrap_or_default();
    IndexerAddFormPartial {
        seed,
        download_clients,
    }
    .into_html_ok()
}

/// Form payload for the stateless `/api/indexers/test` endpoint —
/// probes a transient TorznabIndexer built from form fields, without
/// requiring a save first. Lets the Add/Edit modal verify URL +
/// API key before persisting; complements the existing id-based
/// `/settings/indexers/test-rss` which only works for already-saved
/// rows. `#[serde(default)]` on every field because `hx-include=
/// "closest form"` pulls in extras (priority, min_seeders, etc.) the
/// test path doesn't care about.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct IndexerStatelessTestForm {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub api_key: String,
    /// Optional — when set the response uses the matching cache
    /// entry (re-uses the warm reqwest client + cooldown state).
    /// When unset, a transient TorznabIndexer is built per request.
    #[serde(default)]
    pub id: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/indexers/test",
    tag = "Settings",
    summary = "Test-fetch a torznab/newznab indexer (stateless)",
    description = "Stateless variant of `/settings/indexers/test-rss` that accepts form fields instead of an id, so the Add/Edit modal can probe an unsaved row. Builds a transient TorznabIndexer from the supplied URL + API key and runs an empty-q tvsearch against it. Returns the JSON test envelope with item count + first title or the error message. Toast-rendered on the frontend.",
    request_body = IndexerStatelessTestForm,
    responses(
        (status = 200, description = "Test result envelope", body = IndexerRssTestResponse),
    ),
)]
pub async fn settings_indexers_test_stateless(
    State(state): State<AppState>,
    Form(form): Form<IndexerStatelessTestForm>,
) -> Response {
    // Validate the basics before building anything. Empty URL or
    // unknown kind would just produce a confusing builder error
    // downstream; surface those upfront.
    let kind = form.kind.trim();
    if !matches!(kind, "torznab" | "newznab") {
        return indexer_test_trigger(
            false,
            &format!("Invalid kind: {kind:?} (expected torznab or newznab)"),
        );
    }
    let url = form.url.trim();
    if url.is_empty() {
        return indexer_test_trigger(false, "URL required");
    }
    if reqwest::Url::parse(url).is_err() {
        return indexer_test_trigger(false, &format!("Invalid URL syntax: {url}"));
    }

    // Prefer the cached client when an id is provided AND it resolves
    // to a row in the IndexerCache — keeps the warm reqwest client +
    // cooldown state intact for the Edit case. Fall back to building
    // a transient indexer from form fields for the Add case (no id
    // yet) or when the id missed the cache (saved-but-disabled).
    let indexer: std::sync::Arc<dyn crate::services::indexers::Indexer> = if let Some(id) = form.id
    {
        let snapshot = state.indexers.read().await.clone();
        if let Some(cached) = snapshot.iter().find(|i| i.id() == id).cloned() {
            cached
        } else {
            match build_transient_indexer(0, kind, url, form.api_key.trim()) {
                Ok(c) => c,
                Err(e) => {
                    return indexer_test_trigger(
                        false,
                        &format!("Failed to build indexer client: {e}"),
                    );
                }
            }
        }
    } else {
        match build_transient_indexer(0, kind, url, form.api_key.trim()) {
            Ok(c) => c,
            Err(e) => {
                return indexer_test_trigger(
                    false,
                    &format!("Failed to build indexer client: {e}"),
                );
            }
        }
    };

    match crate::services::indexers::fetch_indexer_rss(&*indexer).await {
        Ok(items) => {
            // ASCII-only message body. The HX-Trigger header is the
            // transport, and HTTP headers carry no charset metadata.
            // Non-ASCII bytes survive as raw bytes which htmx then
            // interprets as Latin-1 when JSON-parsing the header
            // value, producing mojibake (e.g. an em-dash's UTF-8
            // bytes \xe2\x80\x94 render as `â\u{80}\u{94}` in the
            // toast). The first-item title CAN carry arbitrary
            // UTF-8 (groups put kanji in titles all the time) and
            // will mojibake the same way; trade-off is accepting
            // some title garbling vs dropping the title entirely
            // since the user usually wants confirmation that
            // SOMETHING is being parsed. ASCII-only on the prefix
            // keeps the most-load-bearing words readable.
            let msg = if items.is_empty() {
                "Connected. 0 items returned (try a real query to confirm grabs work).".to_string()
            } else {
                let first = items
                    .first()
                    .map(|i| i.title.as_str())
                    .unwrap_or("(no title)");
                format!("Connected. {} item(s). First: {}", items.len(), first)
            };
            indexer_test_trigger(true, &msg)
        }
        Err(err) => indexer_test_trigger(false, &err),
    }
}

/// Build the HTMX response for the indexer Test probe. Same shape
/// as the DC `test_result_response` helper — empty body + an
/// `HX-Trigger` header carrying the result. The frontend listener
/// in `static/js/settings.js` (`ryokan-indexer-test-result`) reads
/// the trigger payload and fires a toast. Empty body keeps the
/// modal-footer button row at a stable height regardless of result
/// text length.
fn indexer_test_trigger(ok: bool, message: &str) -> Response {
    let payload = serde_json::json!({
        "ryokan-indexer-test-result": {
            "ok": ok,
            "message": message,
        }
    });
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        "HX-Trigger",
        payload
            .to_string()
            .parse()
            .unwrap_or_else(|_| "ryokan-indexer-test-result".parse().unwrap()),
    );
    resp
}

/// Build a transient TorznabIndexer for the stateless test path.
/// Mirrors the production `from_row` constructor, but synthesizes
/// the `Indexer` row in-memory from the form fields so unsaved
/// configurations can be probed. Test-only id is `0` since this
/// indexer never goes through the dedup pass.
fn build_transient_indexer(
    id: i64,
    kind: &str,
    url: &str,
    api_key: &str,
) -> Result<std::sync::Arc<dyn crate::services::indexers::Indexer>, String> {
    let row = crate::models::indexers::Indexer {
        id,
        name: "Test".to_string(),
        kind: kind.to_string(),
        url: url.to_string(),
        api_key: api_key.to_string(),
        priority: 25,
        enabled: true,
        is_private_tracker: false,
        seed_ratio: None,
        seed_time_minutes: None,
        min_seeders: 0,
        request_timeout_secs: Some(15),
        download_client_id: None,
        rss_enabled: false,
        rss_last_polled_at: None,
        rss_last_poll_error: String::new(),
        rss_last_item_count: 0,
        categories: String::new(),
        caps_json: String::new(),
        caps_refreshed_at: None,
        created_at: 0,
        updated_at: 0,
    };
    crate::services::indexers::torznab::TorznabIndexer::from_row_arc(&row)
}

/// Coerce the priority form field into the Sonarr-convention
/// range. Anything out of [1, 50] (or unparseable) lands at 25 —
/// the default — rather than rejecting the submission. Matches
/// the validate_* helpers in the parent settings module.
pub(crate) fn parse_priority(raw: &Option<String>) -> i32 {
    let parsed = raw
        .as_deref()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(25);
    parsed.clamp(1, 50)
}

fn parse_optional_i32(raw: &Option<String>, default: i32) -> i32 {
    raw.as_deref()
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<i32>().ok()
            }
        })
        .unwrap_or(default)
}

fn parse_optional_i64(raw: &Option<String>) -> Option<i64> {
    raw.as_deref().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<i64>().ok()
        }
    })
}

fn parse_optional_f64(raw: &Option<String>) -> Option<f64> {
    raw.as_deref().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<f64>().ok()
        }
    })
}

/// Per-indexer search timeout. Stored as `Option<i64>` (NULL =
/// use default). Out-of-range values (< 1s or > 600s) coerce to
/// None rather than persist a value that would force every
/// search to immediately timeout or block forever.
fn parse_optional_secs(raw: &Option<String>) -> Option<i64> {
    parse_optional_i64(raw).filter(|n| (1..=600).contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_priority_clamps_into_sonarr_range() {
        assert_eq!(parse_priority(&Some("0".into())), 1);
        assert_eq!(parse_priority(&Some("51".into())), 50);
        assert_eq!(parse_priority(&Some("25".into())), 25);
        assert_eq!(parse_priority(&Some("-100".into())), 1);
    }

    #[test]
    fn parse_priority_falls_back_to_25_on_unparseable() {
        assert_eq!(parse_priority(&None), 25);
        assert_eq!(parse_priority(&Some(String::new())), 25);
        assert_eq!(parse_priority(&Some("garbage".into())), 25);
        assert_eq!(parse_priority(&Some("3.14".into())), 25);
    }

    #[test]
    fn parse_optional_secs_filters_out_of_range_values() {
        // <1 or >600 → None (defensive: prevents a typo persisting
        // a 0s timeout that fails every search instantly, or a
        // 30000s value that blocks the auto-search loop forever).
        assert_eq!(parse_optional_secs(&Some("0".into())), None);
        assert_eq!(parse_optional_secs(&Some("601".into())), None);
        assert_eq!(parse_optional_secs(&Some("30".into())), Some(30));
    }

    #[test]
    fn parse_optional_i64_treats_empty_string_as_none() {
        assert_eq!(parse_optional_i64(&Some(String::new())), None);
        assert_eq!(parse_optional_i64(&Some("   ".into())), None);
        assert_eq!(parse_optional_i64(&Some("42".into())), Some(42));
    }

    #[test]
    fn parse_optional_f64_treats_empty_string_as_none() {
        assert_eq!(parse_optional_f64(&Some(String::new())), None);
        assert_eq!(parse_optional_f64(&Some("2.5".into())), Some(2.5));
    }

    /// protocol-mismatch validation on the indexer
    /// upsert path. Pinning a torznab indexer to a SAB client (or a
    /// newznab indexer to a BT client) used to silently save the row
    /// and only fail at grab time when the client rejected the URL.
    /// These tests pin the upfront-rejection shape so a future
    /// refactor can't drop the guard and re-introduce the silent-
    /// fail surface.
    mod protocol_guard {
        use super::super::*;
        use crate::models::download_clients::{DownloadClientForm, insert as insert_dc};
        use crate::test_support::{build_test_app_state, in_memory_pool};
        use axum::extract::{Form, State};

        fn extract_location(resp: axum::response::Response) -> String {
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_default()
        }

        fn upsert_form(kind: &str, dc_id: i64) -> IndexerUpsertForm {
            IndexerUpsertForm {
                id: None,
                name: "Test".into(),
                kind: kind.to_string(),
                url: "https://prowlarr.local/1/api".into(),
                api_key: "k".into(),
                priority: Some("25".into()),
                enabled: Some("on".into()),
                is_private_tracker: None,
                seed_ratio: None,
                seed_time_minutes: None,
                min_seeders: Some("1".into()),
                request_timeout_secs: None,
                download_client_id: Some(dc_id.to_string()),
                rss_enabled: None,
                categories: None,
            }
        }

        async fn seed_clients(db: &sqlx::SqlitePool) -> (i64 /* qbit */, i64 /* sab */) {
            let qbit = insert_dc(
                db,
                DownloadClientForm {
                    name: "qBit",
                    kind: "qbittorrent",
                    url: "http://qbit.local",
                    username: "",
                    password: "",
                    label: "",
                    download_path: "",
                    enabled: true,
                    is_default: true,
                },
            )
            .await
            .expect("seed qbit");
            let sab = insert_dc(
                db,
                DownloadClientForm {
                    name: "SAB",
                    kind: "sabnzbd",
                    url: "http://sab.local",
                    username: "",
                    password: "key",
                    label: "tv",
                    download_path: "",
                    enabled: true,
                    is_default: false,
                },
            )
            .await
            .expect("seed sab");
            (qbit, sab)
        }

        #[tokio::test]
        async fn torznab_pinned_to_sab_is_rejected() {
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_indexers_upsert(
                State(state.clone()),
                HxRequest(false),
                Form(upsert_form("torznab", sab)),
            )
            .await;
            let location = extract_location(resp);
            assert!(
                location.contains("err=") && location.contains("protocol"),
                "expected protocol-mismatch err redirect, got: {location}"
            );
            // Row must NOT have been inserted.
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert!(
                rows.is_empty(),
                "torznab→SAB save must be rejected, not silently persisted: {rows:?}"
            );
        }

        #[tokio::test]
        async fn newznab_pinned_to_qbit_is_rejected() {
            let db = in_memory_pool().await;
            let (qbit, _sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_indexers_upsert(
                State(state.clone()),
                HxRequest(false),
                Form(upsert_form("newznab", qbit)),
            )
            .await;
            let location = extract_location(resp);
            assert!(
                location.contains("err=") && location.contains("protocol"),
                "expected protocol-mismatch err redirect, got: {location}"
            );
            assert!(
                crate::models::indexers::list_all(&state.db)
                    .await
                    .unwrap()
                    .is_empty(),
                "newznab→qBit save must be rejected"
            );
        }

        #[tokio::test]
        async fn torznab_pinned_to_qbit_succeeds() {
            // Positive test — same-protocol pair must save through.
            let db = in_memory_pool().await;
            let (qbit, _sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_indexers_upsert(
                State(state.clone()),
                HxRequest(false),
                Form(upsert_form("torznab", qbit)),
            )
            .await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=") && !location.contains("err="),
                "expected success redirect, got: {location}"
            );
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].download_client_id, Some(qbit));
        }

        #[tokio::test]
        async fn newznab_pinned_to_sab_succeeds() {
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_indexers_upsert(
                State(state.clone()),
                HxRequest(false),
                Form(upsert_form("newznab", sab)),
            )
            .await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=") && !location.contains("err="),
                "expected success redirect, got: {location}"
            );
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].download_client_id, Some(sab));
        }

        #[tokio::test]
        async fn no_pin_skips_validation() {
            // The "(use default)" path — empty download_client_id —
            // bypasses the protocol guard since there's no client
            // to validate against. Default-routing happens at grab
            // time per the existing pin-resolution chain.
            let db = in_memory_pool().await;
            let _ = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let mut form = upsert_form("torznab", 0);
            form.download_client_id = None;
            let resp =
                settings_indexers_upsert(State(state.clone()), HxRequest(false), Form(form)).await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=") && !location.contains("err="),
                "expected success redirect, got: {location}"
            );
        }

        #[tokio::test]
        async fn db_error_during_pin_lookup_fails_closed() {
            // PR 112 review #1 (4th pass) — a transient DB error on
            // the protocol-pin lookup must NOT silently skip the
            // gate (the prior `if let Ok(Some(row))` shape did this).
            // Provoke the error by closing the pool, then confirm
            // upsert returns a "DB error" toast and refuses the save.
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            db.close().await;
            let resp = settings_indexers_upsert(
                State(state.clone()),
                HxRequest(false),
                Form(upsert_form("torznab", sab)),
            )
            .await;
            let location = extract_location(resp);
            assert!(
                location.contains("err=")
                    && (location.contains("DB%20error") || location.contains("DB+error")),
                "expected fail-closed err redirect mentioning DB error, got: {location}"
            );
        }

        #[tokio::test]
        async fn htmx_protocol_mismatch_returns_hx_redirect_not_303() {
            // Regression guard for the modal-redesign nesting bug:
            // an HTMX upsert that hits a validation error MUST return
            // 200 + `HX-Redirect: …` so htmx does a real client-side
            // navigation. A 303 Location redirect would silently get
            // followed by htmx's fetch and the resulting full-page
            // HTML would be swapped into the form's hx-target
            // (#indexer-section), producing a duplicate "Settings"
            // h2 + nested tabs inside the fieldset.
            let db = in_memory_pool().await;
            let (_qbit, sab) = seed_clients(&db).await;
            let state = build_test_app_state(db.clone(), None);
            let resp = settings_indexers_upsert(
                State(state.clone()),
                HxRequest(true),
                Form(upsert_form("torznab", sab)),
            )
            .await;
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::OK,
                "HTMX error path must be 200 (no Location 303) so htmx doesn't \
                 silent-follow + nested-swap the full settings page"
            );
            let hx_redirect = resp
                .headers()
                .get("HX-Redirect")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(
                hx_redirect.contains("tab=indexers") && hx_redirect.contains("err="),
                "HTMX error path must carry HX-Redirect header pointing back to the \
                 Indexers tab with the err flash; got: {hx_redirect:?}"
            );
            // Sanity: still rejected the save.
            let rows = crate::models::indexers::list_all(&state.db).await.unwrap();
            assert!(
                rows.is_empty(),
                "save must be rejected on protocol mismatch"
            );
        }
    }

    /// Toast wording is user-facing — `?msg=Saved` was the
    /// pre-PR-108 default and didn't tell the user what
    /// happened. The current handler emits
    /// `Indexer '<name>' added` / `... updated` / `... deleted`
    /// so the toast reads naturally. These tests pin that
    /// surface in case a future refactor shortens or reformats
    /// the message.
    mod toast_format {
        use super::super::*;
        use crate::models::indexers::{IndexerForm, KIND_TORZNAB, insert};
        use crate::test_support::{build_test_app_state, in_memory_pool};
        use axum::extract::{Form, State};

        fn extract_location(resp: axum::response::Response) -> String {
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_default()
        }

        fn upsert_form(id: Option<i64>, name: &str) -> IndexerUpsertForm {
            IndexerUpsertForm {
                id,
                name: name.to_string(),
                kind: "torznab".to_string(),
                url: "https://prowlarr.local/1/api".to_string(),
                api_key: "k".to_string(),
                priority: Some("25".to_string()),
                enabled: Some("on".to_string()),
                is_private_tracker: None,
                seed_ratio: None,
                seed_time_minutes: None,
                min_seeders: Some("1".to_string()),
                request_timeout_secs: None,
                download_client_id: None,
                rss_enabled: None,
                categories: None,
            }
        }

        #[tokio::test]
        async fn upsert_insert_toast_names_added_indexer() {
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let resp = settings_indexers_upsert(
                State(state),
                HxRequest(false),
                Form(upsert_form(None, "Test Indexer")),
            )
            .await;
            let location = extract_location(resp);
            // `'` percent-encodes to %27 via urlencoding::encode.
            assert!(
                location.contains("msg=Indexer%20%27Test%20Indexer%27%20added")
                    || location.contains("msg=Indexer+%27Test+Indexer%27+added"),
                "expected descriptive 'added' toast in redirect URL; got: {location}"
            );
        }

        #[tokio::test]
        async fn upsert_update_toast_names_updated_indexer() {
            let db = in_memory_pool().await;
            // Seed an existing row so the update branch fires.
            let row_id = insert(
                &db,
                IndexerForm {
                    name: "Original Name",
                    kind: KIND_TORZNAB,
                    url: "https://prowlarr.local/1/api",
                    api_key: "k",
                    priority: 25,
                    enabled: true,
                    is_private_tracker: false,
                    seed_ratio: None,
                    seed_time_minutes: None,
                    min_seeders: 1,
                    request_timeout_secs: None,
                    download_client_id: None,
                    rss_enabled: false,
                    categories: "",
                },
            )
            .await
            .expect("seed indexer");
            let state = build_test_app_state(db, None);
            let resp = settings_indexers_upsert(
                State(state),
                HxRequest(false),
                Form(upsert_form(Some(row_id), "Renamed")),
            )
            .await;
            let location = extract_location(resp);
            assert!(
                location.contains("msg=Indexer%20%27Renamed%27%20updated")
                    || location.contains("msg=Indexer+%27Renamed%27+updated"),
                "expected 'updated' toast naming the new value; got: {location}"
            );
        }

        #[tokio::test]
        async fn delete_toast_names_removed_indexer() {
            let db = in_memory_pool().await;
            let row_id = insert(
                &db,
                IndexerForm {
                    name: "Doomed",
                    kind: KIND_TORZNAB,
                    url: "https://prowlarr.local/1/api",
                    api_key: "k",
                    priority: 25,
                    enabled: true,
                    is_private_tracker: false,
                    seed_ratio: None,
                    seed_time_minutes: None,
                    min_seeders: 1,
                    request_timeout_secs: None,
                    download_client_id: None,
                    rss_enabled: false,
                    categories: "",
                },
            )
            .await
            .expect("seed indexer");
            let state = build_test_app_state(db, None);
            let resp = settings_indexers_delete(
                State(state),
                axum_htmx::HxRequest(false),
                Form(IndexerDeleteForm { id: row_id }),
            )
            .await;
            // `delete` returns Redirect (not Response); `IntoResponse`
            // turns it into a Response that has the Location header.
            use axum::response::IntoResponse;
            let resp = resp.into_response();
            let location = extract_location(resp);
            assert!(
                location.contains("msg=Indexer%20%27Doomed%27%20deleted")
                    || location.contains("msg=Indexer+%27Doomed%27+deleted"),
                "expected 'deleted' toast naming the removed row; got: {location}"
            );
        }

        #[tokio::test]
        async fn delete_toast_falls_back_to_id_for_missing_row() {
            // A delete for a row that no longer exists (race or
            // stale tab) reaches the success path because SQLite's
            // `DELETE WHERE id = ?` is a no-op success on a
            // missing row, not an error. The handler's
            // pre-delete `get_by_id(...)` returns None, so
            // `display_name` falls back to `format!("id={}", id)`
            // and the toast becomes "Indexer 'id=9999' deleted".
            // The positive assertion below pins that fallback —
            // a future change that returned `Err(NotFound)`
            // for a missing row, or that dropped the id-fallback,
            // would surface here instead of slipping by under a
            // weaker `!contains("''")` check.
            let db = in_memory_pool().await;
            let state = build_test_app_state(db, None);
            let resp = settings_indexers_delete(
                State(state),
                axum_htmx::HxRequest(false),
                Form(IndexerDeleteForm { id: 9999 }),
            )
            .await;
            use axum::response::IntoResponse;
            let resp = resp.into_response();
            let location = extract_location(resp);
            // `=` percent-encodes to `%3D` (uppercase via
            // `urlencoding::encode`); accept lowercase too in
            // case the encoder ever changes.
            assert!(
                location.contains("Indexer%20%27id%3D9999%27%20deleted")
                    || location.contains("Indexer+%27id%3D9999%27+deleted")
                    || location.contains("Indexer%20%27id%3d9999%27%20deleted")
                    || location.contains("Indexer+%27id%3d9999%27+deleted"),
                "expected id-based fallback in deleted-toast; got: {location}"
            );
        }
    }
}
