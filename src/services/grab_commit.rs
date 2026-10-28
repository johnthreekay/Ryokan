//! Shared grab-commit helper for issue #83's interactive file-picker.
//!
//! Two paths end up with a live, file-priority-applied, resumed torrent
//! that Ryokan wants to attribute to the library:
//!
//!   1. **User-confirmed** — `handlers::grab::grab_confirm` applies the
//!      user's selections and resumes the torrent. Filenames here are
//!      the user-kept subset (decision #7).
//!   2. **Walkaway auto-commit** — `services::grab_sweep::auto_commit_row`
//!      marks every file wanted on a heartbeat-lapsed row (decision #3).
//!      Filenames here are the full file list.
//!
//! Both paths need the same downstream work: write a `grabbed_torrents`
//! row so post-processing picks up the download, and run sibling
//! auto-expand so a batch pack's sequels/prequels/side-stories get
//! their own library rows and per-file routing.
//!
//! The auto-search path in `handlers::library::search` already does this
//! with a pre-computed classification from the scoring pipeline. The
//! interactive path doesn't — the modal doesn't run source
//! classification. We fall back to `ClassificationResult::unknown()`
//! and let post-processing backfill the real `(source, resolution,
//! is_remux)` verdict once files land on disk. `needs_review` flips
//! true on the unknown row so the classifier review page surfaces it
//! if post-processing can't confidently classify it either.

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::grabbed_torrents;
use crate::models::log::LogCategory;
use crate::models::pending_grabs::PendingGrab;
use crate::models::series::Series;
use crate::services::anilist;
use crate::services::auto_expand::{self, AutoExpandGrabContext};
use crate::services::auto_search;
use crate::services::logger;
use crate::services::notifications;
use crate::services::release_title::{self, SeriesAnchors};
use crate::services::source::ClassificationResult;
use crate::services::{media, post_processing};

