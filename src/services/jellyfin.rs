//! The media server connection (#315). Jellyfin is the first-class
//! target; Emby and Plex are served by the same client, which knows
//! three things per kind: how to authenticate, where the server info
//! lives, and how to ask for a library scan. The module, the
//! `JellyfinClient` type, `AppState.jellyfin` and the `jellyfin_*`
//! config columns keep their names (as `jikan` did when its default
//! moved to Tenrai); `config.media_server_kind` says which server the
//! URL and key belong to.

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Which server `config.jellyfin_url` points at. Stored as
/// `config.media_server_kind`; anything unrecognized is Jellyfin, the
/// only kind before #315.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaServerKind {
    #[default]
    Jellyfin,
    Emby,
    Plex,
}

impl MediaServerKind {
    pub const ALL: [MediaServerKind; 3] = [Self::Jellyfin, Self::Emby, Self::Plex];

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "emby" => Self::Emby,
            "plex" => Self::Plex,
            _ => Self::Jellyfin,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jellyfin => "jellyfin",
            Self::Emby => "emby",
            Self::Plex => "plex",
        }
    }

    /// The server's name as the user reads it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Jellyfin => "Jellyfin",
            Self::Emby => "Emby",
            Self::Plex => "Plex",
        }
    }
}

#[derive(Clone)]
pub struct JellyfinClient {
    kind: MediaServerKind,
    base_url: String,
    api_key: String,
    http: Client,
}

/// Plex wraps every answer in a `MediaContainer`.
#[derive(Debug, Deserialize)]
struct PlexEnvelope {
    #[serde(default, rename = "MediaContainer")]
    container: PlexServer,
}

#[derive(Debug, Default, Deserialize)]
struct PlexServer {
    #[serde(default, rename = "friendlyName")]
    friendly_name: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "machineIdentifier")]
    machine_identifier: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemInfo {
    #[serde(default, rename = "ServerName")]
    pub server_name: String,
    #[serde(default, rename = "Version")]
    pub version: String,
    #[serde(default, rename = "Id")]
    pub id: String,
}

impl JellyfinClient {
    pub fn new(kind: MediaServerKind, url: &str, api_key: &str) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            // Emby's and Plex's keys ride in their own headers, which
            // reqwest forwards on a redirect to any host (it strips only
            // `Authorization` and cookies). Follow a redirect on the same
            // host (http to https, a trailing slash) and nothing else.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let same_host = attempt
                    .previous()
                    .first()
                    .is_some_and(|first| first.host_str() == attempt.url().host_str());
                if same_host && attempt.previous().len() < 5 {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .expect("Failed to build HTTP client");

        let mut base_url = normalize_base_url(url);
        // Emby's routes live under `/emby`, and its own docs write the
        // server address with that suffix. Take either form.
        if kind == MediaServerKind::Emby
            && base_url.len() >= 5
            && base_url.is_char_boundary(base_url.len() - 5)
            && base_url[base_url.len() - 5..].eq_ignore_ascii_case("/emby")
        {
            base_url.truncate(base_url.len() - 5);
        }

        Self {
            kind,
            base_url,
            api_key: api_key.trim().to_string(),
            http,
        }
    }

    /// The client for the saved connection, or `None` while the URL or
    /// the key is missing.
    pub fn from_config(cfg: &crate::models::config::Config) -> Option<Self> {
        if cfg.jellyfin_url.is_empty() || cfg.jellyfin_api_key.is_empty() {
            return None;
        }
        Some(Self::new(
            MediaServerKind::parse(&cfg.media_server_kind),
            &cfg.jellyfin_url,
            &cfg.jellyfin_api_key,
        ))
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.api_key.is_empty()
    }

    pub fn kind(&self) -> MediaServerKind {
        self.kind
    }

