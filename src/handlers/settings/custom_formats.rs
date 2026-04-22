//! Custom Format CRUD, import/export, and bundled-defaults handlers.
//!
//! Split out of `handlers::settings::mod` because CF-specific code dominated
//! ~1200 of the 2400 lines there. The surface is re-exported from the parent
//! `mod.rs` so `handlers::settings::settings_custom_formats_*` still resolves
//! for main.rs router wiring and utoipa.

use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::{
    Form, Json,
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, custom_formats as cf_model};
use crate::services::{custom_formats as cf_service, logger};

use super::build_settings_template;

// ─────────────────────────────────────────────────────────────────────────
// Form types
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatUpsertForm {
    /// `None` = create a new row; `Some(n)` = update existing row `n`.
    /// Hidden input on the edit form prefill.
    id: Option<i64>,
    name: String,
    score: i32,
    trash_id: Option<String>,
    json: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatDeleteForm {
    id: i64,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatMinScoreForm {
    /// Blank string = clear the floor (`i32::MIN`). Numeric strings are
    /// parsed; anything else falls back to the current value so a fat-
    /// finger save can't silently wipe the user's threshold.
    minimum_score: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatImportForm {
    /// Pasted Sonarr v4 CF JSON export — either a single CF object or
    /// an array of them. Each entry compiles through the same
    /// `compile_from_json` path as the create form; failures are
    /// counted and reported but don't abort the whole import.
    payload: String,
}

/// Form for the import-resolve step. Carries the original payload
/// verbatim (echoed from a hidden field) plus two parallel lists of
/// actions and rename targets, each keyed by the collision's entry
/// index inside the parsed payload.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomFormatImportResolveForm {
    payload: String,
    /// One entry per collision: `"<index>:<action>"` where action is
    /// `skip`, `overwrite`, or `rename`. Serialized as a newline-
    /// delimited string to avoid the axum Form-extractor's complicated
    /// multi-value handling.
    decisions: String,
    /// One entry per rename collision: `"<index>:<new_name>"`. Only
    /// read when the corresponding `decisions` line has action `rename`.
    /// Also newline-delimited.
    renames: String,
}

/// Query parameters for the CF export endpoint. `mode` selects between
/// the default Ryokan-compatible export (keeps `Ryokan.`-namespaced
/// specs verbatim) and the Sonarr-safe variant (drops entire CFs that
/// contain any Ryokan-only spec so the file imports cleanly into a
/// vanilla Sonarr v4 instance). See plan §5.7.5.
#[derive(Deserialize)]
pub struct CfExportQuery {
    /// `"sonarr-safe"` triggers the Sonarr-safe branch; anything else
    /// (or absent) falls through to the default Ryokan-compatible mode.
    mode: Option<String>,
    /// Comma-separated row IDs to include. `None` or empty string =
    /// export all rows (backwards-compatible with curl-based scripts
    /// that just call `/settings/custom-formats/export`). Non-numeric
    /// tokens are skipped silently. #11.4 — the UI populates this
    /// from the per-CF checkbox list; unchecked CFs drop out here.
    ids: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────
// View types shared with SettingsTemplate (rendered by the CF tab)
// ─────────────────────────────────────────────────────────────────────────

/// Per-collision entry shown on the import review block. `index` is
/// the position of the CF inside the parsed payload so the resolve
/// handler can find the right entry after re-parsing the payload.
pub struct ImportCollision {
    pub index: usize,
    pub name: String,
}

/// One row of the import preview panel (#11.3). Status is one of
/// `"new"` / `"collision"` / `"invalid"`; `error` is populated for the
/// `"invalid"` case so the UI can surface the parse reason inline.
/// `specs_count` renders as "N specs" next to the name.
pub struct ImportPreviewEntry {
    pub name: String,
    pub score: i32,
    pub specs_count: usize,
    pub status: String,
    pub error: Option<String>,
}

/// View model for the import review block. Holds the original payload
/// (echoed back into a hidden field so the resolve handler can re-parse
/// it) plus the full entries list (for the preview panel) and the
/// collisions subset (for the resolve form).
pub struct ImportReviewView {
    pub payload: String,
    pub collisions: Vec<ImportCollision>,
    pub entries: Vec<ImportPreviewEntry>,
    pub has_invalid: bool,
}

/// Per-import decision on a name collision (plan §6.2). Derived from
/// the review form's radio-button values. `Skip` keeps the existing
/// row untouched; `Overwrite` replaces it in place; `Rename` writes a
/// new row under the user-supplied rename_to value.
#[derive(Clone, Debug)]
enum CollisionDecision {
    Skip,
    Overwrite,
    Rename(String),
}

/// Counts returned by `install_default_cfs_core`. Shared between the
/// install-defaults and reset-defaults handlers so the summary-string
/// construction can live in one place.
struct InstallDefaultsReport {
    installed: usize,
    skipped: usize,
    failed: usize,
    first_error: Option<String>,
}

/// The raw bundled-defaults JSON baked into the binary at compile time
/// via `include_str!`. Parsed once per install/reset click rather than
/// at startup — the payload is a few KB, the user rarely clicks this,
/// and compile-time validation doesn't catch field-level typos anyway.
const DEFAULTS_JSON: &str = include_str!("../../../static/default_custom_formats.json");

// ─────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────

/// Create or update a Custom Format row. Validates the supplied JSON
/// via `compile_from_json` before touching the database — if the parse
/// fails, the user is bounced back to the CF tab with the error and
/// the edit form re-prefilled from the attempted id so their work
/// isn't lost. On success, rebuilds the compiled-CF cache so the next
/// scoring pass sees the change.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/upsert",
    tag = "Settings",
    summary = "Create or update a Custom Format",
    description = "Upsert a Sonarr v4-compatible Custom Format and its V1-profile score. Validates the JSON via the CF compiler before writing to the database. On success, rebuilds the compiled-CF cache so the next scoring pass sees the change. Redirects back to the Custom Formats settings tab with a flash message.",
    request_body(content = CustomFormatUpsertForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_upsert(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatUpsertForm>,
) -> Redirect {
    let name = form.name.trim();
    if name.is_empty() {
        return Redirect::to(&cf_redirect(
            form.id,
            None,
            Some("Custom Format name cannot be blank."),
        ));
    }
    let trash_id = form
        .trash_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let json_trimmed = form.json.trim();

    if let Err(e) = cf_service::compile_from_json(json_trimmed, form.score, form.id.unwrap_or(0)) {
        return Redirect::to(&cf_redirect(
            form.id,
            None,
            Some(&format!("Parse error: {e}")),
        ));
    }

    let save_result = if let Some(id) = form.id {
        cf_model::update(&state.db, id, name, trash_id, json_trimmed, form.score)
            .await
            .map(|_| id)
    } else {
        cf_model::insert(
            &state.db,
            name,
            trash_id,
            json_trimmed,
            form.score,
            cf_model::ORIGIN_MANUAL,
        )
        .await
    };

    match save_result {
        Ok(id) => {
            cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Custom Format saved: {name} (id={id})"),
                "",
            )
            .await;
            Redirect::to(&cf_redirect(None, Some(&format!("Saved '{name}'.")), None))
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Custom Format save failed",
                &e.to_string(),
            )
            .await;
            Redirect::to(&cf_redirect(
                form.id,
                None,
                Some(&format!("Database error: {e}")),
            ))
        }
    }
}

/// Delete a Custom Format row by id. Score row is dropped automatically
/// via the `ON DELETE CASCADE` on `custom_format_scores`.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/delete",
    tag = "Settings",
    summary = "Delete a Custom Format",
    description = "Delete a Custom Format row by id. The associated score row is dropped automatically via ON DELETE CASCADE. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatDeleteForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_delete(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatDeleteForm>,
) -> Redirect {
    match cf_model::delete(&state.db, form.id).await {
        Ok(_) => {
            cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
            logger::info(
                &state.db,
                LogCategory::System,
                &format!("Custom Format deleted: id={}", form.id),
                "",
            )
            .await;
            Redirect::to(&cf_redirect(None, Some("Custom Format deleted."), None))
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::System,
                "Custom Format delete failed",
                &e.to_string(),
            )
            .await;
            Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Delete failed: {e}")),
            ))
        }
    }
}

