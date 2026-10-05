//! Issue #28 — autobrr API key rotation.
//!
//! `POST /settings/autobrr/regenerate-key` mints a fresh
//! 32-byte URL-safe random key, writes it onto the singleton
//! config row, and redirects back to the indexers tab (where
//! the autobrr fieldset lives — co-located with the torznab /
//! newznab indexer rows since both are release sources).
//! Distinct from the regular settings save flow so an
//! accidental tab POST can't silently rotate or wipe the key —
//! the user has to click the dedicated button (with confirm
//! prompt) to mint a new key, which invalidates any existing
//! autobrr deployments using the old one.

use axum::{extract::State, response::Response};
use axum_htmx::HxRequest;
use base64::Engine;
use rand::Rng;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::models::config;
use crate::models::log::LogCategory;
use crate::services::logger;

/// Mint a fresh 32-byte random key, base64url-encoded (43 chars,
/// no padding). Same shape as the OAuth state nonce + PKCE
/// verifier helpers — URL-safe so it survives the user pasting
/// into autobrr's `X-Api-Key` header field, no padding to keep
/// it on one line.
fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[utoipa::path(
    post,
    path = "/settings/autobrr/regenerate-key",
    tag = "Settings",
    summary = "Regenerate the autobrr webhook API key",
    description = "Mints a fresh 32-byte URL-safe random key and persists it on the config row. Existing autobrr deployments using the old key will start receiving 401 from `/api/webhook/autobrr` until reconfigured. Redirects back to the indexers tab.",
    responses(
        (status = 303, description = "Redirect back to the indexers tab"),
    ),
)]
pub async fn settings_autobrr_regenerate_key(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
) -> Response {
    // Read-modify-write of the whole config row: under the same lock as
    // every other settings save, or a concurrent save of another tab
    // writes back its stale copy over this change.
    let _guard = super::CONFIG_WRITE_LOCK.lock().await;
    // Read the existing config to preserve every other field —
    // this handler must rotate ONLY the autobrr_api_key.
    let mut cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        _ => {
            // No config row yet (fresh install). Build a default
            // and save with the new key. The Connections tab
            // would normally have the user's other choices but
            // this branch only runs pre-first-save.
            config::Config::default()
        }
    };
    cfg.autobrr_api_key = generate_key();
    if let Err(e) = config::save_config(&state.db, &cfg).await {
        logger::error(
            &state.db,
            LogCategory::System,
            "autobrr key rotation failed",
            &e.to_string(),
        )
        .await;
        return htmx_aware_redirect(
            is_htmx,
            "/settings?tab=indexers&err=autobrr+key+rotation+failed",
        );
    }
    logger::info(
        &state.db,
        LogCategory::System,
        "autobrr API key rotated",
        "",
    )
    .await;
    htmx_aware_redirect(
        is_htmx,
        "/settings?tab=indexers&msg=autobrr+key+regenerated",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_is_url_safe_and_43_chars() {
        let k = generate_key();
        // 32 bytes → 43 chars in URL_SAFE_NO_PAD encoding.
        assert_eq!(k.len(), 43);
        assert!(
            k.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "key must be URL-safe: {k}"
        );
    }

    #[test]
    fn successive_keys_are_distinct() {
        // Smoke check on the RNG — a fixed value here would mean
        // the key is predictable, breaking the auth model.
        let a = generate_key();
        let b = generate_key();
        assert_ne!(a, b);
    }
}
