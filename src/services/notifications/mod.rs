//! Outbound notification dispatch (issue #118).
//!
//! Foundation for the per-provider issues (#119 webhook, #120 Discord)
//! and the settings UI (#121). This module ships the trait, the
//! event taxonomy, the cache shape, and the per-provider fan-out
//! dispatcher — but no concrete provider impls. With an empty cache
//! every `dispatch` call is a no-op `tokio::spawn` that exits on the
//! `pool.is_empty()` early-return.
//!
//! ## Storage shape
//!
//! ```text
//! AppState.notification_providers : Arc<RwLock<Arc<Vec<Arc<dyn NotificationProvider>>>>>
//!         └── outer RwLock owns swap (Settings save → rebuild)
//!             └── inner Arc<Vec<_>> cheap-cloned out per dispatch
//!                 └── per-provider Arc<dyn NotificationProvider>
//! ```
//!
//! Mirrors `CompiledCfCache` / `DownloadClientsCache` / `IndexerCache`
//! exactly. The dispatch hot path clones the inner `Arc` once under
//! the read lock and runs lock-free over the snapshot.
//!
//! ## Dispatch is fire-and-forget
//!
//! `dispatch` spawns a task and returns immediately so a hung
//! receiver can't block the user-visible operation that triggered
//! the event. Per-provider `send` is wrapped in
//! `tokio::time::timeout(5s)` so even one wedged Discord webhook
//! can't keep the dispatch task alive forever.
//!
//! ## No persistent retry queue
//!
//! Failed sends log via `LogCategory::Notifications` and drop. A
//! durable queue (dedup, backoff, ordering) is real follow-up work
//! and gets deferred until users report dropped events as a real
//! problem.

use async_trait::async_trait;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub mod discord;
pub mod event;
pub mod store;
pub mod webhook;

#[cfg(test)]
mod wiremock_tests;

pub use event::{ALL_EVENT_KINDS, DEFAULT_ON_EVENT_KINDS, NotificationEvent};

use crate::models::log::LogCategory;

/// Per-provider send budget. One slow / hung receiver must not stall
/// the dispatch task indefinitely. 5 s matches the issue spec; the
/// actual receiver-side budget is whatever the provider impl picks
/// for its `reqwest::Client::timeout` — this is an outer ceiling.
const PROVIDER_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// One configured outbound destination (one webhook URL, one
/// Discord webhook, one future Telegram bot, etc.). Object-safe
/// so `Arc<dyn NotificationProvider>` storage on the cache works.
#[async_trait]
pub trait NotificationProvider: Send + Sync {
    /// Stable id from `notification_providers.id`. Lets the per-event
    /// matrix key by provider without round-tripping through `name`,
    /// and lets the test-send endpoint (`/api/notifications/{id}/test`,
    /// landing in the per-provider issues) target a single provider
    /// from the snapshot.
    fn id(&self) -> i64;

    /// User-given label. Used in log lines + Settings UI.
    fn name(&self) -> &str;

    /// Trait-impl discriminator. `&'static str` so we get it for
    /// log-line tagging at zero cost. Must match the `kind` column
    /// in `notification_providers` so the cache rebuild can pick the
    /// right impl per row.
    fn kind(&self) -> &'static str;

    /// Per-provider send. `Result<_, String>` matches the project
    /// convention; the dispatcher prefix-tags failures into the
    /// `Notifications` log category. Implementations return Err on
    /// transport failures and on receiver-returned errors that are
    /// not 2xx; they should not return Err for "I logged it locally
    /// instead" — that path is for the dispatcher.
    async fn send(&self, event: &NotificationEvent) -> Result<(), String>;
}

/// Outcome of a `send_test` round-trip — the receiver's HTTP status
/// plus a truncated body. Returned to the Settings UI's "Send test"
/// button so users can see what the receiver said inline rather than
/// opening browser devtools. Generic across providers, so it works
/// both for the generic webhook (body is whatever the receiver echoes)
/// and Discord (success bodies are usually empty `204 No Content`,
/// failure bodies are JSON error envelopes).
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct TestSendResult {
    pub status: u16,
    pub body: String,
}