/// Update the global `custom_format_minimum_score` floor. Blank input
/// clears the floor (sets it back to `i32::MIN`, the "no floor"
/// sentinel). Non-numeric input falls through to the existing value so
/// a typo can't silently wipe the threshold.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/minimum-score",
    tag = "Settings",
    summary = "Set the Custom Format minimum-score floor",
    description = "Update the global minimum-score threshold. Auto-search drops releases whose summed CF score falls below this value; interactive search still shows everything. Blank clears the floor. Redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatMinScoreForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_minimum_score(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatMinScoreForm>,
) -> Redirect {
    let existing = config::get_config(&state.db).await.ok().flatten();
    let Some(mut cfg) = existing else {
        return Redirect::to(&cf_redirect(None, None, Some("Config not initialized.")));
    };

    let trimmed = form.minimum_score.trim();
    let new_floor = if trimmed.is_empty() {
        i32::MIN
    } else {
        match trimmed.parse::<i32>() {
            Ok(n) => n,
            Err(_) => {
                return Redirect::to(&cf_redirect(
                    None,
                    None,
                    Some("Minimum score must be an integer (leave blank for 'no floor')."),
                ));
            }
        }
    };

    cfg.custom_format_minimum_score = new_floor;
    match config::save_config(&state.db, &cfg).await {
        Ok(_) => {
            let msg = if new_floor == i32::MIN {
                "Minimum score cleared (no floor).".to_string()
            } else {
                format!("Minimum score set to {new_floor}.")
            };
            Redirect::to(&cf_redirect(None, Some(&msg), None))
        }
        Err(e) => Redirect::to(&cf_redirect(None, None, Some(&format!("Save failed: {e}")))),
    }
}

