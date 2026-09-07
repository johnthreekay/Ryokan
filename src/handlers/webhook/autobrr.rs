//! Issue #28 — autobrr push webhook.
//!
//! `POST /api/webhook/autobrr` is a Ryokan-native webhook
//! receiver for autobrr's IRC-announce push integration. The
//! flow:
//!
//!   1. autobrr's filter step fires when a tracker's IRC announce
//!      matches user-configured rules (resolution, group, size).
//!   2. autobrr POSTs a JSON body to this endpoint with the
//!      release metadata + the indexer name.
//!   3. Ryokan authenticates via API key, looks up the matching
//!      indexer row to apply seed rules, dedups against
//!      `grabbed_torrents`, matches the release title to a
//!      tracked series via [`crate::services::rss::match_library_title`],
//!      and adds the torrent to the active download client.
//!
//! ## Wire shape
//!
//! ```json
//! {
//!   "torrent_name": "[Group] Show - 01 [BD 1080p]",
//!   "info_hash": "ec039a525a6feac4b15889323f4f443de381e7cc",
//!   "magnet_uri": "magnet:?xt=urn:btih:...",
//!   "torrent_url": "https://tracker.example/torrent/123",
//!   "indexer": "AnimeBytes",
//!   "filter": "Anime - 1080p",
//!   "size_bytes": 1234567890
//! }
//! ```
//!
//! Required: `torrent_name`, one of (`magnet_uri` | `torrent_url`),
//! `indexer`. Everything else is best-effort. The user configures
//! their autobrr Webhook action with a JSON body template that
//! emits these fields — Ryokan documents the template in the
//! Settings → Connections → autobrr panel.
//!
//! ## Authentication
//!
//! `X-Api-Key` header OR `?apikey=` query param, constant-time
//! compared against `config.autobrr_api_key`. Missing key /
//! mismatch = 401. Empty configured key = 503 + Retry-After
//! ("autobrr webhook is disabled").
//!
//! ## Out of scope (future work)
//!
//! - autobrr's own filter trace (filter ID, matched rule). Their
//!   webhook macro set carries this; v1 logs it but doesn't act
//!   on it.
//! - Per-indexer-defaults overrides on push (e.g., "this autobrr
//!   filter for AB skips Ryokan's PT-upgrade gate"). The plan
//!   defers this to a follow-up.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::handlers::auth::sanitize_for_log_capped;
use crate::models::log::LogCategory;
use crate::models::{config, grabbed_torrents};
use crate::services::download_client::{self, AddOutcome};
use crate::services::{logger, rss};

/// JSON body autobrr POSTs. Field names match the Ryokan-native
/// shape documented in the module header. Everything but
/// `torrent_name` + `indexer` + at least one download URL is
/// optional; `#[serde(default)]` covers tolerance to missing keys
/// from a user's hand-edited template.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AutobrrPayload {
    pub torrent_name: String,
    #[serde(default)]
    pub info_hash: String,
    #[serde(default)]
    pub magnet_uri: String,
    #[serde(default)]
    pub torrent_url: String,
    pub indexer: String,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub size_bytes: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AutobrrResponse {
    pub status: &'static str,
    pub message: String,
}

fn ok(message: impl Into<String>) -> (StatusCode, Json<AutobrrResponse>) {
    (
        StatusCode::OK,
        Json(AutobrrResponse {
            status: "ok",
            message: message.into(),
        }),
    )
}

fn skipped(message: impl Into<String>) -> (StatusCode, Json<AutobrrResponse>) {
    // Use 200 for "we successfully decided not to grab this"
    // outcomes (dedup hit, untracked series). autobrr treats
    // non-2xx as a delivery failure and retries; "no series
    // tracked yet" isn't a failure worth retrying.
    (
        StatusCode::OK,
        Json(AutobrrResponse {
            status: "skipped",
            message: message.into(),
        }),
    )
}

