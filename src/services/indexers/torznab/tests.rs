//! Torznab parser tests. Synthetic show names per the project
//! convention (no real release titles in fixtures).

use super::parser::{
    extract_all_categories, parse_caps_response, parse_error, parse_repeating_attr,
    parse_search_response,
};

// ── parse_error ──────────────────────────────────────────────────

#[test]
fn parse_error_recognizes_self_closing_error_body() {
    // Per torznab spec: bad creds return HTTP 200 with an error
    // body, NOT a 401. The parser must catch this before the
    // search-response path sees it.
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <error code="100" description="Invalid API Key"/>"#;
    let err = parse_error(xml).expect("error body must parse");
    assert_eq!(err.code, 100);
    assert_eq!(err.description, "Invalid API Key");
}

#[test]
fn parse_error_handles_attribute_order_variations() {
    // Some impls emit description before code. Pin both shapes.
    let xml_a = r#"<error code="200" description="Missing required parameter"/>"#;
    let err_a = parse_error(xml_a).expect("code-first must parse");
    assert_eq!(err_a.code, 200);

    // Description-first ordering — caught by the `[^>]*` between
    // the two attribute captures.
    let xml_b = r#"<error description="Missing required parameter" code="200"/>"#;
    // The current regex pattern requires code BEFORE description,
    // but real impls only emit code-first. If a future indexer
    // breaks this, swap to a two-pass regex. Assert on the
    // observed behavior so a parser tweak is visible.
    assert!(
        parse_error(xml_b).is_none(),
        "description-first not currently supported; flag if a real indexer breaks this"
    );
}

#[test]
fn parse_error_returns_none_on_success_body() {
    let xml = r#"<rss version="2.0"><channel><item><title>X</title></item></channel></rss>"#;
    assert!(parse_error(xml).is_none());
}

#[test]
fn parse_error_decodes_xml_entities_in_description() {
    let xml = r#"<error code="900" description="Unknown failure: &lt;trace&gt;"/>"#;
    let err = parse_error(xml).expect("must parse");
    assert_eq!(err.description, "Unknown failure: <trace>");
}

// ── parse_search_response ────────────────────────────────────────

const SEARCH_RESPONSE_BASIC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>Test Indexer</title>
    <item>
      <title>Synthetic.Show.S01E01.1080p</title>
      <guid isPermaLink="true">https://indexer.test/release/abc</guid>
      <link>https://indexer.test/release/abc</link>
      <pubDate>Fri, 24 Apr 2026 18:32:01 +0000</pubDate>
      <enclosure url="https://indexer.test/dl/abc?apikey=KEY"
                 length="1460985071" type="application/x-bittorrent"/>
      <torznab:attr name="category" value="5070"/>
      <torznab:attr name="size" value="1460985071"/>
      <torznab:attr name="seeders" value="42"/>
      <torznab:attr name="leechers" value="3"/>
      <torznab:attr name="peers" value="45"/>
      <torznab:attr name="infohash" value="EC039A525A6FEAC4B15889323F4F443DE381E7CC"/>
      <torznab:attr name="downloadvolumefactor" value="1"/>
      <torznab:attr name="uploadvolumefactor" value="1"/>
    </item>
  </channel>
</rss>"#;

#[test]
fn search_response_extracts_basic_release_fields() {
    let result = parse_search_response(SEARCH_RESPONSE_BASIC, 7, 25, "TestIndexer")
        .expect("parse must succeed")
        .expect("body must not be an error");
    assert_eq!(result.len(), 1);
    let r = &result[0];
    assert_eq!(r.indexer_id, 7, "indexer_id stamped from caller");
    assert_eq!(r.indexer_priority, 25);
    assert_eq!(r.title, "Synthetic.Show.S01E01.1080p");
    assert_eq!(r.guid, "https://indexer.test/release/abc");
    assert_eq!(r.link, "https://indexer.test/dl/abc?apikey=KEY");
    assert_eq!(r.size_bytes, 1_460_985_071);
    assert_eq!(r.seeders, 42);
    assert_eq!(r.leechers, 3);
    assert_eq!(
        r.info_hash, "ec039a525a6feac4b15889323f4f443de381e7cc",
        "infohash must be lowercased"
    );
    assert_eq!(r.categories, vec![5070]);
    assert_eq!(r.download_volume_factor, Some(1.0));
}

