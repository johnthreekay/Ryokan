//! Settings → Connections → Downloads CRUD handlers (multi-client
//! refactor). Companion module to `models::download_clients`.
//!
//! Surface mirrors the indexers handler shape: form-driven upsert +
//! delete + set-default that redirect back to the Connections tab.
//! A separate JSON test endpoint at `/api/download-clients/test`
//! lets the user verify a configuration before saving without
//! mutating any DB row.

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::models::download_clients::{
    DownloadClientForm, DownloadClientRow, delete, get_by_id, insert, list_all, set_default, update,
};
use crate::models::log::LogCategory;
use crate::services::download_client::{
    DownloadClient, deluge, qbittorrent, rtorrent, sabnzbd, transmission,
};
use crate::services::logger;

/// `kind` discriminators accepted on the form. Mirrors the values
/// `services::download_client::rebuild_clients_cache` dispatches on
/// — keep these strings in sync if a new client is added.
const KIND_QBITTORRENT: &str = "qbittorrent";
const KIND_DELUGE: &str = "deluge";
const KIND_TRANSMISSION: &str = "transmission";
const KIND_RTORRENT: &str = "rtorrent";
const KIND_SABNZBD: &str = "sabnzbd";

fn is_known_kind(kind: &str) -> bool {
    matches!(
        kind,
        KIND_QBITTORRENT | KIND_DELUGE | KIND_TRANSMISSION | KIND_RTORRENT | KIND_SABNZBD
    )
}

/// Pretty-print the wire `kind` discriminator for the per-card badge
/// in `templates/partials/settings/download_clients/list.html`.
/// Public because Askama calls it via the `crate::handlers::...`
/// path from the template.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        KIND_QBITTORRENT => "qBittorrent",
        KIND_DELUGE => "Deluge",
        KIND_TRANSMISSION => "Transmission",
        KIND_RTORRENT => "rTorrent",
        KIND_SABNZBD => "SABnzbd",
        _ => "Unknown",
    }
}

/// Per-kind copy (placeholders, hint text, label names, visibility)
/// for the Add/Edit form body. Mirrors `DC_KIND_COPY` in
/// `static/js/settings.js` — keep both in lockstep when a new kind
/// lands or copy is updated. Server-rendered into the templates so
/// the modal opens with the kind-correct shape on first paint;
/// without this the form rendered the qBit-style defaults and the
/// JS path swapped them in async on `htmx:afterSettle`, which read
/// as a structural flash on Edit-on-SAB / Edit-on-Deluge clicks.
/// JS still owns the live kind-flip case (user toggles the dropdown
/// after the modal opens).
pub struct DcKindCopy {
    pub url_placeholder: &'static str,
    pub url_hint: &'static str,
    pub username_visible: bool,
    pub username_hint: &'static str,
    pub password_label: &'static str,
    /// HTML `input type` — `password` masks, `text` reveals (used
    /// for SAB API keys where visual verification of the pasted
    /// value is more useful than masking).
    pub password_type: &'static str,
    pub password_hint: &'static str,
    pub label_label: &'static str,
    pub label_hint: &'static str,
}

pub fn copy_for_kind(kind: &str) -> DcKindCopy {
    match kind {
        KIND_DELUGE => DcKindCopy {
            url_placeholder: "http://localhost:8112",
            url_hint: "Point at Deluge's Web UI base.",
            username_visible: false,
            username_hint: "",
            password_label: "Password",
            password_type: "password",
            password_hint: "Deluge Web UI password. Deluge has no per-user auth at the API layer; the password is the only credential.",
            label_label: "Label",
            label_hint: "Deluge's Label plugin tag. The plugin must be enabled; Ryokan auto-enables it on first connect when Label shows up in available_plugins but not enabled_plugins.",
        },
        KIND_TRANSMISSION => DcKindCopy {
            url_placeholder: "http://localhost:9091",
            url_hint: "Point at Transmission's RPC endpoint base.",
            username_visible: true,
            username_hint: "Transmission HTTP Basic auth user (matches rpc-username in settings.json).",
            password_label: "Password",
            password_type: "password",
            password_hint: "Transmission HTTP Basic auth password (matches rpc-password in settings.json).",
            label_label: "Label",
            label_hint: "Transmission native label (4.x+). On 3.x and earlier Ryokan falls back to a save-path prefix for scoping.",
        },
        KIND_RTORRENT => DcKindCopy {
            url_placeholder: "http://localhost/RPC2",
            url_hint: "Point at rtorrent's XML-RPC endpoint (typically /RPC2 under the SCGI / nginx proxy).",
            username_visible: true,
            username_hint: "HTTP Basic auth user if the RPC endpoint is fronted by nginx with auth_basic. Leave blank for unauthenticated RPC.",
            password_label: "Password",
            password_type: "password",
            password_hint: "HTTP Basic auth password matching the username above.",
            label_label: "Label",
            label_hint: "Sets the custom1 field on every added torrent (the ruTorrent label convention). Ryokan filters list_scoped by this tag.",
        },
        KIND_SABNZBD => DcKindCopy {
            url_placeholder: "http://localhost:8080",
            url_hint: "Point at SABnzbd's Web UI base. Ryokan appends /api. If your SAB has URL_BASE set (e.g. /sabnzbd), include it: http://host:8080/sabnzbd.",
            username_visible: false,
            username_hint: "",
            password_label: "API Key",
            password_type: "text",
            password_hint: "SABnzbd's API key. Find it in SABnzbd \u{2192} Config \u{2192} General \u{2192} Security \u{2192} API Key.",
            label_label: "Category",
            label_hint: "SAB category. Determines the post-processing target directory. Ryokan filters list_scoped by category so it only sees jobs it added.",
        },
        // qBit + unknown fall through to the qBit default. Matches the
        // JS map's `DC_KIND_COPY[kind] || DC_KIND_COPY.qbittorrent`
        // shape.
        _ => DcKindCopy {
            url_placeholder: "http://localhost:8080",
            url_hint: "Point at qBittorrent's Web UI base. Ryokan handles the API path internally.",
            username_visible: true,
            username_hint: "qBit's Web UI username (default is admin).",
            password_label: "Password",
            password_type: "password",
            password_hint: "qBit's Web UI password. qBittorrent 4.6.1+ generates a random temporary password on first start. Pre-4.6.1's default password is 'adminadmin'.",
            label_label: "Category",
            label_hint: "qBit category Ryokan tags every torrent with. Determines scoping (Ryokan only sees torrents in this category) AND the post-processing target directory if qBit's category-rule has one set.",
        },
    }
}

