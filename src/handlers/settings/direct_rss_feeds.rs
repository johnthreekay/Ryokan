//! Settings → Direct RSS feeds CRUD + Test-button handler.
//!
//! Multi-rss commit G. Mirrors the shape of
//! `handlers::settings::indexers` — form-driven upsert + delete
//! that redirect back to the indexers tab (where the new fieldset
//! lives), plus a JSON Test endpoint the form's "Test feed"
//! button polls before Save.
//!
//! Test endpoint contract (per plan §"Test button error
//! surfacing"): always returns a JSON envelope with
//! `{ok, error?, item_count?, first_title?, detected_protocol?}`.
//! A bare-string error response would parse as `{}` in the
//! frontend and leave the test pill stuck spinning — same class
//! of regression as the `sync_now` JSON-body fix (PR #94 r2).

use axum::{Form, Json, extract::State, response::Response};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::handlers::responses::htmx_aware_redirect;

use crate::AppState;
use crate::models::direct_rss_feeds::{
    DirectRssFeedForm, delete, get_by_id, insert, set_detected_protocol, update,
};
use crate::models::log::LogCategory;
use crate::services::logger;
use crate::services::rss::{RssSource, feed};

/// Form for create / update of a `direct_rss_feeds` row.
/// `id == None` creates; `id == Some(n)` updates row `n`.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DirectRssFeedUpsertForm {
    pub id: Option<i64>,
    pub name: String,
    pub url: String,
    /// HTML checkbox — only POSTs when checked; presence-
    /// equivalent to true.
    pub enabled: Option<String>,
    /// Empty string = NULL (use default client at grab time).
    pub download_client_id: Option<String>,
    /// Empty string = NULL (use global default timeout).
    pub request_timeout_secs: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct DirectRssFeedDeleteForm {
    pub id: i64,
}

/// Test-feed request — caller passes either a row id (test an
/// existing feed) or a raw URL (test before save). At most one of
/// the two is meaningful per request; the handler picks `id`
/// first, falling back to `url`.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct DirectRssFeedTestForm {
    pub id: Option<i64>,
    pub url: Option<String>,
}

fn parse_optional_i64(s: &Option<String>) -> Option<i64> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
}

