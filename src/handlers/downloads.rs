use askama::Template;
use axum::{
    extract::Form,
    extract::Query,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use std::sync::Arc;

use crate::AppState;
use crate::models::grabbed_torrents;
use crate::services::download_client::{DownloadClient, DownloadItemState};

/// Route a queue-action (pause/resume/delete) to the right client by
/// looking up the grab row for `hash` and resolving its
/// `download_client_id`. The composed fallback chain via
/// `resolve_grab_client` handles legacy NULL stamps for SAB grabs
/// (nzo_id-shape heuristic) and finally the torrent default.
/// Returns `None` only when no client is configured at all.
async fn resolve_client_for_hash(state: &AppState, hash: &str) -> Option<Arc<dyn DownloadClient>> {
    let dc_id = grabbed_torrents::client_id_for_hash(&state.db, hash).await;
    state.resolve_grab_client(dc_id, hash).await
}

struct QueueTorrentView {
    hash: String,
    name: String,
    size_display: String,
    progress_pct: String,
    speed_display: String,
    eta_display: String,
    state_label: String,
    state_badge_class: String,
    is_paused: bool,
}

fn format_size(bytes: i64) -> String {
    if bytes <= 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes as f64).ln() / 1024f64.ln()).floor() as usize;
    let i = i.min(units.len() - 1);
    let val = bytes as f64 / 1024f64.powi(i as i32);
    if i == 0 {
        format!("{} {}", val as i64, units[i])
    } else {
        format!("{:.1} {}", val, units[i])
    }
}

fn format_speed(bps: i64) -> String {
    if bps <= 0 {
        return String::new();
    }
    format!("{}/s", format_size(bps))
}

fn format_eta(seconds: i64) -> String {
    if seconds <= 0 || seconds >= 8640000 {
        return String::new();
    }
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{}h {}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

fn state_label(kind: DownloadItemState) -> &'static str {
    match kind {
        DownloadItemState::Downloading => "Downloading",
        DownloadItemState::DownloadingStalled => "Stalled",
        DownloadItemState::DownloadingQueued => "Queued",
        DownloadItemState::CheckingDownload => "Checking",
        DownloadItemState::Seeding | DownloadItemState::SeedingStalled => "Seeding",
        DownloadItemState::SeedingQueued => "Queued",
        DownloadItemState::CheckingSeed => "Checking",
        DownloadItemState::Paused | DownloadItemState::PausedComplete => "Paused",
        DownloadItemState::Errored => "Error",
    }
}

fn state_badge_class(kind: DownloadItemState) -> &'static str {
    match kind {
        DownloadItemState::Downloading => "log-badge-debug",
        DownloadItemState::DownloadingStalled
        | DownloadItemState::DownloadingQueued
        | DownloadItemState::SeedingQueued
        | DownloadItemState::Paused => "log-badge-warn",
        DownloadItemState::Seeding
        | DownloadItemState::SeedingStalled
        | DownloadItemState::PausedComplete => "log-badge-info",
        DownloadItemState::Errored => "log-badge-error",
        DownloadItemState::CheckingDownload | DownloadItemState::CheckingSeed => "",
    }
}

fn torrent_to_view(t: &crate::services::download_client::DownloadItem) -> QueueTorrentView {
    QueueTorrentView {
        hash: t.hash.clone(),
        name: t.name.clone(),
        size_display: format_size(t.size),
        progress_pct: format!("{:.1}", t.progress * 100.0),
        speed_display: format_speed(t.dlspeed),
        eta_display: format_eta(t.eta),
        state_label: state_label(t.state_kind).to_string(),
        state_badge_class: state_badge_class(t.state_kind).to_string(),
        is_paused: matches!(
            t.state_kind,
            DownloadItemState::Paused | DownloadItemState::PausedComplete
        ),
    }
}

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsTemplate {
    page: String,
    tab: String,
    queue: Vec<QueueTorrentView>,
    queue_error: String,
    history: Vec<grabbed_torrents::GrabbedTorrentWithSeries>,
    blocklist: Vec<grabbed_torrents::GrabbedTorrentWithSeries>,
    title_language: String,
}

#[derive(Deserialize)]
pub struct DownloadsQuery {
    tab: Option<String>,
}

/// User-facing label for a `download_clients.kind` value. Matches the
/// option labels in the Settings → Download Clients add/edit forms so
/// error copy and the form speak the same names.
fn client_kind_display(kind: &str) -> &'static str {
    match kind {
        "qbittorrent" => "qBittorrent",
        "deluge" => "Deluge",
        "transmission" => "Transmission",
        "rtorrent" => "rTorrent",
        "sabnzbd" => "SABnzbd",
        _ => "download client",
    }
}

