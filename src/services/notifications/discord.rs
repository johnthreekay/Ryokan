//! Discord notification provider (issue #120).
//!
//! Native Discord webhook embed shape. Covers the "I just want a
//! Discord ping when something grabs" 80% case without forcing
//! users to learn the generic webhook payload format from #119.
//!
//! ## Wire shape
//!
//! POST `<webhook_url>` with the Discord webhook JSON envelope:
//!
//! ```json
//! {
//!   "username": "Ryokan",
//!   "allowed_mentions": {"parse": []},
//!   "embeds": [{
//!     "title": "Grabbed: Mushoku Tensei E07",
//!     "color": 5763719,
//!     "thumbnail": {"url": "https://…cover.jpg"},
//!     "fields": [...],
//!     "footer": {"text": "Ryokan v…"},
//!     "timestamp": "2026-04-27T14:30:00Z"
//!   }]
//! }
//! ```
//!
//! ## `allowed_mentions: {"parse": []}` is load-bearing
//!
//! A malicious release title containing `@everyone` or `@here`
//! would ping every user on the server (or every online member)
//! when the embed renders, because Discord parses mentions in
//! embed field values by default. Setting `parse = []` disables
//! every mention category — `everyone`, `roles`, `users` — so
//! none renders as an actual notification regardless of what
//! the field bodies contain. **Always sent**, on every payload,
//! even Health events. Cheap insurance against a content-derived
//! abuse vector.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::time::Duration;

use super::{NotificationEvent, NotificationProvider, TestSendResult, truncate};
use crate::services::notifications::webhook::WEBHOOK_HTTP_CLIENT;

/// Discord embed limits per the API spec. Pinned in code so a
/// future copy-paste from the bigger
/// [Discord docs reference][https://discord.com/developers/docs/resources/channel#embed-object-embed-limits]
/// can be reviewed against these constants.
const EMBED_TITLE_MAX: usize = 256;
const EMBED_FIELD_VALUE_MAX: usize = 1024;
const RESPONSE_BODY_LOG_CAP: usize = 256;

// Decimal-RGB color palette matching Discord's UI conventions.
//
// Pinned (vs. computed from RGB tuples) so the test asserts on
// the exact values Discord sees. A change here is a wire-shape
// change.
const COLOR_GRABBED_OR_IMPORTED: u32 = 5_763_719; // #57F287 success green
const COLOR_HEALTH: u32 = 5_793_266; // #5865F2 blurple — Discord's brand color
const COLOR_NEEDS_REVIEW: u32 = 16_705_372; // #FEE75C warning yellow
const COLOR_FAILURE: u32 = 15_548_997; // #ED4245 danger red

/// Persisted shape of `notification_providers.config_json` for
/// `kind = 'discord'`. Single field — the Discord webhook URL,
/// which is itself the secret (anyone with the token-bearing
/// path can post to the channel). The settings UI must mask
/// the field by default and never echo it back on save responses.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordConfig {
    pub webhook_url: String,
}

/// Save-time URL validator. Settings save handler invokes this
/// before persisting the row. Sticks to the
/// `https://discord.com/api/webhooks/<id>/<token>` shape because
/// other Discord webhook hosts (`canary.discord.com`,
/// `ptb.discord.com`) all funnel through the same prefix anyway.
/// Allows `discordapp.com` for backward-compat with the legacy
/// host that still resolves.
pub fn validate_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("Discord webhook URL is empty".into());
    }
    let parsed = reqwest::Url::parse(trimmed)
        .map_err(|e| format!("Discord webhook URL parse failed: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("Discord webhook URL must use https://".into());
    }
    let host = parsed.host_str().unwrap_or("");
    let host_ok = matches!(
        host,
        "discord.com" | "canary.discord.com" | "ptb.discord.com" | "discordapp.com"
    );
    if !host_ok {
        return Err(format!(
            "Discord webhook URL must point to discord.com (got {host:?})"
        ));
    }
    if !parsed.path().starts_with("/api/webhooks/") {
        return Err("Discord webhook URL path must start with /api/webhooks/".into());
    }
    Ok(())
}