#[utoipa::path(
    post,
    path = "/settings/direct-rss-feeds/upsert",
    tag = "Settings",
    summary = "Create or update a direct RSS feed",
    description = "Form-driven upsert for the new Direct RSS feeds fieldset on the Settings → Indexers tab. Direct feeds are user-supplied RSS URLs that don't go through Prowlarr/Jackett (e.g. SubsPlease's per-quality feeds). Redirects back to the indexers tab.",
    responses(
        (status = 303, description = "Redirect to settings tab"),
    ),
)]
pub async fn settings_direct_rss_feeds_upsert(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DirectRssFeedUpsertForm>,
) -> Response {
    let name = form.name.trim().to_string();
    let url = form.url.trim().to_string();
    if name.is_empty() || url.is_empty() {
        let msg = urlencoding::encode("Direct RSS feed: name and URL are required.").into_owned();
        return htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&err={msg}"));
    }
    // Only http(s) reaches the table; `feed::fetch_user_feed` applies
    // the same rule on every poll, so a row that predates this check
    // fails closed too.
    if let Err(e) = feed::validate_feed_url(&url) {
        let msg = urlencoding::encode(&format!("Direct RSS feed: {e}")).into_owned();
        return htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&err={msg}"));
    }

    let download_client_id = parse_optional_i64(&form.download_client_id);

    // PR 112 review #1 — protocol guard. The model doc promises
    // the upsert path enforces protocol match against
    // `detected_protocol` (populated by the Test button on first
    // successful fetch). Without this gate, a user who tested a
    // torrent feed and then saved with an SAB pin would silently
    // persist the mismatch and only fail at grab time. Mirrors
    // the indexer-pin guard at `handlers::settings::indexers`.
    //
    // Only enforced on the update path: a fresh feed has no
    // detected_protocol yet (the Test button hasn't run), so the
    // first save is permissive. Once the user runs Test, the
    // protocol becomes known and subsequent saves are gated.
    //
    // PR 112 review #2 (4th pass) — the INSERT path is intentionally
    // unguarded. The current Add form doesn't expose a download-
    // client picker on insert (only the Test button → confirm → save
    // flow is wired), so an INSERT can't carry a `download_client_id`
    // through the UI. Curl-driven inserts and a future Add-form-with-
    // client-picker would bypass this branch; if/when that lands,
    // either run Test inline during INSERT or extend this guard to
    // cover both paths.
    //
    // PR 112 review #C — fail closed on transient DB errors. A
    // hiccup at save time shouldn't let a mismatch through; if
    // we can't read the row, refuse the save with a retry-now
    // toast rather than skipping the gate.
    if let (Some(id), Some(client_id)) = (form.id, download_client_id) {
        let feed_row = match get_by_id(&state.db, id).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => None, // intentional: row deleted between page-load and submit
            Err(e) => {
                let msg = urlencoding::encode(&format!(
                    "Couldn't verify protocol pin (DB error: {e}); please retry."
                ))
                .into_owned();
                return htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&err={msg}"));
            }
        };
        if let Some(feed_row) = feed_row
            && !feed_row.detected_protocol.is_empty()
        {
            let client_row =
                match crate::models::download_clients::get_by_id(&state.db, client_id).await {
                    Ok(Some(row)) => Some(row),
                    Ok(None) => None, // intentional: client deleted between page-load and submit
                    Err(e) => {
                        let msg = urlencoding::encode(&format!(
                            "Couldn't verify protocol pin (DB error: {e}); please retry."
                        ))
                        .into_owned();
                        return htmx_aware_redirect(
                            is_htmx,
                            &format!("/settings?tab=indexers&err={msg}"),
                        );
                    }
                };
            if let Some(client_row) = client_row {
                let client_proto =
                    crate::services::download_client::protocol_for_client_kind(&client_row.kind);
                if let Some(cp) = client_proto
                    && feed_row.detected_protocol != cp
                {
                    let msg = urlencoding::encode(&format!(
                        "Can't pin a {} feed to a {} client (protocol mismatch — \
                         the feed delivers {} releases, {} accepts {})",
                        feed_row.detected_protocol,
                        client_row.kind,
                        feed_row.detected_protocol,
                        client_row.kind,
                        cp
                    ))
                    .into_owned();
                    return htmx_aware_redirect(
                        is_htmx,
                        &format!("/settings?tab=indexers&err={msg}"),
                    );
                }
            }
        }
    }

    let payload = DirectRssFeedForm {
        name: &name,
        url: &url,
        enabled: form.enabled.is_some(),
        download_client_id,
        request_timeout_secs: parse_optional_i64(&form.request_timeout_secs),
    };

    let result = match form.id {
        Some(id) => update(&state.db, id, payload).await.map(|_| id),
        None => insert(&state.db, payload).await,
    };

    match result {
        Ok(_id) => {
            let verb = if form.id.is_some() {
                "updated"
            } else {
                "added"
            };
            logger::info(
                &state.db,
                LogCategory::Rss,
                &format!("Direct RSS feed {verb}: {name}"),
                &url,
            )
            .await;
            let msg =
                urlencoding::encode(&format!("Direct RSS feed '{name}' {verb}.")).into_owned();
            htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&msg={msg}"))
        }
        Err(e) => {
            let err =
                urlencoding::encode(&format!("Direct RSS feed save failed: {e}")).into_owned();
            htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&err={err}"))
        }
    }
}

#[utoipa::path(
    post,
    path = "/settings/direct-rss-feeds/delete",
    tag = "Settings",
    summary = "Delete a direct RSS feed",
    description = "Removes the direct_rss_feeds row by id. Existing `rss_seen` rows are left intact (audit trail per the plan §retention rationale).",
    responses(
        (status = 303, description = "Redirect to settings tab"),
    ),
)]
pub async fn settings_direct_rss_feeds_delete(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<DirectRssFeedDeleteForm>,
) -> Response {
    let name = match get_by_id(&state.db, form.id).await {
        Ok(Some(row)) => row.name,
        _ => "(unknown)".to_string(),
    };
    match delete(&state.db, form.id).await {
        Ok(()) => {
            logger::info(
                &state.db,
                LogCategory::Rss,
                &format!("Direct RSS feed deleted: {name}"),
                &format!("id={}", form.id),
            )
            .await;
            let msg =
                urlencoding::encode(&format!("Direct RSS feed '{name}' deleted.")).into_owned();
            htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&msg={msg}"))
        }
        Err(e) => {
            let err = urlencoding::encode(&format!("Delete failed: {e}")).into_owned();
            htmx_aware_redirect(is_htmx, &format!("/settings?tab=indexers&err={err}"))
        }
    }
}