/// Write the `grabbed_torrents` row and kick off sibling auto-expand
/// for a pending grab that's now live on the download client. Returns
/// `Some(grab_id)` on success, `None` when library attribution was
/// skipped (no series context, empty title, DB insert deduped against
/// an in-flight row).
///
/// `filenames` is the file list to feed into auto-expand — the
/// user-selected subset on the confirm path, the full list on the
/// auto-commit path. Auto-expand writes `grabbed_torrent_series`
/// routes only for files present in the list, so a subset here
/// correctly limits sibling library attribution to what's actually
/// going to import (decision #7's post-confirm timing shift).
///
/// Auto-expand fires as a `tokio::spawn` — the caller returns to the
/// HTTP response before the metadata-bound relation walk completes.
/// Post-processing has its own auto-expand safety net so a failure
/// here doesn't prevent eventual library attribution.
pub async fn commit_grab_and_expand(
    state: &AppState,
    row: &PendingGrab,
    filenames: Vec<String>,
    release_title: &str,
    is_batch: bool,
) -> Option<i64> {
    // Bare-magnet grab from the global search page with no series
    // context — we can't attribute the download to a library row, so
    // skip the write. The torrent still downloads (the caller already
    // issued the resume); it just won't show up in any series's
    // grabbed list. Post-processing will ignore it for the same
    // reason — no `grabbed_torrents` row to key off.
    let Some(series_id) = row.series_id else {
        logger::debug(
            &state.db,
            LogCategory::Grab,
            "skipping grab-row write — no series_id on pending grab",
            &row.info_hash,
        )
        .await;
        return None;
    };

    if release_title.trim().is_empty() {
        logger::warn(
            &state.db,
            LogCategory::Grab,
            "skipping grab-row write — release title empty",
            &row.info_hash,
        )
        .await;
        return None;
    }

    // The series' absolute-numbering offset, so a pack whose files are
    // numbered `25..48` for the second season records episodes 1..24
    // the way the Wanted page asked for them and the import files them.
    let series = GrabSeries::load(&state.db, series_id).await;
    let ep_nums = episode_numbers_for_commit(release_title, &filenames, &series);

    let grab_id = match grabbed_torrents::record_grab(
        &state.db,
        &row.info_hash,
        release_title,
        series_id,
        &ep_nums,
        is_batch,
    )
    .await
    {
        Ok(Some(id)) => {
            // Stamp the dispatch client id captured at preview time so
            // post-processing's `resolve_grab_client` routes the import /
            // delete back through the same client even if defaults change.
            // Without this, picker-confirm and walkaway-auto-commit grabs
            // land NULL-stamped: SAB rows are still rescued by the
            // `SABnzbd_nzo_` hash heuristic, but a BT grab routed to a
            // non-default client (e.g. indexer pinned to seedbox-qBit
            // while local-qBit is the default) silently falls through to
            // the torrent default at import time.
            if let Err(e) =
                grabbed_torrents::set_download_client(&state.db, id, row.download_client_id).await
            {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    "set_download_client failed after record_grab",
                    &format!("{} ({})", e, row.info_hash),
                )
                .await;
            }
            id
        }
        Ok(None) => {
            // Dedup hit against an in-flight `pending` row — another
            // flow is mid-commit on this hash. Don't stomp it; the
            // other flow will drive auto-expand. This matches the
            // auto_search path's `grab_id.flatten()` behavior.
            logger::debug(
                &state.db,
                LogCategory::Grab,
                "grab dedup hit — skipping auto-expand",
                &row.info_hash,
            )
            .await;
            return None;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::Grab,
                "record_grab failed",
                &format!("{} ({})", e, row.info_hash),
            )
            .await;
            return None;
        }
    };

    // The grab's own episodes read as downloading from here on. The
    // direct grab endpoints write these rows themselves; this path
    // (picker confirm, walkaway auto-commit) used to leave the parent
    // untagged, so the Wanted page kept listing a picked episode as
    // missing until the import landed. Auto-expand only backfills what
    // the grab did not already claim, so nothing is written twice.
    let classification = crate::services::source::classify_release_sync(release_title, None);
    let release_group = release_group_from_metadata(&row.release_metadata_json);
    let tags = crate::models::episode_tags::get_for_series(&state.db, series_id)
        .await
        .unwrap_or_default();
    for ep in &ep_nums {
        // An episode with a file on disk keeps its `completed` row. The
        // upsert would replace the file's classification with the
        // pack's, and a cancelled download would then delete the row
        // (`clear_tags_for_removal`) with the file still there. The
        // walkaway auto-commit takes a whole pack, episodes on disk
        // included; a hand-picked upgrade reads as downloading only
        // once the import has replaced the file.
        if tags.get(ep).is_some_and(|t| t.state == "completed") {
            continue;
        }
        if let Err(e) = crate::models::episode_tags::record_grab(
            &state.db,
            series_id,
            *ep,
            &classification,
            release_title,
            &release_group,
            0,
            is_batch,
        )
        .await
        {
            logger::warn(
                &state.db,
                LogCategory::Grab,
                &format!("failed to record grabbed tag for episode {ep}"),
                &format!("{} ({})", e, row.info_hash),
            )
            .await;
        }
    }

    // Issue #118 — fire `NotificationEvent::Grabbed` for this commit.
    // No-op early-return when no providers are configured (the
    // foundation PR ships an always-empty cache); subsequent provider
    // PRs flip this from a tree-fall into a real outbound dispatch
    // without further changes here. Indexer name + score aren't
    // resolvable from the picker/walkaway path's `PendingGrab` row
    // (the modal doesn't carry the auto_search scoring context);
    // both default to None. Episode number is the lowest in the
    // parsed range — single-episode grabs use it directly, batches
    // pick the first.
    notifications::emit_grabbed(
        state,
        series_id,
        ep_nums.first().copied().unwrap_or(0),
        release_title,
        None,
        None,
        Some(row.client_kind.clone()),
    )
    .await;

    // Fire-and-forget the sibling auto-expand. Don't block the HTTP
    // response — the transitive relation walk can take several
    // seconds on cold DETAIL_CACHE, and confirm / auto-commit both
    // want to return as soon as the client-side work is done.
    //
    // A failed fetch or panic inside the spawn drops the auto-expand
    // silently; the import-time call in `services::post_processing`
    // is the safety net that guarantees siblings land eventually.
    if !filenames.is_empty() {
        let db_task = state.db.clone();
        let title_task = release_title.to_string();
        let ep_nums_task = ep_nums.clone();
        let info_hash_task = row.info_hash.clone();
        tokio::spawn(async move {
            run_auto_expand(
                db_task,
                info_hash_task,
                series_id,
                ep_nums_task,
                grab_id,
                title_task,
                filenames,
            )
            .await;
        });
    }

    Some(grab_id)
}

/// The episodes a confirmed grab is for: the selected files' own
/// numbers when any of them parse (a picker grab that kept two files
/// of a twelve-episode batch is a grab for those two, so the Wanted
/// page and the series page show only those as downloading and the
/// import claims only those), else the episodes the release title
/// names for the series ([`relative_title_episodes`]: a single, a
/// range, an absolute-numbered range); an unparseable title yields an
/// empty list, which post-processing tolerates.
///
/// A special (`OVA 01`) is no episode slot, and a pick made only of
/// specials is a grab for no episode at all: the title's range must
/// not stand in for it, or one OVA would tag a whole pack as
/// downloading. Numbers are series-relative: `cumulative_prior_episodes`
/// is taken off an absolute-numbered file the way the import does it
/// (`fallback_ep_offset`), so the row's list matches what the Wanted
/// page asked for and the tag on the imported slot is the one promoted.
pub(crate) fn episode_numbers_for_commit(
    release_title: &str,
    filenames: &[String],
    series: &GrabSeries,
) -> Vec<i32> {
    let cumulative_prior_episodes = series.cumulative_prior_episodes;
    let mut any_parsed = false;
    let mut eps: Vec<i32> = Vec::new();
    for name in filenames
        .iter()
        .filter(|n| auto_search::is_media_filename(n))
    {
        let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
        // A file whose number the series' name holds (`Mob Psycho 100
        // (BD).mkv`) read as no episode, which is still a reading: the
        // title's range must not stand in for it.
        if media::parse_episode_span(&base).is_some() {
            any_parsed = true;
        }
        let Some(span) = media::parse_episode_span_for(&base, &series.anchors) else {
            continue;
        };
        if span.special {
            continue;
        }
        eps.extend(file_slot_episodes(span, cumulative_prior_episodes));
    }
    if eps.is_empty() && !any_parsed {
        eps = relative_title_episodes(release_title, series, None);
    }
    eps.sort_unstable();
    eps.dedup();
    eps
}