/// One configured Discord destination. Holds the SqlitePool so the
/// per-event `send` path can resolve `series.cover_url` for the
/// embed thumbnail without threading state through the trait. (The
/// generic webhook provider is stateless because it doesn't need
/// per-series enrichment; Discord-specific enrichment lives here.)
pub struct DiscordProvider {
    id: i64,
    name: String,
    webhook_url: String,
    db: SqlitePool,
}

impl DiscordProvider {
    pub fn new(id: i64, name: String, webhook_url: String, db: SqlitePool) -> Self {
        Self {
            id,
            name,
            webhook_url,
            db,
        }
    }

    /// Construct from a raw `notification_providers` row's
    /// `config_json` blob. Caller has already filtered by
    /// `kind = 'discord'`.
    pub fn from_row(
        id: i64,
        name: String,
        config_json: &str,
        db: SqlitePool,
    ) -> Result<Self, String> {
        let config: DiscordConfig = serde_json::from_str(config_json)
            .map_err(|e| format!("invalid discord config_json: {e}"))?;
        validate_url(&config.webhook_url)?;
        Ok(Self::new(id, name, config.webhook_url, db))
    }
}

#[async_trait]
impl NotificationProvider for DiscordProvider {
    fn id(&self) -> i64 {
        self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "discord"
    }

    async fn send(&self, event: &NotificationEvent) -> Result<(), String> {
        let cover_url = resolve_cover_url(event, &self.db).await;
        let payload = build_payload(event, cover_url.as_deref());
        post_payload(&self.webhook_url, &payload).await.map(|_| ())
    }
}

/// Awaited single-provider send for the Settings UI's "Send test"
/// button. Returns the receiver's HTTP status + truncated body
/// inline. Bypasses the per-event matrix (caller always synthesizes
/// `Health` which is default-off).
pub async fn send_test(
    provider: &DiscordProvider,
    event: &NotificationEvent,
) -> Result<TestSendResult, String> {
    let cover_url = resolve_cover_url(event, &provider.db).await;
    let payload = build_payload(event, cover_url.as_deref());
    post_payload(&provider.webhook_url, &payload).await
}

/// Look up `series.cover_url` for events that carry `series_id`.
/// Returns `None` for the no-series-id event variants and on any
/// DB miss — embed thumbnails are nice-to-have, not load-bearing,
/// so swallow errors rather than failing the whole send.
async fn resolve_cover_url(event: &NotificationEvent, db: &SqlitePool) -> Option<String> {
    let series_id: Option<i64> = match event {
        NotificationEvent::Grabbed { series_id, .. }
        | NotificationEvent::Imported { series_id, .. }
        | NotificationEvent::ClassifierNeedsReview { series_id, .. } => Some(*series_id),
        NotificationEvent::ImportFailed { series_id, .. }
        | NotificationEvent::Misgrabbed { series_id, .. } => Some(*series_id),
        _ => None,
    };
    let id = series_id?;
    sqlx::query_scalar::<_, String>("SELECT cover_url FROM series WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

async fn post_payload(webhook_url: &str, payload: &Value) -> Result<TestSendResult, String> {
    let response = match WEBHOOK_HTTP_CLIENT
        .post(webhook_url)
        .json(payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return Err("Discord POST timed out".into());
        }
        Err(e) => return Err(format!("Discord POST failed: {e}")),
    };
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Discord webhooks: 5 req / 2s per webhook + global limit.
        // No auto-retry in v1.8 (see issue #120 "Rate limit handling")
        // — log + drop. The Retry-After value is captured in the
        // error string so a future batching/retry impl has it; the
        // user-facing path treats it as a transient log entry.
        let retry_after = parse_retry_after(&response);
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Discord 429 (retry-after={}s): {}",
            retry_after.as_secs(),
            truncate(&body, RESPONSE_BODY_LOG_CAP)
        ));
    }
    let status_u16 = status.as_u16();
    let body = response.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(TestSendResult {
            status: status_u16,
            body: truncate(&body, RESPONSE_BODY_LOG_CAP),
        })
    } else {
        Err(format!(
            "Discord {}: {}",
            status_u16,
            truncate(&body, RESPONSE_BODY_LOG_CAP)
        ))
    }
}

