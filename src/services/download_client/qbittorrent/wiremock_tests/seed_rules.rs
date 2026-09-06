//! Issue #28 — `set_seed_rules` wire shape against
//! `/api/v2/torrents/setShareLimits`.
//!
//! The endpoint takes form fields:
//!   * `hashes` — comma-separated lowercase hex
//!   * `ratioLimit` — float (`-2` use global, `-1` no limit, value
//!     = override)
//!   * `seedingTimeLimit` — int minutes (same convention)
//!   * `inactiveSeedingTimeLimit` — int minutes (`-2` use global)
//!
//! Tests pin the `None` → `-2` (defer-to-global) translation and
//! confirm both fields ride through the same form post. Before #228
//! the impl sent `-1`, qBit's "no limit", which silently disabled the
//! global limits on every dimension an indexer left unset.

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::{DownloadClient, SeedRules};

const HASH: &str = "abc1234567";

#[tokio::test]
async fn seed_rules_with_ratio_only_passes_minus_two_for_time() {
    // `None` translates to `-2` (use the global limit), NOT to
    // omitted and NOT to `-1` (no limit). Pin both the path + the
    // form contents.
    let (server, client) = new_fixture().await;
    // Note: 2.0_f64.to_string() = "2" (no trailing decimal). qBit
    // accepts both "2" and "2.0" — the wire shape Ryokan emits is
    // the integer-style form when the float has no fractional part.
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/setShareLimits"))
        .and(body_string_contains(format!("hashes={}", HASH)))
        .and(body_string_contains("ratioLimit=2&"))
        .and(body_string_contains("seedingTimeLimit=-2"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let rules = SeedRules {
        ratio: Some(2.0),
        time_minutes: None,
    };
    client
        .set_seed_rules(HASH, rules)
        .await
        .expect("seed rules");
}

#[tokio::test]
async fn seed_rules_with_time_only_passes_minus_two_for_ratio() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/setShareLimits"))
        .and(body_string_contains("ratioLimit=-2"))
        .and(body_string_contains("seedingTimeLimit=120"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let rules = SeedRules {
        ratio: None,
        time_minutes: Some(120),
    };
    client
        .set_seed_rules(HASH, rules)
        .await
        .expect("seed rules");
}

#[tokio::test]
async fn seed_rules_with_both_fields_sends_both_to_qbit() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/setShareLimits"))
        // 1.5 keeps the decimal (it's not whole).
        .and(body_string_contains("ratioLimit=1.5"))
        .and(body_string_contains("seedingTimeLimit=60"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let rules = SeedRules {
        ratio: Some(1.5),
        time_minutes: Some(60),
    };
    client
        .set_seed_rules(HASH, rules)
        .await
        .expect("seed rules");
}

#[tokio::test]
async fn seed_rules_failure_status_surfaces_as_err() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/setShareLimits"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .mount(&server)
        .await;
    let result = client
        .set_seed_rules(
            HASH,
            SeedRules {
                ratio: Some(1.0),
                time_minutes: None,
            },
        )
        .await;
    let err = result.expect_err("must surface non-200 as Err");
    assert!(err.contains("403"), "error must mention status: {err}");
}