    /// The server's name for log lines and messages.
    pub fn label(&self) -> &'static str {
        self.kind.label()
    }

    pub fn web_base_url(&self) -> &str {
        &self.base_url
    }

    /// The header that carries the key.
    ///
    /// Jellyfin: `Authorization: MediaBrowser Token="<key>"`, the scheme
    /// every Jellyfin 10.x accepts. The legacy `X-Emby-Token` header this
    /// used to send is off by default since 10.11
    /// (`EnableLegacyAuthorization`) and answers 401, so every library
    /// refresh after an import failed on current servers. Emby still
    /// takes `X-Emby-Token`; Plex takes `X-Plex-Token`.
    fn auth_header(&self) -> (reqwest::header::HeaderName, String) {
        use reqwest::header::{AUTHORIZATION, HeaderName};
        match self.kind {
            MediaServerKind::Jellyfin => (
                AUTHORIZATION,
                format!("MediaBrowser Token=\"{}\"", self.api_key.replace('"', "")),
            ),
            MediaServerKind::Emby => (
                HeaderName::from_static("x-emby-token"),
                self.api_key.clone(),
            ),
            MediaServerKind::Plex => (
                HeaderName::from_static("x-plex-token"),
                self.api_key.clone(),
            ),
        }
    }

    /// One request, answered with the response once the status is a
    /// success. Every failure names the server.
    async fn send(
        &self,
        method: reqwest::Method,
        endpoint: &str,
    ) -> Result<reqwest::Response, String> {
        let label = self.label();
        if !self.is_configured() {
            return Err(format!("{label} is not configured"));
        }

        let (header, value) = self.auth_header();
        let resp = self
            .http
            .request(method, format!("{}{}", self.base_url, endpoint))
            .header(header, value)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("{label} request failed: {e}"))?;

        let status = resp.status();
        if let Some(rejected) = rejected_key(self.kind, status) {
            return Err(rejected);
        }
        // Every route asked for here exists on its own kind of server,
        // so a 404 is another app at that address or the wrong type
        // picked (Plex answers Jellyfin's routes with an HTML page).
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(self.not_this_server());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "{label} request failed ({status}): {}",
                truncate(&body)
            ));
        }
        Ok(resp)
    }

    fn not_this_server(&self) -> String {
        format!(
            "That address answered, but not like {} does. Check the type and the URL.",
            self.label()
        )
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, endpoint: &str) -> Result<T, String> {
        self.send(reqwest::Method::GET, endpoint)
            .await?
            .json::<T>()
            .await
            // A 200 that isn't the server's own JSON, like a 404.
            .map_err(|_| self.not_this_server())
    }

    /// Asks for the server's own info, which needs the key on every
    /// kind. Jellyfin's `/System/Info/Public`, which this used to call,
    /// answers anyone, so a wrong key read as "Connected" while every
    /// real request failed. Plex's `/identity` is open the same way; its
    /// root is not.
    pub async fn test_connection(&self) -> Result<SystemInfo, String> {
        match self.kind {
            MediaServerKind::Jellyfin => self.get("/System/Info").await,
            MediaServerKind::Emby => self.get("/emby/System/Info").await,
            MediaServerKind::Plex => {
                let server = self.get::<PlexEnvelope>("/").await?.container;
                Ok(SystemInfo {
                    server_name: server.friendly_name,
                    version: server.version,
                    id: server.machine_identifier,
                })
            }
        }
    }

    /// Queues a scan of every library. Plex scans on a GET.
    pub async fn refresh_library(&self) -> Result<(), String> {
        use reqwest::Method;
        let (method, endpoint) = match self.kind {
            MediaServerKind::Jellyfin => (Method::POST, "/Library/Refresh"),
            MediaServerKind::Emby => (Method::POST, "/emby/Library/Refresh"),
            MediaServerKind::Plex => (Method::GET, "/library/sections/all/refresh"),
        };
        self.send(method, endpoint).await.map(|_| ())
    }
}

/// The error for a key the server turned away, so the message names
/// the fix instead of an empty 401 body.
fn rejected_key(kind: MediaServerKind, status: reqwest::StatusCode) -> Option<String> {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
    .then(|| match kind {
        MediaServerKind::Jellyfin => format!(
            "Jellyfin rejected the API key ({status}). Create one under Dashboard → API Keys and paste it in Settings → Connections."
        ),
        MediaServerKind::Emby => format!(
            "Emby rejected the API key ({status}). Create one in Emby's server settings under API Keys and paste it in Settings → Connections."
        ),
        MediaServerKind::Plex => format!(
            "Plex rejected the token ({status}). Paste the X-Plex-Token of the account that owns the server in Settings → Connections."
        ),
    })
}

