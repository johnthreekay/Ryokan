//! OAuth start / submit endpoints for AniList and MyAnimeList.
//!
//! Issue #62. Each provider has two endpoints:
//!
//!   - `GET  /settings/oauth/{provider}/start`  — redirects the user's
//!     browser to the provider's authorize URL. For MAL we first
//!     generate a PKCE verifier and stash it in
//!     [`services::oauth_state`] so the submit path can echo it back.
//!   - `POST /settings/oauth/{provider}/submit` — accepts the token
//!     (AL) or code (MAL) the user pasted from the Ryokan-hosted
//!     broker page, validates it by calling the provider's user-info
//!     endpoint, and persists the linked account via
//!     [`models::external_accounts::link`].
//!
//! Plus one shared unlink endpoint:
//!
//!   - `POST /settings/oauth/unlink` — drops the currently-linked
//!     account's row. Per decision #8, imported series stay put.
//!
//! Flow diagrams live in the #62 plan doc. Summary:
//!
//! - **AL (Implicit Grant):** Ryokan → AL authorize → broker page
//!   (token in URL fragment, never reaches a server) → user copies
//!   → pastes into Ryokan → Ryokan calls AL's `Viewer` GraphQL query
//!   with the pasted token to validate + fetch username +
//!   score_format.
//! - **MAL (Auth Code + PKCE plain):** Ryokan generates verifier →
//!   stashes → redirects to MAL authorize with
//!   `code_challenge = verifier` + `code_challenge_method = plain`
//!   (MAL doesn't support S256) → broker page displays code → user
//!   pastes → Ryokan retrieves verifier, POSTs to MAL's token
//!   endpoint with `code` + `code_verifier` → receives access +
//!   refresh tokens → calls `GET /v2/users/@me` for username.
//!
//! Client IDs are hardcoded per decision #2 (shared native-app
//! client pattern — Mihon, Taiga, etc. do the same). No secrets:
//! AL implicit grant doesn't need one; MAL's "other" app type gets
//! none since PKCE substitutes.

use std::sync::LazyLock;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::models::external_accounts::{self, LinkRequest, PROVIDER_ANILIST, PROVIDER_MAL};
use crate::models::log::LogCategory;
use crate::services::{anilist, external_sync, logger, oauth_state, progress};

/// AniList public client ID. Registered 2026-04-22 against the
/// author's AL account; the AL app page documents the redirect URI
/// as `https://johnthreekay.github.io/Ryokan/auth/anilist/`. Safe
/// to ship in the binary per OAuth public-client conventions.
const ANILIST_CLIENT_ID: &str = "39806";

/// MyAnimeList public client ID (App Type `other`). Registered
/// 2026-04-22. Native-client OAuth flow substitutes PKCE for a
/// client secret, which is the honest OAuth 2.0 public-client model.
const MAL_CLIENT_ID: &str = "5205ccde38839a4afc6b03bbecfaa9c7";

/// Ryokan-hosted broker pages (orphan `gh-pages` branch on the
/// GitHub repo). Static HTML that reads the token / code out of the
/// URL and displays it for the user to copy-paste back into Ryokan.
/// URL fragments on the AL side never touch an HTTP server; the MAL
/// side query param is public-by-design (MAL's own auth page already
/// displayed it).
///
/// AL's redirect URI lives on AniList's developer-settings side (the
/// app config); we don't include it in the authorize URL because AL's
/// docs example doesn't, and including it triggered an
/// `unsupported_grant_type` error against AL's oauth-server backend
/// post-approval. The URL is documented here for reference (it's the
/// value the user sets in https://anilist.co/settings/developer
/// against the `Ryokan` app), but isn't read from code.
/// MAL's redirect URI *is* sent in the URL — that's required by MAL
/// and matches their docs.
const MAL_REDIRECT_URI: &str = "https://johnthreekay.github.io/Ryokan/auth/mal/";

/// Verifier length in base64url characters. RFC 7636 allows 43-128.
/// 43 is the minimum, derived from 32 random bytes. Enough entropy;
/// keeps the URL short.
const PKCE_VERIFIER_LEN: usize = 43;

// ── AniList start ────────────────────────────────────────────────────

