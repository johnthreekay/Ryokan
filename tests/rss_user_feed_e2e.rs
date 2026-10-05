//! Wiremock coverage for `services::rss::feed::fetch_user_feed`
//! (Multi-RSS, Option A). The Nyaa-direct path has no
//! integration test (it hits nyaa.si live in dev / RSS sync); the
//! user-feed path is the new generic-RSS surface and needs one.
//!
//! Same env-isolation pattern as the other `tests/*_e2e.rs` files,
//! though this one has nothing to coordinate — `fetch_user_feed`
//! takes the URL as an argument, no env var to race on.

use ryokan::services::rss::{RssSource, feed};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal Nyaa-flavored RSS shape (the `nyaa:*` namespace tags
/// are how the parser pulls torrent / magnet / info_hash). Non-
/// Nyaa user feeds will leave those empty and the parser falls
/// back to `link`/`title`-only — covered separately.
const NYAA_FLAVORED_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa">
<channel>
<item>
<title>[GroupX] Show - 01 [1080p]</title>
<link>https://feed.example/view/1</link>
<guid>guid-feed-1</guid>
<nyaa:downloadurl>https://feed.example/torrent/1.torrent</nyaa:downloadurl>
<nyaa:magneturi>magnet:?xt=urn:btih:abcdef0001abcdef0001abcdef0001abcdef0001</nyaa:magneturi>
<nyaa:infohash>ABCDEF0001ABCDEF0001ABCDEF0001ABCDEF0001</nyaa:infohash>
</item>
</channel>
</rss>"#;

#[tokio::test]
async fn fetch_user_feed_parses_items_and_stamps_source() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(NYAA_FLAVORED_FIXTURE)
                .insert_header("content-type", "application/rss+xml"),
        )
        .mount(&mock)
        .await;

    let url = format!("{}/feed", mock.uri());
    let source = RssSource::UserFeed {
        id: 42,
        name: "TestFeed".into(),
    };

    let items = feed::fetch_user_feed(&url, source.clone())
        .await
        .expect("fetch should succeed");

    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it.title, "[GroupX] Show - 01 [1080p]");
    assert_eq!(it.info_hash, "abcdef0001abcdef0001abcdef0001abcdef0001"); // lowercased per parser
    assert_eq!(
        it.magnet,
        "magnet:?xt=urn:btih:abcdef0001abcdef0001abcdef0001abcdef0001"
    );
    // Source attribution carried through.
    assert_eq!(it.source, source);
}

#[tokio::test]
async fn fetch_user_feed_returns_err_on_5xx() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/broken"))
        .respond_with(ResponseTemplate::new(503).set_body_string("rate limited"))
        .mount(&mock)
        .await;

    let url = format!("{}/broken", mock.uri());
    let result = feed::fetch_user_feed(
        &url,
        RssSource::UserFeed {
            id: 1,
            name: "Broken".into(),
        },
    )
    .await;
    let err = result.expect_err("5xx must surface as Err");
    assert!(err.contains("503"), "error must include status code: {err}");
}

#[tokio::test]
async fn fetch_user_feed_caps_body_at_10mb() {
    // PR 112 review #6 — user-supplied URLs are arbitrary; the
    // 30s timeout caps a hung connection but a hostile /
    // misconfigured source can return arbitrarily large XML.
    // Pin the cap so a regression that drops it surfaces here.
    // Body size is 11 MB to cross the 10 MB threshold.
    let mock = MockServer::start().await;
    let huge_body = "x".repeat(11 * 1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/huge"))
        .respond_with(ResponseTemplate::new(200).set_body_string(huge_body))
        .mount(&mock)
        .await;

    let url = format!("{}/huge", mock.uri());
    let result = feed::fetch_user_feed(
        &url,
        RssSource::UserFeed {
            id: 1,
            name: "Hostile".into(),
        },
    )
    .await;
    let err = result.expect_err("11 MB body must be rejected");
    assert!(
        err.contains("MB cap") || err.to_lowercase().contains("exceeded"),
        "expected size-cap error, got: {err}"
    );
}

#[tokio::test]
async fn fetch_user_feed_returns_empty_for_no_items() {
    // Empty feed (channel with no items) is a valid response shape
    // for an idle period — must NOT be an Err. Pin so a future tweak
    // that treats an empty parse as a parse failure doesn't kill
    // sync on every quiet feed.
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/empty"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel></channel></rss>"#),
        )
        .mount(&mock)
        .await;

    let url = format!("{}/empty", mock.uri());
    let items = feed::fetch_user_feed(
        &url,
        RssSource::UserFeed {
            id: 1,
            name: "Empty".into(),
        },
    )
    .await
    .expect("empty feed is not an error");
    assert!(items.is_empty());
}