/// Section partial — the entire card list + add slot wrapped in
/// `#dc-section`. Every successful HTMX action (upsert / delete /
/// set-default) returns this so a single swap re-renders the
/// whole tab body without a page reload.
#[derive(Template)]
#[template(path = "partials/settings/download_clients/list.html")]
struct DownloadClientsListPartial {
    rows: Vec<DownloadClientRow>,
    // Per-protocol "first client" flags consumed by the inline
    // include of `add_form_body.html` at the bottom of `list.html`
    // (the section partial pre-renders the Add form body so the
    // modal opens fast on first click). Same data the GET-only
    // `DownloadClientAddFormPartial` carries; populated from the
    // section's `rows` so a fresh install pre-checks Default on
    // both protocols.
    first_torrent_client: bool,
    first_usenet_client: bool,
    /// Per-row cached probe status, keyed by `download_clients.id`.
    /// Populated from `AppState.dc_status_cache` for entries fresh
    /// within `DC_STATUS_CACHE_TTL`. When the row's id is present
    /// here, the template renders the full pill server-side (same
    /// shape as `partials/settings/download_clients/status_pill.html`)
    /// instead of emitting the `hx-trigger="load"` placeholder. When
    /// absent (cold cache, expired entry), the placeholder + JS-driven
    /// probe runs as before.
    cached_status: std::collections::HashMap<i64, crate::DcStatusEntry>,
}

impl DownloadClientsListPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Edit form body — the inner content of the shared modal when
/// the user is editing an existing row. Returned by `GET
/// /settings/download-clients/{id}/edit-form`; swapped into
/// `#dc-modal-body` (innerHTML) when the user clicks a card. The
/// surrounding modal-backdrop / modal-header come from the
/// section partial; this is just the form.
#[derive(Template)]
#[template(path = "partials/settings/download_clients/edit_form_body.html")]
struct DownloadClientEditFormPartial {
    row: DownloadClientRow,
    /// Same per-protocol "first client" flags as the Add partial.
    /// Used by the Default-checkbox initial-render condition AND by
    /// the JS kind-relabel helper after a kind change. Computed
    /// from the current DB state (including this row's contribution
    /// — if this row IS the default torrent, `first_torrent_client`
    /// resolves to false, and the template-side condition
    /// `row.is_default || first_<protocol>_client` lands on
    /// row.is_default for the checked state).
    first_torrent_client: bool,
    first_usenet_client: bool,
}

impl DownloadClientEditFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Inline error blurb rendered into `#dc-modal-body` when the edit
/// form fetch fails (404 stale-id, 500 DB error). Returns 200 + an
/// error partial INSTEAD of the prior 4xx/5xx because htmx 2.x's
/// default error policy skips the swap on non-200, leaving the
/// modal body showing the previous form while the modal title says
/// "Editing FooClient" — silent breakage.
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

/// Add form body — symmetric with the edit form but for fresh
/// inserts. Returned by `GET /api/download-clients/add-form`;
/// swapped into `#dc-modal-body` when the user clicks the "+
/// Add download client" tile.
///
/// `first_torrent_client` / `first_usenet_client` drive the
/// auto-check on the "Default client" checkbox. Per-protocol so the
/// first SAB added gets default-checked even when a torrent default
/// already exists (and vice versa) — without this, a user adding
/// SAB after qBit would end up with no usenet default until they
/// manually clicked Set Default. The kind-relabel JS in
/// `static/js/settings.js` reads both flags via data attributes on
/// the form and toggles the checkbox state when the user flips the
/// kind dropdown.
#[derive(Template)]
#[template(path = "partials/settings/download_clients/add_form_body.html")]
struct DownloadClientAddFormPartial {
    first_torrent_client: bool,
    first_usenet_client: bool,
}

impl DownloadClientAddFormPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Status pill — probed live via the cached client's `test()`. Lives
/// on each card and loads via `hx-trigger="load"`. Always 200 so
/// HTMX swaps the pill in even on error (the failure pill carries
/// the error message in its title attribute).
#[derive(Template)]
#[template(path = "partials/settings/download_clients/status_pill.html")]
struct DownloadClientStatusPillPartial {
    /// `Some(version)` on success — combined with `kind_label`
    /// to render "qBittorrent 5.1.4". `None` on error.
    version: Option<String>,
    /// Tooltip on failure (`title=`); ignored on success.
    error: String,
    /// Pre-formatted kind label so the template doesn't need
    /// to call the helper itself.
    kind_label: &'static str,
}

impl DownloadClientStatusPillPartial {
    fn into_html_ok(self) -> Response {
        Html(self.render().unwrap_or_default()).into_response()
    }
}

