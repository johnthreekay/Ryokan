use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct JellyfinClient {
    base_url: String,
    api_key: String,
    http: Client,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JellyfinItem {
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Name")]
    pub name: String,
    #[serde(default, rename = "Path")]
    pub path: String,
    #[serde(default, rename = "Type")]
    pub item_type: String,
    #[serde(default, rename = "ProductionYear")]
    pub production_year: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct JellyfinItemsResponse {
    #[serde(default, rename = "Items")]
    items: Vec<JellyfinItem>,
}

impl JellyfinClient {
    pub fn new(url: &str, api_key: &str) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(url),
            api_key: api_key.trim().to_string(),
            http,
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.api_key.is_empty()
    }

    pub fn web_base_url(&self) -> &str {
        &self.base_url
    }

    /// `Authorization: MediaBrowser Token="<key>"`, the scheme every
    /// Jellyfin 10.x accepts. The legacy `X-Emby-Token` header this used
    /// to send is off by default since 10.11 (`EnableLegacyAuthorization`)
    /// and answers 401, so every library refresh after an import failed
    /// on current servers.
    fn auth_header(&self) -> String {
        format!("MediaBrowser Token=\"{}\"", self.api_key.replace('"', ""))
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        query: &[(&str, String)],
    ) -> Result<T, String> {
        if !self.is_configured() {
            return Err("Jellyfin is not configured".to_string());
        }

        let url = format!("{}{}", self.base_url, endpoint);
        let mut req = self
            .http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header("Accept", "application/json");

        if !query.is_empty() {
            req = req.query(query);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("Jellyfin request failed: {}", e))?;
        let status = resp.status();
        if let Some(rejected) = rejected_key(status) {
            return Err(rejected);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Jellyfin request failed ({}): {}",
                status,
                truncate(&body)
            ));
        }

        resp.json::<T>()
            .await
            .map_err(|e| format!("Jellyfin response parse failed: {}", e))
    }

    async fn post_empty(&self, endpoint: &str) -> Result<(), String> {
        if !self.is_configured() {
            return Err("Jellyfin is not configured".to_string());
        }

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .post(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| format!("Jellyfin request failed: {}", e))?;

        let status = resp.status();
        if let Some(rejected) = rejected_key(status) {
            return Err(rejected);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Jellyfin request failed ({}): {}",
                status,
                truncate(&body)
            ));
        }

        Ok(())
    }

    /// `/System/Info` needs the key; `/System/Info/Public`, which this
    /// used to call, answers anyone, so a wrong key read as "Connected"
    /// while every real request failed.
    pub async fn test_connection(&self) -> Result<SystemInfo, String> {
        self.get("/System/Info", &[]).await
    }

    pub async fn refresh_library(&self) -> Result<(), String> {
        self.post_empty("/Library/Refresh").await
    }

    pub async fn find_items(&self, search_term: &str) -> Result<Vec<JellyfinItem>, String> {
        if search_term.trim().is_empty() {
            return Ok(Vec::new());
        }

        let resp: JellyfinItemsResponse = self
            .get(
                "/Items",
                &[
                    ("Recursive", "true".to_string()),
                    ("IncludeItemTypes", "Series,Season,Movie,BoxSet".to_string()),
                    ("SearchTerm", search_term.trim().to_string()),
                    ("Limit", "12".to_string()),
                    ("Fields", "Path".to_string()),
                ],
            )
            .await?;

        Ok(resp.items)
    }
}

/// The error for a key Jellyfin turned away, so the message names the
/// fix instead of an empty 401 body.
fn rejected_key(status: reqwest::StatusCode) -> Option<String> {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
    .then(|| {
        format!(
            "Jellyfin rejected the API key ({status}). Create one under Dashboard → API Keys and paste it in Settings → Connections."
        )
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
fn is_local_address(lower: &str) -> bool {
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
        let client = JellyfinClient::new(&server.uri(), "k123");
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
        let client = JellyfinClient::new(&server.uri(), "not-the-key");
        let err = client.test_connection().await.expect_err("rejected");
        assert!(err.contains("rejected the API key"), "{err}");
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
