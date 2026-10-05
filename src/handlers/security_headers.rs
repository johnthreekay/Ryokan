//! Browser hardening headers on every response.
//!
//! - `X-Content-Type-Options: nosniff`: browsers use the declared
//!   content type instead of guessing one.
//! - `Referrer-Policy: same-origin`: links out (Nyaa, AniList, GitHub)
//!   don't learn the instance's hostname or URLs like
//!   `/system/import?session=...`. Never `no-referrer`: under that
//!   policy browsers send `Origin: null` on POSTs, and the Origin-based
//!   CSRF check in `handlers::auth` would reject every form. With
//!   `same-origin` a same-origin POST keeps its real `Origin`.
//! - `Content-Security-Policy: object-src 'none'; base-uri 'self'`:
//!   no plugin embeds, no `<base>` rewriting of relative URLs. Scripts
//!   and styles are deliberately unrestricted: the templates still
//!   carry inline `<script>` blocks, `on...=` handlers and `hx-on`
//!   attributes (htmx runs those through the `Function` constructor),
//!   so a `script-src` policy needs those moved into JS files first.
//! - `frame-ancestors` only when `RYOKAN_FRAME_ANCESTORS` is set.
//!   Framing stays allowed by default because people embed *arr apps
//!   in dashboards (Organizr, Homarr); the `SameSite=Lax` session
//!   cookie already keeps a cross-site frame logged out.
//! - `Cache-Control: no-store`: nothing is written to a browser's disk
//!   cache or kept by a shared proxy. Secrets ride on ordinary
//!   responses: the API-key reveal JSON, Settings tabs that render API
//!   keys and client passwords into their forms, the calendar page's
//!   keyed iCal URL, backup archives. A default catches the next one
//!   too. Responses that are meant to be cached set their own value and
//!   keep it: `/static` (`main.rs`), `/media/art`, the iCal feed
//!   (`private`). Browsers generally skip the back/forward cache for
//!   `no-store` pages, so Back re-requests the page.
//!
//! Each header is set only when the handler didn't set its own.

use std::sync::LazyLock;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use regex_lite::Regex;

const BASE_POLICY: &str = "object-src 'none'; base-uri 'self'";

/// An origin `frame-ancestors` can name: scheme, host (an optional
/// leading `*.` wildcard), optional port. No paths, quotes, `;` or `,`,
/// so an entry can never close the directive and start another.
static RE_FRAME_ORIGIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://(\*\.)?[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*(:([0-9]{1,5}|\*))?$")
        .expect("frame origin regex compiles")
});

/// The headers this layer adds, built once at startup.
#[derive(Clone)]
pub struct SecurityHeaders {
    content_security_policy: HeaderValue,
}

impl SecurityHeaders {
    /// Reads `RYOKAN_FRAME_ANCESTORS` and logs what it made of it.
    pub fn from_env() -> Self {
        let raw = crate::services::paths::env_nonblank("RYOKAN_FRAME_ANCESTORS");
        let frame_ancestors = raw.as_deref().map(|raw| {
            let parsed = parse_frame_ancestors(raw);
            for entry in &parsed.rejected {
                tracing::warn!(
                    "RYOKAN_FRAME_ANCESTORS: ignoring '{entry}'; expected self, none, \
                     or an origin like https://dash.example.com"
                );
            }
            tracing::info!("Framing restricted: frame-ancestors {}", parsed.sources);
            parsed.sources
        });
        Self::new(frame_ancestors.as_deref())
    }

    fn new(frame_ancestors: Option<&str>) -> Self {
        let policy = content_security_policy(frame_ancestors);
        Self {
            content_security_policy: HeaderValue::from_str(&policy)
                .expect("CSP is built from validated ASCII sources"),
        }
    }
}

