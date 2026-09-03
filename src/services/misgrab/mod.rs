//! Misgrab guardrails: verify a grab against the files the download
//! client reports, and remediate the ones that turn out to be a
//! different series.
//!
//! Two halves. The **verdict** (`verdict.rs`) is pure and runs wherever
//! a file list first shows up: the grab-time metadata wait in
//! `auto_expand`, the post-processing import path, or the sweep below.
//! It is stamped once on the grab row (`grabbed_torrents.verification`)
//! and never flips on its own. **Remediation** (delete from the client,
//! blocklist, notify, re-search) runs only from the supervised
//! `misgrab_sweep`, the one place with `AppState` that covers all nine
//! grab paths and survives restarts.

pub mod verdict;

use std::time::Duration;

use sqlx::SqlitePool;

use crate::models::grabbed_torrents::{self, GrabbedTorrent};
use crate::models::log::LogCategory;
use crate::models::{metadata_cache, series};
use crate::services::anilist::AnimeDetail;
use crate::services::{auto_search, logger};

pub use verdict::{Verdict, VerdictInput, assess};

/// How often the sweep looks for unverified grabs and unhandled
/// misgrabs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// A grab younger than this is left to its grab-time spawn.
pub const MIN_AGE_SECS: i64 = 20;
/// How long the sweep keeps asking the client for a file list before
/// giving up on a grab as unverifiable.
pub const METADATA_GRACE: Duration = Duration::from_secs(15 * 60);

/// The aliases a verdict is judged against.
#[derive(Debug, Clone, Default)]
pub struct AliasSet {
    pub own: Vec<String>,
    pub siblings: Vec<String>,
    pub expected_season: i32,
}

/// Own titles plus synonyms, and every related entry's titles plus the
/// arc subtitles sibling detection recognized in these file names.
pub fn aliases_from_detail(detail: &AnimeDetail, filenames: &[String]) -> AliasSet {
    let mut own = auto_search::collect_aliases(detail);
    own.extend(detail.synonyms.iter().cloned());
    let own = auto_search::dedupe_strings(own);
    let mut siblings = auto_search::collect_sibling_aliases(detail, &own);
    for rel in &detail.relations {
        for title in [&rel.title_romaji, &rel.title_english, &rel.title_native] {
            if !title.trim().is_empty() {
                siblings.push(title.clone());
            }
        }
    }
    for sibling in auto_search::detect_sibling_entries_in_pack(filenames, detail) {
        if !sibling.matched_subtitle.trim().is_empty() {
            siblings.push(sibling.matched_subtitle.clone());
        }
        for title in [
            &sibling.title_romaji,
            &sibling.title_english,
            &sibling.title_native,
        ] {
            if !title.trim().is_empty() {
                siblings.push(title.clone());
            }
        }
    }
    AliasSet {
        own,
        siblings: auto_search::dedupe_strings(siblings),
        expected_season: auto_search::infer_season_from_detail(detail),
    }
}

/// Aliases for a grab the sweep or the import path holds: the cached
/// metadata when there is one, else the series row's own titles (no
/// siblings, no season). `None` when the series row is gone.
pub async fn aliases_for_grab(
    db: &SqlitePool,
    grab: &GrabbedTorrent,
    filenames: &[String],
) -> Option<AliasSet> {
    if let Ok(Some(cached)) = metadata_cache::get_by_series_id(db, grab.series_id).await {
        return Some(aliases_from_detail(&cached.detail, filenames));
    }
    let row = series::get_by_id(db, grab.series_id).await.ok().flatten()?;
    let own = auto_search::dedupe_strings(vec![
        row.title.clone(),
        row.title_romaji.clone(),
        row.title_english.clone(),
        row.title_native.clone(),
    ]);
    Some(AliasSet {
        own,
        siblings: Vec::new(),
        expected_season: 0,
    })
}

/// Judge the file list and stamp the verdict once. A hash the user
/// restored is stamped `whitelisted` without being judged. Logs a
/// warning the first time a misgrab is recorded.
pub async fn assess_and_stamp(
    db: &SqlitePool,
    grab: &GrabbedTorrent,
    filenames: &[String],
    aliases: &AliasSet,
) -> Verdict {
    if grabbed_torrents::is_whitelisted_hash(db, &grab.hash).await {
        let verdict = Verdict::Verified {
            matched_file: String::new(),
            matched_alias: "whitelisted by the user".to_string(),
            notes: Vec::new(),
        };
        let detail = serde_json::to_string(&verdict.detail(filenames)).unwrap_or_default();
        let _ = grabbed_torrents::stamp_verification(db, grab.id, "whitelisted", &detail).await;
        return verdict;
    }
    let verdict = assess(&VerdictInput {
        own_aliases: &aliases.own,
        sibling_aliases: &aliases.siblings,
        filenames,
        expected_season: aliases.expected_season,
    });
    let detail = verdict.detail(filenames);
    let detail_json = serde_json::to_string(&detail).unwrap_or_default();
    let wrote = grabbed_torrents::stamp_verification(db, grab.id, verdict.as_str(), &detail_json)
        .await
        .unwrap_or(false);
    if wrote && verdict.is_misgrab() {
        logger::warn(
            db,
            LogCategory::Grab,
            &format!("Misgrab detected: '{}'", grab.torrent_name),
            &format!(
                "series_id={}, hash={}, files={:?}",
                grab.series_id, grab.hash, detail.files
            ),
        )
        .await;
    } else if wrote {
        logger::debug(
            db,
            LogCategory::Grab,
            &format!(
                "Grab verified as {} : '{}'",
                verdict.as_str(),
                grab.torrent_name
            ),
            &detail.reason,
        )
        .await;
    }
    verdict
}