/// Helper — load the current rows and render the section partial.
/// Used by the success path of every state-changing endpoint plus
/// the `/api/download-clients/section` cancel-edit refresh route.
async fn render_section(state: &AppState) -> Response {
    let rows = list_all(&state.db).await.unwrap_or_default();
    use crate::models::download_clients::protocol_for_kind;
    let first_torrent_client = !rows
        .iter()
        .any(|r| r.is_default && protocol_for_kind(&r.kind) == Some("torrent"));
    let first_usenet_client = !rows
        .iter()
        .any(|r| r.is_default && protocol_for_kind(&r.kind) == Some("usenet"));
    // Snapshot fresh entries from the per-process cache so the
    // template can render the probed pill server-side and skip the
    // hx-trigger="load" placeholder for those cards. Stale entries
    // (older than DC_STATUS_CACHE_TTL) and disabled rows fall through
    // to the placeholder path. The lock window is tiny — a single
    // lookup per row, no async work — so a std Mutex is fine.
    let cached_status = snapshot_fresh_dc_status(&state.dc_status_cache);
    DownloadClientsListPartial {
        rows,
        first_torrent_client,
        first_usenet_client,
        cached_status,
    }
    .into_html_ok()
}

/// Pre-warm the DC status cache at server startup so the first-paint
/// of Settings → Connections doesn't flash through the "Probing…"
/// placeholder. Probes every client in the active pool in parallel
/// (one tokio task per client) and writes results into the cache.
/// Failures are silently captured as "Unreachable" entries; the
/// cache reflects current reality and the user sees a stable pill on
/// first visit either way.
///
/// Called once from `main.rs` after `rebuild_clients_cache` populates
/// the pool. Runs in the background so a slow probe (e.g. SAB on a
/// tarpitting tracker) can't delay the listener bind.
pub async fn prewarm_dc_status_cache(
    pool_cache: &crate::DownloadClientsCache,
    status_cache: &crate::DcStatusCache,
) {
    let pool = pool_cache.read().await.clone();
    let probes: Vec<_> = pool
        .clients
        .iter()
        .map(|(id, client)| {
            let id = *id;
            let client = client.clone();
            async move { (id, client.test().await) }
        })
        .collect();
    let results = futures_util::future::join_all(probes).await;
    let mut guard = status_cache.lock().unwrap();
    let now = std::time::Instant::now();
    for (id, res) in results {
        let entry = match res {
            Ok(version) => crate::DcStatusEntry {
                version: Some(version),
                error: String::new(),
            },
            Err(e) => crate::DcStatusEntry {
                version: None,
                error: e,
            },
        };
        guard.insert(id, (now, entry));
    }
}

/// Walk the DC status cache and return only the entries still within
/// TTL. Stale entries get evicted on the same pass so the map can't
/// grow unboundedly across hours of use without a server restart
/// (the entries themselves are small but a never-evicting cache is a
/// latent leak). `pub(super)` so the parent settings module's full-
/// page render path can read the same cache for its inline include
/// of `download_clients/list.html`.
pub(super) fn snapshot_fresh_dc_status(
    cache: &crate::DcStatusCache,
) -> std::collections::HashMap<i64, crate::DcStatusEntry> {
    use std::time::Instant;
    let mut guard = cache.lock().unwrap();
    let now = Instant::now();
    let ttl = crate::DC_STATUS_CACHE_TTL;
    guard.retain(|_, (probed_at, _)| now.duration_since(*probed_at) < ttl);
    guard
        .iter()
        .map(|(id, (_, entry))| (*id, entry.clone()))
        .collect()
}

/// Form payload for create/update. `id == None` creates a new row;
/// `Some(n)` updates row `n`. Empty / unsanitized strings — the
/// model layer trims and trims_end_matches('/') the URL +
/// download_path before persisting.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientUpsertForm {
    pub id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Label / category — qBit category, Deluge label, etc.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub download_path: String,
    /// Checkbox semantics: only POSTed when checked.
    pub enabled: Option<String>,
    pub is_default: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientIdForm {
    pub id: i64,
}

#[utoipa::path(
    post,
    path = "/settings/download-clients/upsert",
    tag = "Settings",
    summary = "Create or update a download client",
    description = "Form-driven upsert for the Connections → Downloads list. Creates a new row when `id` is omitted; updates the row identified by `id` otherwise. Validates kind ∈ {qbittorrent, deluge, transmission, rtorrent, sabnzbd}. Refreshes the in-process pool so the new/edited client is usable on the next grab without a process restart.",
    responses(
        (status = 303, description = "Redirect back to the Connections tab"),
    ),
)]
pub async fn settings_download_clients_upsert(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DownloadClientUpsertForm>,
) -> Response {
    // Validation errors route through `htmx_aware_redirect`: HTMX
    // callers get `HX-Redirect: /settings?tab=downloads&err=...` so
    // htmx triggers a real client-side navigation; non-HTMX callers
    // get a standard 303. The pre-Phase-A comment claiming "htmx
    // skips the swap on 4xx so a 303 works" was wrong post-boost —
    // boost intercepts every form-POST, follows the 3xx via fetch,
    // and inline-swaps the destination's HTML into the form's
    // target. The HX-Redirect header is the correct shape.
    let name = form.name.trim();
    if name.is_empty() {
        return crate::handlers::responses::htmx_aware_redirect(
            is_htmx,
            "/settings?tab=downloads&err=Name+required",
        );
    }
    if !is_known_kind(&form.kind) {
        return crate::handlers::responses::htmx_aware_redirect(
            is_htmx,
            "/settings?tab=downloads&err=Invalid+client+kind",
        );
    }
    let url = form.url.trim();
    if url.is_empty() {
        return crate::handlers::responses::htmx_aware_redirect(
            is_htmx,
            "/settings?tab=downloads&err=URL+required",
        );
    }
    // Permissive parse — each client impl normalizes the URL itself
    // (prepending `http://` for scheme-less local addresses), so we
    // only reject inputs the url crate can't make sense of at all.
    if reqwest::Url::parse(url).is_err() && reqwest::Url::parse(&format!("http://{url}")).is_err() {
        return crate::handlers::responses::htmx_aware_redirect(
            is_htmx,
            "/settings?tab=downloads&err=Invalid+URL+syntax",
        );
    }

    let payload = DownloadClientForm {
        name,
        kind: form.kind.as_str(),
        url,
        username: form.username.trim(),
        password: &form.password,
        label: form.label.trim(),
        download_path: form.download_path.as_str(),
        enabled: form.enabled.is_some(),
        is_default: form.is_default.is_some(),
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
                &format!("Download client {verb}: {name} ({})", form.kind),
                &format!("id={id}"),
            )
            .await;
            crate::services::download_client::rebuild_clients_cache(
                &state.download_clients,
                &state.db,
            )
            .await;
            // Wipe the status cache so a freshly-edited row re-probes
            // on its next render rather than showing a stale "ok" pill
            // against the old credentials for up to DC_STATUS_CACHE_TTL.
            state.dc_status_cache.lock().unwrap().clear();
            if is_htmx {
                // Re-render the whole section in one swap — picks up
                // the new card, the moved "default" badge if the user
                // flipped that flag, and a refreshed "+ Add" button
                // (since the section partial re-emits the slot).
                render_section(&state).await
            } else {
                let msg =
                    urlencoding::encode(&format!("Download client '{name}' {verb}")).into_owned();
                Redirect::to(&format!("/settings?tab=downloads&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client upsert failed",
                &e.to_string(),
            )
            .await;
            crate::handlers::responses::htmx_aware_redirect(
                is_htmx,
                "/settings?tab=downloads&err=Save+failed",
            )
        }
    }
}