#[test]
fn search_response_promotes_enclosure_url_over_link() {
    // The enclosure URL is the canonical download path per spec;
    // <link> is sometimes a comments-page URL. Prefer enclosure.
    let xml = SEARCH_RESPONSE_BASIC;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(
        result[0].link, "https://indexer.test/dl/abc?apikey=KEY",
        "must prefer enclosure URL"
    );
}

#[test]
fn search_response_falls_back_to_size_from_enclosure_when_no_size_attr() {
    // Some indexers don't repeat size as a torznab:attr — the
    // enclosure length is the only signal. Verify we pick it up.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <enclosure url="u" length="500000000" type="application/x-bittorrent"/>
  <torznab:attr name="seeders" value="10"/>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(result[0].size_bytes, 500_000_000);
}

#[test]
fn search_response_derives_leechers_from_peers_when_missing() {
    // peers = seeders + leechers per the spec. When the indexer
    // emits peers but not leechers, derive.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <torznab:attr name="seeders" value="40"/>
  <torznab:attr name="peers" value="50"/>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(
        result[0].leechers, 10,
        "50 peers - 40 seeders = 10 leechers"
    );
}

#[test]
fn search_response_skips_items_with_empty_title() {
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
<item><title></title><guid>g1</guid></item>
<item><title>Real Title</title><guid>g2</guid></item>
</channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(result.len(), 1, "title-less items must drop");
    assert_eq!(result[0].title, "Real Title");
}

#[test]
fn search_response_decodes_xml_entities_in_title() {
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><item>
  <title>Show &amp; Friends</title>
  <guid>g1</guid>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(result[0].title, "Show & Friends");
}

#[test]
fn search_response_handles_cdata_wrapped_title() {
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><item>
  <title><![CDATA[Show & <Friends>]]></title>
  <guid>g1</guid>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(result[0].title, "Show & <Friends>");
}

#[test]
fn search_response_returns_inner_err_for_torznab_error_body() {
    // HTTP 200 + error body must surface as the inner Err so the
    // caller can branch on the torznab error code.
    let err_xml = r#"<?xml version="1.0"?>
<error code="100" description="Bad API Key"/>"#;
    let result =
        parse_search_response(err_xml, 1, 25, "TestIndexer").expect("outer parse must not fail");
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("error body must surface as Err"),
    };
    assert_eq!(err.code, 100);
}

#[test]
fn search_response_empty_channel_returns_empty_releases_not_error() {
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Empty</title></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert!(result.is_empty());
}

#[test]
fn search_response_carries_multiple_categories_when_release_is_double_tagged() {
    // PR #107 review fix #2: regression for the AnimeTosho-via-
    // Prowlarr 5999/5070 mis-tag (Prowlarr#1253). A release marked
    // with both `5070` (Anime) and `5999` (Other) used to surface
    // only the first because the single-value attr map dropped
    // repeats. The fix routes through `extract_all_categories`
    // which scans the raw block.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <torznab:attr name="category" value="5999"/>
  <torznab:attr name="category" value="5070"/>
  <torznab:attr name="seeders" value="10"/>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(result[0].categories.len(), 2, "both cats must surface");
    assert!(result[0].categories.contains(&5070), "5070 missing");
    assert!(result[0].categories.contains(&5999), "5999 missing");
}

#[test]
fn search_response_collects_unrecognized_attrs_into_extra() {
    // Anything beyond the well-known torznab:attr set lands in
    // `extra` so the inspector can show indexer-specific metadata
    // without polluting the typed fields.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <torznab:attr name="seeders" value="5"/>
  <torznab:attr name="customField" value="indexer-specific-data"/>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(
        result[0].extra.get("customfield"),
        Some(&"indexer-specific-data".to_string()),
        "non-well-known attrs go into extra (lowercased keys)"
    );
}

#[test]
fn search_response_pubdate_parses_to_unix_timestamp() {
    // RFC 2822 → Unix. Pin the conversion against a known
    // reference date.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <pubDate>Thu, 01 Jan 1970 00:00:00 +0000</pubDate>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(result[0].publish_date, 0, "epoch should round-trip to 0");
}

#[test]
fn search_response_pubdate_handles_positive_timezone_offset() {
    // +0500 means "5 hours ahead of UTC"; the parser must
    // subtract the offset to land at the UTC unix value.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <pubDate>Thu, 01 Jan 1970 05:00:00 +0500</pubDate>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(
        result[0].publish_date, 0,
        "5 hours ahead of UTC at 05:00 local = 00:00 UTC"
    );
}