/// Read the `Retry-After` header. Value is seconds (sometimes
/// fractional in the documented Discord shape; the header is
/// usually integer but the JSON body's `retry_after` is float).
/// Falls back to `1` second on parse failure or non-finite input —
/// matches Discord's own minimum and surfaces the failure as a
/// non-zero log line.
///
/// `is_finite` filter is load-bearing: `Duration::from_secs_f64`
/// panics on `INFINITY` and `NaN`. A misbehaving receiver returning
/// `Retry-After: Infinity` (cheap to construct, hard to predict)
/// would otherwise abort the dispatch task — contained by the
/// outer `tokio::spawn` panic-isolation but noisy for no benefit.
fn parse_retry_after(resp: &reqwest::Response) -> Duration {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|secs| secs.is_finite())
        .map(|secs| Duration::from_secs_f64(secs.max(0.0)))
        .unwrap_or(Duration::from_secs(1))
}

/// Build the Discord webhook JSON envelope for a `NotificationEvent`.
/// Pure function over the event + optional thumbnail URL — easy to
/// test against without spinning up a wiremock server. The wire
/// receiver path tests use this then post; the unit tests check
/// the JSON shape directly.
pub fn build_payload(event: &NotificationEvent, cover_url: Option<&str>) -> Value {
    let title = title_for(event);
    let color = color_for(event);
    let fields = fields_for(event);
    let mut embed = json!({
        "title": truncate(&title, EMBED_TITLE_MAX),
        "color": color,
        "fields": fields,
        "footer": {"text": format!("Ryokan v{}", env!("CARGO_PKG_VERSION"))},
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    if let Some(url) = cover_url
        && !url.is_empty()
    {
        embed["thumbnail"] = json!({"url": url});
    }
    json!({
        "username": "Ryokan",
        // Always send. See module-level "load-bearing" note.
        "allowed_mentions": {"parse": []},
        "embeds": [embed],
    })
}

fn title_for(event: &NotificationEvent) -> String {
    // Anime is overwhelmingly absolute-numbered (one show = ep 1..N
    // across cours, no per-season reset), so the Sonarr-style
    // `S01E07` shape reads weird against the source material's own
    // marketing — `Mushoku Tensei S2 Part 2 ep 47` showing up as
    // `S01E47` is technically true but useless. Drop the `S01E`
    // prefix entirely; `E07` matches how anime is referred to in
    // every fansub release group, every tracker, every Nyaa search.
    match event {
        NotificationEvent::Grabbed {
            series_title,
            episode_number,
            ..
        } => format!("Grabbed: {series_title} E{:02}", episode_number),
        NotificationEvent::Imported {
            series_title,
            episode_number,
            ..
        } => format!("Imported: {series_title} E{:02}", episode_number),
        NotificationEvent::ImportFailed {
            series_title,
            episode_number,
            ..
        } => match episode_number {
            Some(n) => format!("Import failed: {series_title} E{:02}", n),
            None => format!("Import failed: {series_title}"),
        },
        NotificationEvent::Misgrabbed {
            series_title,
            action,
            ..
        } => {
            if action == "flagged" {
                format!("Misgrab flagged: {series_title}")
            } else {
                format!("Misgrab removed: {series_title}")
            }
        }
        NotificationEvent::ClassifierNeedsReview {
            series_title,
            episode_number,
            ..
        } => format!("Needs review: {series_title} E{:02}", episode_number),
        NotificationEvent::IndexerDown { indexer_name, .. } => {
            format!("Indexer down: {indexer_name}")
        }
        NotificationEvent::DownloadClientUnreachable { client_kind, .. } => {
            format!("Download client unreachable: {client_kind}")
        }
        NotificationEvent::ExternalSyncReLinkRequired { provider } => {
            format!("Re-link required: {provider}")
        }
        NotificationEvent::Health { message, .. } => message.clone(),
    }
}

fn color_for(event: &NotificationEvent) -> u32 {
    match event {
        NotificationEvent::Grabbed { .. } | NotificationEvent::Imported { .. } => {
            COLOR_GRABBED_OR_IMPORTED
        }
        NotificationEvent::ClassifierNeedsReview { .. } => COLOR_NEEDS_REVIEW,
        NotificationEvent::ImportFailed { .. }
        | NotificationEvent::Misgrabbed { .. }
        | NotificationEvent::IndexerDown { .. }
        | NotificationEvent::DownloadClientUnreachable { .. }
        | NotificationEvent::ExternalSyncReLinkRequired { .. } => COLOR_FAILURE,
        NotificationEvent::Health { .. } => COLOR_HEALTH,
    }
}

fn fields_for(event: &NotificationEvent) -> Vec<Value> {
    match event {
        NotificationEvent::Grabbed {
            release_title,
            indexer,
            score,
            client_kind,
            ..
        } => {
            let mut fs = vec![field("Release", &code_wrap(release_title), false)];
            if let Some(i) = indexer {
                fs.push(field("Indexer", &code_wrap(i), true));
            }
            if let Some(s) = score {
                fs.push(field("Score", &format!("{:+}", s), true));
            }
            if let Some(c) = client_kind {
                fs.push(field("Client", &code_wrap(c), true));
            }
            fs
        }
        NotificationEvent::Imported {
            source_path,
            dest_path,
            quality_tag,
            ..
        } => vec![
            field("Quality", &code_wrap(quality_tag), true),
            field("From", &code_wrap(source_path), false),
            field("To", &code_wrap(dest_path), false),
        ],
        NotificationEvent::ImportFailed {
            source_path,
            reason,
            ..
        } => vec![
            field("Reason", reason, false),
            field("Source", &code_wrap(source_path), false),
        ],
        NotificationEvent::Misgrabbed {
            release_title,
            files,
            action,
            ..
        } => vec![
            field("Release", &code_wrap(release_title), false),
            field(
                "Files",
                &code_wrap(&if files.is_empty() {
                    "(none listed)".to_string()
                } else {
                    files.join("\n")
                }),
                false,
            ),
            field("Action", action, true),
        ],
        NotificationEvent::ClassifierNeedsReview {
            confidence,
            verdict_summary,
            ..
        } => vec![
            field("Verdict", &code_wrap(verdict_summary), false),
            field("Confidence", &format!("{}%", confidence), true),
        ],
        NotificationEvent::IndexerDown { reason, .. } => vec![field("Reason", reason, false)],
        NotificationEvent::DownloadClientUnreachable { reason, .. } => {
            vec![field("Reason", reason, false)]
        }
        NotificationEvent::ExternalSyncReLinkRequired { provider } => {
            vec![field("Provider", &code_wrap(provider), true)]
        }
        NotificationEvent::Health { kind, .. } => vec![field("Kind", &code_wrap(kind), true)],
    }
}

fn field(name: &str, value: &str, inline: bool) -> Value {
    json!({
        "name": name,
        "value": truncate(value, EMBED_FIELD_VALUE_MAX),
        "inline": inline,
    })
}

/// Wrap user-controlled text in inline-code backticks so Discord's
/// markdown parser leaves it alone (`*emph*`, `_underscore_`,
/// `~strike~`, `|spoiler|`, `> quote`). Backticks inside the value
/// are escaped — Discord accepts `\`` inside inline-code spans.
fn code_wrap(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let escaped = s.replace('`', "\\`");
    format!("`{escaped}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grabbed() -> NotificationEvent {
        NotificationEvent::Grabbed {
            series_id: 1,
            series_title: "Mushoku Tensei".into(),
            episode_number: 7,
            release_title: "[Group] Mushoku Tensei - 07 [1080p].mkv".into(),
            indexer: Some("Nyaa".into()),
            score: Some(125),
            client_kind: Some("qbittorrent".into()),
        }
    }

    #[test]
    fn validate_url_accepts_real_discord_webhook_shape() {
        validate_url("https://discord.com/api/webhooks/12345/abc-token").unwrap();
        validate_url("https://canary.discord.com/api/webhooks/12345/abc-token").unwrap();
        validate_url("https://ptb.discord.com/api/webhooks/12345/abc-token").unwrap();
        // Legacy `discordapp.com` host still resolves; pinned for
        // back-compat with users on old setups.
        validate_url("https://discordapp.com/api/webhooks/12345/abc-token").unwrap();
    }

    #[test]
    fn validate_url_rejects_non_discord_host() {
        assert!(validate_url("https://example.com/api/webhooks/12345/token").is_err());
        // Common typo: discord.gg is the invite host, not the API host.
        assert!(validate_url("https://discord.gg/api/webhooks/12345/token").is_err());
    }

    #[test]
    fn validate_url_rejects_http_scheme() {
        // Webhook tokens go in plaintext over the wire — refuse to
        // ship them un-TLS'd.
        assert!(validate_url("http://discord.com/api/webhooks/12345/abc-token").is_err());
    }

    #[test]
    fn validate_url_rejects_non_webhooks_path() {
        assert!(validate_url("https://discord.com/api/users/@me").is_err());
    }

    #[test]
    fn build_payload_matches_documented_shape() {
        let p = build_payload(&grabbed(), None);
        assert_eq!(p["username"], "Ryokan");
        assert_eq!(p["allowed_mentions"]["parse"].as_array().unwrap().len(), 0);
        let embed = &p["embeds"][0];
        assert_eq!(embed["title"], "Grabbed: Mushoku Tensei E07");
        assert_eq!(embed["color"], COLOR_GRABBED_OR_IMPORTED);
        let fields = embed["fields"].as_array().unwrap();
        assert!(!fields.is_empty(), "fields must be populated");
        assert!(
            embed.get("thumbnail").is_none(),
            "no thumbnail when cover_url is None"
        );
        let footer = embed["footer"]["text"].as_str().unwrap();
        assert!(
            footer.starts_with("Ryokan v"),
            "footer must include Ryokan version, got {footer}"
        );
    }

    #[test]
    fn build_payload_includes_thumbnail_when_cover_url_provided() {
        let p = build_payload(&grabbed(), Some("https://s4.anilist.co/cover.jpg"));
        assert_eq!(
            p["embeds"][0]["thumbnail"]["url"],
            "https://s4.anilist.co/cover.jpg"
        );
    }

    #[test]
    fn build_payload_skips_thumbnail_for_empty_cover_url() {
        // A series row with cover_url = "" must not result in
        // `thumbnail.url = ""` (Discord 400s on the empty URL).
        let p = build_payload(&grabbed(), Some(""));
        assert!(p["embeds"][0].get("thumbnail").is_none());
    }

    #[test]
    fn allowed_mentions_parse_is_always_empty_array() {
        // The non-negotiable @everyone / @here defense. Pinned
        // because a regression that flipped `parse` from `[]` to
        // either omitted (Discord defaults to parsing everything)
        // or `["everyone"]` would let a malicious release title
        // ping the entire server through the embed render.
        for ev in [
            grabbed(),
            NotificationEvent::Health {
                kind: "test".into(),
                message: "hi".into(),
            },
            NotificationEvent::ImportFailed {
                series_id: 1,
                series_title: "X".into(),
                episode_number: None,
                source_path: "/a".into(),
                reason: "y".into(),
            },
        ] {
            let p = build_payload(&ev, None);
            assert_eq!(
                p["allowed_mentions"]["parse"].as_array().map(|a| a.len()),
                Some(0),
                "allowed_mentions.parse must be [] for {ev:?}"
            );
        }
    }

    #[test]
    fn release_title_with_at_everyone_does_not_change_allowed_mentions() {
        // Even with @everyone or <@&123> in the user-controlled
        // strings, the envelope's allowed_mentions stays empty.
        // Pinned per the issue spec's parametrized regression
        // requirement.
        for malicious in [
            "@everyone get this",
            "@here grab this",
            "<@&123456789> ping",
            "<@123456789> ping",
        ] {
            let mut ev = grabbed();
            if let NotificationEvent::Grabbed { release_title, .. } = &mut ev {
                *release_title = malicious.to_string();
            }
            let p = build_payload(&ev, None);
            assert_eq!(
                p["allowed_mentions"]["parse"].as_array().map(|a| a.len()),
                Some(0),
                "allowed_mentions.parse must remain [] for malicious title {malicious:?}"
            );
        }
    }

    #[test]
    fn markdown_chars_in_release_title_get_backtick_wrapped() {
        // `*release_with_underscores*` would render as italic
        // around the middle without wrapping. Pinned because the
        // backtick wrap is what makes the embed legible for
        // realistic release titles.
        let mut ev = grabbed();
        if let NotificationEvent::Grabbed { release_title, .. } = &mut ev {
            *release_title = "*release_with_underscores*".into();
        }
        let p = build_payload(&ev, None);
        let release_field = p["embeds"][0]["fields"][0]["value"].as_str().unwrap();
        assert!(release_field.starts_with('`') && release_field.ends_with('`'));
        assert!(release_field.contains("*release_with_underscores*"));
    }

    #[test]
    fn backticks_in_release_title_are_escaped_inside_wrapper() {
        // A title with a literal backtick would close the inline
        // code span early. `\`` is the escape Discord supports.
        let mut ev = grabbed();
        if let NotificationEvent::Grabbed { release_title, .. } = &mut ev {
            *release_title = "release with `code` chunk".into();
        }
        let p = build_payload(&ev, None);
        let v = p["embeds"][0]["fields"][0]["value"].as_str().unwrap();
        assert!(v.contains("\\`code\\`"));
    }

    #[test]
    fn long_release_title_truncates_to_field_value_max() {
        // 2000-char title -> field value capped at 1024. The
        // backtick wrapper adds 2 chars to the inner string, so
        // the truncation runs on the wrapped form.
        let mut ev = grabbed();
        if let NotificationEvent::Grabbed { release_title, .. } = &mut ev {
            *release_title = "x".repeat(2000);
        }
        let p = build_payload(&ev, None);
        let v = p["embeds"][0]["fields"][0]["value"].as_str().unwrap();
        assert!(
            v.chars().count() <= EMBED_FIELD_VALUE_MAX,
            "field value must be truncated to {EMBED_FIELD_VALUE_MAX} chars; got {}",
            v.chars().count()
        );
        assert!(v.ends_with('…'), "truncated value must end in ellipsis");
    }

    #[test]
    fn long_title_truncates_to_embed_title_max() {
        // 500-char series title -> embed title capped at 256.
        let mut ev = grabbed();
        if let NotificationEvent::Grabbed { series_title, .. } = &mut ev {
            *series_title = "x".repeat(500);
        }
        let p = build_payload(&ev, None);
        let title = p["embeds"][0]["title"].as_str().unwrap();
        assert!(
            title.chars().count() <= EMBED_TITLE_MAX,
            "title truncation to {EMBED_TITLE_MAX} expected; got {}",
            title.chars().count()
        );
    }

    #[test]
    fn color_palette_matches_event_taxonomy() {
        // Pinned to the specific decimal-RGB values from the issue
        // spec's color palette so a rename or a future "let me
        // experiment with the palette" refactor produces a loud
        // diff rather than a silent UI shift.
        assert_eq!(color_for(&grabbed()), 5_763_719);
        assert_eq!(
            color_for(&NotificationEvent::Health {
                kind: "test".into(),
                message: "x".into()
            }),
            5_793_266
        );
        assert_eq!(
            color_for(&NotificationEvent::ImportFailed {
                series_id: 1,
                series_title: "X".into(),
                episode_number: None,
                source_path: "/x".into(),
                reason: "y".into(),
            }),
            15_548_997
        );
        assert_eq!(
            color_for(&NotificationEvent::ClassifierNeedsReview {
                series_id: 1,
                series_title: "X".into(),
                episode_number: 1,
                confidence: 50,
                verdict_summary: "y".into(),
            }),
            16_705_372
        );
    }

    #[test]
    fn code_wrap_handles_empty_string() {
        // Empty user-controlled field shouldn't render as a lone
        // pair of backticks (Discord's parser draws them as
        // visible glyphs in that case).
        assert_eq!(code_wrap(""), "");
    }
}
