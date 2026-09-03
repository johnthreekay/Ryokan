//! `NotificationEvent` — the closed taxonomy of things that fire
//! outbound notifications. Stable across receivers because the
//! webhook provider serializes this verbatim and the Discord
//! provider reads the discriminator to pick embed colors.
//!
//! `#[serde(tag = "kind", content = "data")]` produces JSON like
//! `{"kind": "Grabbed", "data": {...}}`. `#[derive(ToSchema)]`
//! puts the event shape into the OpenAPI schema for free so users
//! wiring custom webhook receivers can grep the spec rather than
//! reverse-engineer the payload.
//!
//! See `services/notifications/CLAUDE.md` (or issue #118 if it's
//! still open) for the per-variant call-site list.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", content = "data")]
pub enum NotificationEvent {
    /// Fired after `record_grab` returns successfully at each of the
    /// five call sites (auto_search → grab_commit, auto_expand siblings,
    /// rss, manual /api/grab, download-client chained-add).
    /// `indexer` is None for the Nyaa-direct path; `score` is None for
    /// RSS / autobrr push (no scoring pass runs there) and inherited
    /// from the parent grab for sibling-series rows.
    Grabbed {
        series_id: i64,
        series_title: String,
        episode_number: i32,
        release_title: String,
        indexer: Option<String>,
        score: Option<i32>,
        client_kind: Option<String>,
    },
    /// Per-file, fired in `post_processing::do_file_op` after a
    /// successful copy/hardlink/move. `quality_tag` is read from
    /// `episode_quality_tags` for the resolved episode.
    Imported {
        series_id: i64,
        series_title: String,
        episode_number: i32,
        source_path: String,
        dest_path: String,
        quality_tag: String,
    },
    /// Per-file, fired when post-processing's per-file outcome is an
    /// error or when a precondition check (parse failure, not a video
    /// file, missing series context) skips the file. `episode_number`
    /// is None when the filename couldn't be parsed.
    ImportFailed {
        series_id: i64,
        series_title: String,
        episode_number: Option<i32>,
        source_path: String,
        reason: String,
    },
    /// Misgrab guardrails: the files inside a grab named a different
    /// series. `action` is what the sweep did: `removed`,
    /// `removed_no_delete` (seed rules kept the torrent), or `flagged`
    /// (auto-remove is off).
    Misgrabbed {
        series_id: i64,
        series_title: String,
        release_title: String,
        hash: String,
        files: Vec<String>,
        action: String,
    },
    /// Fired from `models::episode_tags::update_classification` when
    /// the row being written has `needs_review = true`. One write,
    /// one event — multiple classifier paths (initial classify,
    /// reclassify sweep, manual reclassify) all flow through the
    /// same write site.
    ClassifierNeedsReview {
        series_id: i64,
        series_title: String,
        episode_number: i32,
        confidence: i32,
        verdict_summary: String,
    },
    /// Fired from the RSS-tick indexer poll when `Indexer::search()`
    /// returns Err. Wired with per-indexer-id 1h dedup in
    /// `services::notifications::emit_indexer_down`; rate-limit cooldown
    /// errors are suppressed since the upstream is already signaling
    /// its own backoff.
    IndexerDown {
        indexer_name: String,
        reason: String,
    },
    /// Fired from the Settings → Connections status probe handler when
    /// `client.test()` returns Err. Wired with per-client-id 1h dedup
    /// in `services::notifications::emit_download_client_unreachable`.
    DownloadClientUnreachable { client_kind: String, reason: String },
    /// Fired in `services/external_sync/mod.rs` at the same point that
    /// flips the sticky `last_sync_auth_failed` flag. `provider` is
    /// `"anilist"` or `"mal"`. Default-on — this is something a user
    /// genuinely needs to know.
    ExternalSyncReLinkRequired { provider: String },
    /// Catch-all + powers the `POST /api/notifications/{id}/test`
    /// endpoint that the webhook / Discord provider issues will add.
    /// Not fired by any production path on its own.
    Health { kind: String, message: String },
}

impl NotificationEvent {
    /// The `event_kind` discriminator string written into
    /// `notification_settings.event_kind`. Stable across schema
    /// changes — the column is TEXT precisely so a future Rust-side
    /// rename doesn't silently flip every existing matrix row to
    /// "no opinion."
    pub fn kind(&self) -> &'static str {
        match self {
            NotificationEvent::Grabbed { .. } => "Grabbed",
            NotificationEvent::Imported { .. } => "Imported",
            NotificationEvent::ImportFailed { .. } => "ImportFailed",
            NotificationEvent::Misgrabbed { .. } => "Misgrabbed",
            NotificationEvent::ClassifierNeedsReview { .. } => "ClassifierNeedsReview",
            NotificationEvent::IndexerDown { .. } => "IndexerDown",
            NotificationEvent::DownloadClientUnreachable { .. } => "DownloadClientUnreachable",
            NotificationEvent::ExternalSyncReLinkRequired { .. } => "ExternalSyncReLinkRequired",
            NotificationEvent::Health { .. } => "Health",
        }
    }
}