/// Loop body shared between the no-collision fast path and the resolve
/// handler. Takes a set of (entry, decision) pairs plus the existing
/// name → id map, and performs the insert / update / skip for each.
/// Returns (imported, skipped_for_collision, failed, first_error).
async fn apply_import_entries(
    state: &AppState,
    entries: Vec<(serde_json::Value, CollisionDecision)>,
    existing_by_name: &HashMap<String, i64>,
) -> (usize, usize, usize, Option<String>) {
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;

    for (mut entry, decision) in entries {
        let original_name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let trash_id = entry
            .get("trash_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let score = entry
            .get("score")
            .and_then(|v| v.as_i64())
            .or_else(|| entry.get("trash_score").and_then(|v| v.as_i64()))
            .unwrap_or(0) as i32;

        if original_name.is_empty() {
            failed += 1;
            if first_error.is_none() {
                first_error = Some("one entry is missing a `name` field".to_string());
            }
            continue;
        }

        let (effective_name, existing_id) = match decision {
            CollisionDecision::Skip => {
                skipped += 1;
                continue;
            }
            CollisionDecision::Overwrite => {
                let id = existing_by_name.get(&original_name).copied();
                (original_name.clone(), id)
            }
            CollisionDecision::Rename(new_name) => {
                let trimmed = new_name.trim();
                if trimmed.is_empty() {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!("'{original_name}': rename target is empty"));
                    }
                    continue;
                }
                if existing_by_name.contains_key(trimmed) {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!(
                            "'{original_name}': rename target '{trimmed}' also collides"
                        ));
                    }
                    continue;
                }
                if let serde_json::Value::Object(ref mut map) = entry {
                    map.insert(
                        "name".to_string(),
                        serde_json::Value::String(trimmed.to_string()),
                    );
                }
                (trimmed.to_string(), None)
            }
        };

        let raw_json = entry.to_string();
        if let Err(e) = cf_service::compile_from_json(&raw_json, score, 0) {
            failed += 1;
            if first_error.is_none() {
                first_error = Some(format!("'{effective_name}': {e}"));
            }
            continue;
        }

        let save_result = if let Some(id) = existing_id {
            cf_model::update(
                &state.db,
                id,
                &effective_name,
                trash_id.as_deref(),
                &raw_json,
                score,
            )
            .await
            .map(|_| id)
        } else {
            cf_model::insert(
                &state.db,
                &effective_name,
                trash_id.as_deref(),
                &raw_json,
                score,
                cf_model::ORIGIN_IMPORT,
            )
            .await
        };

        match save_result {
            Ok(_) => imported += 1,
            Err(e) => {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(format!("'{effective_name}': {e}"));
                }
            }
        }
    }

    (imported, skipped, failed, first_error)
}

/// Build a summary flash message from the four counters produced by
/// `apply_import_entries`.
fn import_summary(
    imported: usize,
    skipped: usize,
    failed: usize,
    first_error: Option<String>,
) -> String {
    // Arm order matters: the (0, 0, f) "all-rejected" arm must come
    // BEFORE the general (n, s, f) arm, and it must *not* swallow the
    // skipped count — an earlier version had a `(0, _, f)` arm that
    // silently dropped `skipped` when imported=0 and both skipped and
    // failed were non-zero. The arms below break that case out so
    // every counter combination shows every non-zero counter.
    match (imported, skipped, failed) {
        (0, 0, 0) => "Nothing to import.".to_string(),
        (n, 0, 0) => format!("Imported {n} Custom Format(s)."),
        (n, s, 0) => format!("Imported {n}, skipped {s} on collision."),
        (0, 0, f) => format!(
            "Import failed ({f} rejected). First error: {}",
            first_error.unwrap_or_default()
        ),
        (n, 0, f) => format!(
            "Imported {n}, failed {f}. First error: {}",
            first_error.unwrap_or_default()
        ),
        (n, s, f) => format!(
            "Imported {n}, skipped {s}, failed {f}. First error: {}",
            first_error.unwrap_or_default()
        ),
    }
}

/// Import a Sonarr v4 CF JSON export. Accepts a single object, a bare
/// array, or a `{custom_formats: [...]}` wrapper (plan §6.2).
#[utoipa::path(
    post,
    path = "/settings/custom-formats/import",
    tag = "Settings",
    summary = "Import Custom Formats from Sonarr v4 JSON",
    description = "Import one or more Custom Formats from a Sonarr v4 JSON export. Accepts a single object, an array, or a `{custom_formats:[…]}` wrapper. On a name collision the page re-renders with an inline review block; the user picks overwrite/rename/skip per conflict and submits to the resolve endpoint. Rebuilds the compiled-CF cache on success.",
    request_body(content = CustomFormatImportForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Review page rendered (collisions exist)"),
        (status = 303, description = "Redirect back to the Custom Formats settings tab (no collisions)"),
    ),
)]
pub async fn settings_custom_formats_import(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatImportForm>,
) -> Response {
    let payload = form.payload.trim();
    if payload.is_empty() {
        return Redirect::to(&cf_redirect(None, None, Some("Import payload is empty.")))
            .into_response();
    }

    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Import failed: invalid JSON ({e})")),
            ))
            .into_response();
        }
    };

    let entries: Vec<serde_json::Value> = match normalize_cf_import_entries(value) {
        Ok(entries) => entries,
        Err(msg) => {
            return Redirect::to(&cf_redirect(None, None, Some(&msg))).into_response();
        }
    };

    let existing_rows = match cf_model::list_with_scores(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Failed to read existing CFs: {e}")),
            ))
            .into_response();
        }
    };
    let existing_by_name: HashMap<String, i64> =
        existing_rows.into_iter().map(|r| (r.name, r.id)).collect();

    let mut collisions: Vec<ImportCollision> = Vec::new();
    let mut preview: Vec<ImportPreviewEntry> = Vec::with_capacity(entries.len());
    for (idx, entry) in entries.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let score = entry.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let specs_count = entry
            .get("specifications")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let compile_err = cf_service::compile_from_json(&entry.to_string(), score, 0).err();
        let (status, error) = if let Some(e) = compile_err.as_ref() {
            ("invalid".to_string(), Some(e.clone()))
        } else if name.is_empty() {
            (
                "invalid".to_string(),
                Some("CF is missing a non-empty `name` field.".to_string()),
            )
        } else if existing_by_name.contains_key(&name) {
            ("collision".to_string(), None)
        } else {
            ("new".to_string(), None)
        };
        if status == "collision" {
            collisions.push(ImportCollision {
                index: idx,
                name: name.clone(),
            });
        }
        preview.push(ImportPreviewEntry {
            name,
            score,
            specs_count,
            status,
            error,
        });
    }

    let has_invalid = preview.iter().any(|p| p.status == "invalid");
    if !collisions.is_empty() || has_invalid {
        let review = ImportReviewView {
            payload: payload.to_string(),
            collisions,
            entries: preview,
            has_invalid,
        };
        let template = build_settings_template(
            &state,
            Some("custom_formats".to_string()),
            None,
            None,
            None,
            Some(review),
        )
        .await;
        return Html(template.render().unwrap_or_default()).into_response();
    }

    let decisions: Vec<(serde_json::Value, CollisionDecision)> = entries
        .into_iter()
        .map(|e| (e, CollisionDecision::Overwrite))
        .collect();
    let (imported, skipped, failed, first_error) =
        apply_import_entries(&state, decisions, &existing_by_name).await;

    if imported > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    }

    let summary = import_summary(imported, skipped, failed, first_error);
    if imported == 0 && failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary))).into_response()
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None)).into_response()
    }
}

