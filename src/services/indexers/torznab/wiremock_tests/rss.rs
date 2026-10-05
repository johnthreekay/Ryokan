//! Wiremock coverage for `services::indexers::fetch_indexer_rss`
//! (the per-indexer RSS polling path). The polling URL is the same
//! `?t=tvsearch&cat=5070` shape the
//! search path uses, just with empty `q` — so a single XML
//! parser handles both directions.
//!
//! What's pinned here:
//!   * Fetch issues `?t=tvsearch&cat=5070&apikey=…&q=` (empty q).
//!   * The categories the library needs reach the wire, narrowed by
//!     the caps and replaced by the row's override like a search's.
//!   * Each release converts to an `RssItem` with
//!     `RssSource::Indexer { id, name, kind }` attribution.
//!   * `Release::link` becomes the item's `torrent` URL.
//!   * `Release::info_hash` becomes the item's `info_hash`.
//!
//! sync_once integration (cooldown skip, dedup scoping, etc.)
//! lives in commit F per the plan's phasing.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture};
use crate::services::indexers::{TORZNAB_CAT_ANIME, fetch_indexer_rss};
use crate::services::rss::RssSource;

/// Single-item torznab response. Same shape as the search-path
/// fixture; the RSS path doesn't differ on the wire.
const RSS_BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
<item>
  <title>[GroupX] Synthetic.Show.S01E01.1080p.WEB</title>
  <guid>g-rss-1</guid>
  <link>https://server/dl/abc?apikey=KEY</link>
  <enclosure url="https://server/dl/abc?apikey=KEY" length="1500000000" type="application/x-bittorrent"/>
  <torznab:attr name="seeders" value="42"/>
  <torznab:attr name="leechers" value="1"/>
  <torznab:attr name="infohash" value="ABCDEF1234567890ABCDEF1234567890ABCDEF12"/>
  <torznab:attr name="category" value="5070"/>
</item>
</channel>
</rss>"#;

#[tokio::test]
async fn fetch_indexer_rss_polls_tvsearch_with_empty_q_and_anime_cat() {
    // Polling URL is `?t=tvsearch&cat=5070&apikey=…&q=` with
    // empty `q`. Pin so a future endpoint refactor can't silently
    // switch to `?t=search` (which is uncategorized and adds
    // noise the match step has to discard).
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("apikey", TEST_API_KEY))
        .and(query_param("cat", "5070"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let items = fetch_indexer_rss(&client, &[TORZNAB_CAT_ANIME])
        .await
        .expect("rss fetch must succeed");
    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it.title, "[GroupX] Synthetic.Show.S01E01.1080p.WEB");
    assert_eq!(it.guid, "g-rss-1");
    assert_eq!(it.info_hash, "abcdef1234567890abcdef1234567890abcdef12");
    // `torrent` mirrors the enclosure URL (Release::link). The
    // grab path picks `magnet` first, then `torrent`; here the
    // magnet field is empty so the torrent URL is what gets used.
    assert_eq!(it.torrent, "https://server/dl/abc?apikey=KEY");
    assert_eq!(it.resolution, "1080");
    assert!(!it.is_batch);
}

#[tokio::test]
async fn fetch_indexer_rss_stamps_indexer_source_attribution() {
    // Each item's `source` carries the indexer id + name + kind.
    // Drives:
    //   * grab-time download-client routing via the indexer's pin
    //     (PR commit F);
    //   * the protocol guard at grab time (kind == "torznab" →
    //     torrent client);
    //   * `RssSource::label()` for the `rss_seen.source` column.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
        .mount(&server)
        .await;

    let items = fetch_indexer_rss(&client, &[TORZNAB_CAT_ANIME])
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    match &items[0].source {
        RssSource::Indexer { id, name, kind } => {
            assert_eq!(*id, 7); // fixture's row id
            assert_eq!(name, "Wiremock");
            assert_eq!(kind, "torznab");
        }
        other => panic!("expected Indexer source, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_indexer_rss_returns_empty_for_zero_items_in_response() {
    // An empty channel is a valid quiet-period response — must
    // not Err. Pins the contract that the fan-out can rely on.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel></channel></rss>"#),
        )
        .mount(&server)
        .await;

    let items = fetch_indexer_rss(&client, &[TORZNAB_CAT_ANIME])
        .await
        .expect("empty channel is not an error");
    assert!(items.is_empty());
}

#[tokio::test]
async fn fetch_indexer_rss_propagates_indexer_5xx_error() {
    // The torznab client already maps non-2xx to Err with a
    // status-code-tagged message. RSS fan-out folds that
    // Err into `indexers.rss_last_poll_error` for UI display.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream broken"))
        .mount(&server)
        .await;

    let err = fetch_indexer_rss(&client, &[TORZNAB_CAT_ANIME])
        .await
        .expect_err("503 must surface as Err");
    assert!(
        err.contains("503") || err.to_lowercase().contains("http"),
        "error message must include the status code, got: {err}"
    );
}

/// A library with a film and an adult title polls for Movies and XXX
/// too, so a release an indexer files there is seen by RSS.
#[tokio::test]
async fn fetch_indexer_rss_asks_for_the_library_categories() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("cat", "2000,5070,6000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let items = fetch_indexer_rss(&client, &[2000, 5070, 6000])
        .await
        .expect("rss fetch must succeed");
    assert_eq!(items.len(), 1);
}

/// The poll goes through the same category rules as a search: an
/// indexer whose caps report anime and XXX but not Movies is not
/// asked for Movies, and a row with a Categories override is asked
/// for exactly that.
#[tokio::test]
async fn fetch_indexer_rss_categories_follow_the_caps_and_the_override() {
    use super::fixture::new_fixture_with_row;

    let caps = r#"{"categories":[{"id":5000,"name":"TV","subcategories":[{"id":5070,"name":"Anime","subcategories":[]}]},{"id":6000,"name":"XXX","subcategories":[]}],"search_modes":[],"max_limit":null,"default_limit":null}"#;
    let (server, client) = new_fixture_with_row(|row| row.caps_json = caps.to_string()).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "5070,6000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
        .expect(1)
        .mount(&server)
        .await;
    fetch_indexer_rss(&client, &[2000, 5070, 6000])
        .await
        .expect("caps-narrowed poll must succeed");

    let (server, client) = new_fixture_with_row(|row| row.categories = "5080".to_string()).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "5080"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
        .expect(1)
        .mount(&server)
        .await;
    fetch_indexer_rss(&client, &[2000, 5070, 6000])
        .await
        .expect("override poll must succeed");
}