/// The full set of `event_kind` strings the dispatcher recognizes.
/// Used by the settings layer to seed default rows for a freshly-
/// created provider and by tests to exhaustively walk the matrix.
/// Order is the natural priority for a user-facing per-event
/// settings table (operationally interesting first).
pub const ALL_EVENT_KINDS: &[&str] = &[
    "Grabbed",
    "Imported",
    "ImportFailed",
    "Misgrabbed",
    "ClassifierNeedsReview",
    "IndexerDown",
    "DownloadClientUnreachable",
    "ExternalSyncReLinkRequired",
    "Health",
];

/// Default-on event kinds for a freshly-created provider. Conservative
/// so a brand-new Webhook / Discord setup doesn't fire 200
/// `ClassifierNeedsReview` pings during a user's first library
/// reclassify pass. The settings handler reads this to seed
/// `notification_settings` rows; everything else either gets
/// `enabled = 0` or simply isn't seeded — both shapes mean
/// "don't fire."
pub const DEFAULT_ON_EVENT_KINDS: &[&str] = &[
    "Grabbed",
    "Imported",
    "ImportFailed",
    "Misgrabbed",
    "ExternalSyncReLinkRequired",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant's `kind()` discriminator must round-trip through
    /// `ALL_EVENT_KINDS` — a renamed variant or a missing entry would
    /// cause the per-event matrix to silently flip to "no opinion"
    /// for that variant.
    #[test]
    fn event_kind_strings_match_all_event_kinds_set() {
        let kinds = [
            NotificationEvent::Grabbed {
                series_id: 0,
                series_title: String::new(),
                episode_number: 0,
                release_title: String::new(),
                indexer: None,
                score: None,
                client_kind: None,
            }
            .kind(),
            NotificationEvent::Imported {
                series_id: 0,
                series_title: String::new(),
                episode_number: 0,
                source_path: String::new(),
                dest_path: String::new(),
                quality_tag: String::new(),
            }
            .kind(),
            NotificationEvent::Misgrabbed {
                series_id: 1,
                series_title: "s".into(),
                release_title: "r".into(),
                hash: "h".into(),
                files: vec![],
                action: "removed".into(),
            }
            .kind(),
            NotificationEvent::ImportFailed {
                series_id: 0,
                series_title: String::new(),
                episode_number: None,
                source_path: String::new(),
                reason: String::new(),
            }
            .kind(),
            NotificationEvent::ClassifierNeedsReview {
                series_id: 0,
                series_title: String::new(),
                episode_number: 0,
                confidence: 0,
                verdict_summary: String::new(),
            }
            .kind(),
            NotificationEvent::IndexerDown {
                indexer_name: String::new(),
                reason: String::new(),
            }
            .kind(),
            NotificationEvent::DownloadClientUnreachable {
                client_kind: String::new(),
                reason: String::new(),
            }
            .kind(),
            NotificationEvent::ExternalSyncReLinkRequired {
                provider: String::new(),
            }
            .kind(),
            NotificationEvent::Health {
                kind: String::new(),
                message: String::new(),
            }
            .kind(),
        ];
        for k in kinds {
            assert!(
                ALL_EVENT_KINDS.contains(&k),
                "{k:?} missing from ALL_EVENT_KINDS"
            );
        }
        assert_eq!(
            kinds.len(),
            ALL_EVENT_KINDS.len(),
            "ALL_EVENT_KINDS has stale entries"
        );
    }

    #[test]
    fn default_on_event_kinds_is_a_subset_of_all_event_kinds() {
        for k in DEFAULT_ON_EVENT_KINDS {
            assert!(
                ALL_EVENT_KINDS.contains(k),
                "{k:?} default-on but not in ALL_EVENT_KINDS"
            );
        }
    }

    #[test]
    fn serialization_uses_externally_tagged_shape() {
        // Receivers depend on the {"kind": "...", "data": {...}}
        // envelope. A non-default serde shape (internally tagged,
        // adjacent, untagged) would break every webhook subscriber.
        let ev = NotificationEvent::Health {
            kind: "test".into(),
            message: "hello".into(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "Health");
        assert_eq!(json["data"]["kind"], "test");
        assert_eq!(json["data"]["message"], "hello");
    }
}