/// Parse the newline-delimited decisions string into a HashMap keyed
/// by entry index. Unknown actions are mapped to `Skip` (the safest
/// default) and unknown indices are silently dropped.
fn parse_collision_decisions(decisions: &str, renames: &str) -> HashMap<usize, CollisionDecision> {
    let mut rename_map: HashMap<usize, String> = HashMap::new();
    for line in renames.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((idx_str, new_name)) = line.split_once(':')
            && let Ok(idx) = idx_str.trim().parse::<usize>()
        {
            rename_map.insert(idx, new_name.trim().to_string());
        }
    }

    let mut out: HashMap<usize, CollisionDecision> = HashMap::new();
    for line in decisions.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((idx_str, action)) = line.split_once(':') else {
            continue;
        };
        let Ok(idx) = idx_str.trim().parse::<usize>() else {
            continue;
        };
        let decision = match action.trim() {
            "overwrite" => CollisionDecision::Overwrite,
            "rename" => {
                let new_name = rename_map.get(&idx).cloned().unwrap_or_default();
                CollisionDecision::Rename(new_name)
            }
            _ => CollisionDecision::Skip,
        };
        out.insert(idx, decision);
    }
    out
}

/// Resolve a staged CF import by applying the user's per-collision
/// decisions to the original payload.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/import-resolve",
    tag = "Settings",
    summary = "Resolve a staged CF import with per-collision decisions",
    description = "Second step of the CF import flow: re-parses the original payload (echoed from a hidden field) and applies the user's overwrite/rename/skip decision for each name collision. Entries with no collision default to plain insert. Rebuilds the compiled-CF cache and redirects back to the Custom Formats settings tab.",
    request_body(content = CustomFormatImportResolveForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_import_resolve(
    State(state): State<AppState>,
    Form(form): Form<CustomFormatImportResolveForm>,
) -> Redirect {
    let payload = form.payload.trim();
    if payload.is_empty() {
        return Redirect::to(&cf_redirect(
            None,
            None,
            Some("Import resolve: payload is empty."),
        ));
    }

    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Import resolve: invalid JSON ({e})")),
            ));
        }
    };

    let entries: Vec<serde_json::Value> = match normalize_cf_import_entries(value) {
        Ok(entries) => entries,
        Err(msg) => {
            return Redirect::to(&cf_redirect(None, None, Some(&msg)));
        }
    };

    let existing_rows = match cf_model::list_with_scores(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            return Redirect::to(&cf_redirect(
                None,
                None,
                Some(&format!("Failed to read existing CFs: {e}")),
            ));
        }
    };
    let existing_by_name: HashMap<String, i64> =
        existing_rows.into_iter().map(|r| (r.name, r.id)).collect();

    let decisions_map = parse_collision_decisions(&form.decisions, &form.renames);

    let decisions: Vec<(serde_json::Value, CollisionDecision)> = entries
        .into_iter()
        .enumerate()
        .map(|(idx, entry)| {
            let decision = decisions_map
                .get(&idx)
                .cloned()
                .unwrap_or(CollisionDecision::Overwrite);
            (entry, decision)
        })
        .collect();

    let (imported, skipped, failed, first_error) =
        apply_import_entries(&state, decisions, &existing_by_name).await;

    if imported > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    }

    let summary = import_summary(imported, skipped, failed, first_error);
    if imported == 0 && failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Parse the bundled `static/default_custom_formats.json` into a Vec of
/// CF entry values. Shared by install-defaults and reset-defaults so
/// they fail the same way on a malformed defaults file.
fn parse_default_cf_entries() -> Result<Vec<serde_json::Value>, String> {
    let value: serde_json::Value = serde_json::from_str(DEFAULTS_JSON)
        .map_err(|e| format!("Defaults file is malformed: {e}"))?;
    match value {
        serde_json::Value::Array(items) => Ok(items),
        _ => Err("Defaults file is not a JSON array.".to_string()),
    }
}

/// Loop over parsed defaults entries and insert each one with
/// `ORIGIN_DEFAULTS` within the caller's transaction.
async fn install_defaults_entries_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    entries: Vec<serde_json::Value>,
    existing_names: &HashSet<String>,
    report: &mut InstallDefaultsReport,
) -> Result<(), sqlx::Error> {
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            report.failed += 1;
            if report.first_error.is_none() {
                report.first_error = Some("defaults entry missing `name`".to_string());
            }
            continue;
        }
        if existing_names.contains(&name) {
            report.skipped += 1;
            continue;
        }
        let trash_id = entry
            .get("trash_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let score = entry.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        let raw_json = entry.to_string();
        if let Err(e) = cf_service::compile_from_json(&raw_json, score, 0) {
            report.failed += 1;
            if report.first_error.is_none() {
                report.first_error = Some(format!("'{name}': {e}"));
            }
            continue;
        }

        cf_model::insert_with_tx(
            tx,
            &name,
            trash_id.as_deref(),
            &raw_json,
            score,
            cf_model::ORIGIN_DEFAULTS,
        )
        .await?;
        report.installed += 1;
    }
    Ok(())
}