/// The series a grab records its episode list for: its names (so a bare
/// number right after one is read, a number inside one never is), its
/// format, its absolute-numbering offset and its episode count.
#[derive(Debug, Clone, Default)]
pub(crate) struct GrabSeries {
    anchors: SeriesAnchors,
    cumulative_prior_episodes: i32,
    episode_count: Option<i32>,
}

/// A library series' names and format as `release_title` reads them, for
/// reading its releases and its files.
pub(crate) fn series_anchors(row: &Series) -> SeriesAnchors {
    let mut names = vec![
        row.title.clone(),
        row.title_romaji.clone(),
        row.title_english.clone(),
        row.title_native.clone(),
    ];
    names.extend(row.alternate_title_list());
    names.retain(|n| !n.trim().is_empty());
    SeriesAnchors::new(&names).with_specials_as_episodes(!media::is_tv_format(&row.format))
}

impl GrabSeries {
    pub(crate) fn from_row(row: &Series) -> Self {
        Self {
            anchors: series_anchors(row),
            cumulative_prior_episodes: row.cumulative_prior_episodes,
            episode_count: row.episodes,
        }
    }

    /// The AniList detail's episode count, when it has one: the row's
    /// count can lag a metadata refresh.
    pub(crate) fn with_episode_count(mut self, count: Option<i32>) -> Self {
        if count.is_some_and(|n| n > 0) {
            self.episode_count = count;
        }
        self
    }

    /// The library row's reading, or none of it (no names, no offset,
    /// no count) when the row cannot be read.
    pub(crate) async fn load(db: &SqlitePool, series_id: i64) -> Self {
        crate::models::series::get_by_id(db, series_id)
            .await
            .ok()
            .flatten()
            .map(|row| Self::from_row(&row))
            .unwrap_or_default()
    }
}

/// What a grab is for, in [`grab_episode_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrabFor {
    /// One episode was searched for or clicked.
    Episode(i32),
    /// A search for the series as one release (`SearchTarget::Single`: a
    /// movie, an OVA entry). A non-batch release that names nothing is
    /// its episode 1, the slot the import gives a lone unnumbered file.
    Single,
    /// A batch picked from a list, or a release linked to the series
    /// after the fact.
    Whole,
}

/// Most episodes a pack that names none records from the series'
/// count: a garbage AniList count must not write a million tag rows.
const WHOLE_SERIES_CAP: i32 = 1000;

/// The series-relative episodes `title` names for `series`. A tag
/// (`S02E05`) is relative already. Otherwise a run past
/// `cumulative_prior_episodes` is absolute, judged on its first number
/// the way the import judges a file (`fallback_ep_offset`), unless the
/// episode that was searched for (`searched`) shows which numbering
/// the release used.
pub(crate) fn relative_title_episodes(
    title: &str,
    series: &GrabSeries,
    searched: Option<i32>,
) -> Vec<i32> {
    relative_episodes(&release_title::parse(title), series, searched)
}

/// [`relative_title_episodes`] over an already parsed title. A real tag
/// (`S02E05`) is relative, as the import reads it; so is a season-bound
/// run (`S2 - 05`) that fits the series' episode count. Otherwise a run
/// past `cumulative_prior_episodes` is absolute, judged on its first
/// number (`fallback_ep_offset`), unless `searched` shows the numbering.
fn relative_episodes(
    parsed: &release_title::ParsedTitle,
    series: &GrabSeries,
    searched: Option<i32>,
) -> Vec<i32> {
    let mut raw: Vec<i32> = parsed
        .for_series(&series.anchors)
        .episodes
        .into_iter()
        .collect();
    raw.sort_unstable();
    let (Some(&first), Some(&last)) = (raw.first(), raw.last()) else {
        return raw;
    };
    let cumulative = series.cumulative_prior_episodes;
    let claim = parsed.episodes.as_ref();
    let tagged =
        claim.is_some_and(|c| c.rule == release_title::ClaimRule::Tag && c.season.is_some());
    let bound_and_fits = claim.is_some_and(|c| c.season.is_some())
        && series.episode_count.is_some_and(|n| last <= n);
    let offset = match searched {
        _ if tagged || bound_and_fits => 0,
        Some(ep) if raw.contains(&ep) => 0,
        Some(ep) if cumulative > 0 && raw.contains(&(ep + cumulative)) => cumulative,
        _ => post_processing::fallback_ep_offset(first, cumulative),
    };
    raw.into_iter()
        .map(|n| n - offset)
        .filter(|n| *n > 0)
        .collect()
}