/// Same swap-on-write shape as `IndexerCache` / `CompiledCfCache`.
/// Outer `RwLock` owns swap; the inner `Arc<Vec<_>>` is cheap-cloned
/// out under the read lock and walked lock-free.
pub type NotificationProviders = Arc<RwLock<Arc<Vec<Arc<dyn NotificationProvider>>>>>;

/// Empty cache, used at process boot before
/// `rebuild_notification_providers_cache` runs and as the default
/// for tests that don't care about notifications.
pub fn empty_cache() -> NotificationProviders {
    Arc::new(RwLock::new(Arc::new(Vec::new())))
}

/// Production dispatch. Fire-and-forget — spawns a task, returns
/// immediately. The user-visible operation (grab, import, classify)
/// must not block on Discord webhook latency.
///
/// 1. Cheap-clone the inner `Arc<Vec<_>>` under the read lock.
/// 2. Per-provider, look up the per-event opt-in from
///    `notification_settings`. Default-deny on missing rows.
/// 3. Per-provider, call `send` wrapped in
///    `tokio::time::timeout(PROVIDER_SEND_TIMEOUT)`.
/// 4. Per-provider failures (timeout or `Err`) emit a
///    `LogCategory::Notifications` warn row with the provider name
///    + kind + event kind + truncated error.
pub fn dispatch(cache: &NotificationProviders, db: SqlitePool, event: NotificationEvent) {
    let cache = cache.clone();
    tokio::spawn(async move {
        let providers = cache.read().await.clone();
        if providers.is_empty() {
            return;
        }
        // Build per-provider futures concurrently. Each runs in its
        // own `tokio::spawn` for **panic-isolation specifically**:
        // `join_all` would propagate a panic from any provider up to
        // the outer dispatch task and abort the remaining concurrent
        // sends. The JoinHandle boundary catches the panic so a
        // misbehaving provider impl can't take its peers down with
        // it. Per-provider Err returns and timeouts are already
        // handled cooperatively inside `fan_out_one`; the spawn is
        // strictly for the panic case.
        let mut handles = Vec::with_capacity(providers.len());
        for provider in providers.iter().cloned() {
            let db = db.clone();
            let event = event.clone();
            handles.push(tokio::spawn(async move {
                fan_out_one(provider, db, event).await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

/// Single-provider awaited send for the test endpoint
/// (`POST /api/notifications/{id}/test` — landing in the per-provider
/// issues). Bypasses the per-event matrix so testing a `Health`
/// event from the Settings UI fires even when Health is default-off,
/// and returns the provider's `send` result so the caller can render
/// it in the UI.
pub async fn send_to(
    cache: &NotificationProviders,
    provider_id: i64,
    event: NotificationEvent,
) -> Result<(), String> {
    let providers = cache.read().await.clone();
    let provider = providers
        .iter()
        .find(|p| p.id() == provider_id)
        .cloned()
        .ok_or_else(|| format!("notification provider #{provider_id} not in cache"))?;
    match tokio::time::timeout(PROVIDER_SEND_TIMEOUT, provider.send(&event)).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "notification provider {} ({}) timed out after {}s",
            provider.name(),
            provider.kind(),
            PROVIDER_SEND_TIMEOUT.as_secs(),
        )),
    }
}

async fn fan_out_one(
    provider: Arc<dyn NotificationProvider>,
    db: SqlitePool,
    event: NotificationEvent,
) {
    // Per-event opt-in matrix. Default-deny on missing rows: the
    // settings handler seeds `DEFAULT_ON_EVENT_KINDS` rows at
    // provider creation, so a fresh provider receives the
    // conservative defaults; everything else is explicitly opted
    // in via the Settings UI.
    let matrix = match store::matrix_for_provider(&db, provider.id()).await {
        Ok(m) => m,
        Err(e) => {
            crate::services::logger::warn(
                &db,
                LogCategory::Notifications,
                "matrix lookup failed",
                &format!(
                    "provider={} kind={} err={}",
                    provider.name(),
                    provider.kind(),
                    truncate(&e.to_string(), 200),
                ),
            )
            .await;
            return;
        }
    };
    let event_kind = event.kind();
    if !matrix.get(event_kind).copied().unwrap_or(false) {
        return;
    }

    let send_fut = provider.send(&event);
    match tokio::time::timeout(PROVIDER_SEND_TIMEOUT, send_fut).await {
        Ok(Ok(())) => {
            crate::services::logger::info(
                &db,
                LogCategory::Notifications,
                "sent",
                &format!(
                    "provider={} kind={} event={}",
                    provider.name(),
                    provider.kind(),
                    event_kind,
                ),
            )
            .await;
        }
        Ok(Err(e)) => {
            crate::services::logger::warn(
                &db,
                LogCategory::Notifications,
                "send failed",
                &format!(
                    "provider={} kind={} event={} err={}",
                    provider.name(),
                    provider.kind(),
                    event_kind,
                    truncate(&e, 500),
                ),
            )
            .await;
        }
        Err(_) => {
            crate::services::logger::warn(
                &db,
                LogCategory::Notifications,
                "send timed out",
                &format!(
                    "provider={} kind={} event={} after={}s",
                    provider.name(),
                    provider.kind(),
                    event_kind,
                    PROVIDER_SEND_TIMEOUT.as_secs(),
                ),
            )
            .await;
        }
    }
}

/// Char-iterator-based truncation, UTF-8-safe at multi-byte
/// boundaries. Returns `s` unchanged when `s.chars().count() <= max`,
/// otherwise the first `max - 1` chars plus an ellipsis. Shared
/// across the dispatcher's log-line truncation, the webhook
/// receiver-body cap, and Discord's per-field embed limits — every
/// site has the same shape and the same UTF-8-safety requirement
/// (anime release titles legitimately contain CJK characters).
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Resolve `indexer_id` to its display name via the live indexer
/// cache snapshot. Returns `None` for `indexer_id = None` (Nyaa-direct
/// path) and for ids that aren't currently in the cache (recently
/// deleted indexer). Centralized so the per-call-site `emit_grabbed`
/// wiring doesn't have to repeat the snapshot-walk pattern.
pub async fn resolve_indexer_name(
    state: &crate::AppState,
    indexer_id: Option<i64>,
) -> Option<String> {
    let id = indexer_id?;
    let snap = state.indexers.read().await.clone();
    snap.iter()
        .find(|i| i.id() == id)
        .map(|i| i.name().to_string())
}

/// Resolve `series_id` to a non-empty display title via the standard
/// romaji → english → native → bare-title fallback chain. Returns
/// `None` when the series row is missing OR every title column is
/// empty (a partial-fetch path may insert a row before any title
/// source resolved). Centralized so every `emit_*` helper that needs
/// a series title flows through one SQL shape — drift between four
/// hand-maintained copies surfaces as the wire-event title drifting
/// across event kinds, which would break downstream receivers
/// matching on the value.
async fn resolve_series_title(db: &SqlitePool, series_id: i64) -> Option<String> {
    sqlx::query_scalar(
        "SELECT CASE
                  WHEN COALESCE(title_romaji, '') <> '' THEN title_romaji
                  WHEN COALESCE(title_english, '') <> '' THEN title_english
                  WHEN COALESCE(title_native, '') <> '' THEN title_native
                  ELSE title
                END
         FROM series WHERE id = ?",
    )
    .bind(series_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .filter(|s: &String| !s.is_empty())
}

/// Convenience: build a `Grabbed` event from the call-site context
/// and dispatch it through the cache. Centralizes the field shape so
/// every call site builds the same struct — adding a field on the
/// event later means updating one place + one signature instead of
/// chasing every caller.
///
/// `series_title` is fetched lazily from the `series` row, walking
/// the romaji → english → native → bare-title chain so callers don't
/// have to `JOIN` it in just for the event. The chain matches the
/// classifier's display preference; `config.title_language` isn't
/// honored deliberately — the event JSON is a stable wire contract,
/// and re-deriving the user's localization preference per-receiver
/// belongs in the receiver. Failure to resolve a non-empty title
/// (missing series row OR every title column empty) logs at debug
/// and short-circuits the dispatch — emitting with an empty title
/// is a worse UX signal than no event at all.
#[allow(clippy::too_many_arguments)]
pub async fn emit_grabbed(
    state: &crate::AppState,
    series_id: i64,
    episode_number: i32,
    release_title: &str,
    indexer: Option<String>,
    score: Option<i32>,
    client_kind: Option<String>,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    let Some(series_title) = resolve_series_title(&state.db, series_id).await else {
        tracing::debug!(
            "notifications::emit_grabbed: series #{series_id} not found, skipping dispatch"
        );
        return;
    };
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::Grabbed {
            series_id,
            series_title,
            episode_number,
            release_title: release_title.to_string(),
            indexer,
            score,
            client_kind,
        },
    );
}

/// Convenience: dispatch an `Imported` event from
/// `services::post_processing` after a per-file copy/hardlink/move
/// succeeds. Resolves `series_title` and `quality_tag` from the DB
/// the same way `emit_grabbed` resolves `series_title`. The event is
/// fire-and-forget on top of the dispatch task so the post-processing
/// loop doesn't block on receiver latency.
/// Misgrab guardrails: a grab's files named a different series. One
/// event per grab row by construction (the verdict is stamped once).
pub async fn emit_misgrabbed(
    state: &crate::AppState,
    series_id: i64,
    release_title: &str,
    hash: &str,
    files: Vec<String>,
    action: &str,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    let Some(series_title) = resolve_series_title(&state.db, series_id).await else {
        tracing::debug!(
            "notifications::emit_misgrabbed: series #{series_id} not found, skipping dispatch"
        );
        return;
    };
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::Misgrabbed {
            series_id,
            series_title,
            release_title: release_title.to_string(),
            hash: hash.to_string(),
            files,
            action: action.to_string(),
        },
    );
}

#[allow(clippy::too_many_arguments)]
pub async fn emit_imported(
    state: &crate::AppState,
    series_id: i64,
    episode_number: i32,
    source_path: &str,
    dest_path: &str,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    let Some(series_title) = resolve_series_title(&state.db, series_id).await else {
        tracing::debug!(
            "notifications::emit_imported: series #{series_id} not found, skipping dispatch"
        );
        return;
    };
    // Quality tag is best-effort — the row may not exist for an
    // unmonitored episode the user grabbed manually, in which case
    // we surface an empty tag rather than skipping the event.
    let quality_tag: String = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(quality_tag, '') FROM episode_quality_tags
             WHERE series_id = ? AND episode_number = ?",
    )
    .bind(series_id)
    .bind(episode_number)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::Imported {
            series_id,
            series_title,
            episode_number,
            source_path: source_path.to_string(),
            dest_path: dest_path.to_string(),
            quality_tag,
        },
    );
}

/// Convenience: dispatch an `ImportFailed` event from
/// `services::post_processing`. Fired on per-file failures (couldn't
/// parse episode number, file not video, fs op error). `episode_number`
/// is optional because some failure paths happen before parse.
pub async fn emit_import_failed(
    state: &crate::AppState,
    series_id: i64,
    episode_number: Option<i32>,
    source_path: &str,
    reason: &str,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    let Some(series_title) = resolve_series_title(&state.db, series_id).await else {
        tracing::debug!(
            "notifications::emit_import_failed: series #{series_id} not found, skipping dispatch"
        );
        return;
    };
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::ImportFailed {
            series_id,
            series_title,
            episode_number,
            source_path: source_path.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Convenience: dispatch a `ClassifierNeedsReview` event from
/// `models::episode_tags::update_classification` when the row being
/// written has `needs_review = true`. Single write site; one event
/// per row that flips into the needs-review state. Default-off in
/// the per-event matrix because classifier reclassify sweeps can
/// produce hundreds of rows in a short window.
pub async fn emit_classifier_needs_review(
    state: &crate::AppState,
    series_id: i64,
    episode_number: i32,
    confidence: i32,
    verdict_summary: &str,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    let Some(series_title) = resolve_series_title(&state.db, series_id).await else {
        tracing::debug!(
            "notifications::emit_classifier_needs_review: series #{series_id} not found, skipping dispatch"
        );
        return;
    };
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::ClassifierNeedsReview {
            series_id,
            series_title,
            episode_number,
            confidence,
            verdict_summary: verdict_summary.to_string(),
        },
    );
}

/// Per-id "fired recently" state for `IndexerDown` and
/// `DownloadClientUnreachable`. Both events are emitted opportunistically
/// from hot paths (RSS poll, status probe) that can re-fire dozens of
/// times an hour for a single broken target — without dedup, a
/// misconfigured indexer would spam every Discord webhook on every RSS
/// tick. The cooldown is per-id so two different broken indexers don't
/// suppress each other's first ping.
///
/// `Instant` is the firing time; `remaining` returns `None` once the
/// stamp is older than [`HEALTH_DEDUP_WINDOW`]. Lazily evicts on read so
/// the table doesn't grow forever on long-lived processes.
const HEALTH_DEDUP_WINDOW: Duration = Duration::from_secs(60 * 60);

static INDEXER_DOWN_FIRED: LazyLock<StdMutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

static DC_UNREACHABLE_FIRED: LazyLock<StdMutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Returns true if the caller should fire the event. Stamps `now`
/// when allowed so the next call within the window short-circuits.
fn dedup_check(map: &StdMutex<HashMap<i64, Instant>>, id: i64) -> bool {
    let Ok(mut guard) = map.lock() else {
        // Poisoned lock — rather than swallow the event, allow the
        // fire. The next successful lock will overwrite the stamp.
        return true;
    };
    let now = Instant::now();
    match guard.get(&id) {
        Some(prev) if now.duration_since(*prev) < HEALTH_DEDUP_WINDOW => false,
        _ => {
            guard.insert(id, now);
            true
        }
    }
}

/// Test-only: drop the dedup stamp for a single indexer id.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_indexer_down_dedup_for_tests(id: i64) {
    if let Ok(mut g) = INDEXER_DOWN_FIRED.lock() {
        g.remove(&id);
    }
}

/// Test-only: drop the dedup stamp for a single download-client id.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_dc_unreachable_dedup_for_tests(id: i64) {
    if let Ok(mut g) = DC_UNREACHABLE_FIRED.lock() {
        g.remove(&id);
    }
}