fn normalize_tab(tab: Option<String>) -> String {
    match tab.as_deref() {
        Some("history") => "history".to_string(),
        Some("blocklist") => "blocklist".to_string(),
        _ => "queue".to_string(),
    }
}

pub async fn downloads_page(
    State(state): State<AppState>,
    Query(params): Query<DownloadsQuery>,
) -> Html<String> {
    let tab = normalize_tab(params.tab);

    // Load once up-front so history/blocklist queries can honor the
    // user's title_language preference. Queue doesn't need it — the
    // torrent client reports the release filename, not the series.
    let title_language = crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.title_language)
        .unwrap_or_else(|| "english".to_string());

    let (queue, queue_error) = if tab == "queue" {
        // Fan out across every enabled client so SAB / Usenet jobs
        // appear alongside torrent jobs in the queue. A single
        // client erroring (e.g. SAB unreachable) doesn't blank the
        // whole tab — it logs and the other clients still render.
        let pool = state.download_clients.read().await.clone();
        if pool.clients.is_empty() {
            (
                Vec::new(),
                "No download client is configured. Add one under Settings → Download Clients to see its queue here.".to_string(),
            )
        } else {
            let mut torrents: Vec<crate::services::download_client::DownloadItem> = Vec::new();
            let mut errors: Vec<(i64, String)> = Vec::new();
            for (id, c) in pool.clients.iter() {
                match c.list_scoped().await {
                    Ok(mut items) => torrents.append(&mut items),
                    Err(e) => errors.push((*id, e)),
                }
            }
            let is_downloading = |k: DownloadItemState| {
                matches!(
                    k,
                    DownloadItemState::Downloading
                        | DownloadItemState::DownloadingStalled
                        | DownloadItemState::DownloadingQueued
                        | DownloadItemState::CheckingDownload
                )
            };
            torrents.sort_by(|a, b| {
                let a_down = if is_downloading(a.state_kind) { 0 } else { 1 };
                let b_down = if is_downloading(b.state_kind) { 0 } else { 1 };
                a_down.cmp(&b_down).then(
                    b.progress
                        .partial_cmp(&a.progress)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
            });
            let views = torrents.iter().map(torrent_to_view).collect();
            let error_msg = if errors.is_empty() {
                String::new()
            } else {
                // Interface-voice summary: identify the failing
                // client(s) by configured name AND kind — with a
                // multi-client pool a bare row name like "qbit" is
                // ambiguous — and say whether the rest of the queue
                // still rendered. The raw transport error (reqwest
                // URL, connect detail) goes to the DB log instead of
                // the page; it's diagnostic material, not direction.
                let rows = crate::models::download_clients::list_all(&state.db)
                    .await
                    .unwrap_or_default();
                let describe = |id: i64| {
                    rows.iter()
                        .find(|r| r.id == id)
                        .map(|r| format!("\"{}\" ({})", r.name, client_kind_display(&r.kind)))
                        .unwrap_or_else(|| format!("download client #{}", id))
                };
                let detail = errors
                    .iter()
                    .map(|(id, e)| format!("{}: {}", describe(*id), e))
                    .collect::<Vec<_>>()
                    .join("; ");
                crate::services::logger::warn(
                    &state.db,
                    crate::models::log::LogCategory::DownloadClient,
                    "Queue load failed for one or more download clients",
                    &detail,
                )
                .await;
                let names = errors
                    .iter()
                    .map(|(id, _)| describe(*id))
                    .collect::<Vec<_>>()
                    .join(", ");
                let lead = if errors.len() == 1 {
                    format!("Can't reach the download client {}.", names)
                } else {
                    format!("Can't reach {} download clients: {}.", errors.len(), names)
                };
                let partial = if errors.len() < pool.clients.len() {
                    " Queues from the other clients are still shown."
                } else {
                    ""
                };
                format!(
                    "{}{} Check the connection details under Settings → Download Clients; the full error is in System → Logs.",
                    lead, partial
                )
            };
            (views, error_msg)
        }
    } else {
        (Vec::new(), String::new())
    };

    let history = if tab == "history" {
        grabbed_torrents::get_all_with_series(&state.db, 500, &title_language)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let blocklist = if tab == "blocklist" {
        grabbed_torrents::get_blocked(&state.db, &title_language)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let template = DownloadsTemplate {
        page: "downloads".to_string(),
        tab,
        queue,
        queue_error,
        history,
        blocklist,
        title_language: title_language.clone(),
    };
    Html(template.render().unwrap_or_default())
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TorrentActionForm {
    hash: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TorrentDeleteForm {
    hash: String,
    #[serde(default)]
    delete_files: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct BlocklistRemoveForm {
    pub id: i64,
}

#[utoipa::path(
    post,
    path = "/api/downloads/pause",
    tag = "Downloads",
    summary = "Pause a torrent",
    description = "Pause an active torrent download in the configured download client.",
    request_body = TorrentActionForm,
    responses(
        (status = 200, description = "Torrent paused", body = serde_json::Value),
        (status = 400, description = "Download client not configured"),
    ),
)]
pub async fn api_pause_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentActionForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = resolve_client_for_hash(&state, &form.hash).await.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;
    client
        .pause(&form.hash)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/downloads/resume",
    tag = "Downloads",
    summary = "Resume a torrent",
    description = "Resume a paused torrent download in the configured download client.",
    request_body = TorrentActionForm,
    responses(
        (status = 200, description = "Torrent resumed", body = serde_json::Value),
        (status = 400, description = "Download client not configured"),
    ),
)]
pub async fn api_resume_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentActionForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = resolve_client_for_hash(&state, &form.hash).await.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;
    client
        .resume(&form.hash)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/downloads/delete",
    tag = "Downloads",
    summary = "Delete a torrent",
    description = "Remove a torrent from the configured download client. Optionally delete downloaded files.",
    request_body = TorrentDeleteForm,
    responses(
        (status = 200, description = "Torrent deleted", body = serde_json::Value),
        (status = 400, description = "Download client not configured"),
    ),
)]
pub async fn api_delete_torrent(
    State(state): State<AppState>,
    Json(form): Json<TorrentDeleteForm>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let client = resolve_client_for_hash(&state, &form.hash).await.ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Download client not configured".to_string(),
    ))?;
    client
        .delete(&form.hash, form.delete_files)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[utoipa::path(
    post,
    path = "/api/downloads/blocklist/remove",
    tag = "Downloads",
    summary = "Remove from blocklist",
    description = "Remove a grabbed torrent entry from the blocklist by its database ID. \
                   HTMX requests get an empty 200 (so `hx-swap=outerHTML` removes the row); \
                   non-HTMX requests get a 303 redirect back to the blocklist tab. \
                   Form-encoded body (was JSON pre-issue-#129; the JSON path had no API \
                   consumer beyond the JS that this handler's HTMX form replaces).",
    request_body = BlocklistRemoveForm,
    responses(
        (status = 200, description = "Entry removed (HTMX)"),
        (status = 303, description = "Entry removed (form-POST fallback) — redirects to /downloads?tab=blocklist"),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn api_blocklist_remove(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<BlocklistRemoveForm>,
) -> Result<Response, (StatusCode, String)> {
    grabbed_torrents::remove(&state.db, form.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if is_htmx {
        // Empty 200 — `hx-swap=outerHTML` on the row strips it from
        // the DOM. Same shape as the Phase 1 settings deletes.
        Ok(StatusCode::OK.into_response())
    } else {
        Ok(Redirect::to("/downloads?tab=blocklist&msg=Entry+removed").into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror of the exhaustive-match pattern in
    // `services::download_client::tests::all_variants_with_slugs` —
    // a new enum variant has to be added to the inner match before
    // the test compiles, which forces both state_label and
    // state_badge_class to get an explicit mapping instead of
    // falling through to a fallback arm.
    fn all_variants_with_expected() -> Vec<(DownloadItemState, &'static str, &'static str, bool)> {
        // (variant, expected_label, expected_badge_class, expected_is_paused)
        use DownloadItemState::*;
        fn _exhaustive(v: DownloadItemState) {
            match v {
                Downloading | DownloadingStalled | DownloadingQueued | CheckingDownload => {}
                Seeding | SeedingStalled | SeedingQueued | CheckingSeed => {}
                Paused | PausedComplete => {}
                Errored => {}
            }
        }
        vec![
            (Downloading, "Downloading", "log-badge-debug", false),
            (DownloadingStalled, "Stalled", "log-badge-warn", false),
            (DownloadingQueued, "Queued", "log-badge-warn", false),
            (CheckingDownload, "Checking", "", false),
            (Seeding, "Seeding", "log-badge-info", false),
            (SeedingStalled, "Seeding", "log-badge-info", false),
            (SeedingQueued, "Queued", "log-badge-warn", false),
            (CheckingSeed, "Checking", "", false),
            (Paused, "Paused", "log-badge-warn", true),
            (PausedComplete, "Paused", "log-badge-info", true),
            (Errored, "Error", "log-badge-error", false),
        ]
    }

    #[test]
    fn state_label_covers_every_variant() {
        for (v, label, _, _) in all_variants_with_expected() {
            assert_eq!(state_label(v), label, "label mismatch for {v:?}");
        }
    }

    #[test]
    fn state_badge_class_covers_every_variant() {
        for (v, _, badge, _) in all_variants_with_expected() {
            assert_eq!(state_badge_class(v), badge, "badge mismatch for {v:?}");
        }
    }

    // ── format_size ───────────────────────────────────────────────────

    #[test]
    fn format_size_zero_or_negative_renders_zero_bytes() {
        // Negative bytes can't physically happen, but i64 lets the
        // value through. The defensive `<= 0` arm exists for that
        // case; pin it with both 0 and a negative input. The string
        // is a literal "0 B" so the queue row renders something
        // rather than an empty cell.
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(-1), "0 B");
    }

    #[test]
    fn format_size_under_1_kib_uses_b_with_no_decimals() {
        // First-tier units render with no decimal — "12 B" reads more
        // naturally than "12.0 B" for tiny values.
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_unit_boundaries_round_to_one_decimal() {
        // 1 KB = 1024 → "1.0 KB" (note: KB unit-string, decimal on).
        assert_eq!(format_size(1024), "1.0 KB");
        // 1 MB.
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        // 1 GB.
        assert_eq!(format_size(1024i64.pow(3)), "1.0 GB");
        // 1 TB — top unit, larger values stay in TB.
        assert_eq!(format_size(1024i64.pow(4)), "1.0 TB");
    }

    #[test]
    fn format_size_clamps_at_top_unit() {
        // Beyond TB (1024^4), the index is capped at 4 (TB) so a
        // hypothetical PB-sized torrent doesn't underflow into an
        // empty unit slot.
        let huge = 1024i64.pow(5);
        assert!(format_size(huge).ends_with(" TB"), "{}", format_size(huge));
    }

    // ── format_speed ─────────────────────────────────────────────────

    #[test]
    fn format_speed_zero_renders_blank() {
        // 0 bps means "no transfer in flight" — surface as blank so
        // the queue row reads cleanly rather than "0 B/s".
        assert_eq!(format_speed(0), "");
        assert_eq!(format_speed(-1), "");
    }

    #[test]
    fn format_speed_appends_per_second_to_size() {
        assert_eq!(format_speed(1024), "1.0 KB/s");
        assert_eq!(format_speed(2 * 1024 * 1024), "2.0 MB/s");
    }

    // ── format_eta ───────────────────────────────────────────────────

    #[test]
    fn format_eta_zero_or_sentinel_renders_blank() {
        assert_eq!(format_eta(0), "");
        assert_eq!(format_eta(-1), "");
        // 8_640_000s = 100 days. qBit returns this as the "infinity"
        // sentinel; treat as unknown, render blank.
        assert_eq!(format_eta(8_640_000), "");
        assert_eq!(format_eta(9_999_999), "");
    }

    #[test]
    fn format_eta_seconds_only_under_one_minute() {
        assert_eq!(format_eta(45), "45s");
        assert_eq!(format_eta(1), "1s");
    }

    #[test]
    fn format_eta_minutes_and_seconds_under_one_hour() {
        assert_eq!(format_eta(60), "1m 0s");
        assert_eq!(format_eta(125), "2m 5s");
    }

    #[test]
    fn format_eta_hours_and_minutes_at_or_above_one_hour() {
        // Seconds drop out at the hour level — "1h 30m 5s" would be
        // visual noise for an ETA estimate that's already coarse.
        assert_eq!(format_eta(3600), "1h 0m");
        assert_eq!(format_eta(3661), "1h 1m");
        assert_eq!(format_eta(7320), "2h 2m");
    }

    // ── normalize_tab ────────────────────────────────────────────────

    #[test]
    fn normalize_tab_known_tabs_pass_through() {
        assert_eq!(normalize_tab(Some("history".into())), "history");
        assert_eq!(normalize_tab(Some("blocklist".into())), "blocklist");
    }

    #[test]
    fn normalize_tab_unknown_or_missing_defaults_to_queue() {
        // Queue is the natural landing — opening /downloads with no
        // explicit tab should show what's currently transferring.
        assert_eq!(normalize_tab(None), "queue");
        assert_eq!(normalize_tab(Some("garbage".into())), "queue");
        assert_eq!(normalize_tab(Some("".into())), "queue");
    }

    #[test]
    fn torrent_view_is_paused_flag_matches_enum() {
        // Drives the pause/resume button on the queue row. Reading
        // a client-native "paused" prefix from the legacy
        // `state.starts_with("paused")` path would silently break
        // for Transmission (numeric states) and rtorrent (computed
        // strings) — the derivation has to go through state_kind.
        for (v, _, _, expected_paused) in all_variants_with_expected() {
            let item = crate::services::download_client::DownloadItem {
                hash: "a".repeat(40),
                name: "Release".to_string(),
                size: 0,
                progress: 0.0,
                dlspeed: 0,
                state: String::new(),
                category: String::new(),
                eta: 0,
                save_path: String::new(),
                content_path: String::new(),
                state_kind: v,
                seeding_done: false,
            };
            let view = torrent_to_view(&item);
            assert_eq!(view.is_paused, expected_paused, "is_paused for {v:?}");
        }
    }
}