/// `axum::middleware::from_fn_with_state` handler.
pub async fn apply(State(headers): State<SecurityHeaders>, req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let out = response.headers_mut();
    out.entry(header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(HeaderValue::from_static("nosniff"));
    out.entry(header::REFERRER_POLICY)
        .or_insert(HeaderValue::from_static("same-origin"));
    out.entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(headers.content_security_policy);
    out.entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

fn content_security_policy(frame_ancestors: Option<&str>) -> String {
    match frame_ancestors {
        Some(sources) => format!("{BASE_POLICY}; frame-ancestors {sources}"),
        None => BASE_POLICY.to_string(),
    }
}

#[derive(Debug, PartialEq)]
struct FrameAncestors {
    /// The directive's source list, never empty.
    sources: String,
    rejected: Vec<String>,
}

/// Parse `RYOKAN_FRAME_ANCESTORS`: `none`, or any mix of `self` and
/// origins, separated by spaces or commas (`'self'` / `'none'` with CSP
/// quotes also work). Entries that are neither are rejected and logged.
/// When nothing valid is left the result is `'self'`: the variable asked
/// for a restriction, so a typo must not quietly allow every site.
fn parse_frame_ancestors(raw: &str) -> FrameAncestors {
    let mut sources: Vec<String> = Vec::new();
    let mut rejected = Vec::new();
    let mut none = false;
    for entry in raw.split([' ', ',', '\t', '\n']).filter(|e| !e.is_empty()) {
        let source = match entry.trim_matches('\'').to_ascii_lowercase().as_str() {
            "self" => "'self'".to_string(),
            "none" => {
                none = true;
                continue;
            }
            _ if RE_FRAME_ORIGIN.is_match(entry) => entry.to_ascii_lowercase(),
            _ => {
                rejected.push(entry.to_string());
                continue;
            }
        };
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    let sources = match (none, sources.is_empty()) {
        // `'none'` is only meaningful alone; next to other sources it
        // contradicts them, so it is the entry that gets dropped.
        (true, true) => "'none'".to_string(),
        (true, false) => {
            rejected.push("none".to_string());
            sources.join(" ")
        }
        (false, true) => "'self'".to_string(),
        (false, false) => sources.join(" "),
    };
    FrameAncestors { sources, rejected }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    fn parsed(raw: &str) -> (String, Vec<String>) {
        let p = parse_frame_ancestors(raw);
        (p.sources, p.rejected)
    }

    #[test]
    fn keywords_with_or_without_csp_quotes() {
        assert_eq!(parsed("self"), ("'self'".into(), vec![]));
        assert_eq!(parsed("'self'"), ("'self'".into(), vec![]));
        assert_eq!(parsed("NONE"), ("'none'".into(), vec![]));
        assert_eq!(parsed("'none'"), ("'none'".into(), vec![]));
    }

    #[test]
    fn origins_and_self_mix_with_spaces_or_commas() {
        assert_eq!(
            parsed(
                "self, https://Dash.example.com  http://192.168.1.10:7575,https://*.example.com self"
            ),
            (
                "'self' https://dash.example.com http://192.168.1.10:7575 https://*.example.com"
                    .into(),
                vec![]
            )
        );
    }

    #[test]
    fn entries_that_could_inject_or_widen_the_policy_are_rejected() {
        let (sources, rejected) = parsed(
            "https://ok.example.com; script-src * https: * dash.example.com https://x.com/path 'unsafe-inline'",
        );
        assert_eq!(sources, "'self'", "nothing valid survives, so fail closed");
        assert_eq!(
            rejected,
            vec![
                "https://ok.example.com;",
                "script-src",
                "*",
                "https:",
                "*",
                "dash.example.com",
                "https://x.com/path",
                "'unsafe-inline'",
            ]
        );
    }

    #[test]
    fn none_next_to_other_sources_is_dropped() {
        assert_eq!(
            parsed("none self"),
            ("'self'".into(), vec!["none".to_string()])
        );
    }

    #[test]
    fn frame_ancestors_joins_the_base_policy_only_when_set() {
        assert_eq!(content_security_policy(None), BASE_POLICY);
        assert_eq!(
            content_security_policy(Some("'self'")),
            "object-src 'none'; base-uri 'self'; frame-ancestors 'self'"
        );
    }

    async fn headers_for(app: Router) -> axum::http::HeaderMap {
        app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .headers()
            .clone()
    }

    #[tokio::test]
    async fn every_response_gets_the_default_headers() {
        let app = Router::new().route("/", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(SecurityHeaders::new(None), apply),
        );
        let headers = headers_for(app).await;
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], BASE_POLICY);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn a_cache_policy_the_handler_set_wins() {
        let app = Router::new()
            .route(
                "/",
                get(|| async { ([(header::CACHE_CONTROL, "public, max-age=3600")], "ok") }),
            )
            .layer(axum::middleware::from_fn_with_state(
                SecurityHeaders::new(None),
                apply,
            ));
        let headers = headers_for(app).await;
        assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=3600");
    }

    #[tokio::test]
    async fn a_policy_the_handler_set_wins() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        [(header::CONTENT_SECURITY_POLICY, "default-src 'none'")],
                        "ok",
                    )
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                SecurityHeaders::new(Some("'none'")),
                apply,
            ));
        let headers = headers_for(app).await;
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'"
        );
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }
}
