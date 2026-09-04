//! `Indexer::search` wire shape: GET
//! `<base>?t=tvsearch&apikey=...&cat=5070&q=...`.

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::{TEST_API_KEY, new_fixture, new_fixture_newznab};
use crate::services::indexers::{Indexer, SearchQuery};

const SEARCH_BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
<item>
  <title>Synthetic.Show.S01E01</title>
  <guid>g1</guid>
  <enclosure url="https://server/dl/abc?apikey=KEY" length="1000000000" type="application/x-bittorrent"/>
  <torznab:attr name="seeders" value="20"/>
  <torznab:attr name="leechers" value="2"/>
  <torznab:attr name="infohash" value="ABCDEF1234567890"/>
  <torznab:attr name="category" value="5070"/>
</item>
</channel>
</rss>"#;

#[tokio::test]
async fn search_sends_tvsearch_with_anime_category_default() {
    // When the caller doesn't specify categories, the client
    // defaults to 5070 (anime) per protocol research. Pin both
    // the t=tvsearch function name and the cat=5070 default.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("apikey", TEST_API_KEY))
        .and(query_param("cat", "5070"))
        .and(query_param("q", "Test Show"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: Vec::new(), // default → 5070
        limit: None,
        offset: None,
    };
    let releases = client.search(&query).await.expect("search must succeed");
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].title, "Synthetic.Show.S01E01");
    assert_eq!(releases[0].seeders, 20);
    assert_eq!(releases[0].indexer_id, 7, "stamps caller's id");
    assert_eq!(
        releases[0].indexer_name, "Wiremock",
        "indexer_name must be stamped at parse time so it survives \
         dedup + into_search_result and reaches the interactive-search \
         UI's Indexer column. A regression here would silently render \
         the column blank for every torznab/newznab hit."
    );
}

#[tokio::test]
async fn newznab_search_stamps_indexer_name() {
    // Newznab indexers (Usenet) reuse the same wire format and
    // parser; their kind column just changes from "torznab" to
    // "newznab" so the protocol-mismatch guard at the indexer-pin
    // save path can route torznab → torrent client and newznab →
    // SAB. The "Indexer" column on interactive search must
    // attribute usenet hits identically to torrent hits — the
    // user can't tell which row of `download_clients` to pin a
    // grab to without it.
    let (server, client) = new_fixture_newznab().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("apikey", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: Vec::new(),
        limit: None,
        offset: None,
    };
    let releases = client
        .search(&query)
        .await
        .expect("newznab search must succeed");
    assert_eq!(releases.len(), 1);
    assert_eq!(
        releases[0].indexer_name, "WiremockUsenet",
        "newznab parses to the same Release shape as torznab; the \
         indexer_name plumbing must work for both kinds."
    );
}

#[tokio::test]
async fn search_uses_csv_for_multiple_categories() {
    // Multiple cats join with `,` per torznab spec. Pin the wire
    // shape so a future refactor can't accidentally split into
    // multiple cat= params.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "5070,5080"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel/></rss>"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "Test".to_string(),
        categories: vec![5070, 5080],
        limit: None,
        offset: None,
    };
    let _ = client.search(&query).await.expect("must succeed");
}

#[tokio::test]
async fn search_passes_limit_and_offset_when_set() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("limit", "25"))
        .and(query_param("offset", "50"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel/></rss>"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery {
        q: "X".to_string(),
        categories: Vec::new(),
        limit: Some(25),
        offset: Some(50),
    };
    let _ = client.search(&query).await.expect("must succeed");
}

#[tokio::test]
async fn search_empty_q_omits_q_param() {
    // An empty query string means "indexer's recent items feed"
    // — the protocol allows omitting `q`. Pin the wire behavior:
    // empty `q` results in NO `q=` param, not `q=`.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel/></rss>"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let query = SearchQuery::default();
    let releases = client.search(&query).await.expect("must succeed");
    assert!(releases.is_empty());
}

/// The per-indexer categories override is sent as written, whatever
/// the series asked for.
#[tokio::test]
async fn configured_categories_override_the_request() {
    use super::fixture::new_fixture_with_row;
    let (server, client) =
        new_fixture_with_row(|row| row.categories = "6000,2000".to_string()).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("cat", "6000,2000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;
    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: vec![5070],
        limit: None,
        offset: None,
    };
    client.search(&query).await.expect("search must succeed");
}

/// An indexer whose cached caps report only XXX is asked for XXX when
/// the series wants anime: what sukebei through Prowlarr looks like.
#[tokio::test]
async fn caps_without_the_requested_category_fall_back_to_what_the_indexer_has() {
    use super::fixture::new_fixture_with_row;
    let caps = r#"{"categories":[{"id":6000,"name":"XXX","subcategories":[]},{"id":125996,"name":"Adult Anime","subcategories":[]}],"search_modes":[],"max_limit":null,"default_limit":null}"#;
    let (server, client) = new_fixture_with_row(|row| row.caps_json = caps.to_string()).await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "6000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;
    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: Vec::new(),
        limit: None,
        offset: None,
    };
    client.search(&query).await.expect("search must succeed");
}

/// Newznab rows go through the same client, so the override and the
/// caps fallback apply to usenet indexers unchanged. Pinned separately
/// so a future newznab-specific branch cannot drop them.
#[tokio::test]
async fn newznab_rows_get_the_same_category_override_and_fallback() {
    use super::fixture::new_fixture_with_row;
    use crate::models::indexers::KIND_NEWZNAB;

    let (server, client) = new_fixture_with_row(|row| {
        row.kind = KIND_NEWZNAB.to_string();
        row.categories = "6000,2000".to_string();
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "6000,2000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;
    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: vec![5070],
        limit: None,
        offset: None,
    };
    client.search(&query).await.expect("search must succeed");
    assert_eq!(client.kind(), KIND_NEWZNAB);

    let caps = r#"{"categories":[{"id":2000,"name":"Movies","subcategories":[{"id":2040,"name":"HD","subcategories":[]}]}],"search_modes":[],"max_limit":null,"default_limit":null}"#;
    let (server, client) = new_fixture_with_row(|row| {
        row.kind = KIND_NEWZNAB.to_string();
        row.caps_json = caps.to_string();
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("cat", "2000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_BODY))
        .expect(1)
        .mount(&server)
        .await;
    let query = SearchQuery {
        q: "Test Show".to_string(),
        categories: vec![5070],
        limit: None,
        offset: None,
    };
    client.search(&query).await.expect("search must succeed");
}