#[utoipa::path(
    post,
    path = "/settings/download-clients/delete",
    tag = "Settings",
    summary = "Delete a download client",
    description = "Removes the download_clients row by id. Indexer pins (`indexers.download_client_id`) and the Nyaa pin (`config.nyaa_download_client_id`) that referenced it are NULLed in the same transaction so dangling pins don't silently fall through to the default at grab time.",
    responses(
        (status = 303, description = "Redirect back to the Connections tab"),
    ),
)]
pub async fn settings_download_clients_delete(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DownloadClientIdForm>,
) -> Response {
    let display_name = crate::models::download_clients::get_by_id(&state.db, form.id)
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
                &format!("Download client deleted: {display_name} (id={})", form.id),
                "",
            )
            .await;
            crate::services::download_client::rebuild_clients_cache(
                &state.download_clients,
                &state.db,
            )
            .await;
            // Wipe the status cache so a freshly-edited row re-probes
            // on its next render rather than showing a stale "ok" pill
            // against the old credentials for up to DC_STATUS_CACHE_TTL.
            state.dc_status_cache.lock().unwrap().clear();
            crate::services::indexers::refresh_cache_in_place(&state.indexers, &state.db).await;
            // HTMX redesign (#129 follow-up) — re-render the whole
            // section so the "+ Add" button + empty-state CTA both
            // surface correctly when the table goes from N to 0.
            if is_htmx {
                render_section(&state).await
            } else {
                let msg = urlencoding::encode(&format!("Download client '{display_name}' deleted"))
                    .into_owned();
                Redirect::to(&format!("/settings?tab=downloads&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client delete failed",
                &e.to_string(),
            )
            .await;
            if is_htmx {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            } else {
                Redirect::to("/settings?tab=downloads&err=Delete+failed").into_response()
            }
        }
    }
}

#[utoipa::path(
    post,
    path = "/settings/download-clients/set-default",
    tag = "Settings",
    summary = "Mark a download client as the default",
    description = "Flips `is_default = 1` on the targeted row and clears the flag on every other row in one transaction. Used by the per-row \"Set default\" button on the Connections → Downloads list.",
    responses(
        (status = 303, description = "Redirect back to the Connections tab"),
    ),
)]
pub async fn settings_download_clients_set_default(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DownloadClientIdForm>,
) -> Response {
    let display_name = get_by_id(&state.db, form.id)
        .await
        .ok()
        .flatten()
        .map(|r| r.name)
        .unwrap_or_else(|| format!("id={}", form.id));
    match set_default(&state.db, form.id).await {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                &format!(
                    "Default download client set: {display_name} (id={})",
                    form.id
                ),
                "",
            )
            .await;
            crate::services::download_client::rebuild_clients_cache(
                &state.download_clients,
                &state.db,
            )
            .await;
            // Wipe the status cache so a freshly-edited row re-probes
            // on its next render rather than showing a stale "ok" pill
            // against the old credentials for up to DC_STATUS_CACHE_TTL.
            state.dc_status_cache.lock().unwrap().clear();
            if is_htmx {
                // Section re-render so the "default" badge moves
                // between cards in one swap. Per-card swap would
                // require an OOB pair (clear old, set new) and
                // get fragile fast.
                render_section(&state).await
            } else {
                let msg = urlencoding::encode(&format!("'{display_name}' is now the default"))
                    .into_owned();
                Redirect::to(&format!("/settings?tab=downloads&msg={msg}")).into_response()
            }
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Download client set-default failed",
                &e.to_string(),
            )
            .await;
            crate::handlers::responses::htmx_aware_redirect(
                is_htmx,
                "/settings?tab=downloads&err=Save+failed",
            )
        }
    }
}

/// Form payload for the inline "Test connection" button on the
/// Connections → Downloads add/edit form. Doesn't touch the DB.
/// `#[serde(default)]` on every field — the surrounding upsert form
/// has more inputs than this endpoint cares about (id, name,
/// is_default, enabled, …) and `hx-include="closest form"` will pull
/// all of them. Serde drops unknown fields by default, so the extras
/// are silently ignored.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DownloadClientTestForm {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub label: String,
}