pub async fn anilist_start(State(state): State<AppState>) -> Redirect {
    // Generate a CSRF state nonce and stash it. The verifier slot
    // stays empty for AL (implicit grant has no PKCE step), but we
    // reuse the OAuthAttempt shape so both providers go through the
    // same validation path at /submit.
    let csrf_state = generate_state_nonce();
    oauth_state::stash(
        &state.oauth_state,
        PROVIDER_ANILIST,
        String::new(),
        csrf_state.clone(),
    );

    // Implicit grant: response_type=token. AL returns the access
    // token directly in the URL fragment after user approval, along
    // with our `state` echoed back unchanged.
    //
    // `redirect_uri` is deliberately NOT included in the URL — AL's
    // docs example shows only `client_id` + `response_type` (and
    // optional `state`), and AL uses the redirect URL configured on
    // the developer-settings side for the app. Including
    // `redirect_uri` triggered an `unsupported_grant_type` error
    // post-approval against AL's `league/oauth2-server` backend
    // (live-probed 2026-04-24); dropping it lines up the URL with
    // exactly what AL's docs example shows and what every other
    // AL integration in the wild sends.
    let url = format!(
        "https://anilist.co/api/v2/oauth/authorize?client_id={}&response_type=token&state={}",
        ANILIST_CLIENT_ID,
        urlencoding::encode(&csrf_state),
    );
    Redirect::temporary(&url)
}

// ── MAL start ────────────────────────────────────────────────────────

pub async fn mal_start(State(state): State<AppState>) -> Redirect {
    // Fresh PKCE verifier + CSRF state nonce per /start call.
    // Overwrites any prior pending MAL attempt (decision matched in
    // services::oauth_state — second stash wins, first is discarded).
    let verifier = generate_pkce_verifier();
    let csrf_state = generate_state_nonce();
    oauth_state::stash(
        &state.oauth_state,
        PROVIDER_MAL,
        verifier.clone(),
        csrf_state.clone(),
    );

    // MAL's authorize URL: response_type=code, code_challenge = the
    // verifier itself (plain method), code_challenge_method explicitly
    // set to `plain` because MAL rejects the request when S256 is
    // specified (live-probed 2026-04-22). `state` is the CSRF nonce.
    let url = format!(
        "https://myanimelist.net/v1/oauth2/authorize?response_type=code&client_id={}&code_challenge={}&code_challenge_method=plain&redirect_uri={}&state={}",
        MAL_CLIENT_ID,
        urlencoding::encode(&verifier),
        urlencoding::encode(MAL_REDIRECT_URI),
        urlencoding::encode(&csrf_state),
    );
    Redirect::temporary(&url)
}

// ── AniList submit ───────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TokenSubmitForm {
    /// Raw access token the user pasted from the AL broker page.
    /// Validated by round-tripping through AL's `Viewer` GraphQL
    /// query — an invalid token gets rejected before we persist.
    pub access_token: String,
    /// CSRF state nonce echoed back by the provider, surfaced on
    /// the broker page alongside the token, pasted by the user.
    /// Validated against the value stashed at `/start`.
    pub state: String,
}

#[derive(Serialize)]
pub struct LinkResponse {
    pub provider: String,
    pub username: String,
}

pub async fn anilist_submit(
    State(state): State<AppState>,
    Json(form): Json<TokenSubmitForm>,
) -> Result<Json<LinkResponse>, (StatusCode, String)> {
    let token = form.access_token.trim().to_string();
    let pasted_state = form.state.trim().to_string();
    if token.is_empty() || pasted_state.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Paste both the token and the state from the AniList callback page.".into(),
        ));
    }

    // CSRF check — pasted state must match the nonce we stashed at
    // /start. `take` is single-use and TTL-bounded, so a stale
    // attempt can't be replayed and a missing slot means the user
    // didn't go through /start (or it expired). Constant-time
    // comparison via `subtle` to avoid a per-character timing leak;
    // not strictly necessary for an internal admin-only endpoint
    // but trivially cheap defense in depth.
    let attempt = oauth_state::take(&state.oauth_state, PROVIDER_ANILIST).ok_or((
        StatusCode::BAD_REQUEST,
        "No pending AniList authorization — start the link flow again.".into(),
    ))?;
    if !constant_time_eq(pasted_state.as_bytes(), attempt.state.as_bytes()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "AniList state nonce mismatch — start the link flow again.".into(),
        ));
    }

    // Round-trip through Viewer to validate + populate username /
    // score_format. A 400/401 from AL means the user pasted a bad
    // token; surface that as the same status code.
    let viewer = fetch_anilist_viewer(&token).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("AniList rejected the token: {e}"),
        )
    })?;

    let id = external_accounts::link(
        &state.db,
        LinkRequest {
            provider: PROVIDER_ANILIST.to_string(),
            provider_user_id: viewer.id.to_string(),
            username: viewer.name.clone(),
            access_token: token,
            refresh_token: String::new(),
            access_token_expires_at: None,
            score_format: viewer.score_format.clone(),
        },
    )
    .await
    .map_err(|e| (StatusCode::CONFLICT, e))?;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!("Linked AniList account '{}'", viewer.name),
        &format!("external_account_id={id}"),
    )
    .await;

    Ok(Json(LinkResponse {
        provider: PROVIDER_ANILIST.into(),
        username: viewer.name,
    }))
}