/// Do the heavy lifting of loading the bundled defaults file, looping
/// over entries, and inserting each one with `ORIGIN_DEFAULTS`.
async fn install_default_cfs_core(state: &AppState) -> Result<InstallDefaultsReport, String> {
    let entries = parse_default_cf_entries()?;

    let existing: HashSet<String> = cf_model::list_with_scores(&state.db)
        .await
        .map_err(|e| format!("Failed to read existing CFs: {e}"))?
        .into_iter()
        .map(|r| r.name)
        .collect();

    let mut report = InstallDefaultsReport {
        installed: 0,
        skipped: 0,
        failed: 0,
        first_error: None,
    };

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| format!("Failed to open transaction: {e}"))?;
    install_defaults_entries_tx(&mut tx, entries, &existing, &mut report)
        .await
        .map_err(|e| format!("Install failed mid-loop: {e}"))?;
    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit install transaction: {e}"))?;

    Ok(report)
}

/// Drop every `defaults`-origin row and reinstall the bundled set in
/// the SAME transaction, so a mid-loop sqlx error rolls the delete
/// back too.
async fn reset_defaults_core(state: &AppState) -> Result<(u64, InstallDefaultsReport), String> {
    let entries = parse_default_cf_entries()?;

    let mut report = InstallDefaultsReport {
        installed: 0,
        skipped: 0,
        failed: 0,
        first_error: None,
    };

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| format!("Failed to open transaction: {e}"))?;

    let deleted = cf_model::delete_defaults_with_tx(&mut tx)
        .await
        .map_err(|e| format!("Reset failed (delete step): {e}"))?;

    let existing: HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT name FROM custom_formats")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| format!("Failed to read existing CFs: {e}"))?
            .into_iter()
            .collect();

    install_defaults_entries_tx(&mut tx, entries, &existing, &mut report)
        .await
        .map_err(|e| format!("Reset failed (install step): {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit reset transaction: {e}"))?;

    Ok((deleted, report))
}

/// Install the bundled default Custom Format set (plan §7.2).
#[utoipa::path(
    post,
    path = "/settings/custom-formats/install-defaults",
    tag = "Settings",
    summary = "Install the bundled default Custom Format set",
    description = "One-click install for the bundled anime-tuned default CF set (see plan §7.2). Reads from the compile-time embedded `static/default_custom_formats.json`. CFs whose name already exists are skipped (not overwritten), so repeated clicks are idempotent. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab with a flash message.",
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_install_defaults(State(state): State<AppState>) -> Redirect {
    let report = match install_default_cfs_core(&state).await {
        Ok(r) => r,
        Err(msg) => return Redirect::to(&cf_redirect(None, None, Some(&msg))),
    };

    if report.installed > 0 {
        cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
        logger::info(
            &state.db,
            LogCategory::System,
            &format!("Installed {} default Custom Format(s)", report.installed),
            &format!("skipped={}, failed={}", report.skipped, report.failed),
        )
        .await;
    }

    let summary = match (report.installed, report.skipped, report.failed) {
        (0, _, 0) => "All defaults already present — nothing to install.".to_string(),
        (n, 0, 0) => format!("Installed {n} default Custom Format(s)."),
        (n, s, 0) => format!("Installed {n}, skipped {s} already-present."),
        (0, _, f) => format!(
            "Install failed ({f} rejected). First error: {}",
            report.first_error.unwrap_or_default()
        ),
        (n, s, f) => format!(
            "Installed {n}, skipped {s}, failed {f}. First error: {}",
            report.first_error.unwrap_or_default()
        ),
    };

    if report.installed == 0 && report.failed > 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Reset the bundled default Custom Format set.
#[utoipa::path(
    post,
    path = "/settings/custom-formats/reset-defaults",
    tag = "Settings",
    summary = "Reset the bundled default Custom Format set",
    description = "Drops every CF row whose origin is `defaults` and reinstalls the bundled set from `static/default_custom_formats.json`. User-authored (`manual`) and imported (`import`) rows are left untouched. Rebuilds the compiled-CF cache on success. Redirects back to the Custom Formats settings tab with a flash message.",
    responses(
        (status = 303, description = "Redirect back to the Custom Formats settings tab"),
    ),
)]
pub async fn settings_custom_formats_reset_defaults(State(state): State<AppState>) -> Redirect {
    let (deleted, report) = match reset_defaults_core(&state).await {
        Ok(pair) => pair,
        Err(msg) => return Redirect::to(&cf_redirect(None, None, Some(&msg))),
    };

    cf_service::rebuild_cf_cache(&state.custom_formats, &state.db).await;
    logger::info(
        &state.db,
        LogCategory::System,
        &format!(
            "Reset defaults: dropped {} old, installed {} fresh",
            deleted, report.installed
        ),
        &format!("skipped={}, failed={}", report.skipped, report.failed),
    )
    .await;

    let summary = if report.failed > 0 {
        format!(
            "Reset: dropped {}, installed {}, failed {}. First error: {}",
            deleted,
            report.installed,
            report.failed,
            report.first_error.unwrap_or_default()
        )
    } else {
        format!(
            "Reset complete: dropped {} old default(s), installed {} fresh.",
            deleted, report.installed
        )
    };

    if report.failed > 0 && report.installed == 0 {
        Redirect::to(&cf_redirect(None, None, Some(&summary)))
    } else {
        Redirect::to(&cf_redirect(None, Some(&summary), None))
    }
}

