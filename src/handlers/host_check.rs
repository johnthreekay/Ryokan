//! Opt-in DNS-rebinding defense, Transmission's `rpc-host-whitelist`.
//!
//! A page on another site can point its own hostname at Ryokan's LAN
//! address; the browser then treats Ryokan as that site, and the page's
//! script can use any route that needs no cookie, `/setup` before the
//! first account exists most of all. The `Origin` check can't see it:
//! the origin really is the page's hostname. What gives it away is the
//! `Host` header, which the browser fills with that hostname and script
//! cannot change.
//!
//! Off by default. With `RYOKAN_HOST_CHECK=1` (or any name in
//! `RYOKAN_ALLOWED_HOSTS`) a browser-facing request must name Ryokan by
//! an IP address, `localhost` / `*.localhost`, the machine's hostname,
//! or a name in `RYOKAN_ALLOWED_HOSTS`; anything else gets 421. An IP
//! address is always safe: rebinding needs a DNS name the attacker
//! controls. With `RYOKAN_TRUSTED_PROXY` on, every `X-Forwarded-Host`
//! entry must pass as well; script can set that header, so it can only
//! add a refusal, never get one past the check.
//!
//! The Sonarr / Radarr shims, the autobrr webhook and the iCal feed are
//! not covered (`main.rs` layers this on the browser routes only): each
//! request there carries an API key a rebinding page doesn't have, and
//! other containers call them by service name. Transmission likewise
//! skips its whitelist for authenticated requests.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// One `RYOKAN_ALLOWED_HOSTS` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    /// `ryokan.lan`
    Exact(String),
    /// `*.example.com`: any name ending in `.example.com`.
    Subdomains(String),
}

/// The check's configuration, read once at startup.
#[derive(Debug, Clone)]
pub struct HostCheck {
    allowed: Arc<Vec<Pattern>>,
    /// The machine's own hostname, lowercased.
    machine: Option<String>,
    trust_proxy: bool,
}

impl HostCheck {
    /// `None` (no check) when `RYOKAN_HOST_CHECK` is falsy, or when it
    /// is unset and `RYOKAN_ALLOWED_HOSTS` is blank. Logs what it
    /// allows.
    pub fn from_env() -> Option<Self> {
        let machine = nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok());
        let check = Self::from_values(
            std::env::var("RYOKAN_HOST_CHECK").ok().as_deref(),
            std::env::var("RYOKAN_ALLOWED_HOSTS").ok().as_deref(),
            machine,
            *crate::handlers::auth::TRUST_PROXY_HEADERS,
        )?;
        let mut names: Vec<String> = vec!["IP addresses".into(), "localhost".into()];
        names.extend(check.machine.clone());
        names.extend(check.allowed.iter().map(|p| match p {
            Pattern::Exact(name) => name.clone(),
            Pattern::Subdomains(suffix) => format!("*{suffix}"),
        }));
        tracing::info!(
            "Host check on: browser routes answer to {}",
            names.join(", ")
        );
        Some(check)
    }

    fn from_values(
        enabled: Option<&str>,
        allowed_hosts: Option<&str>,
        machine: Option<String>,
        trust_proxy: bool,
    ) -> Option<Self> {
        let allowed_hosts = allowed_hosts.unwrap_or("");
        let allowed = parse_allowed_hosts(allowed_hosts);
        // Fails closed: only an explicit "off" turns the check off. A
        // value it doesn't recognize (`enabled`) is someone asking for
        // it, and a list whose every entry was dropped is still a list.
        let on = match enabled.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if !v.is_empty() => match v.as_str() {
                "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                _ => {
                    tracing::warn!(
                        "RYOKAN_HOST_CHECK={v:?} isn't a yes or no value, so the host check is on. Set it to 1 or 0."
                    );
                    true
                }
            },
            _ => !allowed_hosts.trim().is_empty(),
        };
        on.then(|| Self {
            allowed: Arc::new(allowed),
            machine: machine
                .map(|m| normalize(&m))
                .filter(|m| !m.is_empty() && m != "localhost"),
            trust_proxy,
        })
    }

    /// Whether `host` (a Host header value, port allowed) names Ryokan.
    fn allows(&self, host: &str) -> bool {
        let Some((name, _)) = crate::handlers::auth::split_authority(host) else {
            return false;
        };
        let name = normalize(&name);
        let bare = name.trim_start_matches('[').trim_end_matches(']');
        if bare.parse::<std::net::IpAddr>().is_ok()
            || name == "localhost"
            || name.ends_with(".localhost")
            || self.machine.as_deref() == Some(name.as_str())
        {
            return true;
        }
        self.allowed.iter().any(|p| match p {
            Pattern::Exact(exact) => *exact == name,
            Pattern::Subdomains(suffix) => name.ends_with(suffix.as_str()),
        })
    }

    /// The first name in `req` that isn't allowed, if any. A request
    /// with no Host at all passes: a browser always sends one.
    fn refused(&self, req: &Request) -> Option<String> {
        let host = req
            .headers()
            .get(header::HOST)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .or_else(|| req.uri().authority().map(|a| a.to_string()));
        if let Some(host) = host
            && !self.allows(&host)
        {
            return Some(host);
        }
        if self.trust_proxy {
            for value in req.headers().get_all("x-forwarded-host") {
                let value = String::from_utf8_lossy(value.as_bytes());
                for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                    if !self.allows(entry) {
                        return Some(entry.to_string());
                    }
                }
            }
        }
        None
    }
}

