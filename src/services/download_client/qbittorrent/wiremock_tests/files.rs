//! `get_files` + `set_file_wanted` wire shape. qBit uses priority
//! 0=skip / 1=normal / 6=high / 7=max; Ryokan only writes 0 and 1
//! but must read any value gracefully (6/7 on re-narrow, missing
//! field on quirky builds).

use rstest::rstest;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::fixture::new_fixture;
use crate::services::download_client::DownloadClient;

fn files_with_priority(priority: i32) -> serde_json::Value {
    serde_json::json!([
        {
            "name": "file1.mkv",
            "size": 800_000_000,
            "progress": 1.0,
            "priority": priority
        }
    ])
}

#[rstest]
#[case(0, false)] // skip
#[case(1, true)] // normal
#[case(6, true)] // high
#[case(7, true)] // max
#[tokio::test]
async fn get_files_maps_priority_to_wanted_flag(
    #[case] priority: i32,
    #[case] expected_wanted: bool,
) {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(files_with_priority(priority)))
        .mount(&server)
        .await;
    let files = client
        .get_files("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("get_files");
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].wanted, expected_wanted,
        "priority {priority} should map to wanted={expected_wanted}"
    );
}

#[tokio::test]
async fn get_files_preserves_other_fields_through_conversion() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(files_with_priority(1)))
        .mount(&server)
        .await;
    let files = client
        .get_files("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("get_files");
    assert_eq!(files[0].name, "file1.mkv");
    assert_eq!(files[0].size, 800_000_000);
    assert_eq!(files[0].progress, 1.0);
}

#[tokio::test]
async fn get_files_missing_priority_defaults_to_wanted_true() {
    // Safety net per the `default_file_priority = 1` fn-level
    // rationale: a missing field must not default to 0 (skip),
    // because our additive-merge logic treats priority-0 files as
    // "this torrent has already been narrowed" and won't remember
    // to enable them on the next grab.
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"name": "file1.mkv", "size": 1, "progress": 0.0}
        ])))
        .mount(&server)
        .await;
    let files = client
        .get_files("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("get_files");
    assert!(
        files[0].wanted,
        "missing priority field must default to wanted=true"
    );
}

#[tokio::test]
async fn get_files_empty_array_returns_empty_vec() {
    // Used by `wait_for_metadata` as the "not ready yet" signal —
    // trait contract says "empty = metadata not yet fetched."
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let files = client
        .get_files("aabbccddeeff00112233445566778899aabbccdd")
        .await
        .expect("get_files");
    assert!(files.is_empty());
}

// ─── set_file_wanted form shape ───────────────────────────────────

#[tokio::test]
async fn set_file_wanted_true_sends_priority_1() {
    // Ryokan's wanted=true policy is "priority 1 (normal)" — not
    // 6 or 7. Pinning the form body ensures a future refactor that
    // decides "max priority is better" doesn't silently change
    // behavior for every user (6/7 interact with qBit's global
    // queue differently than 1 does).
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/filePrio"))
        .and(body_string_contains("priority=1"))
        .and(body_string_contains(
            "hash=aabbccddeeff00112233445566778899aabbccdd",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted("aabbccddeeff00112233445566778899aabbccdd", &[0, 1, 2], true)
        .await
        .expect("set_file_wanted true");
}

#[tokio::test]
async fn set_file_wanted_false_sends_priority_0() {
    let (server, client) = new_fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/filePrio"))
        .and(body_string_contains("priority=0"))
        .and(body_string_contains(
            "hash=aabbccddeeff00112233445566778899aabbccdd",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client
        .set_file_wanted("aabbccddeeff00112233445566778899aabbccdd", &[0, 1], false)
        .await
        .expect("set_file_wanted false");
}

#[tokio::test]
async fn set_file_wanted_empty_slice_is_noop_does_not_call_server() {
    // No indices means nothing to update; the impl must not fire
    // an empty filePrio request (qBit would reject it as malformed
    // and leak a spurious error).
    let (server, client) = new_fixture().await;
    // expect(0) — if filePrio fires, this fails on drop.
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/filePrio"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let result = client
        .set_file_wanted("aabbccddeeff00112233445566778899aabbccdd", &[], true)
        .await;
    assert!(result.is_ok());
}