#[utoipa::path(
    post,
    path = "/api/download-clients/test",
    tag = "System",
    summary = "Test a download client configuration",
    description = "Instantiates the requested client kind with the provided credentials and runs \
                   its `test()` method. Doesn't persist anything. The Connections → Downloads \
                   add/edit form calls this before saving so the user gets immediate feedback on \
                   bad URLs / wrong passwords / missing categories. Phase 1.5 grab-bag (issue \
                   #129) — returns an HTML fragment for HTMX swap into the test-result span; \
                   always 200 so HTMX renders both success and failure (default error policy in \
                   2.x is skip-the-swap on 4xx/5xx).",
    request_body = DownloadClientTestForm,
    responses(
        (status = 200, description = "Result rendered as an HTML fragment (success or failure)"),
    ),
)]
pub async fn settings_download_clients_test(Form(form): Form<DownloadClientTestForm>) -> Response {
    let url = form.url.trim();
    if url.is_empty() {
        return test_result_response(false, "URL required");
    }
    let client: std::sync::Arc<dyn DownloadClient> = match form.kind.as_str() {
        KIND_QBITTORRENT => std::sync::Arc::new(qbittorrent::QbitClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        KIND_DELUGE => std::sync::Arc::new(deluge::DelugeClient::new(
            url,
            &form.password,
            form.label.trim(),
        )),
        KIND_TRANSMISSION => std::sync::Arc::new(transmission::TransmissionClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        KIND_RTORRENT => std::sync::Arc::new(rtorrent::RtorrentClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        KIND_SABNZBD => std::sync::Arc::new(sabnzbd::SabClient::new(
            url,
            form.username.trim(),
            &form.password,
            form.label.trim(),
        )),
        other => {
            return test_result_response(false, &format!("Unknown client kind: {other}"));
        }
    };

    match client.test().await {
        Ok(version) => test_result_response(true, &format!("Connected: {version}")),
        Err(err) => test_result_response(false, &err),
    }
}

/// Build the HTMX response for a Test-connection probe. Empty body
/// (the button has no hx-swap target now), `HX-Trigger` header carries
/// the result so the body-level listener in `static/js/settings.js`
/// fires a toast. This replaces the previous inline-span swap shape
/// — the inline span lived between the Test button and Cancel/Save
/// in the modal footer and grew its height by one line whenever a
/// long error came back, jittering the button row. Toasts surface
/// at the top of the viewport regardless of result length.
fn test_result_response(ok: bool, message: &str) -> Response {
    let payload = serde_json::json!({
        "ryokan-dc-test-result": {
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
            .unwrap_or_else(|_| "ryokan-dc-test-result".parse().unwrap()),
    );
    resp
}

// ── HTMX partial-fragment endpoints (Phase 7 follow-up) ────────────
//
// The Settings → Download Clients tab is rendered through three
// partials: `list.html` (the whole section, swapped on every state-
// changing action), `edit_form.html` (one card swapped in place
// when the user clicks Edit), and `add_form.html` (the slot
// expansion when the user clicks "+ Add"). These read-only
// endpoints surface those partials so HTMX can swap them in without
// a full page reload.

#[utoipa::path(
    get,
    path = "/api/download-clients/section",
    tag = "Settings",
    summary = "Render the Download Clients section partial",
    description = "Returns the cards-list + add-slot fragment that lives at #dc-section on the Download Clients tab. Used by Cancel buttons inside inline edit / add forms to restore the section to its baseline rendering without losing scroll position.",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_download_clients_section(State(state): State<AppState>) -> Response {
    render_section(&state).await
}

#[utoipa::path(
    get,
    path = "/settings/download-clients/{id}/edit-form",
    tag = "Settings",
    summary = "Render the edit form body for one download client",
    description = "Returns the edit_form_body.html fragment for the targeted row, prefilled with current values. Swapped into the shared modal body (`#dc-modal-body`) when the user clicks a card. Returns 404 when the row no longer exists (e.g. concurrent delete from another tab).",
    responses(
        (status = 200, description = "HTML fragment"),
        (status = 404, description = "Row not found"),
    ),
)]
pub async fn settings_download_clients_edit_form(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    match get_by_id(&state.db, id).await {
        Ok(Some(row)) => {
            // Same per-protocol "first client" probe the Add form
            // does, scoped to ALL rows (including this one). The
            // template's checkbox condition is
            // `row.is_default || first_<protocol>_client`, so when
            // this row IS the default the first-flag resolves to
            // false and the row's own state takes precedence; when
            // this row isn't the default but no other row of the
            // same protocol is, the first-flag flips on and the
            // checkbox auto-checks.
            let rows = list_all(&state.db).await.unwrap_or_default();
            use crate::models::download_clients::protocol_for_kind;
            let first_torrent_client = !rows
                .iter()
                .any(|r| r.is_default && protocol_for_kind(&r.kind) == Some("torrent"));
            let first_usenet_client = !rows
                .iter()
                .any(|r| r.is_default && protocol_for_kind(&r.kind) == Some("usenet"));
            DownloadClientEditFormPartial {
                row,
                first_torrent_client,
                first_usenet_client,
            }
            .into_html_ok()
        }
        Ok(None) => ModalErrorPartial {
            message:
                "This download client no longer exists. It may have been deleted in another tab."
                    .into(),
        }
        .into_html_ok(),
        Err(e) => ModalErrorPartial {
            message: format!("Failed to load download client: {e}"),
        }
        .into_html_ok(),
    }
}

#[utoipa::path(
    get,
    path = "/api/download-clients/add-form",
    tag = "Settings",
    summary = "Render the add form body",
    description = "Returns the add_form_body.html fragment swapped into the shared modal body (`#dc-modal-body`) when the user clicks the \"+ Add download client\" tile. The default-checkbox is pre-checked when no clients exist yet — first-row default is required or grabs surface \"no download client configured\" at routing time.",
    responses(
        (status = 200, description = "HTML fragment"),
    ),
)]
pub async fn settings_download_clients_add_form(State(state): State<AppState>) -> Response {
    // Per-protocol "no current default" probe — auto-checks Default
    // when no row of this protocol is marked is_default, so adding
    // SAB when no usenet default exists picks it up automatically
    // (and same for the first torrent client). Semantic-mirror with
    // the Edit partial so behavior is identical across the two
    // forms. DB-error fallback is false-on-both so a transient sqlx
    // hiccup doesn't silently steal an existing default's flag.
    let rows = list_all(&state.db).await.unwrap_or_default();
    use crate::models::download_clients::protocol_for_kind;
    let first_torrent_client = !rows
        .iter()
        .any(|r| r.is_default && protocol_for_kind(&r.kind) == Some("torrent"));
    let first_usenet_client = !rows
        .iter()
        .any(|r| r.is_default && protocol_for_kind(&r.kind) == Some("usenet"));
    DownloadClientAddFormPartial {
        first_torrent_client,
        first_usenet_client,
    }
    .into_html_ok()
}

#[utoipa::path(
    get,
    path = "/api/download-clients/{id}/status",
    tag = "Settings",
    summary = "Probe live status (version) for one download client",
    description = "Calls the cached client's `test()` and returns a small status pill (`<span>`) carrying either the `kind version` text on success or `Unreachable` (with the error in `title=`) on failure. Each card on the Download Clients tab loads this on render via `hx-trigger=\"load\"`. Always 200 so HTMX swaps the pill in even on probe failure — the failure pill is the response, not the absence of one. Returns the not-in-pool pill (\"Unknown\") for ids the rebuild_clients_cache step skipped (disabled rows or unsupported kinds).",
    responses(
        (status = 200, description = "Status pill HTML fragment"),
    ),
)]
pub async fn settings_download_clients_status(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    // Look up the kind label first so even the "not in pool" path
    // can still say "qBittorrent" instead of "Unknown" — disabled
    // rows render with the right kind even though no probe runs.
    let row = get_by_id(&state.db, id).await.ok().flatten();
    let kind_label = row
        .as_ref()
        .map(|r| kind_label(&r.kind))
        .unwrap_or("Client");

    // Cache hit (entry within DC_STATUS_CACHE_TTL) → return the
    // cached pill without re-probing. Lets the placeholder swap
    // resolve in microseconds rather than the 50-500ms+ the
    // network probe takes, which is what the user perceived as
    // flashing on every boost-nav into Settings → Connections.
    // The render_section path also reads this cache and skips the
    // placeholder entirely; this branch handles cards whose
    // initial render predated the cache (cold first paint) or whose
    // cache entry expired between the placeholder render and the
    // hx-trigger="load" fire.
    {
        let mut guard = state.dc_status_cache.lock().unwrap();
        if let Some((probed_at, entry)) = guard.get(&id)
            && probed_at.elapsed() < crate::DC_STATUS_CACHE_TTL
        {
            return DownloadClientStatusPillPartial {
                version: entry.version.clone(),
                error: entry.error.clone(),
                kind_label,
            }
            .into_html_ok();
        }
        // Drop expired entry on the way through so the lookup-then-
        // probe path doesn't leave a dead row behind.
        guard.remove(&id);
    }

    // Pull the cached `Arc<dyn DownloadClient>` out of the pool
    // under the read lock and clone it; release the lock before
    // running the network probe so a slow client can't stall a
    // sibling card's render. Pool is rebuilt on every CRUD so a
    // freshly-edited row's probe runs against the new credentials
    // without a process restart.
    let pool = state.download_clients.read().await.clone();
    let client = pool.clients.get(&id).cloned();
    drop(pool);

    let (version, error) = match client {
        Some(c) => match c.test().await {
            Ok(v) => (Some(v), String::new()),
            Err(e) => {
                // Opportunistic DownloadClientUnreachable notification
                // with per-id 1h dedup. Per-row `kind` is the wire
                // discriminator we already resolved at the top of the
                // handler; pass it through to the event payload so a
                // user with multiple qBit instances can correlate the
                // ping. The "Client not in active pool" arm below is
                // a configuration state, not a reachability failure,
                // so it deliberately doesn't fire.
                if let Some(r) = row.as_ref() {
                    crate::services::notifications::emit_download_client_unreachable(
                        &state, id, &r.kind, &e,
                    )
                    .await;
                }
                (None, e)
            }
        },
        None => (
            None,
            "Client not in active pool (disabled or invalid kind)".into(),
        ),
    };

    // Write the result back into the cache so render_section
    // (and the next probe within TTL) skip the network round-trip.
    state.dc_status_cache.lock().unwrap().insert(
        id,
        (
            std::time::Instant::now(),
            crate::DcStatusEntry {
                version: version.clone(),
                error: error.clone(),
            },
        ),
    );

    DownloadClientStatusPillPartial {
        version,
        error,
        kind_label,
    }
    .into_html_ok()
}

