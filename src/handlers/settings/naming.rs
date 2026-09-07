//! Live preview for Settings → General → File naming (issue #124).
//!
//! The page sends all three templates as typed (debounced) and gets
//! back a per-field verdict plus the combined sample path, rendered by
//! the same `services::naming` functions the save handler validates
//! with. Server-rendered on purpose: one resolver, no JS twin to drift.

use axum::{Json, extract::State};
use serde::Deserialize;

use crate::AppState;
use crate::models::config;
use crate::services::naming;

use super::{NAMING_FIELDS, naming_or_default, naming_path_preview};

#[derive(Deserialize, utoipa::ToSchema)]
pub struct NamingPreviewRequest {
    #[serde(default)]
    pub series_folder_format: String,
    #[serde(default)]
    pub season_folder_format: String,
    #[serde(default)]
    pub episode_file_format: String,
}

/// Always 200 so the page can render a rejection inline; `ok` is per
/// field. Nothing is saved here.
#[utoipa::path(
    post,
    path = "/api/settings/naming-preview",
    tag = "Settings",
    summary = "Preview naming templates",
    description = "Validates the three naming templates and renders a sample path. Always 200; each field carries its own ok flag and error. Saves nothing.",
    request_body = NamingPreviewRequest,
    responses(
        (status = 200, description = "Per-field verdicts plus the combined sample path", body = serde_json::Value),
    ),
)]
pub async fn naming_preview(
    State(state): State<AppState>,
    Json(req): Json<NamingPreviewRequest>,
) -> Json<serde_json::Value> {
    let media_root = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.media_root)
        .unwrap_or_default();
    let raw = [
        req.series_folder_format.as_str(),
        req.season_folder_format.as_str(),
        req.episode_file_format.as_str(),
    ];
    let mut fields = serde_json::Map::new();
    let mut parts = Vec::with_capacity(3);
    for ((kind, input_name, _, _), raw) in NAMING_FIELDS.into_iter().zip(raw) {
        let template = naming_or_default(raw, kind);
        let entry = match naming::preview(kind, &template) {
            Ok(r) => {
                parts.push(r.name.clone());
                serde_json::json!({"ok": true, "preview": r.name, "error": null})
            }
            Err(e) => {
                parts.push(String::new());
                serde_json::json!({"ok": false, "preview": "", "error": e})
            }
        };
        fields.insert(input_name.to_string(), entry);
    }
    let (path, warning) = naming_path_preview(&media_root, &parts);
    Json(serde_json::json!({
        "ok": true,
        "fields": fields,
        "path": path,
        "warning": warning,
    }))
}