// ── MAL submit ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CodeSubmitForm {
    /// Authorization code the user pasted from the MAL broker page.
    pub code: String,
    /// CSRF state nonce echoed back by MAL, surfaced on the broker
    /// page alongside the code. Validated against the value stashed
    /// at `/start`.
    pub state: String,
}

pub async fn mal_submit(
    State(state): State<AppState>,
    Json(form): Json<CodeSubmitForm>,
) -> Result<Json<LinkResponse>, (StatusCode, String)> {
    let code = form.code.trim().to_string();
    let pasted_state = form.state.trim().to_string();
    if code.is_empty() || pasted_state.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Paste both the code and the state from the MyAnimeList callback page.".into(),
        ));
    }

    let attempt = oauth_state::take(&state.oauth_state, PROVIDER_MAL).ok_or((
        StatusCode::BAD_REQUEST,
        "No pending MyAnimeList authorization — start the link flow again.".into(),
    ))?;
    if !constant_time_eq(pasted_state.as_bytes(), attempt.state.as_bytes()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "MyAnimeList state nonce mismatch — start the link flow again.".into(),
        ));
    }
    let verifier = attempt.verifier;

    let tokens = exchange_mal_code(&code, &verifier).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("MyAnimeList rejected the code: {e}"),
        )
    })?;

    let me = fetch_mal_me(&tokens.access_token).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("MAL /v2/users/@me failed: {e}"),
        )
    })?;

    // MAL access tokens expire in 30 days. `expires_in` is seconds-
    // until-expiry; add to current time for an absolute timestamp.
    let expires_at = current_unix_ts() + tokens.expires_in;

    let id = external_accounts::link(
        &state.db,
        LinkRequest {
            provider: PROVIDER_MAL.to_string(),
            // Numeric MAL id (not username) — stable across renames.
            // `username` lives separately for display.
            provider_user_id: me.id.to_string(),
            username: me.name.clone(),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            access_token_expires_at: Some(expires_at),
            score_format: "POINT_10".to_string(),
        },
    )
    .await
    .map_err(|e| (StatusCode::CONFLICT, e))?;

    logger::info(
        &state.db,
        LogCategory::ExternalSync,
        &format!("Linked MyAnimeList account '{}'", me.name),
        &format!("external_account_id={id}"),
    )
    .await;

    Ok(Json(LinkResponse {
        provider: PROVIDER_MAL.into(),
        username: me.name,
    }))
}

// ── Shared unlink ────────────────────────────────────────────────────

/// Update per-list import preferences on the currently-linked
/// account. Settings → External Accounts auto-saves checkbox changes
/// via this endpoint so the sync task's next tick picks them up
/// without a full settings-form submit.
#[derive(Deserialize)]
pub struct PreferencesForm {
    pub import_watching: bool,
    pub import_planning: bool,
    pub import_paused: bool,
    pub import_dropped: bool,
    pub import_completed: bool,
    pub skip_already_watched: bool,
}

pub async fn update_preferences(
    State(state): State<AppState>,
    Json(form): Json<PreferencesForm>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let account = external_accounts::get_current(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or((
            StatusCode::BAD_REQUEST,
            "No external account is linked.".into(),
        ))?;
    external_accounts::update_preferences(
        &state.db,
        account.id,
        external_accounts::ImportPreferences {
            import_watching: form.import_watching,
            import_planning: form.import_planning,
            import_paused: form.import_paused,
            import_dropped: form.import_dropped,
            import_completed: form.import_completed,
            skip_already_watched: form.skip_already_watched,
        },
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn unlink(State(state): State<AppState>) -> impl IntoResponse {
    let current = external_accounts::get_current(&state.db).await;
    match current {
        Ok(Some(account)) => {
            if let Err(e) = external_accounts::unlink(&state.db, account.id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"ok": false, "error": e})),
                );
            }
            logger::info(
                &state.db,
                LogCategory::ExternalSync,
                &format!(
                    "Unlinked {} account '{}'",
                    account.provider, account.username
                ),
                &format!("external_account_id={}", account.id),
            )
            .await;
            (
                StatusCode::OK,
                Json(serde_json::json!({"ok": true, "provider": account.provider})),
            )
        }
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "already": "unlinked"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": e})),
        ),
    }
}

// ── Manual "Sync now" trigger ────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct SyncNowForm {
    /// Opaque client-generated id used to scope the ProgressRegistry
    /// job. The frontend mints one (timestamp + random suffix) when
    /// it clicks Sync now and polls `/api/progress/<id>` for live
    /// status. Missing/empty/over-length values are treated as
    /// "fire and forget" — the sync still runs but no progress
    /// events are emitted.
    #[serde(default)]
    pub progress_id: Option<String>,
}

