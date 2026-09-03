//! Sibling auto-expansion for multi-series batch releases.
//!
//! Given a batch torrent's file list and its parent's AniList detail,
//! detect sibling anime entries in the pack (sequels, prequels, side
//! stories whose own episodes are present), upsert each into the
//! tracked series table, and write `grabbed_torrent_series` route rows
//! so post-processing can move each file into the correct sibling's
//! folder.
//!
//! Callers:
//! - `handlers::library::search::auto_expand_library_from_pack` — runs
//!   at grab time after qBit returns the torrent's file list.
//! - `services::post_processing` — runs at import time as a fallback
//!   when the grab-time path bailed (e.g. qBit metadata wait timed
//!   out on a slow tracker). Post-processing always has the file list
//!   because it's about to move files, so this is the safety net that
//!   guarantees siblings land correctly even when grab-time expansion
//!   failed.
//!
//! Pure async fn — takes a pre-fetched file list so handler tests can
//! exercise the sibling detection, series upsert, and route-writing
//! logic without spinning up qBittorrent.

use sqlx::SqlitePool;

use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, grabbed_torrents, series};
use crate::services::anilist::AnimeDetail;
use crate::services::source::ClassificationResult;
use crate::services::{anilist, auto_search, logger, media, metadata_sync};

/// Grab-time context threaded through the auto-expand path so each
/// detected sibling gets its own `episode_quality_tags` +
/// `episode_grab_history` rows alongside its route record. Without
/// these, the sibling's series page shows UNKNOWN with no progress bar
/// until post-processing runs and backfills the tags.
pub struct AutoExpandGrabContext {
    pub classification: ClassificationResult,
    pub release_group: String,
    pub size_bytes: i64,
}