/// Form payload for the small "Pin Nyaa to client" selector on
/// the Indexers tab. Empty string = NULL (use default).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct NyaaPinForm {
    pub download_client_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/settings/indexers/nyaa-pin",
    tag = "Settings",
    summary = "Pin or unpin Nyaa to a specific download client",
    description = "Sets `config.nyaa_download_client_id` to the selected client id, or NULL when no client is selected. The Indexers tab shows this as a small dropdown above the indexer list.",
    responses(
        (status = 303, description = "Redirect back to the Indexers tab"),
    ),
)]
pub async fn settings_indexers_nyaa_pin(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<NyaaPinForm>,
) -> Response {
    let pin: Option<i64> = form.download_client_id.as_deref().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<i64>().ok()
        }
    });
    // Protocol guard — Nyaa surfaces torrent magnets / .torrent URLs.
    // A SAB pin would resolve at grab time and immediately fail at
    // SAB's `mode=addurl` ("invalid NZB"). Refuse the save with a
    // clear toast instead.
    //
    // PR 112 review #1 (4th pass) — fail closed on transient DB
    // error. The earlier `if let Ok(Some(row))` shape silently
    // skipped the gate when get_by_id returned Err, which would
    // let a Nyaa→SAB pin slip through under a hiccup at save
    // time. Match Err explicitly. Ok(None) still permits (client
    // deleted between page-load and submit is intentional).
    if let Some(client_id) = pin {
        let row = match crate::models::download_clients::get_by_id(&state.db, client_id).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => None, // intentional: client deleted between page-load and submit
            Err(e) => {
                let msg = urlencoding::encode(&format!(
                    "Couldn't verify protocol pin (DB error: {e}); please retry."
                ))
                .into_owned();
                return crate::handlers::responses::htmx_aware_redirect(
                    is_htmx,
                    &format!("/settings?tab=indexers&err={msg}"),
                );
            }
        };
        if let Some(row) = row
            && let Some(client_proto) =
                crate::services::download_client::protocol_for_client_kind(&row.kind)
            && client_proto != "torrent"
        {
            let msg = urlencoding::encode(&format!(
                "Can't pin Nyaa to a {} client (Nyaa returns torrents; {} accepts {client_proto})",
                row.kind, row.kind
            ))
            .into_owned();
            return crate::handlers::responses::htmx_aware_redirect(
                is_htmx,
                &format!("/settings?tab=indexers&err={msg}"),
            );
        }
    }
    let result = sqlx::query("UPDATE config SET nyaa_download_client_id = ? WHERE id = 1")
        .bind(pin)
        .execute(&state.db)
        .await;
    match result {
        Ok(_) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Nyaa pin updated",
                &format!("download_client_id={pin:?}"),
            )
            .await;
            crate::handlers::responses::htmx_aware_redirect(
                is_htmx,
                "/settings?tab=indexers&msg=Nyaa+pin+updated",
            )
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Nyaa pin update failed",
                &e.to_string(),
            )
            .await;
            crate::handlers::responses::htmx_aware_redirect(
                is_htmx,
                "/settings?tab=indexers&err=Save+failed",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::download_clients::{DownloadClientForm, get_by_id, get_default, list_all};
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use axum::extract::{Form, State};

    fn upsert_form(id: Option<i64>, name: &str) -> DownloadClientUpsertForm {
        DownloadClientUpsertForm {
            id,
            name: name.to_string(),
            kind: "qbittorrent".to_string(),
            url: "http://localhost:8080".to_string(),
            username: "u".to_string(),
            password: "p".to_string(),
            label: "anime".to_string(),
            download_path: "/downloads".to_string(),
            enabled: Some("on".to_string()),
            is_default: None,
        }
    }

    fn extract_location(resp: axum::response::Response) -> String {
        resp.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn upsert_insert_persists_row_and_redirects_back() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(upsert_form(None, "Local qBit")),
        )
        .await;
        let location = extract_location(resp);
        assert!(location.contains("tab=downloads"));
        assert!(location.contains("msg="));

        let rows = list_all(&state.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Local qBit");
        assert_eq!(rows[0].kind, "qbittorrent");
    }

    #[tokio::test]
    async fn upsert_update_renames_existing_row() {
        let db = in_memory_pool().await;
        let id = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "Original",
                kind: "qbittorrent",
                url: "http://localhost:8080",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let _ = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(upsert_form(Some(id), "Renamed")),
        )
        .await;
        let row = get_by_id(&state.db, id).await.unwrap().unwrap();
        assert_eq!(row.name, "Renamed");
    }

    #[tokio::test]
    async fn upsert_rejects_invalid_kind() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let mut form = upsert_form(None, "Bad");
        form.kind = "premiumize".to_string();
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(form),
        )
        .await;
        let location = extract_location(resp);
        assert!(location.contains("err=Invalid+client+kind"));
        assert_eq!(list_all(&state.db).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn upsert_rejects_blank_name() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(upsert_form(None, "  ")),
        )
        .await;
        assert!(extract_location(resp).contains("err=Name+required"));
    }

    #[tokio::test]
    async fn upsert_rejects_malformed_url() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let mut form = upsert_form(None, "qbit");
        form.url = "not a url".into();
        let resp =
            settings_download_clients_upsert(State(state), axum_htmx::HxRequest(false), Form(form))
                .await;
        assert!(extract_location(resp).contains("err=Invalid+URL+syntax"));
    }

    #[tokio::test]
    async fn upsert_accepts_localhost_without_scheme() {
        // The picker's UX promise: typing `localhost:8085` works for
        // every client kind. Each impl's `normalize_base_url()`
        // prepends `http://` for local addresses; SAB used to be the
        // outlier (no normalize), which surfaced as "builder error
        // for url" at probe time. Pin the permissive shape so a
        // future stricter validator can't break the consistent UX.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let mut form = upsert_form(None, "sab");
        form.kind = "sabnzbd".into();
        form.url = "localhost:8085".into();
        let resp =
            settings_download_clients_upsert(State(state), axum_htmx::HxRequest(false), Form(form))
                .await;
        assert!(
            extract_location(resp).contains("msg="),
            "scheme-less localhost URL must be accepted; the client impl normalizes it"
        );
        let rows = list_all(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "row must persist with the user-entered URL");
    }

    #[tokio::test]
    async fn upsert_returns_section_partial_when_htmx() {
        // HTMX-driven create: response body is the `#dc-section`
        // partial rendered with the new row included, not a 303.
        // This keeps the inline add slot working without a full
        // page reload.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_download_clients_upsert(
            State(state.clone()),
            axum_htmx::HxRequest(true),
            Form(upsert_form(None, "Local qBit")),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("id=\"dc-section\""), "section root missing");
        assert!(
            html.contains("Local qBit"),
            "freshly-added row should appear in the response: {html}"
        );
    }

    #[tokio::test]
    async fn delete_removes_row_and_nulls_indexer_pins() {
        let db = in_memory_pool().await;
        let id = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "X",
                kind: "qbittorrent",
                url: "http://x",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        // Pin an indexer to the soon-to-be-deleted client.
        crate::models::indexers::insert(
            &db,
            crate::models::indexers::IndexerForm {
                name: "AB",
                kind: crate::models::indexers::KIND_TORZNAB,
                url: "https://prowlarr.local/1/api",
                api_key: "k",
                priority: 25,
                enabled: true,
                is_private_tracker: true,
                seed_ratio: None,
                seed_time_minutes: None,
                min_seeders: 0,
                request_timeout_secs: None,
                download_client_id: Some(id),
                rss_enabled: false,
                categories: "",
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let _ = settings_download_clients_delete(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(DownloadClientIdForm { id }),
        )
        .await;
        assert!(get_by_id(&state.db, id).await.unwrap().is_none());
        let pin: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM indexers WHERE name = 'AB'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(
            pin.is_none(),
            "indexer pin must be NULLed when client deleted"
        );
    }

    #[tokio::test]
    async fn set_default_promotes_one_row_and_demotes_others() {
        let db = in_memory_pool().await;
        let a = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "A",
                kind: "qbittorrent",
                url: "http://a",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        let b = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "B",
                kind: "deluge",
                url: "http://b",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        let state = build_test_app_state(db, None);
        let _ = settings_download_clients_set_default(
            State(state.clone()),
            axum_htmx::HxRequest(false),
            Form(DownloadClientIdForm { id: b }),
        )
        .await;
        let default_row = get_default(&state.db).await.unwrap().unwrap();
        assert_eq!(default_row.id, b);
        let a_row = get_by_id(&state.db, a).await.unwrap().unwrap();
        assert!(!a_row.is_default);
    }

    #[tokio::test]
    async fn nyaa_pin_persists_id_when_set_and_clears_when_blank() {
        let db = in_memory_pool().await;
        let id = crate::models::download_clients::insert(
            &db,
            DownloadClientForm {
                name: "qbit",
                kind: "qbittorrent",
                url: "http://qbit",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        // Ensure config row exists (built-test-app-state doesn't seed one).
        let _ = sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
            .execute(&db)
            .await;
        let state = build_test_app_state(db, None);

        let _ = settings_indexers_nyaa_pin(
            State(state.clone()),
            HxRequest(false),
            Form(NyaaPinForm {
                download_client_id: Some(id.to_string()),
            }),
        )
        .await;
        let pinned: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(pinned, Some(id));

        let _ = settings_indexers_nyaa_pin(
            State(state.clone()),
            HxRequest(false),
            Form(NyaaPinForm {
                download_client_id: Some(String::new()),
            }),
        )
        .await;
        let pinned: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(pinned.is_none());
    }

    /// Nyaa surfaces torrent magnets, so pinning the Nyaa download
    /// client to a SAB (usenet) client would resolve at grab time and
    /// immediately fail at SAB's `mode=addurl`. Reject the save with a
    /// clear toast.
    #[tokio::test]
    async fn nyaa_pin_to_sab_client_is_rejected() {
        let db = in_memory_pool().await;
        let sab = crate::models::download_clients::insert(
            &db,
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
        .unwrap();
        let _ = sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
            .execute(&db)
            .await;
        let state = build_test_app_state(db, None);
        let resp = settings_indexers_nyaa_pin(
            State(state.clone()),
            HxRequest(false),
            Form(NyaaPinForm {
                download_client_id: Some(sab.to_string()),
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            location.contains("err=") && location.contains("Nyaa"),
            "expected protocol-mismatch err redirect, got: {location}"
        );
        // The pin must NOT have been persisted.
        let pinned: Option<i64> =
            sqlx::query_scalar("SELECT nyaa_download_client_id FROM config WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(
            pinned.is_none(),
            "Nyaa→SAB save must be rejected, not silently persisted"
        );
    }

    #[tokio::test]
    async fn nyaa_pin_db_error_during_lookup_fails_closed() {
        // PR 112 review #1 (4th pass) — a transient DB error on the
        // pin's protocol lookup must NOT silently skip the gate. The
        // prior `if let Ok(Some(row))` shape would let a Nyaa→SAB
        // pin through under a hiccup at save time. Provoke the
        // error by closing the pool and confirm we redirect to a
        // "DB error" toast instead of persisting the pin.
        let db = in_memory_pool().await;
        let sab = crate::models::download_clients::insert(
            &db,
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
        .unwrap();
        let _ = sqlx::query("INSERT OR IGNORE INTO config (id) VALUES (1)")
            .execute(&db)
            .await;
        let state = build_test_app_state(db.clone(), None);
        db.close().await;
        let resp = settings_indexers_nyaa_pin(
            State(state.clone()),
            HxRequest(false),
            Form(NyaaPinForm {
                download_client_id: Some(sab.to_string()),
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            location.contains("err=")
                && (location.contains("DB%20error") || location.contains("DB+error")),
            "expected fail-closed err redirect mentioning DB error, got: {location}"
        );
    }
}