fn err_json(code: StatusCode, message: impl Into<String>) -> (StatusCode, Json<AutobrrResponse>) {
    (
        code,
        Json(AutobrrResponse {
            status: "error",
            message: message.into(),
        }),
    )
}

/// API-key auth using the same constant-time-compare shape as
/// the arr-shim middleware. Returns `Ok(())` when authorized,
/// `Err(response)` to short-circuit the handler.
async fn check_api_key(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    query: Option<&str>,
) -> Result<(), (StatusCode, Json<AutobrrResponse>)> {
    let cfg = match config::get_config(&state.db).await {
        Ok(Some(c)) => c,
        _ => {
            return Err(err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "Ryokan config not yet available",
            ));
        }
    };
    let expected = cfg.autobrr_api_key.trim().to_string();
    if expected.is_empty() {
        return Err(err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "autobrr webhook is disabled — generate an API key in Settings → Connections",
        ));
    }
    let provided = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            let q = query.unwrap_or("");
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "apikey" {
                    Some(urlencoding::decode(v).ok()?.into_owned())
                } else {
                    None
                }
            })
        });
    let valid = match &provided {
        Some(k) => bool::from(k.as_bytes().ct_eq(expected.as_bytes())),
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(err_json(
            StatusCode::UNAUTHORIZED,
            "Invalid or missing API key",
        ))
    }
}

