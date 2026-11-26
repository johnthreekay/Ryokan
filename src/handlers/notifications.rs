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
use crate::services::notifications::{
    NotificationEvent, TestSendResult, discord, script, store, webhook,
};

/// What the Send test button gets back: what a receiver answered, or
/// how a script's test run ended.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum TestOutcome {
    Receiver(TestSendResult),
    Script(ScriptTestResult),
}

/// A custom script's test run (#288): its exit code and the end of what
/// it wrote (stderr when there was any, else stdout).
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ScriptTestResult {
    pub exit_code: i32,
    pub output: String,
}

/// How much of a script's output the page gets back.
const SCRIPT_TEST_OUTPUT_CHARS: usize = 500;

/// `POST /api/notifications/{id}/test` — send a synthetic
/// `Health { kind: "test", message: "..." }` event to the targeted
/// provider only. Bypasses the per-event matrix (Health is
/// default-off so a matrix-honoring path would no-op).
///
/// A custom script (#288) gets no event at all: it is run once with
/// `ryokan_eventtype=Test`, as Sonarr tests its own, and the answer is
/// `{"exit_code": <int>, "output": "<truncated>"}`.
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
        (status = 200, description = "Request reached the receiver, or the script ran", body = TestOutcome),
        (status = 403, description = "Custom scripts are turned off"),
        (status = 404, description = "Provider not found or disabled"),
        (status = 422, description = "The script path is refused"),
        (status = 502, description = "Receiver unreachable or timed out"),
        (status = 504, description = "The script ran past the test's time limit"),
    ),
)]
pub async fn test_provider(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<TestOutcome>, (StatusCode, Json<serde_json::Value>)> {
    // A script row is never in the cache while scripts are turned off,
    // and "disabled or recently deleted?" would be the wrong hint.
    if !script::enabled()
        && let Ok(Some(row)) = store::get_provider(&state.db, id).await
        && row.kind == "script"
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": script::disabled_message()})),
        ));
    }

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
            webhook::send_test(&p, &event)
                .await
                .map(|r| Json(TestOutcome::Receiver(r)))
                .map_err(|e| {
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
            discord::send_test(&p, &event)
                .await
                .map(|r| Json(TestOutcome::Receiver(r)))
                .map_err(|e| {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({"error": e})),
                    )
                })
        }
        "script" => {
            let config: script::ScriptConfig =
                serde_json::from_str(&row.config_json).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": format!("invalid script config: {e}"),
                        })),
                    )
                })?;
            script_test_outcome(script::run_test(&state.db, &config).await).map(Json)
        }
        other => Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": format!("test endpoint not yet wired for provider kind {other:?}"),
            })),
        )),
    }
}

/// A test run's result as the endpoint's answer. A run that could not
/// start is the path being refused (or gone), which is the user's to
/// fix: 422. One stopped at the limit is 504.
fn script_test_outcome(
    run: Result<script::ScriptOutput, String>,
) -> Result<TestOutcome, (StatusCode, Json<serde_json::Value>)> {
    let output = run.map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": e})),
        )
    })?;
    let said = script::tail(output.said(), SCRIPT_TEST_OUTPUT_CHARS);
    if output.timed_out {
        return Err((
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": format!(
                    "The script was still running after {} seconds and was stopped. A test run gets no file to work on, so it should finish quickly.",
                    script::TEST_TIMEOUT.as_secs()
                ),
            })),
        ));
    }
    match output.exit_code {
        Some(exit_code) => Ok(TestOutcome::Script(ScriptTestResult {
            exit_code,
            output: said,
        })),
        None => Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "The script was ended by a signal before it finished.",
            })),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn output(exit_code: Option<i32>, timed_out: bool, stderr: &str) -> script::ScriptOutput {
        script::ScriptOutput {
            exit_code,
            timed_out,
            elapsed: Duration::from_millis(5),
            stdout: "from stdout".into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn a_script_test_reports_the_exit_code_and_what_it_wrote() {
        let ok = script_test_outcome(Ok(output(Some(0), false, ""))).unwrap();
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            serde_json::json!({"exit_code": 0, "output": "from stdout"})
        );
        // A non-zero exit is still an answer; the page says it failed.
        let failed = script_test_outcome(Ok(output(Some(2), false, "no such codec"))).unwrap();
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            serde_json::json!({"exit_code": 2, "output": "no such codec"})
        );
    }

    #[test]
    fn a_receivers_answer_keeps_its_shape() {
        let outcome = TestOutcome::Receiver(TestSendResult {
            status: 204,
            body: String::new(),
        });
        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            serde_json::json!({"status": 204, "body": ""})
        );
    }

    #[test]
    fn a_script_that_never_started_or_never_finished_is_an_error() {
        let (status, body) = script_test_outcome(Err("/x.sh is not a file".into())).unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body.0["error"], "/x.sh is not a file");

        let (status, body) = script_test_outcome(Ok(output(None, true, ""))).unwrap_err();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(body.0["error"].as_str().unwrap().contains("30 seconds"));

        let (status, _) = script_test_outcome(Ok(output(None, false, ""))).unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
}
