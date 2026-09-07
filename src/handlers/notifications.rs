//! Notifications handler surface.
//!
//! Settings UI's "Send test" button lands here. Resolves the live
//! provider via cache lookup (gh-121 will wire the save handler to
//! call `rebuild_notification_providers_cache` so a freshly-saved
//! row is visible immediately; until then the cache is only
//! populated on startup), synthesizes a `Health` event, and returns
//! the receiver's HTTP status + truncated body inline so users can
//! debug from the Settings UI without opening browser devtools.
//!
//! Future endpoints (per-provider CRUD, the matrix-toggle endpoints
//! powering issue #121's Settings UI) land as siblings here.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::AppState;
use crate::services::notifications::{NotificationEvent, TestSendResult, discord, store, webhook};

/// `POST /api/notifications/{id}/test` — send a synthetic
/// `Health { kind: "test", message: "..." }` event to the targeted
/// provider only. Bypasses the per-event matrix (Health is
/// default-off so a matrix-honoring path would no-op).
///
/// Response shape:
/// - 200 + `{"status": <int>, "body": "<truncated>"}` on send-side
///   success (means the request hit the receiver — receiver may
///   still have returned a 4xx/5xx, which is what `status` reports
///   for the webhook kind; Discord 200/204 maps the same way).
/// - 4xx / 5xx + `{"error": "..."}` for transport failures, timeouts,
///   serialization errors, or "provider not in cache."
///
/// `provider not in cache` is a 404 because the row may exist in the
/// DB but be disabled, or have just been deleted from another tab.
/// `transport error` / `timeout` / receiver-non-2xx is 502 — Ryokan is
/// the upstream proxy here; the receiver is the unreachable origin.
/// Serialization failures are 500 (programmer error in Ryokan, not a
/// user-fixable state).
#[utoipa::path(
    post,
    path = "/api/notifications/{id}/test",
    tag = "System",
    summary = "Send a test notification",
    description = "Sends a synthetic health event to one notification provider, bypassing the per-event matrix. The status field reports what the receiver returned.",
    params(("id" = i64, Path, description = "Notification provider id")),
    responses(
        (status = 200, description = "Request reached the receiver", body = TestSendResult),
        (status = 404, description = "Provider not found or disabled"),
        (status = 502, description = "Receiver unreachable or timed out"),
    ),
)]
pub async fn test_provider(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<TestSendResult>, (StatusCode, Json<serde_json::Value>)> {
    // Resolve through the live cache first — the cached snapshot is
    // what the dispatcher would actually use, so a "provider not in
    // cache" 404 is the most accurate user signal (row may be
    // disabled, just deleted from another tab, or saved before
    // gh-121 wires settings-driven rebuilds — currently only the
    // boot-time rebuild populates the cache).
    let providers = state.notification_providers.read().await.clone();
    let cached = providers
        .iter()
        .find(|p| p.id() == id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("notification provider #{id} not found in cache (disabled or recently deleted?)"),
                })),
            )
        })?;

    // The trait's `Arc<dyn NotificationProvider>` doesn't expose its
    // underlying config (object-safety), and the inline test path
    // wants the receiver's status + body — which the trait's `send`
    // throws away in service of the dispatcher's fire-and-forget
    // shape. Solution: re-load the row and reconstruct a one-shot
    // typed provider for the test path. Cheap (one DB query + one
    // URL parse) and keeps the trait surface unchanged.
    let row = match store::get_provider(&state.db, id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("notification provider #{id} not found"),
                })),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("DB read failed: {e}")})),
            ));
        }
    };

    let event = NotificationEvent::Health {
        kind: "test".into(),
        message: "Test notification from Ryokan".into(),
    };

    match cached.kind() {
        "webhook" => {
            let p = webhook::WebhookProvider::from_row(row.id, row.name, &row.config_json)
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": format!("invalid webhook config: {e}"),
                        })),
                    )
                })?;
            webhook::send_test(&p, &event).await.map(Json).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e})),
                )
            })
        }
        "discord" => {
            let p = discord::DiscordProvider::from_row(
                row.id,
                row.name,
                &row.config_json,
                state.db.clone(),
            )
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("invalid discord config: {e}"),
                    })),
                )
            })?;
            discord::send_test(&p, &event).await.map(Json).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e})),
                )
            })
        }
        other => Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": format!("test endpoint not yet wired for provider kind {other:?}"),
            })),
        )),
    }
}
