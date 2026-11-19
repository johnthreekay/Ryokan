//! Shared wiremock fixture: a `MockServer` + a [`TorznabIndexer`]
//! built against its base URL. Mirrors the
//! `services/download_client/*/wiremock_tests/fixture.rs` pattern
//! so the test files stay focused on the behavior under test.

use wiremock::MockServer;

use crate::models::indexers::{Indexer as IndexerRow, KIND_NEWZNAB, KIND_TORZNAB};
use crate::services::indexers::torznab::TorznabIndexer;

pub const TEST_API_KEY: &str = "wiremock-key-01234567";

/// Spin up a fresh `MockServer` and return it paired with a
/// `TorznabIndexer` configured to talk to it. The base URL points
/// at `<server>/api` so tests register `Mock`s on `path("/api")`.
pub async fn new_fixture() -> (MockServer, TorznabIndexer) {
    new_fixture_with_kind(KIND_TORZNAB, "Wiremock").await
}

/// Newznab variant — same wire format and same `TorznabIndexer`
/// client (newznab is a strict subset of torznab without the
/// torrent-specific attrs), but the row's `kind` column reads
/// "newznab". Used by the indexer-name-propagation tests so the
/// "Indexer" column on interactive search verifiably attributes
/// usenet-side hits the same way it attributes torrent-side ones.
pub async fn new_fixture_newznab() -> (MockServer, TorznabIndexer) {
    new_fixture_with_kind(KIND_NEWZNAB, "WiremockUsenet").await
}

/// Torznab fixture whose row is adjusted by `tweak` before the client
/// is built (categories override, cached caps, ...).
pub async fn new_fixture_with_row(
    tweak: impl FnOnce(&mut IndexerRow),
) -> (MockServer, TorznabIndexer) {
    let server = MockServer::start().await;
    let mut row = sample_row_for(&server, KIND_TORZNAB, "Wiremock");
    tweak(&mut row);
    let client = TorznabIndexer::from_row(&row).expect("fixture row must build");
    (server, client)
}

/// A row id no other fixture in this process has. The 429 cooldown
/// (`indexers::cooldown`) is a process-wide table keyed by indexer id;
/// with one shared id, a rate-limit test's cooldown failed every other
/// fixture's requests under plain `cargo test` (one process for all
/// tests), where nextest's process per test had hidden it.
fn next_fixture_id() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static NEXT: AtomicI64 = AtomicI64::new(7_000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn sample_row_for(server: &MockServer, kind: &str, display_name: &str) -> IndexerRow {
    IndexerRow {
        id: next_fixture_id(),
        name: display_name.to_string(),
        kind: kind.to_string(),
        url: format!("{}/api", server.uri()),
        api_key: TEST_API_KEY.to_string(),
        priority: 25,
        enabled: true,
        is_private_tracker: false,
        seed_ratio: None,
        seed_time_minutes: None,
        min_seeders: 0,
        request_timeout_secs: Some(5),
        download_client_id: None,
        rss_enabled: false,
        rss_last_polled_at: None,
        rss_last_poll_error: String::new(),
        rss_last_item_count: 0,
        categories: String::new(),
        caps_json: String::new(),
        caps_refreshed_at: None,
        created_at: 0,
        updated_at: 0,
    }
}

async fn new_fixture_with_kind(kind: &str, display_name: &str) -> (MockServer, TorznabIndexer) {
    let server = MockServer::start().await;
    let row = IndexerRow {
        id: next_fixture_id(),
        name: display_name.to_string(),
        kind: kind.to_string(),
        url: format!("{}/api", server.uri()),
        api_key: TEST_API_KEY.to_string(),
        priority: 25,
        enabled: true,
        is_private_tracker: false,
        seed_ratio: None,
        seed_time_minutes: None,
        min_seeders: 0,
        request_timeout_secs: Some(5),
        download_client_id: None,
        rss_enabled: false,
        rss_last_polled_at: None,
        rss_last_poll_error: String::new(),
        rss_last_item_count: 0,
        categories: String::new(),
        caps_json: String::new(),
        caps_refreshed_at: None,
        created_at: 0,
        updated_at: 0,
    };
    let client = TorznabIndexer::from_row(&row).expect("client must build");
    (server, client)
}