/// JSON envelope returned by every Test endpoint. Spelling out
/// the shape so the frontend toast can finalize cleanly on every
/// branch — bare-string error bodies would parse as `{}` and
/// leave the test pill stuck spinning.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct DirectRssFeedTestResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_title: Option<String>,
    /// `"torrent"` / `"usenet"` — populated on success based on
    /// the first item's enclosure URL. Drives the protocol-guard
    /// + display in the form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_protocol: Option<String>,
}

/// Detect protocol from a fetched item: magnet URI / .torrent
/// link → `"torrent"`; .nzb link → `"usenet"`. Empty when neither
/// signal is present (caller treats as "unknown protocol", pin
/// save remains permissive).
fn detect_protocol_from_first_item(item: &crate::services::rss::RssItem) -> Option<&'static str> {
    if !item.magnet.is_empty() {
        return Some("torrent");
    }
    let link_lower = item.link.to_ascii_lowercase();
    let torrent_lower = item.torrent.to_ascii_lowercase();
    if link_lower.contains(".torrent") || torrent_lower.contains(".torrent") {
        return Some("torrent");
    }
    if link_lower.ends_with(".nzb")
        || torrent_lower.ends_with(".nzb")
        || link_lower.contains(".nzb?")
    {
        return Some("usenet");
    }
    None
}