/// Lowercased, without a trailing dot (`ryokan.lan.` is `ryokan.lan`).
fn normalize(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Comma- or space-separated names: `ryokan.lan`, `*.example.com`; a
/// port is ignored. Anything else (`*` alone, paths, schemes) is logged
/// and dropped: `*` would turn the check off, which leaving
/// `RYOKAN_HOST_CHECK` unset already does.
fn parse_allowed_hosts(raw: &str) -> Vec<Pattern> {
    let mut out = Vec::new();
    for entry in raw.split([',', ' ', '\t', '\n']).filter(|e| !e.is_empty()) {
        let (wild, rest) = match entry.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, entry),
        };
        let name = crate::handlers::auth::split_authority(rest)
            .map(|(name, _)| normalize(&name))
            .filter(|n| {
                !n.is_empty()
                    && n.split('.').all(|label| {
                        !label.is_empty()
                            && label
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })
            });
        match name {
            Some(name) if wild => out.push(Pattern::Subdomains(format!(".{name}"))),
            Some(name) => out.push(Pattern::Exact(name)),
            None => tracing::warn!(
                "RYOKAN_ALLOWED_HOSTS: ignoring {entry:?}; entries are host names like ryokan.lan or *.example.com"
            ),
        }
    }
    out
}

/// The middleware: 421 for a request that names Ryokan by a host the
/// check doesn't allow.
pub async fn apply(State(check): State<HostCheck>, req: Request, next: Next) -> Response {
    let Some(host) = check.refused(&req) else {
        return next.run(req).await;
    };
    let shown: String = host.chars().filter(|c| !c.is_control()).take(100).collect();
    if crate::services::logger::first_in_window(
        &format!("host-check:{shown}"),
        std::time::Duration::from_secs(60),
    ) {
        tracing::warn!(
            "Host check refused a request for {shown:?}; add the name to RYOKAN_ALLOWED_HOSTS if it is yours"
        );
    }
    (
        StatusCode::MISDIRECTED_REQUEST,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!(
            "Ryokan's host check doesn't recognize \"{shown}\". Open Ryokan by its IP address, or add this name to RYOKAN_ALLOWED_HOSTS. (A page on another site pointing its own name at Ryokan looks like this; see DNS rebinding.)"
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    fn check(list: &str, machine: Option<&str>, trust_proxy: bool) -> HostCheck {
        HostCheck::from_values(
            Some("1"),
            Some(list),
            machine.map(String::from),
            trust_proxy,
        )
        .expect("on")
    }

    #[test]
    fn off_unless_asked_for() {
        assert!(HostCheck::from_values(None, None, None, false).is_none());
        assert!(HostCheck::from_values(Some(""), Some(""), None, false).is_none());
        assert!(HostCheck::from_values(Some("0"), Some("ryokan.lan"), None, false).is_none());
        assert!(HostCheck::from_values(Some("on"), None, None, false).is_some());
        assert!(
            HostCheck::from_values(None, Some("ryokan.lan"), None, false).is_some(),
            "a list turns it on by itself"
        );
    }

    #[test]
    fn only_an_explicit_off_turns_it_off() {
        // Both used to leave the check off without a word.
        for value in ["enabled", "On", " 1 ", "y"] {
            assert!(
                HostCheck::from_values(Some(value), None, None, false).is_some(),
                "{value:?}"
            );
        }
        for value in ["0", "false", "NO", "off"] {
            assert!(
                HostCheck::from_values(Some(value), Some("ryokan.lan"), None, false).is_none(),
                "{value:?}"
            );
        }
        assert!(
            HostCheck::from_values(None, Some("http://ryokan.lan/"), None, false).is_some(),
            "a list whose entries were all dropped still asks for the check"
        );
    }

    #[test]
    fn ip_addresses_localhost_the_machine_and_listed_names_pass() {
        let c = check("ryokan.lan, *.example.com", Some("NAS"), false);
        for host in [
            "192.168.1.5:8978",
            "10.0.0.2",
            "[fe80::1]:8978",
            "[::1]",
            "localhost:8978",
            "ryokan.localhost",
            "nas:8978",
            "Ryokan.LAN",
            "ryokan.lan.",
            "media.example.com",
        ] {
            assert!(c.allows(host), "{host}");
        }
        for host in [
            "attacker.example",
            "example.com",
            "ryokan.lan.attacker.example",
            "notexample.com",
            "192.168.1.5.nip.io",
            "",
        ] {
            assert!(!c.allows(host), "{host}");
        }
    }

    #[test]
    fn entries_that_are_not_host_names_are_dropped() {
        let parsed = parse_allowed_hosts("* ryokan.lan:8978 http://x/ a..b *.ok.example");
        assert_eq!(
            parsed,
            vec![
                Pattern::Exact("ryokan.lan".into()),
                Pattern::Subdomains(".ok.example".into())
            ]
        );
    }

    async fn status(c: HostCheck, host: Option<&str>, forwarded: Option<&str>) -> StatusCode {
        let app = Router::new()
            .route("/setup", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(c, apply));
        let mut req = Request::builder().uri("/setup");
        if let Some(h) = host {
            req = req.header(header::HOST, h);
        }
        if let Some(f) = forwarded {
            req = req.header("x-forwarded-host", f);
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_rebound_name_is_refused_with_421() {
        let c = check("", None, false);
        assert_eq!(
            status(c.clone(), Some("attacker.example:8978"), None).await,
            StatusCode::MISDIRECTED_REQUEST
        );
        assert_eq!(
            status(c.clone(), Some("192.168.1.5:8978"), None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(c.clone(), None, None).await,
            StatusCode::OK,
            "no Host: not a browser"
        );
        assert_eq!(
            status(c, Some("192.168.1.5"), Some("attacker.example")).await,
            StatusCode::OK,
            "forwarded headers mean nothing without a trusted proxy"
        );
    }

    #[tokio::test]
    async fn behind_a_trusted_proxy_every_forwarded_host_must_pass() {
        let c = check("ryokan.example.com", None, true);
        assert_eq!(
            status(
                c.clone(),
                Some("127.0.0.1:8978"),
                Some("ryokan.example.com")
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            status(c.clone(), Some("127.0.0.1:8978"), Some("attacker.example")).await,
            StatusCode::MISDIRECTED_REQUEST
        );
        assert_eq!(
            status(
                c,
                Some("127.0.0.1"),
                Some("ryokan.example.com, attacker.example")
            )
            .await,
            StatusCode::MISDIRECTED_REQUEST,
            "an entry script added can only refuse"
        );
    }
}