#[utoipa::path(
    post,
    path = "/api/webhook/autobrr",
    tag = "Webhook",
    summary = "autobrr push webhook",
    description = "Receives a release push from autobrr's Webhook action and dispatches it to the active download client. API key required via X-Api-Key header or ?apikey= query param. The release is matched against tracked series via title-token overlap; unmatched releases are skipped (200 with status=skipped) so autobrr doesn't retry. Per-indexer seed rules from the matching `indexers` row apply automatically.",
    request_body = AutobrrPayload,
    responses(
        (status = 200, description = "Push handled (grabbed, deduped, or skipped)", body = AutobrrResponse),
        (status = 401, description = "Missing or invalid API key", body = AutobrrResponse),
        (status = 503, description = "Webhook disabled (no API key configured)", body = AutobrrResponse),
        (status = 400, description = "Malformed payload (missing required fields)", body = AutobrrResponse),
    ),
)]
pub async fn webhook_autobrr(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    let (parts, body) = req.into_parts();
    if let Err(resp) = check_api_key(&state, &parts.headers, parts.uri.query().or(Some(""))).await {
        return resp;
    }
    let bytes = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return err_json(StatusCode::BAD_REQUEST, "body too large or unreadable");
        }
    };
    let payload: AutobrrPayload = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            return err_json(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}"));
        }
    };

    // Validate the minimum field set.
    if payload.torrent_name.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "torrent_name is required");
    }
    if payload.indexer.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "indexer is required");
    }
    let download_url = if !payload.magnet_uri.trim().is_empty() {
        payload.magnet_uri.trim().to_string()
    } else if !payload.torrent_url.trim().is_empty() {
        payload.torrent_url.trim().to_string()
    } else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "either magnet_uri or torrent_url is required",
        );
    };
    let info_hash_lc = payload.info_hash.trim().to_ascii_lowercase();

    // Dedup: skip if Ryokan already has this hash in flight or
    // imported. Same shape as the RSS dedup — autobrr can race
    // against torznab polling (and against the user manually
    // adding via the UI).
    let safe_release = sanitize_for_log_capped(&payload.torrent_name, 256);
    // PR #108 review round 2 #4 — check blocklist BEFORE the
    // pending/imported dedup. When both rows exist (rare: user
    // blocklists a release the post-proc is still cleaning up,
    // then autobrr re-pushes), the blocklist hit is the more-
    // actionable telemetry. Both branches return skipped, but
    // the operator wants to see "blocklisted" not "duplicate."
    if !info_hash_lc.is_empty()
        && grabbed_torrents::is_blocklisted(&state.db, &info_hash_lc)
            .await
            .unwrap_or(false)
    {
        logger::info(
            &state.db,
            LogCategory::Grab,
            &format!("autobrr: skipping {safe_release} — hash is blocklisted"),
            &info_hash_lc,
        )
        .await;
        return skipped("hash is blocklisted");
    }

    // PR 112 review #4 — `info_hash_lc` is the autobrr-supplied
    // BT-style infohash. For SAB grabs, `grabbed_torrents.hash`
    // stores the `nzo_id` (per the PR 112 trait extension), so
    // this lookup misses for previously-grabbed-via-SAB releases
    // and the second push proceeds to `add_torrent_returning_id`.
    // SAB's own pre-queue dedup catches it via the
    // `AlreadyPresent` fallback, so behavior isn't broken — but
    // the dedup-before-wire-call intent is silently bypassed for
    // SAB. Acceptable for v1; a follow-up could record the BT-
    // style hash on a separate column for SAB rows so the lookup
    // matches either id.
    if !info_hash_lc.is_empty() && grabbed_torrents::is_known_hash(&state.db, &info_hash_lc).await {
        logger::info(
            &state.db,
            LogCategory::Grab,
            &format!("autobrr: skipping {safe_release} — hash already in grabbed_torrents"),
            &info_hash_lc,
        )
        .await;
        return skipped("duplicate hash already grabbed");
    }

    // Match the indexer name (case-insensitive) so seed rules
    // apply at grab time. autobrr's `indexer` field carries the
    // tracker name (e.g., "AnimeBytes") and Ryokan's indexer row
    // for the corresponding torznab feed should match by name —
    // the user picks the row's name when adding via Settings.
    //
    // Reads the cached `Vec<Arc<dyn Indexer>>` rather than
    // hitting the DB so a high-rate autobrr push doesn't
    // re-query `indexers` per call. The cache is rebuilt on
    // Settings → Indexers edits.
    let indexer_id = {
        let snapshot = state.indexers.read().await.clone();
        snapshot
            .iter()
            .find(|i| i.name().trim().eq_ignore_ascii_case(payload.indexer.trim()))
            .map(|i| i.id())
    };
    let safe_indexer = sanitize_for_log_capped(&payload.indexer, 256);
    if indexer_id.is_none() {
        // Per the plan: "If autobrr names a release from an
        // unconfigured indexer, surface it as an error in logs +
        // skip rather than grab with default rules." A user who
        // really wants the grab can add the indexer to Ryokan
        // first. Surfacing the gap in logs is the only signal —
        // a 200 with status=skipped keeps autobrr from retrying.
        logger::warn(
            &state.db,
            LogCategory::Grab,
            &format!(
                "autobrr: '{safe_indexer}' refers to an indexer Ryokan doesn't have configured — skipping {safe_release}"
            ),
            &safe_indexer,
        )
        .await;
        return skipped("indexer not configured in Ryokan");
    }

    // Match the release to a tracked series. autobrr filters are
    // configured per series (or per group of series), so a push
    // that doesn't map to any tracked series is a configuration
    // mismatch — log + skip.
    let matched = match rss::match_library_title(&state.db, &payload.torrent_name, false).await {
        Some(m) => m,
        None => {
            logger::info(
                &state.db,
                LogCategory::Grab,
                &format!("autobrr: no tracked series matched release '{safe_release}'"),
                &safe_release,
            )
            .await;
            return skipped("no tracked series matched the release title");
        }
    };
    let (series, ep_nums) = matched;

    // Hand off to the download client. Multi-client routing —
    // resolve via the matched indexer's pin first, then fall through
    // to the default. `indexer_id` is `Some(_)` here because the
    // earlier guard returned `skipped()` if the indexer wasn't
    // configured.
    let (client, dispatch_client_id) = match state.client_for_indexer_with_id(indexer_id).await {
        Some(t) => t,
        None => {
            logger::error(
                &state.db,
                LogCategory::Grab,
                "autobrr: no download client configured — cannot dispatch push",
                &safe_release,
            )
            .await;
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "no download client configured",
            );
        }
    };
    // `add_torrent_returning_id` returns the canonical client-
    // side id alongside the outcome. For BT clients the returned id
    // equals the input info_hash; for SAB it's the `nzo_id` SAB
    // hands back from `mode=addurl`. Either way, persist the
    // returned value so post-processing's `list_scoped` matching
    // works for both protocols.
    let (add_outcome, canonical_id) = match client
        .add_torrent_returning_id(&download_url, &info_hash_lc)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::Grab,
                &format!("autobrr: client add_torrent failed for '{safe_release}'"),
                &e,
            )
            .await;
            return err_json(StatusCode::BAD_GATEWAY, format!("client add failed: {e}"));
        }
    };

    // Record the grab + apply per-indexer seed rules + stamp
    // attribution. Same flow as the auto_search inner loop's
    // post-grab block.
    //
    // `record_grab` returns `None` when the row couldn't be
    // persisted (DB error, or the empty-hash + FK-violation
    // anomaly path documented on the model). The torrent is
    // already in the client at this point, so we don't unwind —
    // we just skip the seed-rule + attribution stamp. The user
    // sees the grab via `client.list_scoped()`; the missing
    // grab row gets caught by the next reconcile pass.
    let grab_id = grabbed_torrents::record_grab(
        &state.db,
        &canonical_id,
        &payload.torrent_name,
        series.id,
        &ep_nums,
        ep_nums.len() > 1,
    )
    .await
    .ok()
    .flatten();
    // Misgrab guardrails: keep the URL so Restore can re-add a removed grab.
    if let Some(gid) = grab_id {
        let _ =
            crate::models::grabbed_torrents::set_source_url(&state.db, gid, &download_url).await;
    }
    if let Some(gid) = grab_id {
        let respected = download_client::apply_indexer_seed_rules(
            &state.db,
            &*client,
            &canonical_id,
            indexer_id,
        )
        .await;
        let _ =
            grabbed_torrents::set_indexer_attribution(&state.db, gid, indexer_id, respected).await;
        // Stamp the client this grab landed on so post-processing
        // routes `list_scoped` / `get_files` to the same place. NULL
        // would force fall-through to the current default — wrong
        // when the grab actually went to a non-default client.
        let _ =
            grabbed_torrents::set_download_client(&state.db, gid, Some(dispatch_client_id)).await;
        // Issue #118 — fire `Grabbed` on the autobrr push path. No
        // scoring pass runs (autobrr already filtered upstream), so
        // `score = None`. Indexer resolves from the autobrr-supplied
        // tracker → `indexers` row mapping in `indexer_id`.
        let indexer =
            crate::services::notifications::resolve_indexer_name(&state, indexer_id).await;
        crate::services::notifications::emit_grabbed(
            &state,
            series.id,
            ep_nums.first().copied().unwrap_or(0),
            &payload.torrent_name,
            indexer,
            None,
            Some(client.sonarr_impl_name().to_string()),
        )
        .await;
    }

    let outcome_label = match add_outcome {
        AddOutcome::Added => "added",
        AddOutcome::AlreadyPresent => "already_present",
    };
    let safe_filter = sanitize_for_log_capped(&payload.filter, 256);
    let size_label = if payload.size_bytes > 0 {
        format!(", size_bytes={}", payload.size_bytes)
    } else {
        String::new()
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!(
            "autobrr push: '{}' → series #{} ({}) [{}]",
            safe_release, series.id, series.title, outcome_label
        ),
        &format!("indexer={safe_indexer}, filter={safe_filter}{size_label}"),
    )
    .await;
    ok(format!(
        "grabbed for series '{}' ({})",
        series.title, outcome_label
    ))
}
