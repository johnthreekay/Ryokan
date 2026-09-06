//! Issue #28 asked for per-torrent seed rules on rTorrent; issue #228
//! found there is nothing to call. rTorrent's only per-item ratio
//! command is the read-only `d.ratio`; ratio handling is configured per
//! group in `.rtorrent.rc`. The earlier impl posted a `d.ratio.max.set`
//! that does not exist and every seed-ruled grab faulted. Now
//! `set_seed_rules` makes no request and returns an error naming the
//! ratio-group configuration, which `apply_indexer_seed_rules` logs.

use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::{DownloadClient, SeedRules};

const HASH_LC: &str = "aabbccddeeff00112233445566778899aabbccdd";

async fn assert_no_rpc_and_honest_error(rules: SeedRules) {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/RPC2"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let err = client
        .set_seed_rules(HASH_LC, rules)
        .await
        .expect_err("rTorrent has no per-torrent seed limits");
    assert!(
        err.contains("ratio group") && err.contains(HASH_LC),
        "error must say how rTorrent does ratios and name the item: {err}"
    );
}

#[tokio::test]
async fn ratio_rule_makes_no_rpc_call_and_says_why() {
    assert_no_rpc_and_honest_error(SeedRules {
        ratio: Some(1.5),
        time_minutes: None,
    })
    .await;
}

#[tokio::test]
async fn time_rule_makes_no_rpc_call_and_says_why() {
    assert_no_rpc_and_honest_error(SeedRules {
        ratio: None,
        time_minutes: Some(60),
    })
    .await;
}

#[tokio::test]
async fn both_rules_make_no_rpc_call_and_say_why() {
    assert_no_rpc_and_honest_error(SeedRules {
        ratio: Some(2.0),
        time_minutes: Some(120),
    })
    .await;
}
