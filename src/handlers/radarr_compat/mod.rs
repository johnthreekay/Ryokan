//! Radarr v3 API compatibility layer for Seerr integration (anime movies).
//!
//! Mounted under `/radarr/api/v3/`. Split per Phase 4: types / system /
//! movie / helpers submodules.

use axum::{extract::State, http::Request, middleware::Next, response::Response};

use crate::AppState;

mod helpers;
mod movie;
mod queue;
mod system;
mod types;

pub use movie::{add_movie, execute_command, get_movie, list_movies, movie_lookup, update_movie};
pub use queue::queue;
pub use system::{
    create_tag, list_download_clients, list_tags, quality_profiles, root_folders, system_status,
};

// ── Authentication middleware ──────────────────────────────────────────────

/// Thin wrapper over [`crate::handlers::arr_auth::check_api_key`] that pins
/// the config-field selector to the Radarr slots.
pub async fn require_api_key(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    crate::handlers::arr_auth::check_api_key(
        state,
        req,
        next,
        |cfg| (cfg.radarr_enabled, cfg.radarr_api_key.clone()),
        "Radarr",
    )
    .await
}

#[cfg(test)]
mod tests;
