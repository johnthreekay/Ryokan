//! `pause` / `resume` / `delete`. Two load-bearing qBit quirks
//! tested here:
//!
//!   * The 5.x rename of `/torrents/pause` → `/torrents/stop` and
//!     `/torrents/resume` → `/torrents/start`. Ryokan's impl tries
//!     the new name first and falls back to the old one on
//!     non-success — the tests assert both paths behave correctly.
//!   * The `deleteFiles` flag passes through to the form body so
//!     the caller can decide whether to keep data on disk (seed-
//!     preserving "blocklist this torrent" flows need deleteFiles
//!     = false).

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;

// ─── pause: stop-first, pause-fallback ─────────────────────────────

#[tokio::test]
async fn pause_tries_new_name_first_and_succeeds_on_200() {
    let (server, client) = new_fixture().await;
    // `/torrents/stop` is the new name — 200 means "we're on 5.x,
    // the rename took effect, don't touch the legacy endpoint."
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/pause"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    client
        .pause("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("pause");
}

#[tokio::test]
async fn pause_falls_back_to_legacy_name_when_new_returns_non_success() {
    // 4.x behavior: `/torrents/stop` doesn't exist; qBit returns a
    // non-success status (typically 404). Impl falls back to the
    // legacy `/torrents/pause`.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/pause"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .pause("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("pause");
}

#[tokio::test]
async fn pause_sends_hashes_form_body() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/stop"))
        .and(body_string_contains(
            "hashes=aabbccddeeff00112233445566778899aabbccdd",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .pause("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("pause");
}

// ─── resume: start-first, resume-fallback ──────────────────────────

#[tokio::test]
async fn resume_tries_new_name_first_and_succeeds_on_200() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/start"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/resume"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    client
        .resume("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("resume");
}

#[tokio::test]
async fn resume_falls_back_to_legacy_name_when_new_returns_non_success() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/start"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/resume"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .resume("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("resume");
}

// ─── delete: deleteFiles pass-through ─────────────────────────────

#[tokio::test]
async fn delete_with_delete_files_true_sends_true_in_form() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .and(body_string_contains("deleteFiles=true"))
        .and(body_string_contains(
            "hashes=aabbccddeeff00112233445566778899aabbccdd",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .delete("aabbccddeeff00112233445566778899aabbccdd", true)
        .await
        .expect("delete");
}

#[tokio::test]
async fn delete_with_delete_files_false_preserves_data_flag() {
    // Blocklist flow sets deleteFiles=false so the user's on-disk
    // files stick around. Pinning the form value ensures a refactor
    // that defaults to "true" for simplicity would break the
    // blocklist contract.
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .and(body_string_contains("deleteFiles=false"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .delete("aabbccddeeff00112233445566778899aabbccdd", false)
        .await
        .expect("delete");
}

#[tokio::test]
async fn delete_surfaces_non_2xx_as_error() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/delete"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let err = client
        .delete("aabbccddeeff00112233445566778899aabbccdd", true)
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("delete") || err.contains("Failed"));
}

#[tokio::test]
async fn hashes_qbit_reads_as_many_torrents_are_refused_before_any_request() {
    // `hashes=all` is every torrent in qBittorrent and `a|b` is a list;
    // a stored `all` once turned one grab's delete into deleting them
    // all. Nothing but a real hash reaches the client.
    let (server, client) = new_fixture().await;
    let before = server.received_requests().await.unwrap_or_default().len();
    let rules = crate::services::download_client::SeedRules {
        ratio: Some(1.0),
        time_minutes: None,
    };
    for hash in [
        "all",
        "aabbccddeeff00112233445566778899aabbccdd|ffeeddccbbaa00112233445566778899aabbccdd",
        "x&deleteFiles=true",
    ] {
        assert!(client.delete(hash, true).await.is_err(), "delete {hash}");
        assert!(client.pause(hash).await.is_err(), "pause {hash}");
        assert!(client.resume(hash).await.is_err(), "resume {hash}");
        assert!(
            client.set_seed_rules(hash, rules).await.is_err(),
            "seed rules {hash}"
        );
        assert!(client.get_files(hash).await.is_err(), "get_files {hash}");
        assert!(
            client.set_file_wanted(hash, &[0], true).await.is_err(),
            "file prio {hash}"
        );
    }
    // A hashless grab's cleanup stays the old no-op.
    client
        .delete("", true)
        .await
        .expect("empty hash is a no-op");
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        before,
        "no request may reach qBittorrent"
    );
}