/// Convenience: dispatch an `IndexerDown` event with per-id dedup
/// (1h cooldown per `indexer_id`). Fired from the RSS-tick indexer poll
/// when an `Indexer::search()` call returns Err — that's the continuous
/// background poll that surfaces "your indexer started failing"
/// without waiting on a user to open the Settings UI. Subsequent
/// failures during the cooldown window are suppressed; once the
/// indexer recovers and a single window passes without a fire, the
/// next failure re-fires.
///
/// `indexer_name` is resolved from the live indexer cache snapshot
/// (matching `resolve_indexer_name`'s shape); a stale id that's been
/// deleted from the cache short-circuits with no event.
pub async fn emit_indexer_down(state: &crate::AppState, indexer_id: i64, reason: &str) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    if !dedup_check(&INDEXER_DOWN_FIRED, indexer_id) {
        return;
    }
    let Some(indexer_name) = resolve_indexer_name(state, Some(indexer_id)).await else {
        tracing::debug!(
            "notifications::emit_indexer_down: indexer #{indexer_id} not in cache, skipping dispatch"
        );
        return;
    };
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::IndexerDown {
            indexer_name,
            reason: reason.to_string(),
        },
    );
}

/// Convenience: dispatch a `DownloadClientUnreachable` event with
/// per-id dedup (1h cooldown per `download_client_id`). Fired from the
/// Settings → Connections status-probe handler when `client.test()`
/// returns Err. The probe fires on Settings → Connections page load
/// and on the auto-refresh cadence, so dedup is the difference
/// between one ping and a stream of identical alerts.
///
/// `client_kind` is the wire kind discriminator ("qbittorrent",
/// "deluge", etc.) resolved from the row, so the event payload can be
/// correlated to the impl. `client_label` is the kind-pretty-print used
/// in the log line.
pub async fn emit_download_client_unreachable(
    state: &crate::AppState,
    client_id: i64,
    client_kind: &str,
    reason: &str,
) {
    let providers = state.notification_providers.read().await.clone();
    if providers.is_empty() {
        return;
    }
    if !dedup_check(&DC_UNREACHABLE_FIRED, client_id) {
        return;
    }
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::DownloadClientUnreachable {
            client_kind: client_kind.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Convenience: dispatch an `ExternalSyncReLinkRequired` event for a
/// given provider string (`"anilist"` / `"mal"`). Fired at the same
/// point the sticky `last_sync_auth_failed` flag is flipped on.
pub fn emit_external_sync_relink_required(state: &crate::AppState, provider: &str) {
    dispatch(
        &state.notification_providers,
        state.db.clone(),
        NotificationEvent::ExternalSyncReLinkRequired {
            provider: provider.to_string(),
        },
    );
}

/// Atomically swap in a fresh `Vec<Arc<dyn NotificationProvider>>`
/// built from every enabled row in `notification_providers`. Called
/// at startup (after `migrations::migrate`) and from the Settings
/// handler that mutates the table.
///
/// Until provider impls land in the per-provider issues, this just
/// loads the rows and logs an unknown-kind warning for each one
/// before installing an empty snapshot. The shape is in place so
/// follow-up PRs only need to add a per-kind constructor arm here.
pub async fn rebuild_notification_providers_cache(cache: &NotificationProviders, db: &SqlitePool) {
    // On DB-load failure, keep the existing snapshot live. A
    // transient blip (lock contention, brief WAL checkpoint stall,
    // a concurrent migration) shouldn't silence every notification
    // provider until the next rebuild — we'd rather keep delivering
    // events with the last-known config than no events at all. The
    // previously-installed snapshot stays valid until the next
    // successful rebuild after a settings save.
    let rows = match store::list_enabled(db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "notification_providers: failed to load from DB; keeping previous snapshot: {e}"
            );
            return;
        }
    };
    // Per-kind constructor dispatch. New `kind` strings land here
    // alongside their per-impl module — `webhook` is #119, `discord`
    // is #120. A row with an unrecognized kind is logged and dropped
    // so a hand-edited DB / pre-provider Settings save surfaces in
    // System → Logs rather than silently swallowing.
    let mut providers: Vec<Arc<dyn NotificationProvider>> = Vec::new();
    for row in rows {
        let built: Option<Arc<dyn NotificationProvider>> = match row.kind.as_str() {
            "webhook" => {
                match webhook::WebhookProvider::from_row(row.id, row.name.clone(), &row.config_json)
                {
                    Ok(p) => Some(Arc::new(p)),
                    Err(e) => {
                        tracing::warn!(
                            "notification_providers: skipping webhook #{} ({}): {}",
                            row.id,
                            row.name,
                            e,
                        );
                        None
                    }
                }
            }
            "discord" => match discord::DiscordProvider::from_row(
                row.id,
                row.name.clone(),
                &row.config_json,
                db.clone(),
            ) {
                Ok(p) => Some(Arc::new(p)),
                Err(e) => {
                    tracing::warn!(
                        "notification_providers: skipping discord #{} ({}): {}",
                        row.id,
                        row.name,
                        e,
                    );
                    None
                }
            },
            other => {
                tracing::warn!(
                    "notification_providers: skipping #{} ({}); unknown kind {:?}",
                    row.id,
                    row.name,
                    other,
                );
                None
            }
        };
        if let Some(p) = built {
            providers.push(p);
        }
    }
    *cache.write().await = Arc::new(providers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that records every event it received. Used to
    /// cover dispatch concurrency, isolation, and the per-event
    /// matrix.
    struct RecordingProvider {
        id: i64,
        name: String,
        sent: Arc<AtomicUsize>,
        behavior: Behavior,
    }

    enum Behavior {
        Ok,
        Err(String),
        Hang,
    }

    #[async_trait]
    impl NotificationProvider for RecordingProvider {
        fn id(&self) -> i64 {
            self.id
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> &'static str {
            "test"
        }
        async fn send(&self, _event: &NotificationEvent) -> Result<(), String> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::Ok => Ok(()),
                Behavior::Err(e) => Err(e.clone()),
                Behavior::Hang => {
                    // Hang past the dispatcher's per-provider timeout.
                    tokio::time::sleep(PROVIDER_SEND_TIMEOUT * 3).await;
                    Ok(())
                }
            }
        }
    }

    async fn build_provider(db: &SqlitePool, id: i64, name: &str, seed_defaults: bool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO notification_providers (id, name, kind, enabled, config_json)
             VALUES (?, ?, 'test', 1, '{}') RETURNING id",
        )
        .bind(id)
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap();
        if seed_defaults {
            store::seed_default_matrix(db, row.0).await.unwrap();
        }
        row.0
    }

    fn cache_with(providers: Vec<Arc<dyn NotificationProvider>>) -> NotificationProviders {
        Arc::new(RwLock::new(Arc::new(providers)))
    }

    fn grabbed() -> NotificationEvent {
        NotificationEvent::Grabbed {
            series_id: 1,
            series_title: "Test".into(),
            episode_number: 7,
            release_title: "Test - 07".into(),
            indexer: None,
            score: None,
            client_kind: None,
        }
    }

    #[tokio::test]
    async fn dispatch_fans_out_to_every_opted_in_provider() {
        let db = in_memory_pool().await;
        let id_a = build_provider(&db, 1, "a", true).await;
        let id_b = build_provider(&db, 2, "b", true).await;
        let sent_a = Arc::new(AtomicUsize::new(0));
        let sent_b = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![
            Arc::new(RecordingProvider {
                id: id_a,
                name: "a".into(),
                sent: sent_a.clone(),
                behavior: Behavior::Ok,
            }),
            Arc::new(RecordingProvider {
                id: id_b,
                name: "b".into(),
                sent: sent_b.clone(),
                behavior: Behavior::Ok,
            }),
        ]);

        dispatch(&cache, db, grabbed());
        // Allow the spawned dispatch task to run.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent_a.load(Ordering::SeqCst), 1);
        assert_eq!(sent_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_isolates_per_provider_failures() {
        // Provider A returns Err, provider B must still receive the
        // event. Per-provider failure isolation is the core invariant
        // that prevents a single bad webhook from blackholing every
        // other receiver.
        let db = in_memory_pool().await;
        let id_a = build_provider(&db, 1, "a", true).await;
        let id_b = build_provider(&db, 2, "b", true).await;
        let sent_a = Arc::new(AtomicUsize::new(0));
        let sent_b = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![
            Arc::new(RecordingProvider {
                id: id_a,
                name: "a".into(),
                sent: sent_a.clone(),
                behavior: Behavior::Err("nope".into()),
            }),
            Arc::new(RecordingProvider {
                id: id_b,
                name: "b".into(),
                sent: sent_b.clone(),
                behavior: Behavior::Ok,
            }),
        ]);

        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent_a.load(Ordering::SeqCst), 1);
        assert_eq!(sent_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn matrix_default_deny_skips_provider_without_rows() {
        // No matrix rows seeded — provider must be skipped, not
        // default-on'd. Pinned because a regression that flipped
        // default to "on" would suddenly fan out every event to
        // every provider on a fresh schema, defeating the
        // per-event matrix entirely.
        let db = in_memory_pool().await;
        let id = build_provider(&db, 1, "a", false).await;
        let sent = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![Arc::new(RecordingProvider {
            id,
            name: "a".into(),
            sent: sent.clone(),
            behavior: Behavior::Ok,
        })]);
        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn matrix_skips_opted_out_event_kinds() {
        let db = in_memory_pool().await;
        let id = build_provider(&db, 1, "a", true).await;
        // Default-on includes Grabbed; flip it off to assert opt-out.
        sqlx::query(
            "UPDATE notification_settings SET enabled = 0
             WHERE provider_id = ? AND event_kind = 'Grabbed'",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![Arc::new(RecordingProvider {
            id,
            name: "a".into(),
            sent: sent.clone(),
            behavior: Behavior::Ok,
        })]);
        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dispatch_does_not_block_on_a_hung_provider() {
        // The hung provider's send sleeps past `PROVIDER_SEND_TIMEOUT`.
        // The healthy provider must still receive the event well
        // within that window — pinned at "the dispatch task scheduled
        // both before sleeping," not "the dispatch task blocked
        // serially on the hung send." The fan-out shape uses a per-
        // provider tokio::spawn so this is a reachable state.
        let db = in_memory_pool().await;
        let id_hang = build_provider(&db, 1, "hung", true).await;
        let id_ok = build_provider(&db, 2, "ok", true).await;
        let sent_ok = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![
            Arc::new(RecordingProvider {
                id: id_hang,
                name: "hung".into(),
                sent: Arc::new(AtomicUsize::new(0)),
                behavior: Behavior::Hang,
            }),
            Arc::new(RecordingProvider {
                id: id_ok,
                name: "ok".into(),
                sent: sent_ok.clone(),
                behavior: Behavior::Ok,
            }),
        ]);
        dispatch(&cache, db, grabbed());
        // Healthy provider must complete well before the hung
        // provider's timeout (5s).
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            sent_ok.load(Ordering::SeqCst),
            1,
            "healthy provider must not be blocked by the hung one"
        );
    }

    #[tokio::test]
    async fn dispatch_with_empty_cache_is_a_no_op() {
        let db = in_memory_pool().await;
        let cache = empty_cache();
        // Just must not panic / hang. No assertion necessary beyond
        // "this returns within the test budget."
        dispatch(&cache, db, grabbed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn send_to_bypasses_matrix() {
        // Test endpoint should fire even when the event_kind is
        // default-off. Pinned because the Settings UI surfaces this
        // as a "Send test" button — defaulting it to a Health event
        // (default-off) and silently no-op'ing through the matrix
        // would misrepresent the receiver as broken.
        let db = in_memory_pool().await;
        let id = build_provider(&db, 1, "a", false).await;
        let sent = Arc::new(AtomicUsize::new(0));
        let cache = cache_with(vec![Arc::new(RecordingProvider {
            id,
            name: "a".into(),
            sent: sent.clone(),
            behavior: Behavior::Ok,
        })]);
        send_to(
            &cache,
            id,
            NotificationEvent::Health {
                kind: "test".into(),
                message: "hello".into(),
            },
        )
        .await
        .expect("send_to ok");
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        let _ = db;
    }

    #[tokio::test]
    async fn send_to_unknown_provider_returns_err() {
        let cache = empty_cache();
        let res = send_to(
            &cache,
            999,
            NotificationEvent::Health {
                kind: "test".into(),
                message: "x".into(),
            },
        )
        .await;
        assert!(res.is_err());
    }

    #[test]
    fn truncate_handles_unicode_grapheme_count() {
        // The `Notifications` log lines pass receiver error bodies
        // through `truncate`. A naive byte-slice would panic on a
        // multi-byte UTF-8 boundary; the chars-based form must hold.
        // Output must NEVER exceed `max` chars — receivers like
        // Discord enforce hard caps (1024 for embed field values),
        // so a `max + 1` shape would 400 the request right at the
        // boundary case.
        let long = "あいうえお".repeat(100);
        let out = truncate(&long, 5);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn truncate_passes_short_strings_through_unchanged() {
        assert_eq!(truncate("hi", 100), "hi");
        assert_eq!(truncate("", 100), "");
    }

    #[test]
    fn truncate_zero_max_is_passthrough() {
        // max=0 → return unchanged (don't crash). Defensive — no
        // call site uses 0 today, but the saturating_sub and the
        // edge condition cost nothing.
        assert_eq!(truncate("hi", 0), "hi");
    }

    // Dedup tests live in their own private map so they can't race
    // against the production `INDEXER_DOWN_FIRED` /
    // `DC_UNREACHABLE_FIRED` tables under nextest's default parallelism.
    // The production paths exercise the same `dedup_check` function;
    // the production maps just hold the live state.
    fn private_map() -> StdMutex<HashMap<i64, Instant>> {
        StdMutex::new(HashMap::new())
    }

    #[test]
    fn dedup_check_fires_first_call() {
        let map = private_map();
        assert!(dedup_check(&map, 1));
    }

    #[test]
    fn dedup_check_suppresses_second_call_within_window() {
        let map = private_map();
        assert!(dedup_check(&map, 1));
        assert!(
            !dedup_check(&map, 1),
            "second call within HEALTH_DEDUP_WINDOW must short-circuit"
        );
    }

    #[test]
    fn dedup_check_is_per_id() {
        let map = private_map();
        assert!(dedup_check(&map, 1));
        assert!(
            dedup_check(&map, 2),
            "different id must NOT inherit id 1's stamp — per-target dedup"
        );
        assert!(!dedup_check(&map, 1));
        assert!(!dedup_check(&map, 2));
    }

    #[test]
    fn dedup_check_re_fires_after_window_elapses() {
        // Inject an Instant from > HEALTH_DEDUP_WINDOW ago. Without
        // this, the test would have to actually sleep 1h. The
        // production path observes the same comparison.
        let map = private_map();
        let stale = Instant::now() - HEALTH_DEDUP_WINDOW - Duration::from_secs(1);
        map.lock().unwrap().insert(1, stale);
        assert!(
            dedup_check(&map, 1),
            "stamp older than HEALTH_DEDUP_WINDOW must re-fire"
        );
    }
}