/// The episodes a grab records (the grab row's list and its `grabbed`
/// tags), series-relative so a sequel's pack `25-48` is its episodes
/// 1-24. One episode searched for or clicked: a single release holds
/// the run its title names around it (`- 05-06`, one file with both,
/// #246), else that episode alone. A pack: the episodes its title
/// names, else every episode of the series (up to
/// [`WHOLE_SERIES_CAP`]), unless the title is a special, an extra or a
/// half episode (an OVA pack of a TV series holds no regular episode). A
/// [`GrabFor::Single`] release that names nothing is episode 1.
///
/// For a batch the searched episode does not pick the numbering: the
/// import's pack preflight reads every file without it.
pub(crate) fn grab_episode_list(
    title: &str,
    series: &GrabSeries,
    target: GrabFor,
    is_batch: bool,
) -> Vec<i32> {
    let parsed = release_title::parse(title);
    let searched = match target {
        GrabFor::Episode(ep) if !is_batch => Some(ep),
        _ => None,
    };
    let relative = relative_episodes(&parsed, series, searched);
    let names_no_slot = parsed.extra
        || parsed.half_episode
        || (parsed.is_special_for(&series.anchors) && !series.anchors.specials_are_episodes());
    match target {
        GrabFor::Episode(ep) if !is_batch => {
            if relative.contains(&ep) && relative.len() <= media::MAX_FILE_SPAN as usize {
                relative
            } else {
                vec![ep]
            }
        }
        _ if !relative.is_empty() => relative,
        _ if names_no_slot => Vec::new(),
        _ if is_batch && series.episode_count.is_some_and(|n| n > 0) => {
            (1..=series.episode_count.unwrap_or(0).min(WHOLE_SERIES_CAP)).collect()
        }
        GrabFor::Episode(ep) => vec![ep],
        GrabFor::Single if !is_batch => vec![1],
        GrabFor::Whole if series.episode_count == Some(1) => vec![1],
        GrabFor::Single | GrabFor::Whole => Vec::new(),
    }
}

/// The library slots a file's parsed span lands in: the import's own
/// rule (`resolve_episode`), a `SxxExx` name is already relative and a
/// bare number past `cumulative_prior_episodes` is absolute.
pub(crate) fn file_slot_episodes(
    span: crate::services::media::EpisodeSpan,
    cumulative_prior_episodes: i32,
) -> impl Iterator<Item = i32> {
    let offset = if span.season.is_some() {
        0
    } else {
        crate::services::post_processing::fallback_ep_offset(span.first, cumulative_prior_episodes)
    };
    (span.first - offset..=span.last - offset).filter(|e| *e > 0)
}