/// Trigger a one-off watch-list sync against the linked account.
/// Returns 202 immediately after spawning the work; the actual sync
/// runs as a background task and emits progress events into the
/// registry. The frontend polls the progress endpoint to render the
/// sticky-toast status.
///
/// Returns 400 when no account is linked. The supervised background
/// task continues to run on its own cadence regardless — this is an
/// out-of-band "do it right now" trigger, not a replacement for the
/// scheduled tick.
///
/// Both error and success paths emit JSON `{ ok, error?, progress_id? }`
/// — the frontend toast (`static/js/settings.js::syncWatchListNow`)
/// finalizes on `ok: false`, so a plain-text error body would parse
/// to `{}` and leave the toast spinning indefinitely.
pub async fn sync_now(
    State(state): State<AppState>,
    Json(form): Json<SyncNowForm>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let account = external_accounts::get_current(&state.db)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": e})),
            )
        })?
        .ok_or((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "No external account is linked.",
            })),
        ))?;

    let progress_id = progress::sanitize_progress_id(form.progress_id.as_deref());
    let handle = if let Some(id) = progress_id.clone() {
        Some(state.progress.register(id).await)
    } else {
        None
    };

    if let Some(h) = &handle {
        h.emit(
            "start",
            "info",
            format!("Starting watch-list sync for {}", account.username),
            None,
            false,
        )
        .await;
    }

    // Spawn so the HTTP response returns immediately — the sync can
    // take minutes for a large list and we don't want the browser
    // sitting on an open POST while the user pokes around the rest of
    // the UI. The progress poll endpoint is how the toast gets live
    // status during the run.
    let spawn_state = state.clone();
    let spawn_handle = handle.clone();
    tokio::spawn(async move {
        // try-lock variant so a click while a supervised tick is in
        // flight produces an immediate "already running" toast
        // instead of a silent multi-minute wait that the user can't
        // tell apart from a hung backend. Supervised ticks use the
        // await-lock variant (external_sync::tick_once).
        let outcome = external_sync::tick_once_or_busy(&spawn_state).await;
        if let Some(h) = spawn_handle {
            match outcome {
                Ok(summary) => {
                    h.emit("done", "success", "Sync complete", Some(summary), true)
                        .await;
                }
                Err(err) => {
                    h.emit("done", "error", "Sync failed", Some(err), true)
                        .await;
                }
            }
        }
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "progress_id": progress_id,
    })))
}

// ── Provider call helpers ────────────────────────────────────────────

#[derive(Deserialize)]
struct AniListViewer {
    id: i64,
    name: String,
    #[serde(default)]
    score_format: String,
}

async fn fetch_anilist_viewer(token: &str) -> Result<AniListViewer, String> {
    let query = r#"{"query":"query { Viewer { id name mediaListOptions { scoreFormat } } }"}"#;
    let client = http_client();
    let resp = anilist::anilist_post(client)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(query.to_string())
        .send()
        .await
        .map_err(|e| format!("AniList HTTP error: {e}"))?;

    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("AniList response read failed: {e}"))?;
    // Participate in the same rate-limit cooldown the in-module AL
    // callers use. Without this, a 429 / 5xx during link doesn't flip
    // the process-wide cooldown, so a metadata sync or library-page
    // render firing right after the link attempt charges in unaware
    // and earns its own 429.
    anilist::note_external_anilist_response(status, &headers);
    if !status.is_success() {
        return Err(format!("AniList returned {status}: {}", excerpt(&body)));
    }

    // The GraphQL shape is `{"data": {"Viewer": { id, name, mediaListOptions: { scoreFormat } }}}`.
    // Flatten via a custom Deserialize pass — defining the whole tree
    // as serde structs would be overkill for three fields.
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("AniList Viewer parse failed: {e}"))?;
    let viewer_node = v
        .get("data")
        .and_then(|d| d.get("Viewer"))
        .ok_or_else(|| format!("AniList Viewer missing in response: {body}"))?;
    let id = viewer_node
        .get("id")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| format!("AniList Viewer.id missing: {body}"))?;
    let name = viewer_node
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("AniList Viewer.name missing: {body}"))?
        .to_string();
    let score_format = viewer_node
        .get("mediaListOptions")
        .and_then(|m| m.get("scoreFormat"))
        .and_then(|s| s.as_str())
        .unwrap_or("POINT_10")
        .to_string();
    Ok(AniListViewer {
        id,
        name,
        score_format,
    })
}

#[derive(Deserialize)]
struct MalTokenResponse {
    access_token: String,
    refresh_token: String,
    /// Seconds-until-expiry per the OAuth spec; MAL returns 2592000
    /// (30 days) for access tokens on issue.
    expires_in: i64,
}

