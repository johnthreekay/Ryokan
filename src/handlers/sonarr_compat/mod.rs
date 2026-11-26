//! Sonarr v3 API compatibility layer for Seerr integration.
//!
//! Split per Phase 4: types / system / series / helpers submodules.

use axum::{extract::State, http::Request, middleware::Next, response::Response};

use crate::AppState;

mod helpers;
mod queue;
mod series;
mod system;
mod types;

pub use queue::queue;
pub use series::{
    add_series, execute_command, get_series, list_series, series_lookup, update_series,
};
pub use system::{
    create_tag, language_profiles, list_download_clients, list_tags, quality_profiles,
    root_folders, system_status,
};

// ── Authentication middleware ──────────────────────────────────────────────

/// Thin wrapper over [`crate::handlers::arr_auth::check_api_key`] that pins
/// the config-field selector to the Sonarr slots. The shared helper carries
/// the rationale for the 503/401 split, percent-decoding, and constant-time
/// compare.
pub async fn require_api_key(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    crate::handlers::arr_auth::check_api_key(
        state,
        req,
        next,
        |cfg| (cfg.sonarr_enabled, cfg.sonarr_api_key.clone()),
        "Sonarr",
    )
    .await
}

#[cfg(test)]
mod tests;