#[test]
fn search_response_pubdate_handles_negative_timezone_offset() {
    // PR #107 round-2 review fix #9: pin the only branch in the
    // sign-flip math (`* if sign == '+' { -1 } else { 1 }`) that
    // wasn't otherwise exercised. -0500 = "5 hours behind UTC", so
    // 19:00 local on 1969-12-31 is 00:00 UTC on 1970-01-01 = epoch.
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0">
<channel><item>
  <title>Show</title>
  <guid>g1</guid>
  <pubDate>Wed, 31 Dec 1969 19:00:00 -0500</pubDate>
</item></channel></rss>"#;
    let result = parse_search_response(xml, 1, 25, "TestIndexer")
        .expect("parse")
        .expect("not error");
    assert_eq!(
        result[0].publish_date, 0,
        "5 hours behind UTC at 19:00 local on 1969-12-31 = epoch UTC"
    );
}

// ── parse_caps_response ──────────────────────────────────────────

const CAPS_RESPONSE_BASIC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server title="Test Indexer"/>
  <limits max="100" default="50"/>
  <searching>
    <search available="yes" supportedParams="q"/>
    <tv-search available="yes" supportedParams="q,season,ep,tvdbid"/>
    <movie-search available="no"/>
  </searching>
  <categories>
    <category id="5000" name="TV">
      <subcat id="5070" name="Anime"/>
    </category>
    <category id="2000" name="Movies"/>
  </categories>
</caps>"#;

#[test]
fn caps_response_parses_limits() {
    let caps = parse_caps_response(CAPS_RESPONSE_BASIC).expect("must parse");
    assert_eq!(caps.max_limit, Some(100));
    assert_eq!(caps.default_limit, Some(50));
}

#[test]
fn caps_response_parses_search_modes_with_supported_params() {
    let caps = parse_caps_response(CAPS_RESPONSE_BASIC).expect("must parse");
    let tv = caps
        .search_modes
        .iter()
        .find(|m| m.mode == "tv-search")
        .expect("tv-search present");
    assert!(tv.available);
    assert_eq!(tv.supported_params, vec!["q", "season", "ep", "tvdbid"]);

    let movie = caps
        .search_modes
        .iter()
        .find(|m| m.mode == "movie-search")
        .expect("movie-search present");
    assert!(!movie.available, "available=no must parse as false");
}

#[test]
fn caps_response_parses_categories_with_subcategories() {
    let caps = parse_caps_response(CAPS_RESPONSE_BASIC).expect("must parse");
    let tv = caps
        .categories
        .iter()
        .find(|c| c.id == 5000)
        .expect("TV category present");
    assert_eq!(tv.name, "TV");
    let anime_sub = tv
        .subcategories
        .iter()
        .find(|c| c.id == 5070)
        .expect("Anime subcat under TV");
    assert_eq!(anime_sub.name, "Anime");

    let movies = caps
        .categories
        .iter()
        .find(|c| c.id == 2000)
        .expect("Movies category present");
    assert!(
        movies.subcategories.is_empty(),
        "categories without subcats render as empty Vec, not panic"
    );
}

#[test]
fn caps_response_returns_err_on_torznab_error_body() {
    // Caps endpoint also returns the standard torznab error body
    // shape on auth failure — make sure the parser surfaces it
    // instead of yielding empty caps.
    let xml = r#"<?xml version="1.0"?>
<error code="100" description="Bad API Key"/>"#;
    let result = parse_caps_response(xml);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().contains("100"),
        "error code must be visible in the error message"
    );
}

#[test]
fn caps_response_handles_missing_limits_block_with_none_defaults() {
    // Sparse caps response: indexer doesn't emit a <limits>
    // element. Fall back to None per the field's contract.
    let xml = r#"<?xml version="1.0"?>
<caps>
  <searching><search available="yes"/></searching>
  <categories/>
</caps>"#;
    let caps = parse_caps_response(xml).expect("must parse");
    assert_eq!(caps.max_limit, None);
    assert_eq!(caps.default_limit, None);
}

// ── repeating attr extraction ────────────────────────────────────

#[test]
fn extract_all_categories_pulls_every_category_attr_value() {
    // A release marked TV (5000) AND Anime (5070) must surface
    // both ids, not just the first. Single-value path drops
    // repeats; this helper recovers them.
    let block = r#"
        <torznab:attr name="category" value="5000"/>
        <torznab:attr name="category" value="5070"/>
        <torznab:attr name="seeders" value="10"/>
    "#;
    let cats = extract_all_categories(block);
    assert_eq!(cats, vec![5000, 5070]);
}

