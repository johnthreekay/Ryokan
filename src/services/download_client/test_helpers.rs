//! Shared helpers for the per-client `live_smoke*` tests. Only
//! compiled under `#[cfg(test)]`.
//!
//! The smoke tests exercise each `DownloadClient` impl against a
//! real running daemon (qBittorrent / Deluge / Transmission /
//! rtorrent on localhost). They use synthetic `.torrent` files
//! built on-the-fly via shell-out to `transmission-create` rather
//! than depending on a public magnet's DHT metadata fetch landing
//! during the test window — magnets are flaky in containerized
//! networks and make the tests non-deterministic. The synthetic
//! approach gives every test run the same file layout with the same
//! priorities-post-narrow assertions.

use std::path::PathBuf;

/// Build a synthetic multi-file `.torrent` in a tempdir via
/// `transmission-create`. Returns `(tempdir_guard, torrent_path)` —
/// the tempdir handle must outlive the test so the dummy files
/// remain on disk for clients that check file-existence at add time
/// (some clients refuse to add a torrent whose source files are
/// missing). Returns `None` with a printed skip message if
/// `transmission-create` isn't installed.
///
/// The default file layout (7 files, mirrors a typical anime batch):
/// * `testpack/episode_1.mkv` .. `episode_5.mkv` — 8 KB each, the
///   "wanted" subset for narrowing tests.
/// * `testpack/sample.mkv` — 2 KB, what `pick_wanted_file_indices`
///   classifies as likely-unwanted extras.
/// * `testpack/readme.txt` — 512 B, explicitly unwanted sidecar.
///
/// Content is all-zero bytes. That's fine because the smoke tests
/// always pause immediately after upload and verify priorities
/// without ever attempting to actually download content (the fake
/// tracker URL is unreachable anyway).
pub(crate) fn build_testpack_torrent() -> Option<(tempfile::TempDir, PathBuf)> {
    build_inner("testpack", false)
}

/// Like [`build_testpack_torrent`] but takes a unique name and
/// injects it as file content so multiple calls produce *distinct*
/// infohashes. The directory name is baked into one file
/// (`pack_name.txt`) so two invocations with different `name`
/// arguments produce torrents whose `info` dicts differ →
/// different SHA-1 → different hashes.
///
/// Used by tests that need two or more torrents in the client at
/// once (e.g. B2 list_scoped exclusion: one Ryokan-scoped and one
/// non-Ryokan, both present in the client, only the first should
/// appear in `list_scoped`).
///
/// File count is 8 (the canonical 7 + `pack_name.txt`), which is
/// why this is a separate function rather than reusing
/// [`build_testpack_torrent`]'s 7-file assertion.
pub(crate) fn build_named_torrent(name: &str) -> Option<(tempfile::TempDir, PathBuf)> {
    build_inner(name, true)
}

/// Upload a local `.torrent` file to qBit via the multipart
/// `torrents/add` endpoint with `paused=true` and `stopped=true`
/// (qBit 5.x renamed the flag — pass both to survive either
/// version), scoped to the given category. Computes the v1 infohash
/// up front from the `.torrent` bytes and polls for that *specific*
/// hash to confirm registration — safe to call multiple times
/// against the same category (unlike a first-in-category lookup,
/// which would race).
///
/// Returns the confirmed hash once qBit has it. Shared between
/// `qbittorrent::tests::live_smoke_*` and handler-level Wave 2
/// integration tests.
pub(crate) async fn upload_torrent_file_qbit(
    base_url: &str,
    user: &str,
    pass: &str,
    category: &str,
    torrent_path: &std::path::Path,
) -> String {
    let bytes = std::fs::read(torrent_path).expect("read .torrent");
    // Pre-compute the expected infohash so we poll for a specific
    // hash rather than trusting "whatever's in the category first."
    let expected = bencode_info_hash(&bytes)
        .expect("bencode_info_hash on uploaded .torrent")
        .to_ascii_lowercase();

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client");
    let login = client
        .post(format!("{base_url}/api/v2/auth/login"))
        .form(&[("username", user), ("password", pass)])
        .send()
        .await
        .expect("qBit login");
    assert_eq!(login.status(), 200, "qBit login failed");
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("testpack.torrent")
        .mime_str("application/x-bittorrent")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .part("torrents", part)
        .text("category", category.to_string())
        .text("paused", "true")
        .text("stopped", "true");
    let resp = client
        .post(format!("{base_url}/api/v2/torrents/add"))
        .multipart(form)
        .send()
        .await
        .expect("qBit add");
    assert_eq!(resp.status(), 200, "qBit add returned {}", resp.status());

    // Poll `/torrents/info?hashes=<expected>` — qBit filters by hash
    // directly, so we don't care about category isolation. Concurrent
    // tests sharing a category are safe.
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let list_resp = client
            .get(format!("{base_url}/api/v2/torrents/info?hashes={expected}"))
            .send()
            .await
            .expect("qBit info-by-hash");
        let torrents: Vec<serde_json::Value> =
            list_resp.json().await.expect("qBit info-by-hash json");
        if torrents.iter().any(|t| {
            t.get("hash")
                .and_then(|v| v.as_str())
                .map(|h| h.eq_ignore_ascii_case(&expected))
                .unwrap_or(false)
        }) {
            return expected;
        }
    }
    panic!("uploaded torrent {expected} never registered in qBit");
}