/// Resolve the aliases for a grab and judge it. Used by the sweep and
/// the import path; the grab-time spawn already holds a detail and
/// calls `assess_and_stamp` directly.
pub async fn assess_grab(db: &SqlitePool, grab: &GrabbedTorrent, filenames: &[String]) -> Verdict {
    match aliases_for_grab(db, grab, filenames).await {
        Some(aliases) => assess_and_stamp(db, grab, filenames, &aliases).await,
        None => {
            let verdict = Verdict::Unverifiable {
                reason: "series row is gone",
            };
            let detail = serde_json::to_string(&verdict.detail(filenames)).unwrap_or_default();
            let _ =
                grabbed_torrents::stamp_verification(db, grab.id, verdict.as_str(), &detail).await;
            verdict
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        db
    }

    async fn seed(db: &SqlitePool, anilist_id: i64, title: &str, romaji: &str) -> i64 {
        let (id, _) = series::upsert(
            db,
            series::SeriesCore {
                anilist_id,
                mal_id: None,
                title,
                title_romaji: romaji,
                title_english: "",
                title_native: "",
                cover_url: "",
                format: "OVA",
                status: "FINISHED",
                episodes: Some(1),
                season_year: Some(2016),
                end_year: None,
            },
        )
        .await
        .unwrap();
        id
    }

    const GRISAIA: &str = "[Xonline] Grisaia Phantom Trigger The Animation - 02 (BD 1920p x.264-10Bit Flac) [02964F5A].mkv";

    #[tokio::test]
    async fn assess_grab_falls_back_to_series_titles_when_no_cache_and_stamps_once() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let id = grabbed_torrents::record_grab(&db, "abcd", "[Xonline] Grisaia", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        let grab = grabbed_torrents::get_by_id(&db, id).await.unwrap().unwrap();
        let files = vec![GRISAIA.to_string()];
        let verdict = assess_grab(&db, &grab, &files).await;
        assert!(verdict.is_misgrab(), "{verdict:?}");
        assert_eq!(
            grabbed_torrents::get_verification(&db, id).await.as_deref(),
            Some("misgrab")
        );
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE message LIKE 'Misgrab detected:%'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 1);

        // A second look does not re-stamp or re-log.
        let again = assess_grab(&db, &grab, &files).await;
        assert!(again.is_misgrab());
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE message LIKE 'Misgrab detected:%'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(n, 1);

        let legit = vec![
            "[H-Enc] Kowaremono Risa The Animation 01-02/Kowaremono Risa The Animation - 01.mkv"
                .to_string(),
        ];
        let id2 =
            grabbed_torrents::record_grab(&db, "ef01", "[H-Enc] Kowaremono", sid, &[1], false)
                .await
                .unwrap()
                .unwrap();
        let grab2 = grabbed_torrents::get_by_id(&db, id2)
            .await
            .unwrap()
            .unwrap();
        let verdict = assess_grab(&db, &grab2, &legit).await;
        assert!(matches!(verdict, Verdict::Verified { .. }), "{verdict:?}");
        assert_eq!(
            grabbed_torrents::get_verification(&db, id2)
                .await
                .as_deref(),
            Some("verified")
        );
    }

    #[tokio::test]
    async fn assess_and_stamp_honors_whitelist_by_hash() {
        let db = pool().await;
        let sid = seed(
            &db,
            21521,
            "Kowaremono: Risa THE ANIMATION",
            "Kowaremono: Risa THE ANIMATION",
        )
        .await;
        let old = grabbed_torrents::record_grab(&db, "feed", "[Xonline] Grisaia", sid, &[1], false)
            .await
            .unwrap()
            .unwrap();
        grabbed_torrents::whitelist_by_hash(&db, "feed")
            .await
            .unwrap();
        let _ = old;
        // The restored torrent is a new row with the same hash.
        grabbed_torrents::mark_failed_by_hash_with_reason(&db, "feed", "misgrab")
            .await
            .unwrap();
        let fresh =
            grabbed_torrents::record_grab(&db, "feed", "[Xonline] Grisaia", sid, &[1], false)
                .await
                .unwrap()
                .unwrap();
        let grab = grabbed_torrents::get_by_id(&db, fresh)
            .await
            .unwrap()
            .unwrap();
        let aliases = AliasSet {
            own: vec!["Kowaremono: Risa THE ANIMATION".to_string()],
            siblings: Vec::new(),
            expected_season: 0,
        };
        let verdict = assess_and_stamp(&db, &grab, &[GRISAIA.to_string()], &aliases).await;
        assert!(matches!(verdict, Verdict::Verified { .. }), "{verdict:?}");
        assert_eq!(
            grabbed_torrents::get_verification(&db, fresh)
                .await
                .as_deref(),
            Some("whitelisted")
        );
    }
}