#[test]
fn extract_all_categories_returns_empty_when_no_category_attr() {
    let block = r#"<torznab:attr name="seeders" value="10"/>"#;
    assert!(extract_all_categories(block).is_empty());
}

#[test]
fn parse_repeating_attr_rejects_unsafe_attr_names() {
    // Defensive: only alphanum + `-_` allowed in name. Anything
    // else returns empty rather than splicing into the regex.
    let block = r#"<torznab:attr name="seeders" value="10"/>"#;
    assert!(parse_repeating_attr(block, "seeders.*").is_empty());
    assert!(parse_repeating_attr(block, "seed)ers").is_empty());
}

#[test]
fn parse_repeating_attr_works_for_newznab_namespace() {
    // Newznab uses `<newznab:attr>`. The regex pattern is built
    // with `(?:torznab|newznab)` so both work.
    let block = r#"
        <newznab:attr name="files" value="42"/>
        <newznab:attr name="files" value="43"/>
    "#;
    let values = parse_repeating_attr(block, "files");
    assert_eq!(values, vec!["42".to_string(), "43".to_string()]);
}

// ── Bounds and linear parsing ─────────────────────────────────────

fn item(title: &str) -> String {
    format!(
        r#"<item><title>{title}</title><guid>g</guid><link>https://ix/1</link><enclosure url="https://ix/dl/1?a=b" length="5"/><torznab:attr name="seeders" value="3"/></item>"#
    )
}

#[test]
fn a_page_is_read_up_to_the_item_cap() {
    let xml = format!(
        "<rss><channel>{}</channel></rss>",
        item("[G] Show - 01").repeat(1001)
    );
    let releases = parse_search_response(&xml, 1, 25, "ix").unwrap().unwrap();
    assert_eq!(releases.len(), super::parser::MAX_ITEMS);
}

#[test]
fn an_item_whose_title_is_over_the_cap_is_dropped() {
    let long = "1".repeat(crate::services::media::MAX_RELEASE_TITLE_BYTES + 1);
    let xml = format!(
        "<rss><channel>{}{}</channel></rss>",
        item(&long),
        item("[G] Show - 01")
    );
    let releases = parse_search_response(&xml, 1, 25, "ix").unwrap().unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].title, "[G] Show - 01");
}

#[test]
fn tag_names_must_end_where_the_tag_name_ends() {
    // `<items>` is no item and `<titles>` no title; names compare
    // without case.
    let xml = r#"<rss><channel><items><title>nope</title></items><ITEM><Titles>no</Titles><TITLE>[G] Show - 02</TITLE><link>https://ix/2</link></ITEM></channel></rss>"#;
    let releases = parse_search_response(xml, 1, 25, "ix").unwrap().unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].title, "[G] Show - 02");
}

#[test]
fn attribute_names_never_match_inside_another_name() {
    // `x-url` is not `url`; the pair scan reads `url` itself.
    let xml = r#"<rss><channel><item><title>[G] Show - 03</title><enclosure x-url="https://bad/" URL="https://ix/dl/3" length="7"/><torznab:attr data-name="seeders" name="seeders" value="9"/></item></channel></rss>"#;
    let releases = parse_search_response(xml, 1, 25, "ix").unwrap().unwrap();
    assert_eq!(releases[0].link, "https://ix/dl/3");
    assert_eq!(releases[0].size_bytes, 7);
    assert_eq!(releases[0].seeders, 9);
}

#[test]
fn an_unclosed_category_neither_swallows_nor_stalls_the_rest() {
    // `<category ...>(.*?)</category>` kept an unclosed tag's search
    // alive to the end of the input on every later match: quadratic,
    // 12 seconds for 218 KB in a release build. Each tag's body now
    // ends at the next open tag.
    let caps = format!(
        "<caps><categories>{}<category id=\"5000\" name=\"TV\"><subcat id=\"5070\" name=\"Anime\"/></category></categories></caps>",
        "<category id=\"1\" name=\"open\"><category id=\"2\" name=\"c\"/>".repeat(4000)
    );
    let started = std::time::Instant::now();
    let parsed = parse_caps_response(&caps).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(parsed.categories.len(), 8001);
    let tv = parsed.categories.last().unwrap();
    assert_eq!((tv.id, tv.subcategories.len()), (5000, 1));
    assert!(
        parsed.categories[..8000]
            .iter()
            .all(|c| c.subcategories.is_empty()),
        "an unclosed tag never claims a later tag's subcats"
    );
}