async fn exchange_mal_code(code: &str, verifier: &str) -> Result<MalTokenResponse, String> {
    let client = http_client();
    let resp = client
        .post("https://myanimelist.net/v1/oauth2/token")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
        .form(&[
            ("client_id", MAL_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", MAL_REDIRECT_URI),
        ])
        .send()
        .await
        .map_err(|e| format!("MAL token HTTP error: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("MAL token response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "MAL token endpoint returned {status}: {}",
            excerpt(&body)
        ));
    }

    serde_json::from_str::<MalTokenResponse>(&body).map_err(|e| {
        format!(
            "MAL token response parse failed: {e} (body: {})",
            excerpt(&body)
        )
    })
}

#[derive(Deserialize)]
struct MalUserInfo {
    /// MAL's numeric account ID. Stable across username changes —
    /// users can rename on MAL, so storing the username as our
    /// `provider_user_id` would break re-link detection after a
    /// rename. Pulled via `?fields=id` on the same `@me` request
    /// (no extra round-trip).
    id: i64,
    name: String,
}

async fn fetch_mal_me(token: &str) -> Result<MalUserInfo, String> {
    let client = http_client();
    let resp = client
        .get("https://api.myanimelist.net/v2/users/@me?fields=id,name")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::USER_AGENT, "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("MAL @me HTTP error: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("MAL @me response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("MAL @me returned {status}: {}", excerpt(&body)));
    }

    serde_json::from_str::<MalUserInfo>(&body)
        .map_err(|e| format!("MAL @me parse failed: {e} (body: {})", excerpt(&body)))
}

// ── Small helpers ────────────────────────────────────────────────────

/// Build a 43-char base64url PKCE verifier from 32 random bytes.
/// Matches the MAL "plain" challenge format — we send the verifier
/// itself to MAL as the challenge, so it has to be URL-safe.
fn generate_pkce_verifier() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    // `URL_SAFE_NO_PAD` encoding of 32 bytes is 43 chars — defensive
    // truncate just in case the engine ever returns more.
    encoded.chars().take(PKCE_VERIFIER_LEN).collect()
}

/// Random URL-safe state nonce for the OAuth `state` parameter.
/// 32 bytes of entropy → 43 base64url chars; same shape as the
/// PKCE verifier so the two helpers can share intuitions, but
/// purposefully a separate function so a future change to one
/// doesn't silently affect the other.
fn generate_state_nonce() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Constant-time byte comparison. Mirrors the `subtle`-based check
/// the arr-compat shims use for API key comparison — keeps the
/// state-validation path free of per-byte short-circuit timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Shared reqwest client for OAuth provider calls. Matches the
/// `RSS_HTTP_CLIENT` convention in `services::rss::feed` — building
/// a fresh client per call costs DNS + TLS handshake on first use
/// and forfeits keep-alive pooling. the token-refresh path will
/// add a third caller of the same shape.
static OAUTH_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client build")
});

fn http_client() -> &'static reqwest::Client {
    &OAUTH_HTTP_CLIENT
}

