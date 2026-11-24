//! DTOs shared between the Sonarr and Radarr compat shims.
//!
//! The two shims duplicated these six structs before Phase 4. They're
//! hoisted here so the wire-level JSON schema stays in one place —
//! if Seerr's validator suddenly starts requiring a new field on
//! `SystemStatus`, we add it once and both shims inherit the fix.
//!
//! What's NOT in here: RootFolder (Sonarr has `total_space` but Radarr
//! has `accessible: bool` instead — genuinely different shapes), the
//! main `SonarrSeries` / `RadarrMovie` payloads (large, structurally
//! very different), and the Add / Update request bodies (Sonarr has
//! `seasons`, Radarr has `minimumAvailability` — different fields
//! per resource).

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityProfile {
    pub id: i32,
    pub name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub id: i32,
    pub label: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatus {
    pub version: String,
    pub build_time: String,
    pub is_debug: bool,
    pub is_production: bool,
    pub is_admin: bool,
    pub is_user_interactive: bool,
    pub startup_path: String,
    pub app_data: String,
    pub os_name: String,
    pub os_version: String,
    pub is_net_core: bool,
    pub is_mono: bool,
    pub is_linux: bool,
    pub is_osx: bool,
    pub is_windows: bool,
    pub is_docker: bool,
    pub mode: String,
    pub branch: String,
    pub authentication: String,
    pub sqlite_version: String,
    pub migration_version: i32,
    pub url_base: String,
    pub runtime_version: String,
    pub runtime_name: String,
    pub start_time: String,
    pub package_update_mechanism: String,
    pub app_name: String,
}

impl SystemStatus {
    /// Fake-Sonarr / fake-Radarr status payload. Material fields:
    /// `app_name` (Seerr's indicator-pill text), `os_name` (Seerr's
    /// path-separator inference for `root_folder_path` validation),
    /// `start_time` (the connected app's uptime as Seerr's UI
    /// displays it — pass `state.start_time` so it reflects actual
    /// process boot rather than a hardcoded long-stale timestamp).
    pub fn default_with_name(app_name: &str, start_time: time::UtcDateTime) -> Self {
        Self {
            // Sonarr v4 is the current line; the Radarr override below
            // bumps to a v6 string. Seerr uses `app_name` for its UI
            // pill and `os_name` for path-separator inference, but
            // doesn't version-gate its v3 API client off this string.
            // Bump these alongside upstream Sonarr / Radarr stable
            // releases so the indicator in Seerr reads as up-to-date.
            version: "4.0.17.2952".to_string(),
            build_time: "2024-01-01T00:00:00Z".to_string(),
            is_debug: false,
            is_production: true,
            is_admin: false,
            is_user_interactive: false,
            startup_path: String::new(),
            app_data: String::new(),
            os_name: "linux".to_string(),
            os_version: String::new(),
            is_net_core: true,
            is_mono: false,
            is_linux: true,
            is_osx: false,
            is_windows: false,
            is_docker: false,
            mode: "default".to_string(),
            branch: "main".to_string(),
            authentication: "none".to_string(),
            sqlite_version: String::new(),
            migration_version: 0,
            url_base: String::new(),
            runtime_version: String::new(),
            runtime_name: String::new(),
            start_time: crate::services::datetime::rfc3339(start_time),
            package_update_mechanism: "builtIn".to_string(),
            app_name: app_name.to_string(),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DownloadClientEntry {
    pub id: i32,
    pub name: String,
    pub enable: bool,
    pub protocol: String,
    pub implementation: String,
    pub config_contract: String,
    pub priority: i32,
}

#[derive(Deserialize)]
pub struct TagBody {
    pub label: Option<String>,
}

/// Shared `?term=` query shape for `/series/lookup` (Sonarr) and
/// `/movie/lookup` (Radarr). Same field, same Seerr call pattern.
#[derive(Deserialize)]
pub struct LookupQuery {
    pub term: String,
}