/// The `group` the picker stored in the pending row's release
/// metadata, empty when absent.
fn release_group_from_metadata(release_metadata_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(release_metadata_json)
        .ok()
        .and_then(|v| v.get("group").and_then(|g| g.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// Resolve the parent series's AL detail and invoke `expand_from_files`.
/// Broken out of the spawn so the `?`-style control flow reads cleanly
/// without making every step inside the spawn an `if let Some` ladder.
async fn run_auto_expand(
    db: SqlitePool,
    info_hash: String,
    series_id: i64,
    ep_nums: Vec<i32>,
    grab_id: i64,
    title: String,
    filenames: Vec<String>,
) {
    // Look up anilist_id from series_id. Negative AL IDs (MAL-fallback
    // sentinel per CLAUDE.md) route to Jikan inside
    // `get_anime_detail_with_options`, which correctly returns an
    // AnimeDetail for display purposes but carries the negated id —
    // sibling detection still runs against that shape since
    // expand_from_files already filters `parent_detail.id <= 0`
    // internally.
    let anilist_id =
        match sqlx::query_scalar::<_, i64>("SELECT anilist_id FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_optional(&db)
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                logger::warn(
                    &db,
                    LogCategory::Grab,
                    "auto-expand: series row vanished between grab-row write and detail fetch",
                    &format!("series_id={} hash={}", series_id, info_hash),
                )
                .await;
                return;
            }
            Err(e) => {
                logger::warn(
                    &db,
                    LogCategory::Grab,
                    "auto-expand: DB error resolving series_id",
                    &format!("{} ({})", e, info_hash),
                )
                .await;
                return;
            }
        };

    let detail = match anilist::get_anime_detail_with_options(anilist_id, None, false).await {
        Ok(d) => d,
        Err(e) => {
            logger::info(
                &db,
                LogCategory::Grab,
                "auto-expand: AL detail fetch failed; post-processing will retry at import time",
                &format!("{} ({})", e, info_hash),
            )
            .await;
            return;
        }
    };

    let ctx = AutoExpandGrabContext {
        classification: ClassificationResult::unknown(),
        release_group: String::new(),
        size_bytes: 0,
    };
    // `expand_from_files` returns `newly_added_siblings: usize` and
    // handles its own per-sibling error logging; we only surface the
    // count here so a zero-sibling detection run is visible in the
    // logs alongside a match-heavy one. Matches the style of the
    // two DB fetches above which log their outcomes explicitly.
    let newly_added = auto_expand::expand_from_files(
        &db, &filenames, &detail, series_id, &ep_nums, grab_id, &title, &ctx,
    )
    .await;
    if newly_added > 0 {
        logger::info(
            &db,
            LogCategory::Grab,
            &format!("grab auto-expand added {newly_added} sibling series from '{title}'"),
            &info_hash,
        )
        .await;
    } else {
        // Emit a debug tombstone for the zero-sibling run too — makes
        // "did auto-expand even fire on this grab?" answerable from
        // logs without re-deriving from the absence of an info line.
        logger::debug(
            &db,
            LogCategory::Grab,
            &format!("grab auto-expand detected no siblings in '{title}'"),
            &info_hash,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A series known only by its absolute-numbering offset.
    fn offset(cumulative_prior_episodes: i32) -> GrabSeries {
        GrabSeries {
            cumulative_prior_episodes,
            ..GrabSeries::default()
        }
    }
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

    fn pending_grab_for(series_id: Option<i64>, info_hash: &str) -> PendingGrab {
        PendingGrab {
            preview_id: "pv-1".to_string(),
            wanted_indices_json: String::new(),
            info_hash: info_hash.to_string(),
            client_kind: "qbittorrent".to_string(),
            indexer_id: None,
            series_id,
            created_at: 0,
            heartbeat_at: 0,
            file_list_json: String::new(),
            release_metadata_json: String::new(),
            error_message: String::new(),
            we_added_torrent: true,
            download_client_id: None,
        }
    }

    #[tokio::test]
    async fn commit_returns_none_when_pending_grab_has_no_series_id() {
        // Bare-magnet grab from the global search page (no series
        // attribution). The handler still issues the resume — this
        // helper just bails on the library-attribution write.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(None, "deadbeef");
        let result = commit_grab_and_expand(&state, &row, vec![], "[Group] Show 01", false).await;
        assert!(result.is_none());
        // Nothing was written.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn commit_returns_none_when_release_title_is_empty() {
        // Defensive — the handler shouldn't be calling with an empty
        // title, but if it does we don't want to write a grab row
        // with `release_title = ''` (post-processing keys naming
        // off this column).
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 1, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "deadbeef");
        let result = commit_grab_and_expand(&state, &row, vec!["a.mkv".into()], "  ", false).await;
        assert!(result.is_none());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn commit_writes_grab_row_with_parsed_episode_numbers() {
        // Happy path: series attribution + a parseable single-episode
        // title → record_grab returns Some(id), the row lands with
        // episode_numbers=[1]. We pass empty `filenames` so the
        // fire-and-forget auto_expand spawn no-ops (the
        // `if !filenames.is_empty()` guard skips it), keeping the
        // test deterministic.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 100, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "abcdef0001");
        let id = commit_grab_and_expand(
            &state,
            &row,
            vec![], // empty so auto_expand spawn doesn't fire
            "[GroupX] Show - 01 [1080p].mkv",
            false,
        )
        .await
        .expect("commit should succeed and return a grab id");

        let (got_hash, got_series, got_eps): (String, i64, String) = sqlx::query_as(
            "SELECT hash, series_id, episode_numbers FROM grabbed_torrents WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(got_hash, "abcdef0001");
        assert_eq!(got_series, series_id);
        assert_eq!(got_eps, "[1]", "parsed episode numbers should round-trip");

        // The grab's episode reads as downloading from now on.
        let state_row: Option<String> = sqlx::query_scalar(
            "SELECT state FROM episode_quality_tags WHERE series_id = ? AND episode_number = 1",
        )
        .bind(series_id)
        .fetch_optional(&db)
        .await
        .unwrap();
        assert_eq!(state_row.as_deref(), Some("grabbed"));
    }

    #[test]
    fn commit_episodes_come_from_the_selected_files_when_they_parse() {
        let title = "[Group] Show - 01-12 (BD 1080p) [Batch]";
        let picked = vec![
            "Show/[Group] Show - 02 (BD 1080p).mkv".to_string(),
            "Show/[Group] Show - 03 (BD 1080p).mkv".to_string(),
            "Show/Extras/[Group] Show - NCOP1.mkv".to_string(),
            "Show/readme.txt".to_string(),
        ];
        assert_eq!(
            episode_numbers_for_commit(title, &picked, &offset(0)),
            vec![2, 3]
        );
        // No selected file parses: the title's range stands.
        let unnumbered = vec!["Show/movie.mkv".to_string()];
        assert_eq!(
            episode_numbers_for_commit(title, &unnumbered, &offset(0)),
            (1..=12).collect::<Vec<i32>>()
        );
        // A single-file release: file and title agree.
        assert_eq!(
            episode_numbers_for_commit(
                "[Group] Show - 05 (1080p).mkv",
                &["[Group] Show - 05 (1080p).mkv".to_string()],
                &offset(0)
            ),
            vec![5]
        );
    }

    #[test]
    fn a_pick_of_specials_only_is_a_grab_for_no_episode() {
        // One OVA out of a twelve-episode pack used to fall through to
        // the title's range and tag all twelve as downloading.
        let title = "[Group] Show - 01-12 (BD 1080p) [Batch]";
        let ova = vec!["Show/[Group] Show - OVA 01 (BD 1080p).mkv".to_string()];
        assert!(episode_numbers_for_commit(title, &ova, &offset(0)).is_empty());
        // A special beside an episode: only the episode counts.
        let mixed = vec![
            "Show/[Group] Show - OVA 01 (BD 1080p).mkv".to_string(),
            "Show/[Group] Show - 04 (BD 1080p).mkv".to_string(),
        ];
        assert_eq!(
            episode_numbers_for_commit(title, &mixed, &offset(0)),
            vec![4]
        );
    }

    #[test]
    fn commit_episodes_are_series_relative_for_absolute_numbered_files() {
        // Season 2 (24 prior episodes) shipped as `25..48`: the row
        // records what the Wanted page asked for and the import files.
        let title = "[Group] Show - 25-48 (BD 1080p) [Batch]";
        let picked = vec![
            "Show/[Group] Show - 25 (BD 1080p).mkv".to_string(),
            "Show/[Group] Show - 27 (BD 1080p).mkv".to_string(),
        ];
        assert_eq!(
            episode_numbers_for_commit(title, &picked, &offset(24)),
            vec![1, 3]
        );
        // The title fallback takes the same offset.
        let unnumbered = vec!["Show/movie.mkv".to_string()];
        assert_eq!(
            episode_numbers_for_commit(title, &unnumbered, &offset(24)),
            (1..=24).collect::<Vec<i32>>()
        );
        // A `SxxExx` name is already relative; a number under the
        // offset is a first-season file and stays as it is.
        let relative = vec![
            "Show/Show - S02E02.mkv".to_string(),
            "Show/[Group] Show - 03 (BD 1080p).mkv".to_string(),
        ];
        assert_eq!(
            episode_numbers_for_commit(title, &relative, &offset(24)),
            vec![2, 3]
        );
    }

    #[tokio::test]
    async fn commit_handles_unparseable_title_with_empty_episode_list() {
        // A title that the parser can't decode (no number tokens at
        // all) writes the grab row with an empty episode_numbers
        // array. Post-processing per-file classification picks up
        // the slack at import time.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 200, "Movie").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "unparseable01");
        let id = commit_grab_and_expand(&state, &row, vec![], "Some Movie [1080p]", false)
            .await
            .expect("commit should succeed even with unparseable title");
        let eps: String =
            sqlx::query_scalar("SELECT episode_numbers FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(eps, "[]");
    }

    #[tokio::test]
    async fn commit_dedups_against_inflight_pending_row_by_hash() {
        // record_grab returns Ok(None) when an in-flight `pending`
        // row already exists for the same hash (PR 110's dedup
        // guard). Pin that path: a second commit with the same
        // hash returns None and doesn't double-insert.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 300, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "dup-hash-1");

        let first = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 01.mkv", false).await;
        assert!(first.is_some());

        let second = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 01.mkv", false).await;
        assert!(
            second.is_none(),
            "dedup must return None on the second call"
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents WHERE hash = 'dup-hash-1'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1, "exactly one row should exist after the dedup");
    }

    #[tokio::test]
    async fn commit_stamps_download_client_id_from_pending_grab() {
        // Picker confirm + walkaway auto-commit both flow through this
        // helper. Without the stamp, post-processing's resolve_grab_client
        // can't route the import / delete back through the same client
        // that received the grab — a non-default-client BT pin would
        // silently fall through to the torrent default at import.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 500, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let mut row = pending_grab_for(Some(series_id), "stamp-hash");
        row.download_client_id = Some(7);
        let id = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 01.mkv", false)
            .await
            .expect("commit");
        let stamped: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stamped, Some(7));
    }

    #[tokio::test]
    async fn commit_leaves_download_client_id_null_when_pending_row_has_none() {
        // Bare-magnet / legacy pending rows with no client capture still
        // round-trip cleanly: the stamp call writes NULL, which is the
        // sentinel post-processing's heuristic chain expects.
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 501, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "null-stamp-hash");
        let id = commit_grab_and_expand(&state, &row, vec![], "[G] Show - 02.mkv", false)
            .await
            .expect("commit");
        let stamped: Option<i64> =
            sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stamped, None);
    }

    #[tokio::test]
    async fn commit_writes_is_batch_flag_into_grab_row() {
        let db = in_memory_pool().await;
        let series_id = seed_series(&db, 400, "Show").await;
        let state = build_test_app_state(db.clone(), None);
        let row = pending_grab_for(Some(series_id), "batch-hash");
        let id = commit_grab_and_expand(&state, &row, vec![], "[G] Show 01-12 Batch [1080p]", true)
            .await
            .expect("commit");
        let is_batch: i64 =
            sqlx::query_scalar("SELECT is_batch FROM grabbed_torrents WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(is_batch, 1);
    }
    /// A series known by one name, its offset and its episode count.
    fn series(
        name: &str,
        cumulative_prior_episodes: i32,
        episode_count: Option<i32>,
    ) -> GrabSeries {
        GrabSeries {
            anchors: SeriesAnchors::new(&[name]),
            cumulative_prior_episodes,
            episode_count,
        }
    }

    #[test]
    fn a_single_release_holds_the_run_around_the_requested_episode() {
        let show = series("Show", 0, Some(12));
        let one =
            |title: &str, ep: i32| grab_episode_list(title, &show, GrabFor::Episode(ep), false);
        assert_eq!(
            one("[SakuraCircle] Saimin Seishidou - 05-06 (DVD 720x480)", 5),
            vec![5, 6]
        );
        assert_eq!(
            one(
                "[SubsPlease] RWBY - Hyousetsu Teikoku - 01-03 (720p) [CCDDC9CB].mkv",
                2
            ),
            vec![1, 2, 3]
        );
        assert_eq!(
            one(
                "[SubsPlease] RWBY - Hyousetsu Teikoku - 02 (720p) [4A0FE134].mkv",
                2
            ),
            vec![2]
        );
        // A run wider than one file, or one without the requested
        // episode, is not this grab's.
        assert_eq!(
            one(
                "[SubsPlease] RWBY - Hyousetsu Teikoku (01-12) (720p) [Batch]",
                4
            ),
            vec![4]
        );
        assert_eq!(one("Show - 01-12 [BD]", 4), vec![4]);
        assert_eq!(one("Show - 05-06 [1080p]", 9), vec![9]);
        // Season and part markers before the dash are not a run.
        assert_eq!(one("[Group] Show Season 2 - 03", 3), vec![3]);
        assert_eq!(one("[Group] Show Part 2 - 05", 5), vec![5]);
        assert_eq!(one("[Group] Show 3 - 05", 5), vec![5]);
    }

    #[test]
    fn a_pack_records_its_episodes_series_relative() {
        // A sequel's absolute-numbered pack records the episodes the
        // series and the import use, not 25-48.
        let sequel = series("Show", 24, Some(24));
        assert_eq!(
            grab_episode_list(
                "[Group] Show - 25-48 (BD 1080p) [Batch]",
                &sequel,
                GrabFor::Whole,
                true
            ),
            (1..=24).collect::<Vec<i32>>()
        );
        // An absolute weekly found for a relative target records it.
        let jjk = series("Jujutsu Kaisen", 47, Some(23));
        assert_eq!(
            grab_episode_list(
                "[SubsPlease] Jujutsu Kaisen - 56 (1080p)",
                &jjk,
                GrabFor::Episode(9),
                false
            ),
            vec![9]
        );
        // EternalAnimes' dashless pack (#296).
        let dark = series("Dark Gathering", 0, Some(25));
        assert_eq!(
            grab_episode_list(
                "[EA]Dark_Gathering_01-25_End_[1920x1080][BDRIP][HEVC][Flac]",
                &dark,
                GrabFor::Whole,
                true
            ),
            (1..=25).collect::<Vec<i32>>()
        );
    }

    #[test]
    fn a_pack_that_names_no_episodes_takes_the_series_count() {
        let show = series("Show", 0, Some(26));
        assert_eq!(
            grab_episode_list("[Group] Show Complete BD", &show, GrabFor::Whole, true),
            (1..=26).collect::<Vec<i32>>()
        );
        // No count, no guess.
        assert!(
            grab_episode_list(
                "[Group] Show Complete BD",
                &series("Show", 0, None),
                GrabFor::Whole,
                true
            )
            .is_empty()
        );
        assert!(
            grab_episode_list(
                "[Group] Show Complete BD",
                &series("Show", 0, Some(0)),
                GrabFor::Whole,
                true
            )
            .is_empty()
        );
        // A garbage count is cut to the cap rather than dropped.
        assert_eq!(
            grab_episode_list(
                "[Vodes] Some Series (BDRip)",
                &series("Some Series", 0, Some(100_000)),
                GrabFor::Whole,
                true
            ),
            (1..=WHOLE_SERIES_CAP).collect::<Vec<i32>>()
        );
        // The fallback is for packs only; a movie's lone release is its
        // episode 1.
        assert!(
            grab_episode_list(
                "[Vodes] Some Movie (BDRip)",
                &series("Some Movie", 0, Some(12)),
                GrabFor::Whole,
                false
            )
            .is_empty()
        );
        assert_eq!(
            grab_episode_list(
                "[Vodes] Some Movie (BDRip)",
                &series("Some Movie", 0, Some(1)),
                GrabFor::Whole,
                false
            ),
            vec![1]
        );
    }

    #[test]
    fn a_title_number_of_the_series_is_never_recorded() {
        // `Kaiju No. 8 - 09` holds episode 9 alone; a pack of `Mob
        // Psycho 100` that names no episodes takes the series' count.
        let kaiju = series("Kaiju No. 8", 0, Some(12));
        assert_eq!(
            grab_episode_list(
                "[SubsPlease] Kaiju No. 8 - 09 (1080p)",
                &kaiju,
                GrabFor::Episode(9),
                false
            ),
            vec![9]
        );
        let mob = series("Mob Psycho 100", 0, Some(12));
        assert_eq!(
            grab_episode_list(
                "[Group] Mob Psycho 100 (BD 1080p)",
                &mob,
                GrabFor::Whole,
                true
            ),
            (1..=12).collect::<Vec<i32>>()
        );
    }

    #[test]
    fn an_ova_pack_of_a_tv_series_records_no_regular_episode() {
        // The count fallback would tag every regular episode as grabbed,
        // and the specials cleanup would then delete their rows.
        let show = series("Show", 0, Some(12));
        assert!(
            grab_episode_list(
                "[Group] Show - OVA 01-02 (BD 1080p)",
                &show,
                GrabFor::Whole,
                true
            )
            .is_empty()
        );
        // An OVA entry's own pack is its episodes.
        let ova = GrabSeries {
            anchors: SeriesAnchors::new(&["Hellsing Ultimate"]).with_specials_as_episodes(true),
            cumulative_prior_episodes: 0,
            episode_count: Some(10),
        };
        assert_eq!(
            grab_episode_list(
                "[Group] Hellsing Ultimate OVA (BD 1080p) [Batch]",
                &ova,
                GrabFor::Whole,
                true
            ),
            (1..=10).collect::<Vec<i32>>()
        );
    }

    #[test]
    fn a_single_target_release_that_names_nothing_is_episode_one() {
        // The import gives a lone unnumbered file its slot only from the
        // grab list.
        let movie = series("Some Movie", 0, None);
        assert_eq!(
            grab_episode_list(
                "[Group] Some Movie (BD 1080p)",
                &movie,
                GrabFor::Single,
                false
            ),
            vec![1]
        );
        assert!(
            grab_episode_list(
                "[Group] Some Movie (BD 1080p)",
                &movie,
                GrabFor::Whole,
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn a_season_marker_keeps_a_run_relative_only_when_it_fits() {
        // `S2 - 13-24` for a 12-episode sequel after 12: absolute.
        let short = series("Show", 12, Some(12));
        assert_eq!(
            grab_episode_list(
                "[Group] Show S2 - 13-24 (BD) [Batch]",
                &short,
                GrabFor::Whole,
                true
            ),
            (1..=12).collect::<Vec<i32>>()
        );
        // `S2 - 13` of a 24-episode sequel after 12: relative.
        let long = series("Show", 12, Some(24));
        assert_eq!(
            grab_episode_list("[Group] Show S2 - 13 (1080p)", &long, GrabFor::Whole, false),
            vec![13]
        );
    }

    #[test]
    fn a_batch_ignores_the_searched_episode() {
        // The import's pack preflight reads `13-36` past an offset of 12
        // as absolute whatever was searched for.
        let long = series("Show", 12, Some(24));
        assert_eq!(
            grab_episode_list(
                "[Group] Show - 13-36 (BD) [Batch]",
                &long,
                GrabFor::Episode(14),
                true
            ),
            (1..=24).collect::<Vec<i32>>()
        );
    }

    #[test]
    fn a_pick_of_only_name_vetoed_files_is_no_episode() {
        let mob = series("Mob Psycho 100", 0, Some(12));
        let eps = episode_numbers_for_commit(
            "[G] Mob Psycho 100 (BD 1080p)",
            &["[G] Mob Psycho 100 (BD 1080p)/[G] Mob Psycho 100 (BD 1080p).mkv".to_string()],
            &mob,
        );
        assert!(eps.is_empty(), "{eps:?}");
    }
}