/// Compute the v1 infohash of a `.torrent` (the SHA-1 of its top-level
/// bencoded `info` dict), through `services::torrent_file`.
///
/// Shared between the rtorrent smoke (where `load.raw_start_verbose`
/// returns 0 rather than echoing back the hash, so we compute it
/// ourselves) and `upload_torrent_file_qbit` (which uses it to poll
/// qBit's `/torrents/info` for a specific hash rather than
/// first-in-category — makes the helper safe to call multiple times
/// against the same category).
pub(crate) fn bencode_info_hash(bytes: &[u8]) -> Option<String> {
    crate::services::torrent_file::torrent_info_hash(bytes)
}

fn build_inner(name: &str, with_name_file: bool) -> Option<(tempfile::TempDir, PathBuf)> {
    if std::process::Command::new("transmission-create")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: transmission-create not installed");
        return None;
    }
    let tmp = tempfile::tempdir().expect("tempdir creation failed");
    let pack_dir = tmp.path().join(name);
    std::fs::create_dir(&pack_dir).expect("create pack dir");
    for i in 1..=5 {
        std::fs::write(pack_dir.join(format!("episode_{i}.mkv")), vec![0u8; 8192])
            .expect("write episode file");
    }
    std::fs::write(pack_dir.join("sample.mkv"), vec![0u8; 2048]).expect("write sample");
    std::fs::write(pack_dir.join("readme.txt"), b"test readme").expect("write readme");
    if with_name_file {
        // Name-specific content so distinct names produce distinct hashes.
        std::fs::write(pack_dir.join("pack_name.txt"), name.as_bytes()).expect("write pack_name");
    }
    let torrent_path = tmp.path().join(format!("{name}.torrent"));
    let output = std::process::Command::new("transmission-create")
        .args([
            "-o",
            torrent_path.to_str().unwrap(),
            "-t",
            "http://localhost:1/announce",
        ])
        .arg(&pack_dir)
        .output()
        .expect("transmission-create spawn failed");
    assert!(
        output.status.success(),
        "transmission-create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some((tmp, torrent_path))
}

/// Release URLs every client must refuse before sending anything: local
/// paths (Transmission's `filename` and rTorrent's `load.start_verbose`
/// would read them on the client's machine), other schemes, and strings
/// that aren't URLs at all.
const NON_URL_RELEASES: &[&str] = &[
    "file:///etc/passwd",
    "/config/torrents/planted.torrent",
    "ftp://tracker.example/x.torrent",
    "javascript:alert(1)",
    "not-a-url",
    "",
];

/// Drive every trait add method with each of [`NON_URL_RELEASES`] and
/// assert `check_release_url` refused it with no request reaching
/// `server`.
pub(crate) async fn assert_refuses_non_url_releases(
    client: &dyn super::DownloadClient,
    server: &wiremock::MockServer,
) {
    const HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";
    let sent = || async { server.received_requests().await.map_or(0, |r| r.len()) };
    let before = sent().await;
    for url in NON_URL_RELEASES {
        let refused = |method: &str, err: Option<String>| {
            let err = err.unwrap_or_else(|| panic!("{method} accepted {url:?}"));
            assert!(
                err.starts_with("add rejected url="),
                "{method} {url:?}: {err}"
            );
        };
        refused("add_torrent", client.add_torrent(url, HASH).await.err());
        refused(
            "add_torrent_paused",
            client.add_torrent_paused(url, HASH).await.err(),
        );
        refused(
            "add_torrent_returning_id",
            client.add_torrent_returning_id(url, HASH).await.err(),
        );
        let mut pick = |_: &[String]| -> Option<Vec<usize>> { None };
        refused(
            "add_torrent_with_file_filter",
            client
                .add_torrent_with_file_filter(url, HASH, &mut pick)
                .await
                .err(),
        );
    }
    assert_eq!(
        sent().await,
        before,
        "a refused URL must not reach the client"
    );
}