fn current_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncate a provider response body for an error string. Both AL and
/// MAL can return large bodies on edge failure modes (Cloudflare HTML
/// when AL is melting, MAL's own error pages); without this cap, a
/// `Sync now` toast or the link-flow error renders multi-KB inline.
/// **Char-aware** rather than byte-aware: a multi-byte UTF-8 sequence
/// crossing the 240-byte boundary would panic a byte-slice. Mirrors
/// the same-named helpers in `services::mal` and
/// `services::anilist::rate_limit`.
fn excerpt(s: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut iter = s.chars();
    let prefix: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_is_43_chars_url_safe() {
        let v = generate_pkce_verifier();
        assert_eq!(v.len(), PKCE_VERIFIER_LEN, "verifier must be 43 chars");
        // URL-safe alphabet: A-Z, a-z, 0-9, `-`, `_`. No `+` / `/` / `=`.
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier must be URL-safe: {v}"
        );
    }

    #[test]
    fn successive_verifiers_are_distinct() {
        // Not a crypto assertion — just a smoke check that the RNG
        // is wired up. A fixed or monotonic verifier would silently
        // break PKCE's whole point.
        let a = generate_pkce_verifier();
        let b = generate_pkce_verifier();
        assert_ne!(a, b);
    }

    // ── generate_state_nonce ─────────────────────────────────────────

    #[test]
    fn state_nonce_is_url_safe_and_distinct_per_call() {
        // Same RNG-smoke check as the verifier counterpart. The state
        // nonce gates the OAuth callback; predictability here would
        // break the CSRF guard outright.
        let a = generate_state_nonce();
        let b = generate_state_nonce();
        assert_ne!(a, b);
        for nonce in [&a, &b] {
            assert!(
                nonce
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "state nonce must be URL-safe: {nonce}"
            );
            assert!(!nonce.is_empty());
        }
    }

    // ── constant_time_eq ─────────────────────────────────────────────

    #[test]
    fn constant_time_eq_matches_equal_inputs() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"\x00\x01\xff", b"\x00\x01\xff"));
    }

    #[test]
    fn constant_time_eq_rejects_different_inputs() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        // First-byte difference must not short-circuit early — `subtle`
        // is what guarantees this; we can only smoke-test correctness
        // here, not timing.
        assert!(!constant_time_eq(b"X", b"Y"));
    }

    #[test]
    fn constant_time_eq_rejects_length_mismatch() {
        // Different-length inputs return false without a panic. Length
        // is a public attribute (the caller's input lengths come from
        // user-supplied strings), so this fast-path is safe.
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
    }

    // ── excerpt ──────────────────────────────────────────────────────

    #[test]
    fn excerpt_passes_through_short_strings_unchanged() {
        assert_eq!(excerpt(""), "");
        assert_eq!(excerpt("hello"), "hello");
    }

    #[test]
    fn excerpt_caps_long_strings_with_ellipsis() {
        // 240 chars is the cap; anything longer gets a single '…' suffix.
        let long: String = "X".repeat(500);
        let got = excerpt(&long);
        // 240 X's + '…'.
        assert!(got.starts_with(&"X".repeat(240)), "wrong prefix: {got}");
        assert!(got.ends_with('…'));
        assert_eq!(got.chars().count(), 241);
    }

    #[test]
    fn excerpt_is_char_aware_does_not_panic_on_utf8_boundary() {
        // The 240-char boundary intersects a 4-byte emoji — a byte-
        // slice excerpt would panic at `&s[..240]`. The char-aware
        // implementation must not.
        let mut s = String::new();
        s.push_str(&"a".repeat(239));
        s.push('🎌'); // 4-byte UTF-8 char at exact char boundary 240.
        s.push_str("trailing");
        let got = excerpt(&s);
        assert!(got.ends_with('…'));
        assert_eq!(got.chars().count(), 241);
    }

    #[test]
    fn excerpt_at_exact_cap_does_not_append_ellipsis() {
        // Boundary: a 240-char input fits exactly — no '…' suffix.
        let exact: String = "Y".repeat(240);
        let got = excerpt(&exact);
        assert_eq!(got, exact);
        assert!(!got.ends_with('…'));
    }

    #[tokio::test]
    async fn anilist_start_redirects_to_authorize_url() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        let redirect = anilist_start(State(app_state.clone())).await;
        let resp = redirect.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::TEMPORARY_REDIRECT);
        let location = resp
            .headers()
            .get("location")
            .expect("Location header")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.starts_with("https://anilist.co/api/v2/oauth/authorize"),
            "wrong host: {location}"
        );
        assert!(
            location.contains(&format!("client_id={}", ANILIST_CLIENT_ID)),
            "client_id missing: {location}"
        );
        assert!(
            location.contains("response_type=token"),
            "implicit grant response_type missing: {location}"
        );
        assert!(
            location.contains("&state="),
            "state nonce must be included in authorize URL: {location}"
        );
        // Regression for the 2026-04-24 fix: `redirect_uri` must NOT
        // be in the URL. AL's docs example doesn't include it (the
        // app's developer-settings redirect is used instead), and
        // including it triggered an `unsupported_grant_type` error
        // post-approval. Keep the assertion negative so a future
        // refactor that re-adds `redirect_uri` fails this test.
        assert!(
            !location.contains("redirect_uri"),
            "redirect_uri must be omitted — AL's docs don't include it and including it broke the flow: {location}"
        );

        // The state nonce must be stashed for /submit to validate against.
        let stashed = oauth_state::take(&app_state.oauth_state, PROVIDER_ANILIST);
        assert!(stashed.is_some(), "anilist /start must stash a state nonce");
    }

    #[tokio::test]
    async fn mal_start_redirects_with_plain_pkce_challenge() {
        // Critical detail per decision #1 research: MAL requires
        // `code_challenge_method=plain`, NOT S256 (live-probed
        // 2026-04-22). A silent switch to S256 would break the
        // MAL flow entirely — token exchange would fail with
        // "invalid_grant." Pin the value.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let state = crate::test_support::build_test_app_state(db, None);

        let redirect = mal_start(State(state.clone())).await;
        let resp = redirect.into_response();
        let location = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.starts_with("https://myanimelist.net/v1/oauth2/authorize"),
            "wrong host: {location}"
        );
        assert!(location.contains(&format!("client_id={MAL_CLIENT_ID}")));
        assert!(location.contains("response_type=code"));
        assert!(location.contains("code_challenge_method=plain"));
        assert!(location.contains("code_challenge="));
        assert!(
            location.contains("&state="),
            "state nonce must be included in authorize URL: {location}"
        );

        // Both the verifier and the state must now be stashed —
        // /submit needs both to validate + exchange.
        let attempt = oauth_state::take(&state.oauth_state, PROVIDER_MAL);
        let attempt = attempt.expect("start must stash verifier+state for the subsequent submit");
        assert!(!attempt.verifier.is_empty(), "verifier must be populated");
        assert!(!attempt.state.is_empty(), "state must be populated");
    }

    #[tokio::test]
    async fn anilist_submit_rejects_state_mismatch() {
        // Pasting a state nonce that doesn't match what was stashed
        // at /start must fail with 400 — that's the CSRF guard. The
        // pasted token isn't even sent to AniList; we reject before
        // any external call.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        // Simulate a /start by stashing a known state nonce.
        oauth_state::stash(
            &app_state.oauth_state,
            PROVIDER_ANILIST,
            String::new(),
            "stashed-state-aaa".into(),
        );

        let result = anilist_submit(
            State(app_state.clone()),
            Json(TokenSubmitForm {
                access_token: "any-token".into(),
                state: "wrong-state-bbb".into(),
            }),
        )
        .await;

        let (status, msg) = match result {
            Err(e) => e,
            Ok(_) => panic!("submit must reject mismatched state nonce"),
        };
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            msg.to_lowercase().contains("state nonce mismatch"),
            "error must call out the CSRF check, got: {msg}"
        );

        // Even on a mismatch, the stashed attempt is consumed (single-
        // use) — a second submit returns "no pending authorization."
        let result_2 = anilist_submit(
            State(app_state.clone()),
            Json(TokenSubmitForm {
                access_token: "any-token".into(),
                state: "stashed-state-aaa".into(),
            }),
        )
        .await;
        let (status_2, msg_2) = match result_2 {
            Err(e) => e,
            Ok(_) => panic!("second submit must surface 'no pending'"),
        };
        assert_eq!(status_2, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            msg_2.to_lowercase().contains("no pending"),
            "second attempt must surface 'no pending authorization', got: {msg_2}"
        );
    }

    // ── current_unix_ts ──────────────────────────────────────────────

    #[test]
    fn current_unix_ts_returns_a_recent_timestamp() {
        // Smoke-check: returns "now in seconds since epoch" — at least
        // some plausible-looking value (not 0, not in the past). Pin
        // a sane lower bound so a clock regression past the epoch
        // (e.g. accidental Duration::ZERO fallback) surfaces here.
        let t = current_unix_ts();
        // 2024-01-01 ≈ 1704067200 — anything before that is broken.
        assert!(t > 1_700_000_000, "ts looks broken: {t}");
    }

    // ── anilist_submit / mal_submit reject paths ─────────────────────

    #[tokio::test]
    async fn anilist_submit_rejects_empty_token_or_state() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        for (token, state) in [("", "abc"), ("abc", ""), ("   ", "abc"), ("abc", "   ")] {
            let result = anilist_submit(
                State(app_state.clone()),
                Json(TokenSubmitForm {
                    access_token: token.into(),
                    state: state.into(),
                }),
            )
            .await;
            match result {
                Err((status, _)) => assert_eq!(status, axum::http::StatusCode::BAD_REQUEST),
                Ok(_) => panic!("must reject empty token={token:?} state={state:?}"),
            }
        }
    }

    #[tokio::test]
    async fn anilist_submit_rejects_when_no_pending_authorization() {
        // No /start was called, so nothing is stashed. The /submit
        // path must reject with a clean 400 + "no pending" message.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        let result = anilist_submit(
            State(app_state),
            Json(TokenSubmitForm {
                access_token: "tok".into(),
                state: "any".into(),
            }),
        )
        .await;
        let (status, msg) = match result {
            Err(e) => e,
            Ok(_) => panic!("must reject"),
        };
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(msg.to_lowercase().contains("no pending"));
    }

    #[tokio::test]
    async fn mal_submit_rejects_empty_inputs() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        let result = mal_submit(
            State(app_state),
            Json(CodeSubmitForm {
                code: "".into(),
                state: "abc".into(),
            }),
        )
        .await;
        match result {
            Err((status, _)) => assert_eq!(status, axum::http::StatusCode::BAD_REQUEST),
            Ok(_) => panic!("must reject empty code"),
        }
    }

    #[tokio::test]
    async fn mal_submit_rejects_state_mismatch_before_token_exchange() {
        // PKCE verifier + state stashed by /start; pasted state nonce
        // doesn't match. Reject with 400 — never call MAL's token
        // endpoint. Without this guard a cross-window CSRF could
        // exchange a victim's auth code on the attacker's behalf.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        oauth_state::stash(
            &app_state.oauth_state,
            PROVIDER_MAL,
            "verifier-x".into(),
            "stashed-mal".into(),
        );

        let result = mal_submit(
            State(app_state),
            Json(CodeSubmitForm {
                code: "any-code".into(),
                state: "tampered".into(),
            }),
        )
        .await;
        let (status, msg) = match result {
            Err(e) => e,
            Ok(_) => panic!("must reject mismatched state"),
        };
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(msg.to_lowercase().contains("state nonce mismatch"));
    }

    // ── unlink ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn unlink_drops_linked_row_and_emits_provider_in_response() {
        // Happy path: with a linked account, unlink removes it and
        // returns ok:true plus the provider name so the frontend can
        // render "Unlinked AniList" in the toast.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db.clone(), None);

        external_accounts::link(
            &db,
            LinkRequest {
                provider: PROVIDER_ANILIST.to_string(),
                provider_user_id: "1".to_string(),
                username: "u".to_string(),
                access_token: "tok".to_string(),
                refresh_token: String::new(),
                access_token_expires_at: None,
                score_format: "POINT_10".to_string(),
            },
        )
        .await
        .expect("link should succeed");

        let resp = unlink(State(app_state.clone())).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["provider"], "anilist");

        // Row gone — a follow-up get_current returns None.
        let after = external_accounts::get_current(&db).await.unwrap();
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn update_preferences_persists_to_db_for_linked_account() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db.clone(), None);
        external_accounts::link(
            &db,
            LinkRequest {
                provider: PROVIDER_ANILIST.to_string(),
                provider_user_id: "1".to_string(),
                username: "u".to_string(),
                access_token: "tok".to_string(),
                refresh_token: String::new(),
                access_token_expires_at: None,
                score_format: "POINT_10".to_string(),
            },
        )
        .await
        .unwrap();

        let resp = update_preferences(
            State(app_state),
            Json(PreferencesForm {
                import_watching: true,
                import_planning: false,
                import_paused: true,
                import_dropped: false,
                import_completed: true,
                skip_already_watched: true,
            }),
        )
        .await
        .expect("ok");
        assert_eq!(resp.0["ok"], true);

        let acct = external_accounts::get_current(&db)
            .await
            .unwrap()
            .expect("account");
        assert!(acct.import_watching);
        assert!(!acct.import_planning);
        assert!(acct.import_paused);
        assert!(!acct.import_dropped);
        assert!(acct.import_completed);
        assert!(acct.skip_already_watched);
    }

    #[tokio::test]
    async fn unlink_returns_already_unlinked_when_no_account() {
        // Idempotent endpoint: a click when nothing is linked must not
        // 500. The toast renders the success state without trying to
        // surface a stale username.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);
        let resp = unlink(State(app_state)).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["already"], "unlinked");
    }

    // ── update_preferences ──────────────────────────────────────────

    #[tokio::test]
    async fn update_preferences_returns_400_when_no_account_is_linked() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        let result = update_preferences(
            State(app_state),
            Json(PreferencesForm {
                import_watching: true,
                import_planning: true,
                import_paused: false,
                import_dropped: false,
                import_completed: false,
                skip_already_watched: false,
            }),
        )
        .await;
        let (status, _) = match result {
            Err(e) => e,
            Ok(_) => panic!("must reject without a linked account"),
        };
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sync_now_returns_json_body_when_no_account_linked() {
        // Regression for the PR #94 r2 finding: this path used to
        // return `(StatusCode, String)` which Axum serves as plain
        // text. The frontend toast parses the body as JSON to decide
        // whether to finalize as error; an unparseable body left the
        // toast spinning indefinitely. The handler now emits JSON so
        // both transport-layer and application-layer errors finalize
        // the toast cleanly.
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let app_state = crate::test_support::build_test_app_state(db, None);

        // No `external_accounts` row → handler hits the `.ok_or(...)`
        // branch and returns the JSON-bodied 400.
        let result = sync_now(
            State(app_state),
            Json(SyncNowForm {
                progress_id: Some("p_test_1".into()),
            }),
        )
        .await;
        let (status, body) = match result {
            Err(e) => e,
            Ok(_) => panic!("sync_now must reject when no account is linked"),
        };
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

        let body_value = body.0.clone();
        assert_eq!(
            body_value.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "JSON body MUST carry ok:false so the toast finalizes; \
             previous plain-text body left the spinner stuck"
        );
        let err_text = body_value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            err_text.contains("no external account"),
            "error string should call out the no-link state, got: {err_text}"
        );
    }
}