/// Normalize a parsed CF import payload into a flat list of per-CF
/// entries. Plan §6.2 requires that every shape Sonarr v4 might emit
/// imports cleanly.
fn normalize_cf_import_entries(value: serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    match value {
        serde_json::Value::Array(items) => Ok(items),
        serde_json::Value::Object(ref map) => {
            if let Some(inner) = map.get("custom_formats") {
                match inner {
                    serde_json::Value::Array(items) => Ok(items.clone()),
                    serde_json::Value::Object(_) => Ok(vec![inner.clone()]),
                    _ => Err(
                        "Import failed: `custom_formats` must be an object or array.".to_string(),
                    ),
                }
            } else {
                Ok(vec![value])
            }
        }
        _ => Err("Import failed: top-level must be an object or array.".to_string()),
    }
}

/// Returns `true` if this CF's `specifications` array contains any spec
/// whose `implementation` begins with `"Ryokan."` — i.e. a Ryokan-only
/// kind that a vanilla Sonarr v4 install wouldn't recognize.
fn cf_has_ryokan_spec(cf: &serde_json::Value) -> bool {
    let Some(specs) = cf.get("specifications").and_then(|v| v.as_array()) else {
        return false;
    };
    specs.iter().any(|spec| {
        spec.get("implementation")
            .and_then(|v| v.as_str())
            .map(|s| s.starts_with("Ryokan."))
            .unwrap_or(false)
    })
}

/// Export every Custom Format as a JSON array download.
#[utoipa::path(
    get,
    path = "/settings/custom-formats/export",
    tag = "Settings",
    summary = "Export all Custom Formats as JSON",
    description = "Download every saved Custom Format as a JSON array. Default mode keeps Ryokan-namespaced specs verbatim; `?mode=sonarr-safe` drops entire CFs containing `Ryokan.`-only specs so the file imports into vanilla Sonarr v4. Each row's persisted V1-profile score is merged into the exported object.",
    params(
        ("mode" = Option<String>, Query, description = "`sonarr-safe` to drop Ryokan-only CFs"),
    ),
    responses(
        (status = 200, description = "JSON array of Custom Formats", body = serde_json::Value, content_type = "application/json"),
        (status = 500, description = "Database error"),
    ),
)]
pub async fn settings_custom_formats_export(
    State(state): State<AppState>,
    Query(query): Query<CfExportQuery>,
) -> Result<(axum::http::HeaderMap, String), (axum::http::StatusCode, String)> {
    let sonarr_safe = query
        .mode
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("sonarr-safe"))
        .unwrap_or(false);

    let id_filter: Option<HashSet<i64>> = query
        .ids
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.split(',')
                .filter_map(|tok| tok.trim().parse::<i64>().ok())
                .collect()
        });

    let rows = cf_model::list_with_scores(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut out: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut dropped_for_sonarr: Vec<String> = Vec::new();
    for row in rows {
        if let Some(ref allow) = id_filter
            && !allow.contains(&row.id)
        {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&row.json) {
            Ok(mut v) => {
                if sonarr_safe && cf_has_ryokan_spec(&v) {
                    dropped_for_sonarr.push(row.name.clone());
                    continue;
                }

                if let serde_json::Value::Object(ref mut map) = v {
                    map.insert(
                        "score".to_string(),
                        serde_json::Value::Number(row.score.into()),
                    );
                }
                out.push(v);
            }
            Err(e) => {
                tracing::warn!(
                    "custom_formats export: skipping id={} name={} — parse error: {}",
                    row.id,
                    row.name,
                    e
                );
            }
        }
    }

    if sonarr_safe && !dropped_for_sonarr.is_empty() {
        logger::info(
            &state.db,
            LogCategory::System,
            &format!(
                "Sonarr-safe CF export dropped {} CF(s) containing Ryokan-only specs",
                dropped_for_sonarr.len()
            ),
            &dropped_for_sonarr.join(", "),
        )
        .await;
    }

    let body = serde_json::to_string_pretty(&serde_json::Value::Array(out))
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    let disposition = if sonarr_safe {
        "attachment; filename=\"ryokan-custom-formats-sonarr-safe.json\""
    } else {
        "attachment; filename=\"ryokan-custom-formats.json\""
    };
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static(disposition),
    );
    Ok((headers, body))
}

/// Build a redirect target for the Custom Formats tab.
fn cf_redirect(edit_id: Option<i64>, msg: Option<&str>, err: Option<&str>) -> String {
    let mut url = String::from("/settings?tab=custom_formats");
    if let Some(id) = edit_id {
        url.push_str(&format!("&edit_id={id}"));
    }
    if let Some(m) = msg {
        url.push_str(&format!("&msg={}", urlencoding::encode(m)));
    }
    if let Some(e) = err {
        url.push_str(&format!("&err={}", urlencoding::encode(e)));
    }
    url
}

// ─────────────────────────────────────────────────────────────────────────
// CF test box (#18) — paste a release title, see which CFs match + score
// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CfTestRequest {
    pub release_title: String,
}