fn truncate(s: &str) -> String {
    let trimmed = s.trim();
    // Cut at a character, not a byte: byte 180 inside a multi-byte
    // character (a Japanese path in a Jellyfin error) panicked
    // post-processing's library refresh.
    if let Some((cut, _)) = trimmed.char_indices().nth(180) {
        format!("{}...", &trimmed[..cut])
    } else if trimmed.is_empty() {
        "empty response".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }

    let lower = trimmed.to_ascii_lowercase();
    let is_local = is_local_address(&lower);

    if is_local {
        format!("http://{}", trimmed)
    } else {
        format!("https://{}", trimmed)
    }
}

/// True when `s` (lowercased, scheme-stripped) refers to an address
/// that warrants plain HTTP instead of HTTPS by default — loopback
/// names + the three RFC 1918 ranges + the IPv6 loopback.
///
/// The 172.16.0.0/12 range needs each second-octet enumerated rather
/// than a `starts_with("172.2")` check: that prefix catches 172.20
/// through 172.29 (which IS private) but ALSO 172.200 through 172.255
/// (which is PUBLIC). The pre-fix path mis-classified those public
/// IPs as local and emitted an `http://172.200.x.x:…` URL — the
/// Jellyfin API key would then ride a plain-text request to a public
/// host.
///
/// IPv6 loopback handling: only the bracketed form (`[::1]` or
/// `[::1]:port`) and the bare `::1` exact match count. An earlier
/// fix tried to also accept `::1:port` (bracketless host:port), but
/// that prefix mis-matches valid non-loopback addresses like
/// `::1:abcd:1234` (which expands to `0:0:0:0:0:1:abcd:1234`) —
/// the same shape as the original 172.x bug. RFC 3986 requires
/// brackets around an IPv6 host in a URL anyway, so any input that
/// would lose the bracketless prefix path was already malformed.
pub(crate) fn is_local_address(lower: &str) -> bool {
    if lower.starts_with("localhost") || lower.starts_with("127.") {
        return true;
    }
    // IPv6 loopback — bracketed form (`[::1]` / `[::1]:port`) or the
    // bare `::1` exact literal. See doc comment for why the bracketless
    // `::1:port` prefix was deliberately dropped.
    if lower.starts_with("[::1]") || lower == "::1" {
        return true;
    }
    if lower.starts_with("10.") || lower.starts_with("192.168.") {
        return true;
    }
    // 172.16.0.0/12 — second octet must be 16-31 inclusive.
    if let Some(rest) = lower.strip_prefix("172.")
        && let Some((second_octet_str, _tail)) = rest.split_once('.').or(Some((rest, "")))
        && let Ok(second_octet) = second_octet_str.parse::<u8>()
    {
        return (16..=31).contains(&second_octet);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Auth: the header Jellyfin 10.11+ accepts ────────────────────

    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A server that answers only the `Authorization: MediaBrowser`
    /// scheme with the right key, the way Jellyfin 12 does with legacy
    /// authorization off.
    async fn jellyfin_12(key: &str) -> MockServer {
        let server = MockServer::start().await;
        let good = format!("MediaBrowser Token=\"{key}\"");
        Mock::given(method("GET"))
            .and(path("/System/Info"))
            .and(header("Authorization", good.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ServerName": "jf", "Version": "12.1.0", "Id": "abc"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/Library/Refresh"))
            .and(header("Authorization", good.as_str()))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        // Like the real server, the public info answers anyone.
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ServerName": "jf", "Version": "12.1.0", "Id": "abc"
            })))
            .with_priority(1)
            .mount(&server)
            .await;
        // Anything else (the legacy header, a wrong key) is a 401.
        Mock::given(header_exists("X-Emby-Token"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn requests_use_the_mediabrowser_authorization_header() {
        let server = jellyfin_12("k123").await;
        let client = JellyfinClient::new(MediaServerKind::Jellyfin, &server.uri(), "k123");
        let info = client.test_connection().await.expect("connects");
        assert_eq!(info.version, "12.1.0");
        client
            .refresh_library()
            .await
            .expect("refresh is authorized");
    }

    #[tokio::test]
    async fn a_wrong_key_fails_the_test_with_a_clear_message() {
        // The test used to call /System/Info/Public, which needs no key,
        // so a made-up key reported "Connected".
        let server = jellyfin_12("k123").await;
        let client = JellyfinClient::new(MediaServerKind::Jellyfin, &server.uri(), "not-the-key");
        let err = client.test_connection().await.expect_err("rejected");
        assert!(err.contains("rejected the API key"), "{err}");
    }

    // ── Emby and Plex (#315) ────────────────────────────────────────

    /// An Emby server: its routes under `/emby`, the key in
    /// `X-Emby-Token`, and no idea what `Authorization: MediaBrowser` is.
    async fn emby(key: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/emby/System/Info"))
            .and(header("X-Emby-Token", key))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ServerName": "em", "Version": "4.9.1.80", "Id": "e1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/emby/Library/Refresh"))
            .and(header("X-Emby-Token", key))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&server)
            .await;
        server
    }

    /// A Plex server: the token in `X-Plex-Token`, a `MediaContainer`
    /// around the answer, and a scan that starts on a GET.
    async fn plex(token: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("X-Plex-Token", token))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "MediaContainer": {
                    "friendlyName": "px", "version": "1.42.2.10156", "machineIdentifier": "m1"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/library/sections/all/refresh"))
            .and(header("X-Plex-Token", token))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn emby_is_tested_and_refreshed_with_its_own_header_and_routes() {
        let server = emby("ek").await;
        let client = JellyfinClient::new(MediaServerKind::Emby, &server.uri(), "ek");
        let info = client.test_connection().await.expect("connects");
        assert_eq!(
            (info.server_name.as_str(), info.version.as_str()),
            ("em", "4.9.1.80")
        );
        client
            .refresh_library()
            .await
            .expect("refresh is authorized");
    }

    #[tokio::test]
    async fn plex_is_tested_and_refreshed_with_its_token() {
        let server = plex("pt").await;
        let client = JellyfinClient::new(MediaServerKind::Plex, &server.uri(), "pt");
        let info = client.test_connection().await.expect("connects");
        assert_eq!(
            (info.server_name.as_str(), info.version.as_str()),
            ("px", "1.42.2.10156")
        );
        client
            .refresh_library()
            .await
            .expect("refresh is authorized");
    }

    #[tokio::test]
    async fn a_rejected_key_names_the_server_it_was_sent_to() {
        let server = emby("ek").await;
        let err = JellyfinClient::new(MediaServerKind::Emby, &server.uri(), "wrong")
            .test_connection()
            .await
            .expect_err("rejected");
        assert!(err.starts_with("Emby rejected the API key"), "{err}");

        let server = plex("pt").await;
        let client = JellyfinClient::new(MediaServerKind::Plex, &server.uri(), "wrong");
        let err = client.test_connection().await.expect_err("rejected");
        assert!(err.starts_with("Plex rejected the token"), "{err}");
        let err = client.refresh_library().await.expect_err("rejected");
        assert!(err.starts_with("Plex rejected the token"), "{err}");
    }

    #[tokio::test]
    async fn an_emby_address_may_end_in_the_emby_suffix() {
        let server = emby("ek").await;
        for suffix in ["/emby", "/emby/", "/EMBY"] {
            let url = format!("{}{suffix}", server.uri());
            JellyfinClient::new(MediaServerKind::Emby, &url, "ek")
                .test_connection()
                .await
                .unwrap_or_else(|e| panic!("{suffix}: {e}"));
        }
        // Only Emby's own suffix, and only for Emby.
        let jf = JellyfinClient::new(MediaServerKind::Jellyfin, "http://h:8096/emby", "k");
        assert_eq!(jf.web_base_url(), "http://h:8096/emby");
        let em = JellyfinClient::new(MediaServerKind::Emby, "http://h:8096/media/emby", "k");
        assert_eq!(em.web_base_url(), "http://h:8096/media");
        let em = JellyfinClient::new(MediaServerKind::Emby, "http://h:8096/notemby", "k");
        assert_eq!(em.web_base_url(), "http://h:8096/notemby");
    }

    /// The token must not follow a redirect to another host: reqwest
    /// forwards `X-Emby-Token` / `X-Plex-Token` wherever it is sent.
    #[tokio::test]
    async fn the_key_is_not_sent_to_another_host_on_a_redirect() {
        let elsewhere = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "MediaContainer": {"friendlyName": "evil", "version": "1"}
            })))
            .expect(0)
            .mount(&elsewhere)
            .await;
        // wiremock listens on 127.0.0.1; `localhost` is another host name
        // for the same listener, which is all the policy compares.
        let other_host = elsewhere.uri().replace("127.0.0.1", "localhost");
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", format!("{other_host}/")),
            )
            .mount(&server)
            .await;
        for kind in [MediaServerKind::Plex, MediaServerKind::Emby] {
            let client = JellyfinClient::new(kind, &server.uri(), "secret");
            let err = client.test_connection().await.expect_err("not followed");
            assert!(err.contains("302"), "{err}");
            client.refresh_library().await.expect_err("not followed");
        }
    }

    /// A same-host redirect (a proxy adding a slash, http to https) is
    /// still followed.
    #[tokio::test]
    async fn a_redirect_on_the_same_host_is_followed() {
        let server = plex("pt").await;
        Mock::given(method("GET"))
            .and(path("/library/sections/all/refresh"))
            .respond_with(ResponseTemplate::new(301).insert_header("Location", "/"))
            .with_priority(1)
            .mount(&server)
            .await;
        JellyfinClient::new(MediaServerKind::Plex, &server.uri(), "pt")
            .refresh_library()
            .await
            .expect("followed to a route that answers 200");
    }

    /// The wrong type for the server reads as that, not as an HTML
    /// error page or a parse error. Checked against real servers: Plex
    /// answers Jellyfin's routes 404, Jellyfin answers Emby's 404, and
    /// both answer Plex's root with a web page.
    #[tokio::test]
    async fn the_wrong_type_for_a_server_says_so() {
        let plex_server = plex("pt").await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(404).set_body_string("<html><h1>404</h1></html>"))
            .with_priority(5)
            .mount(&plex_server)
            .await;
        let web_page = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<!DOCTYPE html><html></html>"),
            )
            .mount(&web_page)
            .await;
        for (kind, uri) in [
            (MediaServerKind::Jellyfin, plex_server.uri()),
            (MediaServerKind::Emby, plex_server.uri()),
            (MediaServerKind::Plex, web_page.uri()),
        ] {
            let err = JellyfinClient::new(kind, &uri, "pt")
                .test_connection()
                .await
                .expect_err("not that server");
            assert_eq!(
                err,
                format!(
                    "That address answered, but not like {} does. Check the type and the URL.",
                    kind.label()
                )
            );
        }
    }

    /// A key no header can carry is an error, never a panic.
    #[tokio::test]
    async fn a_key_that_is_not_a_header_value_is_an_error() {
        for kind in MediaServerKind::ALL {
            let client = JellyfinClient::new(kind, "http://127.0.0.1:1", "bad\nkey ключ");
            let err = client.test_connection().await.expect_err("refused");
            assert!(err.starts_with(kind.label()), "{err}");
        }
    }

    #[test]
    fn an_unknown_kind_is_jellyfin() {
        // Rows saved before #315 hold the column default.
        for (stored, kind) in [
            ("", MediaServerKind::Jellyfin),
            ("jellyfin", MediaServerKind::Jellyfin),
            ("kodi", MediaServerKind::Jellyfin),
            ("Emby", MediaServerKind::Emby),
            (" plex ", MediaServerKind::Plex),
        ] {
            assert_eq!(MediaServerKind::parse(stored), kind, "{stored:?}");
        }
        for kind in MediaServerKind::ALL {
            assert_eq!(MediaServerKind::parse(kind.as_str()), kind);
        }
    }

    #[test]
    fn truncate_cuts_at_a_character_not_a_byte() {
        // 179 ASCII bytes then a 3-byte character straddling byte 180.
        let body = format!("{}日本語のパス", "x".repeat(179));
        let out = truncate(&body);
        assert!(out.ends_with("..."), "{out}");
        assert!(out.chars().count() <= 183);
    }

    // ── truncate ─────────────────────────────────────────────────────

    #[test]
    fn truncate_handles_empty_input() {
        // Empty trimmed input gets the "empty response" placeholder so
        // log lines reading the truncated body have something useful to
        // surface.
        assert_eq!(truncate(""), "empty response");
        assert_eq!(truncate("   \n\t"), "empty response");
    }

    #[test]
    fn truncate_passes_through_short_input() {
        assert_eq!(truncate("hello"), "hello");
        assert_eq!(truncate("  hello  "), "hello"); // trims
    }

    #[test]
    fn truncate_caps_long_input_with_ellipsis() {
        let long = "X".repeat(500);
        let got = truncate(&long);
        // Cap is 180 chars + "..." suffix.
        assert!(got.starts_with(&"X".repeat(180)), "got {got:?}");
        assert!(got.ends_with("..."));
        assert_eq!(got.len(), 183);
    }

    // ── normalize_base_url: the load-bearing private-IP check ────────

    #[test]
    fn normalize_keeps_explicit_scheme_intact() {
        // If the user types a scheme, honor it — never override.
        assert_eq!(
            normalize_base_url("http://my-public.example.com:8096"),
            "http://my-public.example.com:8096"
        );
        assert_eq!(
            normalize_base_url("https://my.example.com:8096"),
            "https://my.example.com:8096"
        );
        // Trailing slash gets trimmed even with a scheme.
        assert_eq!(
            normalize_base_url("https://my.example.com/"),
            "https://my.example.com"
        );
    }

    #[test]
    fn normalize_empty_input_returns_empty() {
        assert!(normalize_base_url("").is_empty());
        assert!(normalize_base_url("   ").is_empty());
    }

    #[test]
    fn normalize_local_addresses_get_http() {
        // The four canonical local shapes — must default to plain HTTP
        // since the user is almost certainly running Jellyfin on the
        // same LAN with no certificate.
        for addr in [
            "localhost:8096",
            "127.0.0.1:8096",
            "10.0.5.42:8096",
            "192.168.1.10:8096",
        ] {
            assert!(
                normalize_base_url(addr).starts_with("http://"),
                "{addr} should default to http://, got {:?}",
                normalize_base_url(addr)
            );
        }
    }

    #[test]
    fn normalize_172_16_range_correctly_classified_as_private() {
        // RFC 1918 specifies 172.16.0.0/12 — second octet 16..=31
        // inclusive. Each boundary deserves a pin; the loop checks
        // every value in the range.
        for second in 16..=31 {
            let addr = format!("172.{}.0.5:8096", second);
            assert!(
                normalize_base_url(&addr).starts_with("http://"),
                "172.{second}.0.5 is RFC 1918 private; expected http://, got {:?}",
                normalize_base_url(&addr)
            );
        }
    }

    #[test]
    fn normalize_172_outside_private_range_classified_as_public() {
        // The pre-fix bug: `starts_with("172.2")` matched 172.200 through
        // 172.255 even though those are public. A user with that as
        // their Jellyfin host would get http://, leaking the API key.
        for second in [0, 15, 32, 100, 200, 255] {
            let addr = format!("172.{}.0.5:8096", second);
            assert!(
                normalize_base_url(&addr).starts_with("https://"),
                "172.{second}.0.5 is public; expected https://, got {:?}",
                normalize_base_url(&addr)
            );
        }
    }

    #[test]
    fn normalize_public_addresses_default_to_https() {
        // Conservative: anything we can't prove is local gets HTTPS.
        for addr in [
            "jellyfin.example.com:8096",
            "8.8.8.8:8096",
            "203.0.113.5:8096",
            "my.public.server",
        ] {
            assert!(
                normalize_base_url(addr).starts_with("https://"),
                "{addr} should default to https://, got {:?}",
                normalize_base_url(addr)
            );
        }
    }

    #[test]
    fn normalize_ipv6_loopback_classified_as_private() {
        // `[::1]` literal form is what `url::Url` round-trips. Some
        // users type bare `::1` in settings and the input still
        // resolves to localhost.
        for addr in ["[::1]:8096", "[::1]"] {
            assert!(
                normalize_base_url(addr).starts_with("http://"),
                "{addr} should be classified as IPv6 loopback (http://), got {:?}",
                normalize_base_url(addr)
            );
        }
    }

    #[test]
    fn normalize_ipv6_non_loopback_with_one_hextet_classified_as_public() {
        // PR #101 review: the earlier fix accepted `::1:port` as a
        // bracketless loopback shorthand, which over-matched valid
        // non-loopback IPv6 addresses whose second hextet starts with
        // `1`. `::1:abcd:1234` expands to `0:0:0:0:0:1:abcd:1234` —
        // public, not loopback. Pin both that the bracketless input
        // gets HTTPS now and that the bracketed loopback path still
        // works.
        for addr in ["::1:abcd:1234", "::1:9000:9000:9000"] {
            assert!(
                normalize_base_url(addr).starts_with("https://"),
                "{addr} is non-loopback IPv6; expected https://, got {:?}",
                normalize_base_url(addr)
            );
        }
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert!(normalize_base_url("localhost:8096/").ends_with(":8096"));
        assert!(!normalize_base_url("localhost:8096/").ends_with("/"));
    }
}