#[utoipa::path(
    post,
    path = "/settings/direct-rss-feeds/test",
    tag = "Settings",
    summary = "Test-fetch a direct RSS feed",
    description = "Fires a single fetch against the supplied URL (or the URL of the row identified by `id`) and returns a JSON envelope describing the result: item count, first item's title, and the detected protocol (torrent/usenet) inferred from the first item's enclosure shape. Used by the Settings → Direct RSS feeds form's Test button.",
    responses(
        (status = 200, description = "Test result envelope", body = DirectRssFeedTestResponse),
    ),
)]
pub async fn settings_direct_rss_feeds_test(
    State(state): State<AppState>,
    Json(form): Json<DirectRssFeedTestForm>,
) -> Json<DirectRssFeedTestResponse> {
    // Resolve the URL to test from either the row id or the raw
    // form-supplied URL. Row id wins so a saved-then-edited feed
    // tests the persisted URL rather than the un-saved form value.
    let url = if let Some(id) = form.id {
        match get_by_id(&state.db, id).await {
            Ok(Some(row)) => row.url,
            _ => {
                return Json(DirectRssFeedTestResponse {
                    ok: false,
                    error: Some(format!("No direct_rss_feeds row with id={id}")),
                    item_count: None,
                    first_title: None,
                    detected_protocol: None,
                });
            }
        }
    } else if let Some(url) = form.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        url.to_string()
    } else {
        return Json(DirectRssFeedTestResponse {
            ok: false,
            error: Some("Test request missing both id and url".into()),
            item_count: None,
            first_title: None,
            detected_protocol: None,
        });
    };

    let source = RssSource::UserFeed {
        id: form.id.unwrap_or(0),
        name: "test".to_string(),
    };
    match feed::fetch_user_feed(&url, source).await {
        Ok(items) => {
            let count = items.len() as i32;
            let first_title = items.first().map(|i| i.title.clone());
            let detected_protocol = items
                .first()
                .and_then(detect_protocol_from_first_item)
                .map(str::to_string);

            // If the caller supplied a row id and we detected a
            // protocol, persist it so the pin save path can enforce
            // protocol match next save. Test → Save flow.
            if let (Some(id), Some(proto)) = (form.id, detected_protocol.as_deref()) {
                let _ = set_detected_protocol(&state.db, id, proto).await;
            }

            Json(DirectRssFeedTestResponse {
                ok: true,
                error: None,
                item_count: Some(count),
                first_title,
                detected_protocol,
            })
        }
        Err(err) => Json(DirectRssFeedTestResponse {
            ok: false,
            error: Some(err),
            item_count: None,
            first_title: None,
            detected_protocol: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn upsert_rejects_empty_name_or_url() {
        let state = build_test_app_state(in_memory_pool().await, None);
        let resp = settings_direct_rss_feeds_upsert(
            State(state),
            HxRequest(false),
            Form(DirectRssFeedUpsertForm {
                id: None,
                name: "".into(),
                url: "https://x.example/rss".into(),
                enabled: None,
                download_client_id: None,
                request_timeout_secs: None,
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.contains("err="));
    }

    #[tokio::test]
    async fn upsert_rejects_a_file_url_and_saves_nothing() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_direct_rss_feeds_upsert(
            State(state),
            HxRequest(false),
            Form(DirectRssFeedUpsertForm {
                id: None,
                name: "evil".into(),
                url: "file:///etc/passwd".into(),
                enabled: Some("on".into()),
                download_client_id: None,
                request_timeout_secs: None,
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.contains("err="), "{location}");
        assert!(location.contains("http"), "{location}");
        let rows = crate::models::direct_rss_feeds::list_all(&db)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn test_endpoint_rejects_a_file_url_without_fetching() {
        let state = build_test_app_state(in_memory_pool().await, None);
        let resp = settings_direct_rss_feeds_test(
            State(state),
            Json(DirectRssFeedTestForm {
                id: None,
                url: Some("file:///etc/passwd".into()),
            }),
        )
        .await;
        assert!(!resp.0.ok);
        assert!(resp.0.error.unwrap().contains("http://"));
    }

    #[tokio::test]
    async fn upsert_persists_new_row_then_lists_via_get_by_id() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_direct_rss_feeds_upsert(
            State(state),
            HxRequest(false),
            Form(DirectRssFeedUpsertForm {
                id: None,
                name: "SubsPlease 1080p".into(),
                url: "https://subsplease.org/rss/?r=1080".into(),
                enabled: Some("on".into()),
                download_client_id: None,
                request_timeout_secs: None,
            }),
        )
        .await;
        // Find by name via list.
        let rows = crate::models::direct_rss_feeds::list_all(&db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "SubsPlease 1080p");
        assert!(rows[0].enabled);
    }

    #[tokio::test]
    async fn test_endpoint_returns_json_envelope_with_error_for_missing_id() {
        let state = build_test_app_state(in_memory_pool().await, None);
        let resp = settings_direct_rss_feeds_test(
            State(state),
            Json(DirectRssFeedTestForm {
                id: Some(9999),
                url: None,
            }),
        )
        .await;
        assert!(!resp.0.ok);
        assert!(resp.0.error.unwrap().contains("9999"));
    }

    #[tokio::test]
    async fn test_endpoint_rejects_request_without_id_or_url() {
        let state = build_test_app_state(in_memory_pool().await, None);
        let resp = settings_direct_rss_feeds_test(
            State(state),
            Json(DirectRssFeedTestForm {
                id: None,
                url: None,
            }),
        )
        .await;
        assert!(!resp.0.ok);
    }

    #[tokio::test]
    async fn upsert_protocol_guard_rejects_torrent_feed_pinned_to_sab() {
        // PR 112 review #1 — the model doc says the upsert path
        // enforces protocol match against detected_protocol. Pin
        // the rejection so a regression that drops the guard
        // fails this test loudly.
        let db = in_memory_pool().await;
        let sab_id = crate::models::download_clients::insert(
            &db,
            crate::models::download_clients::DownloadClientForm {
                name: "SAB",
                kind: "sabnzbd",
                url: "http://sab",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        // Insert a feed + stamp detected_protocol = "torrent"
        // (mimics post-Test state). Then attempt to update with a
        // SAB pin — should reject.
        let feed_id = crate::models::direct_rss_feeds::insert(
            &db,
            crate::models::direct_rss_feeds::DirectRssFeedForm {
                name: "SubsPlease",
                url: "https://subsplease.org/rss",
                enabled: true,
                download_client_id: None,
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();
        crate::models::direct_rss_feeds::set_detected_protocol(&db, feed_id, "torrent")
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let resp = settings_direct_rss_feeds_upsert(
            State(state),
            HxRequest(false),
            Form(DirectRssFeedUpsertForm {
                id: Some(feed_id),
                name: "SubsPlease".into(),
                url: "https://subsplease.org/rss".into(),
                enabled: Some("on".into()),
                download_client_id: Some(sab_id.to_string()),
                request_timeout_secs: None,
            }),
        )
        .await
        .into_response();
        let location = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.contains("err=") && location.contains("protocol+mismatch")
                || location.contains("protocol%20mismatch"),
            "expected protocol-mismatch err in redirect: {location}"
        );

        // Verify the row's pin was NOT updated.
        let feed = crate::models::direct_rss_feeds::get_by_id(&db, feed_id)
            .await
            .unwrap()
            .unwrap();
        assert!(feed.download_client_id.is_none(), "pin must remain None");
    }

    #[tokio::test]
    async fn upsert_protocol_guard_permits_matching_protocol() {
        // Sibling case: torrent feed pinned to qBit (matching
        // protocol) saves cleanly.
        let db = in_memory_pool().await;
        let qbit_id = crate::models::download_clients::insert(
            &db,
            crate::models::download_clients::DownloadClientForm {
                name: "qBit",
                kind: "qbittorrent",
                url: "http://qbit",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        let feed_id = crate::models::direct_rss_feeds::insert(
            &db,
            crate::models::direct_rss_feeds::DirectRssFeedForm {
                name: "SubsPlease",
                url: "https://subsplease.org/rss",
                enabled: true,
                download_client_id: None,
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();
        crate::models::direct_rss_feeds::set_detected_protocol(&db, feed_id, "torrent")
            .await
            .unwrap();

        let state = build_test_app_state(db.clone(), None);
        let _ = settings_direct_rss_feeds_upsert(
            State(state),
            HxRequest(false),
            Form(DirectRssFeedUpsertForm {
                id: Some(feed_id),
                name: "SubsPlease".into(),
                url: "https://subsplease.org/rss".into(),
                enabled: Some("on".into()),
                download_client_id: Some(qbit_id.to_string()),
                request_timeout_secs: None,
            }),
        )
        .await;
        let feed = crate::models::direct_rss_feeds::get_by_id(&db, feed_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(feed.download_client_id, Some(qbit_id));
    }

    #[tokio::test]
    async fn upsert_protocol_guard_permissive_when_protocol_not_yet_detected() {
        // Fresh feed (no Test pass yet) — detected_protocol is
        // empty, so the pin save permits any client. The user
        // can re-test later and the next save will gate.
        let db = in_memory_pool().await;
        let sab_id = crate::models::download_clients::insert(
            &db,
            crate::models::download_clients::DownloadClientForm {
                name: "SAB",
                kind: "sabnzbd",
                url: "http://sab",
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default: false,
            },
        )
        .await
        .unwrap();
        let feed_id = crate::models::direct_rss_feeds::insert(
            &db,
            crate::models::direct_rss_feeds::DirectRssFeedForm {
                name: "Untested",
                url: "https://untested.example/rss",
                enabled: true,
                download_client_id: None,
                request_timeout_secs: None,
            },
        )
        .await
        .unwrap();
        // detected_protocol is empty (no Test ran).

        let state = build_test_app_state(db.clone(), None);
        let _ = settings_direct_rss_feeds_upsert(
            State(state),
            HxRequest(false),
            Form(DirectRssFeedUpsertForm {
                id: Some(feed_id),
                name: "Untested".into(),
                url: "https://untested.example/rss".into(),
                enabled: Some("on".into()),
                download_client_id: Some(sab_id.to_string()),
                request_timeout_secs: None,
            }),
        )
        .await;
        let feed = crate::models::direct_rss_feeds::get_by_id(&db, feed_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            feed.download_client_id,
            Some(sab_id),
            "untested feed permits any pin; subsequent Test+Save will gate"
        );
    }

    #[test]
    fn detect_protocol_from_first_item_recognizes_magnet_torrent_nzb() {
        // Pin the protocol-detection mapping. The function is the
        // single source of truth for turning an RssItem into a
        // protocol label, and the protocol-guard at pin-save time
        // keys off this — a regression here silently breaks the
        // SAB-vs-BT routing.
        let mk = |magnet: &str, link: &str, torrent: &str| crate::services::rss::RssItem {
            title: "x".into(),
            link: link.into(),
            guid: String::new(),
            torrent: torrent.into(),
            magnet: magnet.into(),
            info_hash: String::new(),
            group: String::new(),
            resolution: String::new(),
            is_batch: false,
            source: RssSource::Nyaa,
        };
        assert_eq!(
            detect_protocol_from_first_item(&mk("magnet:?xt=urn:btih:abc", "", "")),
            Some("torrent")
        );
        assert_eq!(
            detect_protocol_from_first_item(&mk("", "https://x/file.torrent", "")),
            Some("torrent")
        );
        assert_eq!(
            detect_protocol_from_first_item(&mk("", "https://x/file.nzb", "")),
            Some("usenet")
        );
        assert_eq!(
            detect_protocol_from_first_item(&mk("", "https://x/comments", "")),
            None
        );
    }
}