/// Returns the number of siblings *newly added* to the library
/// (upserts that hit an existing row don't count). Route rows are
/// written regardless — a re-grab of a pack whose siblings are already
/// in the library should still get per-file routing so post-processing
/// can move files to each sibling's folder.
#[allow(clippy::too_many_arguments)]
pub async fn expand_from_files(
    db: &SqlitePool,
    filenames: &[String],
    parent_detail: &AnimeDetail,
    parent_series_id: i64,
    parent_episode_numbers: &[i32],
    grab_id: i64,
    torrent_title: &str,
    grab_ctx: &AutoExpandGrabContext,
) -> usize {
    let parent_title = if !parent_detail.title_english.is_empty() {
        parent_detail.title_english.as_str()
    } else {
        parent_detail.title_romaji.as_str()
    };

    if parent_detail.id <= 0 {
        logger::debug(
            db,
            LogCategory::Library,
            "Auto-expand: skipping sibling detection, parent has no AniList id",
            &format!(
                "parent_series_id={}, torrent='{}'",
                parent_series_id, torrent_title
            ),
        )
        .await;
        return 0;
    }

    logger::debug(
        db,
        LogCategory::Library,
        &format!(
            "Auto-expand: scanning {} file(s) for siblings of '{}'",
            filenames.len(),
            parent_title
        ),
        &format!(
            "parent_anilist_id={}, torrent='{}'",
            parent_detail.id, torrent_title
        ),
    )
    .await;

    // Depth-1 transitive relation walk: AniList's relation graph has
    // missing direct edges across split sagas (Monogatari is the
    // motivating case — Owarimonogatari 21262 does not list
    // Owarimonogatari 2nd Season 99423 as a direct neighbor, but
    // reaches it via the shared saga graph). Before running sibling
    // detection we fetch each walkable direct neighbor's AL detail,
    // then graft its OWN relations onto the parent so
    // `detect_sibling_entries_in_pack` sees a broader candidate pool.
    // Fetches are capped by `auto_search::TRANSITIVE_WALK_MAX_FETCHES`.
    // Failures are soft — any neighbor we can't fetch is silently
    // skipped and detection falls back to the parent's direct relations.
    let mut walk_ids: Vec<i64> = Vec::new();
    let mut walk_id_to_type: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    for rel in &parent_detail.relations {
        if walk_ids.len() >= auto_search::TRANSITIVE_WALK_MAX_FETCHES {
            break;
        }
        if !auto_search::is_transitive_walk_source(&rel.relation_type) {
            continue;
        }
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            continue;
        }
        if rel.id <= 0 {
            continue;
        }
        walk_ids.push(rel.id);
        walk_id_to_type.insert(rel.id, rel.relation_type.clone());
    }
    let neighbor_details: std::collections::HashMap<i64, anilist::AnimeDetail> =
        if walk_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            match anilist::get_anime_details_batch(&walk_ids).await {
                Ok(map) => map,
                Err(e) => {
                    tracing::debug!(
                        "auto-expand: transitive neighbor batch fetch failed err={}",
                        e
                    );
                    // Recover partial results from DETAIL_CACHE: chunks
                    // that completed before the failure already wrote
                    // their entries (the batch helper aborts on Err but
                    // the writes survive).
                    let mut partial = std::collections::HashMap::new();
                    for rel_id in &walk_ids {
                        if let Some(detail) = anilist::cached_anime_detail(*rel_id).await {
                            partial.insert(*rel_id, detail);
                        }
                    }
                    if !partial.is_empty() {
                        tracing::debug!(
                            "auto-expand: recovered {} partial neighbor(s) from DETAIL_CACHE",
                            partial.len()
                        );
                    }
                    partial
                }
            }
        };
    for rel_id in &walk_ids {
        if !neighbor_details.contains_key(rel_id) {
            let rel_type = walk_id_to_type.get(rel_id).cloned().unwrap_or_default();
            tracing::debug!(
                "auto-expand: transitive neighbor missing from batch rel_id={} rel_type={}",
                rel_id,
                rel_type
            );
        }
    }
    let expanded_parent =
        auto_search::expand_parent_with_transitive_relations(parent_detail, &neighbor_details);
    let siblings = auto_search::detect_sibling_entries_in_pack(filenames, &expanded_parent);

    // No early bail when `siblings.is_empty()` — the parent-file walk
    // below still needs to run so the grab row's `episode_numbers` +
    // `is_batch` get corrected from the discovered file shape. A
    // single-series BD batch (no siblings) is the common case for the
    // Houseki no Kuni / Land of the Lustrous bug: title carries no
    // batch keyword + no episode range, `record_grab` registered
    // `[1]` + `is_batch=false`, post-processing's `grab_claims_episode`
    // guard then rejects every file beyond ep 1.
    if siblings.is_empty() {
        logger::info(
            db,
            LogCategory::Library,
            &format!(
                "Auto-expand: no siblings detected in pack '{}', running parent-coverage pass",
                torrent_title
            ),
            &format!(
                "parent='{}', parent_anilist_id={}, files={}",
                parent_title,
                parent_detail.id,
                filenames.len()
            ),
        )
        .await;
    }

    let siblings_considered = siblings.len();
    let mut added = 0_usize;
    let mut claimed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut routes: Vec<grabbed_torrents::GrabSeriesRoute> = Vec::new();

    for sibling in siblings {
        let primary_title = if !sibling.title_english.is_empty() {
            sibling.title_english.clone()
        } else {
            sibling.title_romaji.clone()
        };

        // Upsert dedups by mal_id then anilist_id, so reconciled
        // entries that already have both IDs populated update in
        // place instead of duplicating.
        let upsert_result = series::upsert(
            db,
            series::SeriesCore {
                anilist_id: sibling.anilist_id,
                mal_id: sibling.mal_id,
                title: &primary_title,
                title_romaji: &sibling.title_romaji,
                title_english: &sibling.title_english,
                title_native: &sibling.title_native,
                cover_url: &sibling.cover_url,
                format: &sibling.format,
                status: &sibling.status,
                episodes: sibling.episodes,
                season_year: sibling.season_year,
                // Relation cards don't carry end_year — the background
                // metadata refresh populates it.
                end_year: None,
            },
        )
        .await;
        let (sibling_id, created) = match upsert_result {
            Ok(pair) => pair,
            Err(e) => {
                logger::warn(
                    db,
                    LogCategory::Library,
                    &format!("auto-expand: failed to upsert sibling '{}'", primary_title),
                    &e.to_string(),
                )
                .await;
                continue;
            }
        };

        if created {
            added += 1;
            logger::info(
                db,
                LogCategory::Library,
                &format!(
                    "Auto-expand: added sibling '{}' from batch '{}'",
                    primary_title, torrent_title
                ),
                &format!(
                    "anilist_id={}, matched_subtitle={:?}, files={}",
                    sibling.anilist_id,
                    sibling.matched_subtitle,
                    sibling.file_indices.len()
                ),
            )
            .await;

            // Kick off a background metadata refresh so the full
            // detail (description, artwork, end_year, etc.) gets
            // hydrated for the UI. Fire-and-forget — the route is
            // already recorded below either way.
            let db_clone = db.clone();
            tokio::spawn(async move {
                if let Ok(Some(tracked)) = series::get_by_id(&db_clone, sibling_id).await {
                    let force_fallback = config::get_config(&db_clone)
                        .await
                        .ok()
                        .flatten()
                        .map(|c| c.force_mal_fallback)
                        .unwrap_or(false);
                    let _ =
                        metadata_sync::refresh_series_metadata(&db_clone, &tracked, force_fallback)
                            .await;
                }
            });
        }

        // Derive episode numbers per sibling so find_imported_for_episode
        // can locate this route when an upgrade later targets one of
        // the sibling's episodes.
        //
        // The stored ep_nums are *effective* (post-offset) numbers so
        // an upgrade searching by episode 1 of Owari S2 finds a route
        // whose files were originally numbered E14 on disk.
        let mut ep_nums: Vec<i32> = Vec::new();
        for &file_idx in &sibling.file_indices {
            if let Some(name) = filenames.get(file_idx)
                && let Some((_, raw)) = media::parse_episode_number(&name.to_lowercase())
            {
                let effective = raw - sibling.episode_offset;
                if effective > 0 {
                    ep_nums.push(effective);
                }
            }
        }
        ep_nums.sort_unstable();
        ep_nums.dedup();

        for &i in &sibling.file_indices {
            claimed.insert(i);
        }

        // Write per-episode grab history + quality tag rows for this
        // sibling so its episode list shows `state=grabbed` in the UI
        // (progress bar + "came from X GB batch" tooltip). Every
        // auto-expand firing is a batch by definition, so
        // `is_batch=true` is always correct here.
        for &local_ep in &ep_nums {
            if let Err(e) = episode_tags::record_grab(
                db,
                sibling_id,
                local_ep,
                &grab_ctx.classification,
                torrent_title,
                &grab_ctx.release_group,
                grab_ctx.size_bytes,
                true,
            )
            .await
            {
                logger::warn(
                    db,
                    LogCategory::Library,
                    &format!(
                        "Auto-expand: failed to backfill grab history for sibling {} ep {}",
                        sibling_id, local_ep,
                    ),
                    &format!("{}: {}", torrent_title, e),
                )
                .await;
            }
        }

        routes.push(grabbed_torrents::GrabSeriesRoute {
            grab_id,
            series_id: sibling_id,
            file_indices: sibling.file_indices,
            episode_numbers: ep_nums,
            matched_subtitle: sibling.matched_subtitle,
            episode_offset: sibling.episode_offset,
        });
    }

    // Parent route: every media file not claimed by a sibling routes
    // to the parent series.
    let parent_file_indices: Vec<usize> = (0..filenames.len())
        .filter(|i| {
            filenames
                .get(*i)
                .map(|n| auto_search::is_media_filename(n))
                .unwrap_or(false)
                && !claimed.contains(i)
        })
        .collect();

    // Backfill grab-tag rows for parent files whose parsed episode
    // number exceeds what the caller's `parent_episode_numbers` covered.
    // Motivating case: the [smol] Owarimonogatari BD splits the 48-min
    // aired ep 1 into two files, so S1 has files for eps 1..=13 on
    // disk even though AL reports 12 eps. `batch_episode_numbers` only
    // produced 1..=12 at grab time; without this pass the S1 ep 13
    // row never renders in the UI until post-processing imports the
    // file. Write the overflow tag rows now so the user sees a
    // "downloading" row for ep 13 the moment the pack is queued.
    //
    // While walking parent files, also accumulate the full set of
    // episode numbers actually present so we can correct the grab
    // row's `episode_numbers` + `is_batch` once we know the true
    // file shape. Without that correction, post-processing's
    // `grab_claims_episode` guard rejects every parent file beyond
    // what the grab originally registered (BD batches with no
    // episode-range or batch keyword in the title — like
    // "[Arid] Land of the Lustrous [BDRip 1080p Hi10 FLAC]" —
    // come in as `episode_numbers=[1], is_batch=0` and post-
    // processing only imports ep 1).
    let parent_eps_covered: std::collections::HashSet<i32> =
        parent_episode_numbers.iter().copied().collect();
    let mut all_parent_eps: std::collections::BTreeSet<i32> =
        parent_eps_covered.iter().copied().collect();
    for &file_idx in &parent_file_indices {
        let Some(name) = filenames.get(file_idx) else {
            continue;
        };
        let Some((_, raw_ep)) = media::parse_episode_number(&name.to_ascii_lowercase()) else {
            continue;
        };
        if raw_ep <= 0 {
            continue;
        }
        all_parent_eps.insert(raw_ep);
        if parent_eps_covered.contains(&raw_ep) {
            continue;
        }
        if let Err(e) = episode_tags::record_grab(
            db,
            parent_series_id,
            raw_ep,
            &grab_ctx.classification,
            torrent_title,
            &grab_ctx.release_group,
            grab_ctx.size_bytes,
            true,
        )
        .await
        {
            logger::warn(
                db,
                LogCategory::Library,
                &format!(
                    "Auto-expand: failed to backfill grab history for parent {} ep {} (AL-overflow)",
                    parent_series_id, raw_ep,
                ),
                &format!("{}: {}", torrent_title, e),
            )
            .await;
        }
    }

    // Correct the grab row's `episode_numbers` + `is_batch` if the
    // discovered file set goes beyond what `record_grab` originally
    // wrote. Drives post-processing's `grab_claims_episode` guard so
    // every parent file imports correctly even when title-based batch
    // detection missed (no `[BD]` / `Batch` / `Complete` / episode-
    // range token, common with BD release groups like Arid / Legion).
    //
    // Filter on `> 1` so a single-episode grab whose file count is 1
    // doesn't get falsely flagged as `is_batch=true`.
    let parent_ep_vec: Vec<i32> = all_parent_eps.iter().copied().collect();
    if parent_ep_vec.len() > 1
        && let Err(e) =
            grabbed_torrents::update_episode_coverage(db, grab_id, &parent_ep_vec, true).await
    {
        logger::warn(
            db,
            LogCategory::Library,
            &format!(
                "Auto-expand: failed to update grab {} episode coverage (parent {})",
                grab_id, parent_series_id,
            ),
            &format!("{}: {}", torrent_title, e),
        )
        .await;
    }

    if !routes.is_empty() && !parent_file_indices.is_empty() {
        logger::warn(
            db,
            LogCategory::Library,
            &format!(
                "Auto-expand: {} unclaimed file(s) in batch '{}' routed to parent series",
                parent_file_indices.len(),
                torrent_title,
            ),
            &format!(
                "parent_id={}, siblings_added={}, unclaimed_count={}",
                parent_series_id,
                added,
                parent_file_indices.len()
            ),
        )
        .await;

        routes.push(grabbed_torrents::GrabSeriesRoute {
            grab_id,
            series_id: parent_series_id,
            file_indices: parent_file_indices,
            episode_numbers: parent_episode_numbers.to_vec(),
            matched_subtitle: String::new(),
            // Parent-route files always use their own arc-local
            // numbering — no offset ever needed here.
            episode_offset: 0,
        });
    }

    if !routes.is_empty()
        && let Err(e) = grabbed_torrents::record_grab_series_routes(db, &routes).await
    {
        logger::warn(
            db,
            LogCategory::Library,
            &format!(
                "auto-expand: failed to write route rows for '{}'",
                torrent_title
            ),
            &e.to_string(),
        )
        .await;
    }

    logger::info(
        db,
        LogCategory::Library,
        &format!(
            "Auto-expand: finished batch '{}' — {} sibling(s) added",
            torrent_title, added
        ),
        &format!(
            "parent='{}', siblings_considered={}, routes_written={}",
            parent_title,
            siblings_considered,
            routes.len()
        ),
    )
    .await;

    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anilist::RelatedEntry;

    fn empty_anime_detail(id: i64, title_english: &str, episodes: Option<i32>) -> AnimeDetail {
        AnimeDetail {
            is_adult: false,
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            duration: Some(24),
            season: String::new(),
            season_year: Some(2014),
            end_year: Some(2014),
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn related_entry(id: i64, title_english: &str, episodes: Option<i32>) -> RelatedEntry {
        RelatedEntry {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            relation_type: "SIDE_STORY".to_string(),
            season_year: Some(2014),
            media_type: "ANIME".to_string(),
        }
    }

    /// Issue #45 follow-up: simulate the post-processing retry path.
    /// Grab-time auto-expand failed (metadata wait timed out on a slow
    /// tracker), so there's a parent series row + a grab row but no
    /// route rows. post_processing::import_torrent calls
    /// `expand_from_files` at import time with the file list qBit
    /// finally returned. Verify that:
    ///   1. the sibling (Egypt-hen) gets auto-added,
    ///   2. sibling + parent routes get written,
    ///   3. sibling route has offset=24 mapping E25..=E48 to local 1..=24.
    ///
    /// This is the path that would have rescued the
    /// HorribleSubs JoJo P3 48-ep grab the user reported.
    #[tokio::test]
    async fn expand_from_files_rescues_grab_when_called_at_post_processing_time() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 20899,
                mal_id: None,
                title: "JoJo's Bizarre Adventure: Stardust Crusaders",
                title_romaji: "JoJo no Kimyou na Bouken: Stardust Crusaders",
                title_english: "JoJo's Bizarre Adventure: Stardust Crusaders",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(24),
                season_year: Some(2014),
                end_year: Some(2014),
            },
        )
        .await
        .expect("parent upsert");

        // A grab row exists (the user did grab the pack) but no routes
        // were written because grab-time auto-expand timed out.
        let grab_id = grabbed_torrents::record_grab(
            &db,
            "slowtrackerhash000000000000000000000000000",
            "[HorribleSubs] JoJo's Bizarre Adventure - Stardust Crusaders (1-48) [720p] (Unofficial Batch)",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        let routes_before = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert!(
            routes_before.is_empty(),
            "precondition: grab-time auto-expand failed, no routes yet"
        );

        let mut parent_detail = empty_anime_detail(
            20899,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            Some(24),
        );
        parent_detail.relations.push(related_entry(
            22663,
            "JoJo's Bizarre Adventure: Stardust Crusaders - Egypt-hen",
            Some(24),
        ));

        let filenames: Vec<String> = (1..=48)
            .map(|n| format!("[HorribleSubs] JoJo Stardust - {:02} [720p].mkv", n))
            .collect();

        let ctx = AutoExpandGrabContext {
            classification: ClassificationResult::unknown(),
            release_group: String::new(),
            size_bytes: 0,
        };

        let added = expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &(1..=24).collect::<Vec<_>>(),
            grab_id,
            "[HorribleSubs] JoJo P3 (1-48)",
            &ctx,
        )
        .await;

        assert_eq!(added, 1, "Egypt-hen should be auto-added at retry time");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling + parent route written");

        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        assert_eq!(
            sibling_route.episode_offset, 24,
            "absolute numbering → offset = parent_cap = 24"
        );
        assert_eq!(
            sibling_route.episode_numbers,
            (1..=24).collect::<Vec<_>>(),
            "sibling eps 25..=48 map to local 1..=24"
        );

        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        assert_eq!(parent_route.file_indices, (0..=23).collect::<Vec<_>>());
        assert_eq!(parent_route.episode_offset, 0);
    }

    /// Issue #45 follow-up: auto-expand must backfill a grab-tag row
    /// for parent-side episodes whose parsed number exceeds the
    /// caller's `parent_episode_numbers` set (AL-overflow case). Without
    /// this, the [smol] Owari BD's E13 never renders in the UI until
    /// post-processing imports it — the user sees 12 rows during the
    /// download with no indication that a 13th is coming.
    #[tokio::test]
    async fn expand_writes_grab_tag_for_parent_al_overflow_episodes() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21262,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "owarialoverflowbackfill00000000000000000000",
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        let mut parent_detail = empty_anime_detail(21262, "Owarimonogatari", Some(12));
        parent_detail.relations.push(related_entry(
            21745,
            "Owarimonogatari Second Season",
            Some(7),
        ));

        // 13 parent files + 7 sibling files. Parent caller passes
        // 1..=12 (AL's count), so E13 is the overflow case we want to
        // backfill.
        let mut filenames: Vec<String> = Vec::new();
        for n in 1..=13 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
                n
            ));
        }
        for n in 14..=20 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
                n
            ));
        }
        let parent_episode_numbers: Vec<i32> = (1..=12).collect();

        let ctx = AutoExpandGrabContext {
            classification: ClassificationResult::unknown(),
            release_group: String::new(),
            size_bytes: 0,
        };

        expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &parent_episode_numbers,
            grab_id,
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            &ctx,
        )
        .await;

        // The parent should now have a quality_tag row for E13 even
        // though it wasn't in parent_episode_numbers. The caller's
        // grab handler writes 1..=12; expand_from_files fills the gap.
        let tags = episode_tags::get_for_series(&db, parent_id)
            .await
            .expect("get_for_series");
        assert!(
            tags.contains_key(&13),
            "auto-expand must backfill a grab tag for AL-overflow ep 13 so the UI renders it during the download; got tags for {:?}",
            tags.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            tags.get(&13).unwrap().state,
            "grabbed",
            "ep 13 tag should start in 'grabbed' state (post-processing will flip to 'completed')"
        );
    }

    /// Regression for the Land of the Lustrous (Houseki no Kuni) case:
    /// Seerr-driven add → auto_search picked the [Arid] BD batch
    /// release. The release title carries no episode-range token and
    /// no "[BD]"/"Batch"/"Complete" keyword, so:
    ///   - `parse_release_numbers` returns empty,
    ///   - `batch_episode_numbers` falls back to `[1]`,
    ///   - `detect_batch_from_title` returns false.
    ///
    /// The grab row lands as `episode_numbers=[1], is_batch=0`, and
    /// post-processing's `grab_claims_episode` guard rejects every
    /// file beyond ep 1 — eps 2..=12 land on disk via the auto-expand
    /// overflow path but their history rows stay stuck at `grabbed`
    /// because post-processing never imports their files.
    ///
    /// `expand_from_files` should observe the actual file shape
    /// (12 episode files for the parent series) and overwrite the
    /// grab row's `episode_numbers` + `is_batch` so the post-
    /// processing guard sees the full picture when the torrent lands.
    #[tokio::test]
    async fn expand_corrects_grab_row_when_title_misses_batch_indicators() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 99041,
                mal_id: None,
                title: "Land of the Lustrous",
                title_romaji: "Houseki no Kuni",
                title_english: "Land of the Lustrous",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2017),
                end_year: Some(2017),
            },
        )
        .await
        .expect("parent upsert");

        // record_grab simulates the title-only registration: episode_numbers=[1]
        // (the auto_search target ep) and is_batch=false (title-detect missed).
        let torrent_title =
            "[Arid] Land of the Lustrous [Dual-Audio][BDRip 1080p Hi10 FLAC] | Houseki no Kuni";
        let grab_id = grabbed_torrents::record_grab(
            &db,
            "9a73f66894222116fd1f306331bfa3c8c4834af0",
            torrent_title,
            parent_id,
            &[1],
            false,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        let parent_detail = empty_anime_detail(99041, "Land of the Lustrous", Some(12));
        let filenames: Vec<String> = (1..=12)
            .map(|n| {
                format!(
                    "[Arid] Land of the Lustrous - {:02} (BD 1080p Hi10 FLAC).mkv",
                    n
                )
            })
            .collect();
        let parent_episode_numbers: Vec<i32> = vec![1]; // mirrors batch_episode_numbers' fallback

        let ctx = AutoExpandGrabContext {
            classification: ClassificationResult::unknown(),
            release_group: String::new(),
            size_bytes: 0,
        };

        expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &parent_episode_numbers,
            grab_id,
            torrent_title,
            &ctx,
        )
        .await;

        // The grab row's episode_numbers should now cover 1..=12 (the
        // discovered set) and is_batch should be true.
        let row: (String, i64) =
            sqlx::query_as("SELECT episode_numbers, is_batch FROM grabbed_torrents WHERE id = ?")
                .bind(grab_id)
                .fetch_one(&db)
                .await
                .expect("fetch grab row");
        let eps: Vec<i32> =
            serde_json::from_str(&row.0).expect("episode_numbers parses as JSON array");
        assert_eq!(
            eps,
            (1..=12).collect::<Vec<i32>>(),
            "grab row should be corrected to cover all 12 discovered episodes"
        );
        assert_eq!(row.1, 1, "grab row should be flipped to is_batch=true");
    }
}