/// Test a release title against the current Custom Format set. Returns a
/// per-CF verdict (matched / not-matched) plus the summed score of
/// matching CFs.
///
/// **Scope limitation:** evaluates `ReleaseTitle` / `ReleaseGroup` /
/// `Resolution` / `Source` specs only, using what `classify_filename`
/// extracts from the title string. `Size` and `SeaDexBest` specs are
/// not testable from a title alone (no torrent size, no SeaDex lookup
/// without an AniList ID) — CFs that depend on them will always report
/// not-matched under this endpoint. The UI note on the test box calls
/// this out so users don't chase a phantom mismatch.
#[utoipa::path(
    post,
    path = "/api/custom-formats/test",
    tag = "Custom Formats",
    summary = "Test a release title against loaded CFs",
    description = "Returns per-CF match/not-match plus summed score. Title-based specs only; Size and SeaDex specs are not evaluated.",
    request_body = CfTestRequest,
    responses(
        (status = 200, description = "Per-CF verdicts", body = serde_json::Value),
    ),
)]
pub async fn settings_custom_formats_test(
    State(state): State<AppState>,
    Json(req): Json<CfTestRequest>,
) -> Json<serde_json::Value> {
    use crate::services::custom_formats::{EvalContext, evaluate};
    use crate::services::nyaa::SearchResult;
    use crate::services::source::{ClassificationResult, DecisionRule, Resolution, Source, WebKind};
    use crate::services::source_filename::classify_filename;

    let title = req.release_title.trim().to_string();
    if title.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "release_title is empty",
            "matched": [],
            "not_matched": [],
            "total_score": 0,
        }));
    }

    // Parse the title via the existing filename layer to extract as much
    // classification as a title alone can yield. Source is derived by
    // running the filename evidence through the aggregator — same path
    // production classification uses at layer 1.
    let fc = classify_filename(&title);
    let classification = ClassificationResult {
        source: {
            let agg = crate::services::source::aggregate(&fc.evidence);
            // aggregate() doesn't carry resolution/is_remux/is_bdmv/web_kind
            // forward — they come from `fc`.
            if agg.source == Source::Unknown && !fc.evidence.is_empty() {
                // Fall back to the strongest single piece of evidence on
                // the rare case aggregate ties everything to Unknown.
                fc.evidence
                    .iter()
                    .max_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|e| e.source)
                    .unwrap_or(Source::Unknown)
            } else {
                agg.source
            }
        },
        resolution: fc.resolution,
        is_remux: fc.is_remux,
        is_bdmv: fc.is_bdmv,
        web_kind: fc.web_kind,
        confidence: 1.0,
        needs_review: false,
        evidence: fc.evidence.clone(),
        decision_rule: DecisionRule::Empty,
    };

    let group = fc.release_group.clone().unwrap_or_default();
    let release = SearchResult {
        title: title.clone(),
        link: String::new(),
        magnet: String::new(),
        torrent: String::new(),
        size: String::new(),
        size_bytes: 0,
        seeders: 0,
        leechers: 0,
        downloads: 0,
        group: group.clone(),
        resolution: match fc.resolution {
            Resolution::Unknown => String::new(),
            r => r.as_str().to_string(),
        },
        quality_label: String::new(),
        source: String::new(),
        web_kind: match fc.web_kind {
            WebKind::Unknown => String::new(),
            w => w.as_str().to_string(),
        },
        is_remux: fc.is_remux,
        is_bdmv: fc.is_bdmv,
        is_batch: false,
        is_trusted: false,
        score: 0,
        info_hash: String::new(),
    };
    let seadex: std::collections::HashSet<String> = std::collections::HashSet::new();
    let ctx = EvalContext {
        result: &release,
        classification: &classification,
        seadex_hashes: &seadex,
    };

    let cfs = state.custom_formats.read().await.clone();
    let mut matched: Vec<serde_json::Value> = Vec::new();
    let mut not_matched: Vec<serde_json::Value> = Vec::new();
    let mut total = 0_i64;
    for cf in cfs.iter() {
        let is_match = evaluate(cf, &ctx);
        let row = serde_json::json!({
            "id": cf.id,
            "name": cf.name,
            "score": cf.score,
        });
        if is_match {
            total += cf.score as i64;
            matched.push(row);
        } else {
            not_matched.push(row);
        }
    }

    Json(serde_json::json!({
        "ok": true,
        "release_title": title,
        "parsed": {
            "source": classification.source.as_str(),
            "resolution": classification.resolution.as_str(),
            "is_remux": classification.is_remux,
            "is_bdmv": classification.is_bdmv,
            "web_kind": classification.web_kind.as_str(),
            "group": group,
        },
        "matched": matched,
        "not_matched": not_matched,
        "total_score": total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_has_ryokan_spec_returns_false_for_pure_sonarr_cf() {
        let cf = serde_json::json!({
            "name": "Synthetic BluRay CF",
            "specifications": [
                {
                    "name": "Is BluRay",
                    "implementation": "SourceSpecification",
                    "negate": false,
                    "required": true,
                    "fields": [{"name": "value", "value": 6}]
                }
            ]
        });
        assert!(!cf_has_ryokan_spec(&cf));
    }

    #[test]
    fn cf_has_ryokan_spec_returns_true_when_any_spec_is_ryokan_only() {
        let cf = serde_json::json!({
            "name": "SeaDex Best",
            "specifications": [
                {
                    "name": "Is BluRay",
                    "implementation": "SourceSpecification",
                    "negate": false,
                    "required": false,
                    "fields": [{"name": "value", "value": 6}]
                },
                {
                    "name": "SeaDex best",
                    "implementation": "Ryokan.SeaDexBestSpecification",
                    "negate": false,
                    "required": true,
                    "fields": []
                }
            ]
        });
        assert!(cf_has_ryokan_spec(&cf));
    }

    #[test]
    fn cf_has_ryokan_spec_handles_missing_or_empty_specs() {
        let empty = serde_json::json!({ "name": "Empty", "specifications": [] });
        assert!(!cf_has_ryokan_spec(&empty));

        let missing = serde_json::json!({ "name": "Missing" });
        assert!(!cf_has_ryokan_spec(&missing));
    }

    #[test]
    fn cf_has_ryokan_spec_requires_exact_prefix() {
        let cf = serde_json::json!({
            "name": "Edge",
            "specifications": [
                {
                    "implementation": "ryokan.SeaDexBestSpecification",
                    "fields": []
                },
                {
                    "implementation": "SomeRyokanThing",
                    "fields": []
                }
            ]
        });
        assert!(!cf_has_ryokan_spec(&cf));
    }

    #[test]
    fn normalize_cf_import_entries_accepts_bare_array() {
        let input = serde_json::json!([
            {"name": "First", "specifications": []},
            {"name": "Second", "specifications": []},
        ]);
        let entries = normalize_cf_import_entries(input).expect("array shape must be accepted");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "First");
        assert_eq!(entries[1]["name"], "Second");
    }

    #[test]
    fn normalize_cf_import_entries_wraps_bare_single_object() {
        let input = serde_json::json!({"name": "Solo", "specifications": []});
        let entries = normalize_cf_import_entries(input).expect("object shape must be accepted");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "Solo");
    }

    #[test]
    fn normalize_cf_import_entries_unwraps_custom_formats_wrapper() {
        let input = serde_json::json!({
            "custom_formats": [
                {"name": "Wrapped One", "specifications": []},
                {"name": "Wrapped Two", "specifications": []},
            ]
        });
        let entries = normalize_cf_import_entries(input).expect("wrapper shape must be accepted");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "Wrapped One");
        assert_eq!(entries[1]["name"], "Wrapped Two");
    }

    #[test]
    fn normalize_cf_import_entries_unwraps_single_object_inside_wrapper() {
        let input = serde_json::json!({
            "custom_formats": {"name": "Wrapped Solo", "specifications": []}
        });
        let entries = normalize_cf_import_entries(input).expect("wrapped object must be accepted");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "Wrapped Solo");
    }

    #[test]
    fn normalize_cf_import_entries_rejects_scalar_top_level() {
        let err = normalize_cf_import_entries(serde_json::json!("not a cf"))
            .expect_err("scalar must be rejected");
        assert!(err.contains("top-level"), "got error: {err}");
    }

    #[test]
    fn normalize_cf_import_entries_rejects_scalar_inner_wrapper() {
        let input = serde_json::json!({"custom_formats": 42});
        let err =
            normalize_cf_import_entries(input).expect_err("scalar inside wrapper must be rejected");
        assert!(err.contains("custom_formats"), "got error: {err}");
    }

    #[test]
    fn parse_collision_decisions_handles_all_three_actions() {
        let decisions = "0:skip\n1:overwrite\n2:rename";
        let renames = "2:My New Name";
        let out = parse_collision_decisions(decisions, renames);

        assert_eq!(out.len(), 3);
        assert!(matches!(out.get(&0), Some(CollisionDecision::Skip)));
        assert!(matches!(out.get(&1), Some(CollisionDecision::Overwrite)));
        match out.get(&2) {
            Some(CollisionDecision::Rename(name)) => assert_eq!(name, "My New Name"),
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn parse_collision_decisions_treats_unknown_actions_as_skip() {
        let out = parse_collision_decisions("7:nonsense", "");
        assert!(matches!(out.get(&7), Some(CollisionDecision::Skip)));
    }

    #[test]
    fn parse_collision_decisions_drops_malformed_lines() {
        let decisions = "\n  \nnot_a_number:skip\nmissing_colon\n3:overwrite";
        let out = parse_collision_decisions(decisions, "");
        assert_eq!(out.len(), 1);
        assert!(matches!(out.get(&3), Some(CollisionDecision::Overwrite)));
    }

    #[test]
    fn parse_collision_decisions_rename_without_entry_has_empty_name() {
        let out = parse_collision_decisions("5:rename", "");
        match out.get(&5) {
            Some(CollisionDecision::Rename(name)) => assert!(name.is_empty()),
            other => panic!("expected Rename with empty name, got {other:?}"),
        }
    }

    #[test]
    fn import_summary_shapes_by_counter_combinations() {
        assert_eq!(import_summary(0, 0, 0, None), "Nothing to import.");
        assert_eq!(
            import_summary(3, 0, 0, None),
            "Imported 3 Custom Format(s)."
        );
        assert_eq!(
            import_summary(2, 1, 0, None),
            "Imported 2, skipped 1 on collision."
        );
        assert_eq!(
            import_summary(0, 0, 2, Some("oops".to_string())),
            "Import failed (2 rejected). First error: oops"
        );
        assert_eq!(
            import_summary(1, 1, 1, Some("bad".to_string())),
            "Imported 1, skipped 1, failed 1. First error: bad"
        );
    }

    #[test]
    fn import_summary_preserves_skipped_count_when_imported_is_zero() {
        let msg = import_summary(0, 1, 1, Some("regex".to_string()));
        assert!(msg.contains("skipped 1"), "missing skipped count: {msg}");
        assert!(msg.contains("failed 1"), "missing failed count: {msg}");
        assert!(msg.contains("regex"), "missing error context: {msg}");
    }

    #[test]
    fn import_summary_shapes_imports_with_failures_only() {
        assert_eq!(
            import_summary(2, 0, 1, Some("oops".to_string())),
            "Imported 2, failed 1. First error: oops"
        );
    }
}
