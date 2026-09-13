//! Manual import for existing libraries (#122).
//!
//! The wizard at `/system/import` points Ryokan at a folder the user
//! already has, walks it ([`walk`]), works out which series and
//! episode each file is ([`parse`]), matches every series to AniList
//! ([`matching`]), and renders a per-series preview ([`preview`]) the
//! user corrects before anything is written. Decisions live in an
//! in-memory [`ImportSession`] ([`session`]) keyed by an opaque id in
//! the URL.
//!
//! This module owns the orchestration: the background preview job that
//! runs walk → parse → group → match → resolve-existing under a
//! `ProgressRegistry` handle (the page polls the same id), and the
//! smaller re-search / re-resolve steps the override controls call.
//!
//! Matching is deliberately ID-based once AniList has answered: the
//! parsed title only seeds the search, and "is this series already in
//! the library" is answered by `series.anilist_id` / `series.mal_id`,
//! never by comparing titles.
//!
//! The preview side never mutates the library; [`import`] is the one
//! place that does, and only from a `Ready` session the user confirmed.

pub mod import;
pub mod mapping;
pub mod matching;
pub mod parse;
pub mod preview;
pub mod session;
pub mod walk;

pub use import::{GroupReport, ImportOptions, ImportReport};
pub use session::ImportSessionStore;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use futures_util::{FutureExt, StreamExt, stream};
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::{config, episode_tags, log::LogCategory, series};
use crate::services::anilist::{self, AnimeEntry};
use crate::services::source::{self, ClassificationResult, WebKind};
use crate::services::{logger, progress};

/// AniList lookups in flight at once during matching. Each call still
/// goes through the AL throttle; this cap only stops a 200-series
/// library from queuing 200 requests behind the 30/min window at once.
pub const MATCH_CONCURRENCY: usize = 4;

/// How many next-best AL candidates the preview offers per series.
pub const MAX_ALTERNATIVES: usize = 4;

/// How files reach the media root. All three map onto
/// `post_processing::do_file_op` strategies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImportMode {
    /// Default. Seed-safe: the original stays for the client. Falls
    /// back to copy across filesystems (the preview warns).
    Hardlink,
    Copy,
    Move,
}

impl ImportMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hardlink" => Some(Self::Hardlink),
            "copy" => Some(Self::Copy),
            "move" => Some(Self::Move),
            _ => None,
        }
    }

    /// The `do_file_op` mode string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hardlink => "hardlink",
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hardlink => "Hardlink",
            Self::Copy => "Copy",
            Self::Move => "Move",
        }
    }

    /// "hardlinked" / "copied" / "moved", for copy that describes the
    /// finished action.
    pub fn past_tense(self) -> &'static str {
        match self {
            Self::Hardlink => "hardlinked",
            Self::Copy => "copied",
            Self::Move => "moved",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Scanning,
    Ready,
    Importing,
    /// The import job finished (or was cancelled part-way); the
    /// report is what the page shows.
    Done(Box<ImportReport>),
    Failed(String),
}

/// One video file under the walk root, as parsed.
#[derive(Clone, Debug)]
pub struct CandidateFile {
    pub path: PathBuf,
    /// Relative to the walk root, forward slashes; what the preview
    /// shows.
    pub rel_path: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub parsed_title: Option<String>,
    pub title_source: parse::TitleSource,
    pub season: Option<i32>,
    /// The first (usually only) episode the file holds; renumbering by
    /// the TMDB mapping or the sequel chain moves it, and the count
    /// below rides along.
    pub episode: Option<i32>,
    /// How many episodes the file holds (issue #246): 1 for the usual
    /// file, 2 for `S01E05-E06`.
    pub episode_count: i32,
    pub year: Option<i32>,
    pub group: Option<String>,
    /// Filename-only classification label (`BD-1080p`, `WEB-720p`,
    /// `Unknown`). A preview projection; the import step runs the full
    /// ffprobe pipeline the library scan uses.
    pub quality_label: String,
    /// Unticked in the preview means "leave this file alone".
    pub selected: bool,
    /// The episode number as parsed, when `episode` was renumbered
    /// into the AniList entry's numbering by the TMDB mapping.
    pub source_episode: Option<i32>,
}

/// What Ryokan already holds for one episode of a matched series.
#[derive(Clone, Debug)]
pub struct ExistingTag {
    pub quality_label: String,
    /// `grabbed` (download in flight) / `completed` / `failed`.
    pub state: String,
    pub manual_override: bool,
    pub classification: ClassificationResult,
}

/// The library row a group resolved to, by AL id (or MAL id for the
/// negative-AL-id fallback rows).
#[derive(Clone, Debug)]
pub struct ExistingSeries {
    pub id: i64,
    pub anilist_id: i64,
    /// Display title in the preferred language.
    pub title: String,
    /// The stored title variants and premiere year, so the preview's
    /// season folder renders from the same `SeriesNames` the import
    /// builds from the row (#124).
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub season_year: Option<i32>,
    pub folder_name: String,
    pub tags: HashMap<i32, ExistingTag>,
}

/// Files that parsed to the same series title (and season).
#[derive(Clone, Debug)]
pub struct SeriesGroup {
    pub key: String,
    pub parsed_title: String,
    /// Season the files carry, when it is 2+ (season 1 and "no
    /// season" are the same thing to AniList's naming).
    pub season: Option<i32>,
    /// Season the files carry in TMDB / TVDB numbering, season 1
    /// included; what the mapping resolver keys on. `None` when the
    /// files carry no season at all.
    pub tmdb_season: Option<i32>,
    pub year: Option<i32>,
    /// Current AL search string. Starts as `build_query(...)`; the
    /// re-search box replaces it.
    pub query: String,
    pub files: Vec<CandidateFile>,
    /// Ranked AL candidates, best first.
    pub candidates: Vec<AnimeEntry>,
    /// Index into `candidates`. `None` when nothing matched (or the
    /// user picked "none of these").
    pub pick: Option<usize>,
    pub low_confidence: bool,
    pub search_error: Option<String>,
    pub skipped: bool,
    pub existing: Option<ExistingSeries>,
    /// Set by the TMDB-mapping and sequel-chain resolvers once one of
    /// them shaped the group: the other must leave it alone, and a
    /// hand pick clears it. `mapping_note` is the human-readable
    /// trail for tests and logs, not control flow.
    pub resolved_by_id: bool,
    /// How a resolver shaped this group (diagnostic; not rendered).
    pub mapping_note: Option<String>,
    /// Results of the picker's last live search, so a "Use" click can
    /// promote one into `candidates` without a second lookup.
    pub search_results: Vec<AnimeEntry>,
}

impl SeriesGroup {
    pub fn picked(&self) -> Option<&AnimeEntry> {
        self.pick.and_then(|i| self.candidates.get(i))
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size_bytes).sum()
    }

    /// Candidates other than the pick, in rank order, capped at
    /// [`MAX_ALTERNATIVES`]. `(index, entry)` so the picker can post
    /// the index back.
    pub fn alternatives(&self) -> Vec<(usize, &AnimeEntry)> {
        self.candidates
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != self.pick)
            .take(MAX_ALTERNATIVES)
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ImportSession {
    pub id: String,
    pub created_at: Instant,
    pub last_touched: Instant,
    /// The path as typed.
    pub root: PathBuf,
    /// The path as walked (canonicalized when it resolved).
    pub walked_root: PathBuf,
    pub mode: ImportMode,
    pub follow_symlinks: bool,
    pub include_hidden: bool,
    pub status: SessionStatus,
    pub stats: walk::WalkStats,
    /// The source and the media root are on different filesystems, so
    /// hardlink mode would silently copy. Surfaced as a preview
    /// warning; `None` when it couldn't be determined.
    pub cross_fs: Option<bool>,
    /// Files with no title hint from the filename or any folder.
    pub unmatched_files: Vec<CandidateFile>,
    pub groups: Vec<SeriesGroup>,
    /// Set by the Cancel button while an import runs; the job checks
    /// it between files. Shared with the running job through the Arc
    /// so a session clone sees the same flag.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ImportSession {
    pub fn new(
        id: String,
        root: PathBuf,
        mode: ImportMode,
        follow_symlinks: bool,
        include_hidden: bool,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            created_at: now,
            last_touched: now,
            walked_root: root.clone(),
            root,
            mode,
            follow_symlinks,
            include_hidden,
            status: SessionStatus::Scanning,
            stats: walk::WalkStats::default(),
            cross_fs: None,
            unmatched_files: Vec::new(),
            groups: Vec::new(),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// Register the session as `Scanning`, bind a progress handle to its
/// id, and run the preview job in the background. The caller redirects
/// to the wizard page, which polls `/api/progress/{id}` and reloads
/// on the terminal event.
pub async fn start_preview(state: &AppState, session: ImportSession) {
    let id = session.id.clone();
    session::insert(&state.import_sessions, session);
    let handle = state.progress.register(id.clone()).await;
    let state = state.clone();
    tokio::spawn(async move {
        // Boxed + owned args: `catch_unwind` over an `async fn` that
        // borrows trips a higher-ranked `Send` inference error; the
        // erased `dyn Future + Send` sidesteps it.
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> =
            Box::pin(run_preview(state.clone(), id.clone()));
        let result = progress::scope(
            handle.clone(),
            std::panic::AssertUnwindSafe(fut).catch_unwind(),
        )
        .await;
        let outcome: Result<(), String> = match result {
            Ok(r) => r,
            Err(_) => Err("the scan crashed; see the server log".to_string()),
        };
        if let Err(msg) = outcome {
            session::update(&state.import_sessions, &id, |s| {
                s.status = SessionStatus::Failed(msg.clone());
            });
            logger::warn(
                &state.db,
                LogCategory::Library,
                "Manual import preview failed",
                &msg,
            )
            .await;
            handle
                .emit("error", "error", "Scan failed", Some(msg), true)
                .await;
        }
    });
}

/// The preview pipeline. Runs under the progress scope so `emit`
/// reaches the page's toast.
async fn run_preview(state: AppState, id: String) -> Result<(), String> {
    let state = &state;
    let id = id.as_str();
    let Some(session) = session::get(&state.import_sessions, id) else {
        return Err("preview session vanished".to_string());
    };
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| format!("config read failed: {e}"))?
        .unwrap_or_default();

    progress::emit(
        "walk",
        "info",
        "Scanning folders",
        Some(session.root.display().to_string()),
        false,
    )
    .await;

    let mut opts = walk::WalkOptions::new(session.root.clone());
    opts.follow_symlinks = session.follow_symlinks;
    opts.include_hidden = session.include_hidden;
    if !cfg.media_root.trim().is_empty() {
        opts.excludes.push(PathBuf::from(cfg.media_root.trim()));
    }
    if !cfg.recycle_bin_path.trim().is_empty() {
        opts.excludes
            .push(PathBuf::from(cfg.recycle_bin_path.trim()));
    }
    let walked = walk::walk(opts).await?;
    let stats = walked.stats.clone();
    let walked_root = walked.root.clone();
    progress::emit(
        "walk",
        "info",
        "Reading filenames",
        Some(format!(
            "{} video file{} in {} folder{}",
            stats.files,
            if stats.files == 1 { "" } else { "s" },
            stats.dirs,
            if stats.dirs == 1 { "" } else { "s" },
        )),
        false,
    )
    .await;

    // Parsing is CPU-only but an anitomy pass per file over 50k files
    // adds up; keep it off the runtime threads.
    let files: Vec<CandidateFile> = tokio::task::spawn_blocking(move || {
        walked.files.into_iter().map(candidate_from_raw).collect()
    })
    .await
    .map_err(|e| format!("parse task failed: {e}"))?;

    let (mut groups, unmatched) = group_files(files);
    progress::emit(
        "match",
        "info",
        "Matching series",
        Some(format!("0 of {}", groups.len())),
        false,
    )
    .await;
    match_groups(&mut groups).await;
    resolve_existing_all(&state.db, &mut groups).await;

    let cross_fs = if cfg.media_root.trim().is_empty() {
        None
    } else {
        crate::services::recycle::same_filesystem(&walked_root, Path::new(cfg.media_root.trim()))
            .map(|same| !same)
    };

    let group_count = groups.len();
    let file_count = stats.files;
    let unmatched_count = unmatched.len();
    session::update(&state.import_sessions, id, |s| {
        s.walked_root = walked_root;
        s.stats = stats;
        s.cross_fs = cross_fs;
        s.groups = groups;
        s.unmatched_files = unmatched;
        s.status = SessionStatus::Ready;
    })
    .ok_or_else(|| "preview session vanished".to_string())?;

    logger::info(
        &state.db,
        LogCategory::Library,
        "Manual import preview ready",
        &format!(
            "root={}, files={}, groups={}, unmatched_files={}",
            session.root.display(),
            file_count,
            group_count,
            unmatched_count
        ),
    )
    .await;
    progress::emit(
        "done",
        "success",
        "Preview ready",
        Some(format!(
            "{} series, {} file{}",
            group_count,
            file_count,
            if file_count == 1 { "" } else { "s" }
        )),
        true,
    )
    .await;
    Ok(())
}

fn candidate_from_raw(f: walk::RawFile) -> CandidateFile {
    let parsed = parse::parse_file(&f.rel_path);
    let file_name = f
        .rel_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let quality_label = source::classify_release_sync(&file_name, None).label();
    CandidateFile {
        path: f.path,
        rel_path: f.rel_path.to_string_lossy().replace('\\', "/"),
        file_name,
        size_bytes: f.size_bytes,
        parsed_title: parsed.title,
        title_source: parsed.title_source,
        season: parsed.season,
        episode: parsed.episode,
        episode_count: parsed.episode_count,
        year: parsed.year,
        group: parsed.group,
        quality_label,
        selected: true,
        source_episode: None,
    }
}

/// Bucket files by `(normalized title, season)`. Files with no title
/// hint come back separately. Groups are returned alphabetically by
/// parsed title so the preview reads like a directory listing.
pub fn group_files(files: Vec<CandidateFile>) -> (Vec<SeriesGroup>, Vec<CandidateFile>) {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, SeriesGroup> = HashMap::new();
    let mut unmatched = Vec::new();
    for f in files {
        let Some(title) = f.parsed_title.clone() else {
            unmatched.push(f);
            continue;
        };
        let season = f.season.filter(|s| *s > 1);
        let key = matching::group_key(&title, season);
        let g = by_key.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            SeriesGroup {
                key,
                parsed_title: title.clone(),
                season,
                tmdb_season: None,
                year: None,
                query: matching::build_query(&title, season),
                files: Vec::new(),
                candidates: Vec::new(),
                pick: None,
                low_confidence: false,
                search_error: None,
                skipped: false,
                existing: None,
                resolved_by_id: false,
                mapping_note: None,
                search_results: Vec::new(),
            }
        });
        g.files.push(f);
    }
    let mut groups: Vec<SeriesGroup> = order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .map(|mut g| {
            // Majority year across the files, if any carried one.
            let mut counts: HashMap<i32, usize> = HashMap::new();
            for y in g.files.iter().filter_map(|f| f.year) {
                *counts.entry(y).or_default() += 1;
            }
            g.year = counts
                .into_iter()
                .max_by_key(|(y, c)| (*c, -*y))
                .map(|(y, _)| y);
            // Majority parsed season, season 1 included, for the TMDB
            // mapping; `season` above is the AniList-facing one that
            // folds season 1 into "none".
            let mut seasons: HashMap<i32, usize> = HashMap::new();
            for sn in g.files.iter().filter_map(|f| f.season) {
                *seasons.entry(sn).or_default() += 1;
            }
            g.tmdb_season = seasons
                .into_iter()
                .max_by_key(|(sn, c)| (*c, -*sn))
                .map(|(sn, _)| sn);
            g.files.sort_by(|a, b| {
                a.season
                    .unwrap_or(1)
                    .cmp(&b.season.unwrap_or(1))
                    .then(
                        a.episode
                            .unwrap_or(i32::MAX)
                            .cmp(&b.episode.unwrap_or(i32::MAX)),
                    )
                    .then_with(|| a.rel_path.cmp(&b.rel_path))
            });
            g
        })
        .collect();
    groups.sort_by_key(|g| (g.parsed_title.to_lowercase(), g.season.unwrap_or(1)));
    (groups, unmatched)
}

/// Search + rank one group. Leaves `candidates` empty and sets
/// `search_error` when AL (and the Jikan fallback) failed, so the
/// preview can say why rather than showing "no match".
///
/// `with_fallbacks` (the automatic pass) retries an empty result with
/// [`matching::fallback_queries`]: the bare title, then the title cut
/// at its subtitle. A query the user typed is searched as typed.
/// Whichever query produced the hits becomes `group.query`, so the
/// card shows what actually matched.
pub async fn search_and_rank(group: &mut SeriesGroup, with_fallbacks: bool) {
    group.search_error = None;
    let input = matching::RankInput {
        title: &group.parsed_title,
        season: group.season,
        year: group.year,
        file_count: group.files.len(),
    };
    let mut queries = vec![group.query.clone()];
    if with_fallbacks {
        queries.extend(matching::fallback_queries(
            &group.parsed_title,
            &group.query,
        ));
    }
    let mut result: Result<Vec<AnimeEntry>, String> = Ok(Vec::new());
    for q in &queries {
        result = anilist::search_anime(q).await;
        match &result {
            Ok(hits) if !hits.is_empty() => {
                group.query = q.clone();
                break;
            }
            // Empty: try the next shape. Error (throttle, outage):
            // stop here rather than burn more of the budget.
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    match result {
        Ok(hits) => {
            group.candidates = matching::rank_entries(&input, hits);
            group.pick = if group.candidates.is_empty() {
                None
            } else {
                Some(0)
            };
            // Judge confidence against what was actually searched: after
            // a user re-search the parsed title is stale by definition,
            // and flagging their own correction would be noise.
            group.low_confidence = group
                .picked()
                .is_some_and(|e| matching::is_low_confidence(&group.query, e));
        }
        Err(e) => {
            group.candidates.clear();
            group.pick = None;
            group.low_confidence = false;
            group.search_error = Some(e);
        }
    }
}

/// Fan the AL searches out at [`MATCH_CONCURRENCY`], reporting
/// progress per finished group. Groups move through the stream by
/// value (a closure returning an `async` block over `&mut` trips a
/// higher-ranked lifetime error under `buffer_unordered`) and are
/// put back in their original order afterwards.
pub async fn match_groups(groups: &mut Vec<SeriesGroup>) {
    let total = groups.len();
    let owned = std::mem::take(groups);
    let mut results = stream::iter(owned.into_iter().enumerate())
        .map(|(i, mut g)| async move {
            search_and_rank(&mut g, true).await;
            // The TMDB mapping may split a season across AniList
            // entries, and absolute numbering may spread a folder
            // along the sequel chain; each group comes back as one
            // or more.
            let mut resolved = Vec::new();
            for g in mapping::apply_season_mapping(g).await {
                resolved.extend(mapping::apply_absolute_numbering(g).await);
            }
            (i, resolved)
        })
        .buffer_unordered(MATCH_CONCURRENCY);
    let mut finished: Vec<(usize, Vec<SeriesGroup>)> = Vec::with_capacity(total);
    while let Some(item) = results.next().await {
        finished.push(item);
        progress::emit(
            "match",
            "info",
            "Matching series",
            Some(format!("{} of {}", finished.len(), total)),
            false,
        )
        .await;
    }
    finished.sort_by_key(|(i, _)| *i);
    *groups = finished.into_iter().flat_map(|(_, gs)| gs).collect();
}

/// Snapshot of a tracked series's episode tags, for the merge preview.
/// `title` follows `config.title_language`, not the frozen
/// `series.title` column.
async fn existing_from_row(db: &SqlitePool, row: series::Series) -> ExistingSeries {
    let pref = crate::services::library_link::title_language(db).await;
    let title = crate::services::nfo::title_for_preference(&row, &pref);
    let tags = episode_tags::get_for_series(db, row.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(ep, t)| {
            let classification = source::classification_from_stored_full(
                &t.source,
                &t.resolution,
                t.is_remux,
                t.is_bdmv,
                WebKind::from_str(&t.web_kind),
                t.classification_confidence,
                t.needs_review,
            );
            (
                ep,
                ExistingTag {
                    quality_label: t.quality_tag,
                    state: t.state,
                    manual_override: t.manual_override,
                    classification,
                },
            )
        })
        .collect();
    ExistingSeries {
        id: row.id,
        title_romaji: row.title_romaji.clone(),
        title_english: row.title_english.clone(),
        title_native: row.title_native.clone(),
        season_year: row.season_year,
        anilist_id: row.anilist_id,
        title,
        folder_name: row.folder_name,
        tags,
    }
}

/// Library row for the group's current pick: AL id first, MAL id for
/// the negative-AL-id fallback rows. Title never enters into it.
pub async fn resolve_existing(db: &SqlitePool, group: &mut SeriesGroup) {
    group.existing = None;
    let Some(entry) = group.picked().cloned() else {
        return;
    };
    let row = if entry.id > 0 {
        series::get_by_anilist_id(db, entry.id).await.ok().flatten()
    } else {
        None
    };
    let row = match (row, entry.id_mal) {
        (Some(r), _) => Some(r),
        (None, Some(mal)) => series::get_by_mal_id(db, mal).await.ok().flatten(),
        (None, None) => None,
    };
    if let Some(r) = row {
        group.existing = Some(existing_from_row(db, r).await);
    }
}

/// Batch variant for the initial preview: one `IN (...)` lookup for
/// every picked AL id, then the MAL fallback only for the misses.
pub async fn resolve_existing_all(db: &SqlitePool, groups: &mut [SeriesGroup]) {
    let ids: Vec<i64> = groups
        .iter()
        .filter_map(|g| g.picked().map(|e| e.id))
        .filter(|id| *id > 0)
        .collect();
    let mut by_al = series::get_by_anilist_ids(db, &ids)
        .await
        .unwrap_or_default();
    for g in groups.iter_mut() {
        g.existing = None;
        let Some(entry) = g.picked().cloned() else {
            continue;
        };
        let row = match by_al.remove(&entry.id) {
            Some(r) => Some(r),
            None => match entry.id_mal {
                Some(mal) => series::get_by_mal_id(db, mal).await.ok().flatten(),
                None => None,
            },
        };
        if let Some(r) = row {
            g.existing = Some(existing_from_row(db, r).await);
        }
    }
}

/// One group of a live session, cloned out for an action to work on.
/// Says which of the two lookups missed.
fn group_or_err(state: &AppState, session_id: &str, idx: usize) -> Result<SeriesGroup, String> {
    let session = session::get(&state.import_sessions, session_id)
        .ok_or_else(|| "Preview session not found".to_string())?;
    session
        .groups
        .get(idx)
        .cloned()
        .ok_or_else(|| "Unknown series in this preview".to_string())
}

/// Undo resolver renumbering on a group's files: a candidate the user
/// picked by hand overrides whatever the TMDB mapping or the sequel
/// chain decided, and the parsed numbers are the only ones that still
/// mean anything for it.
fn revert_renumbering(group: &mut SeriesGroup) {
    for f in &mut group.files {
        if let Some(src) = f.source_episode.take() {
            f.episode = Some(src);
        }
    }
    group.resolved_by_id = false;
    group.mapping_note = None;
}

/// Write a re-resolved group back into its slot, keeping the per-file
/// ticks the slot has *now*. The actions clone a group, await a
/// provider, and store it; a checkbox toggled in that gap must not be
/// reverted, and only the ticks may be carried over (carrying the
/// files wholesale would undo a renumbering revert).
fn store_group(
    state: &AppState,
    session_id: &str,
    idx: usize,
    mut group: SeriesGroup,
) -> Result<(), String> {
    session::update(&state.import_sessions, session_id, |s| {
        match s.groups.get_mut(idx) {
            Some(slot) => {
                for (f, current) in group.files.iter_mut().zip(slot.files.iter()) {
                    f.selected = current.selected;
                }
                *slot = group;
                Ok(())
            }
            None => Err("Unknown series in this preview".to_string()),
        }
    })
    .unwrap_or_else(|| Err("Preview session not found".to_string()))
}

/// The picker's live search: rank the provider hits for `query`
/// against the group's file evidence and remember them on the group
/// so a pick can promote one. Doesn't touch the current pick.
pub async fn live_search(
    state: &AppState,
    session_id: &str,
    idx: usize,
    query: &str,
) -> Result<Vec<AnimeEntry>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Type a title to search for".to_string());
    }
    let group = group_or_err(state, session_id, idx)?;
    let hits = anilist::search_anime(query).await?;
    let input = matching::RankInput {
        title: &group.parsed_title,
        season: group.season,
        year: group.year,
        file_count: group.files.len(),
    };
    let ranked = matching::rank_entries(&input, hits);
    session::update(&state.import_sessions, session_id, |s| {
        if let Some(slot) = s.groups.get_mut(idx) {
            slot.search_results = ranked.clone();
        }
    });
    Ok(ranked)
}

/// Pick a candidate by provider id (positive AniList, negative MAL):
/// from the ranked candidates, or promoted from the last live search.
/// A hand pick clears the resolver's renumbering and notes.
pub async fn pick_by_id(
    state: &AppState,
    session_id: &str,
    idx: usize,
    id: i64,
) -> Result<(), String> {
    let mut group = group_or_err(state, session_id, idx)?;
    let pick = match group.candidates.iter().position(|e| e.id == id) {
        Some(i) => i,
        None => {
            let Some(entry) = group.search_results.iter().find(|e| e.id == id).cloned() else {
                return Err("Unknown candidate".to_string());
            };
            group.candidates.push(entry);
            group.candidates.len() - 1
        }
    };
    group.pick = Some(pick);
    group.skipped = false;
    group.low_confidence = false;
    revert_renumbering(&mut group);
    resolve_existing(&state.db, &mut group).await;
    store_group(state, session_id, idx, group)
}

/// Re-search one group with a user-typed query, then re-resolve the
/// library row for the new pick. The AL call runs outside the store
/// lock; only the swap-in is locked.
pub async fn research_group(
    state: &AppState,
    session_id: &str,
    idx: usize,
    query: &str,
) -> Result<(), String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Type a title to search for".to_string());
    }
    let mut group = group_or_err(state, session_id, idx)?;
    group.query = query.to_string();
    group.skipped = false;
    revert_renumbering(&mut group);
    search_and_rank(&mut group, false).await;
    resolve_existing(&state.db, &mut group).await;
    store_group(state, session_id, idx, group)
}

/// Switch a group's pick to another ranked candidate (or `None` for
/// "none of these") and re-resolve the library row.
pub async fn pick_candidate(
    state: &AppState,
    session_id: &str,
    idx: usize,
    pick: Option<usize>,
) -> Result<(), String> {
    let mut group = group_or_err(state, session_id, idx)?;
    if let Some(p) = pick
        && p >= group.candidates.len()
    {
        return Err("Unknown candidate".to_string());
    }
    group.pick = pick;
    group.skipped = false;
    group.low_confidence = false;
    revert_renumbering(&mut group);
    resolve_existing(&state.db, &mut group).await;
    store_group(state, session_id, idx, group)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cf(
        rel: &str,
        title: Option<&str>,
        season: Option<i32>,
        ep: Option<i32>,
        year: Option<i32>,
    ) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from(rel),
            rel_path: rel.to_string(),
            file_name: Path::new(rel)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            size_bytes: 10,
            parsed_title: title.map(|t| t.to_string()),
            title_source: parse::TitleSource::Filename,
            season,
            episode: ep,
            year,
            group: None,
            quality_label: "Unknown".into(),
            selected: true,
            episode_count: 1,
            source_episode: None,
        }
    }

    #[test]
    fn import_mode_roundtrip() {
        for m in [ImportMode::Hardlink, ImportMode::Copy, ImportMode::Move] {
            assert_eq!(ImportMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(ImportMode::parse("HardLink"), Some(ImportMode::Hardlink));
        assert_eq!(ImportMode::parse("symlink"), None);
    }

    #[test]
    fn group_files_buckets_by_title_and_season_and_sorts() {
        let files = vec![
            cf("Naruto/02.mkv", Some("Naruto"), None, Some(2), None),
            cf(
                "Naruto/01.mkv",
                Some("NARUTO"),
                Some(1),
                Some(1),
                Some(2002),
            ),
            cf(
                "Mob/S02E01.mkv",
                Some("Mob Psycho 100"),
                Some(2),
                Some(1),
                None,
            ),
            cf(
                "Mob/S01E01.mkv",
                Some("Mob Psycho 100"),
                Some(1),
                Some(1),
                Some(2016),
            ),
            cf("loose/01.mkv", None, None, Some(1), None),
        ];
        let (groups, unmatched) = group_files(files);
        assert_eq!(unmatched.len(), 1);
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["mob psycho 100", "mob psycho 100|s2", "naruto"]);
        let naruto = &groups[2];
        assert_eq!(naruto.files.len(), 2);
        assert_eq!(naruto.files[0].episode, Some(1), "files sorted by episode");
        assert_eq!(naruto.year, Some(2002));
        assert_eq!(naruto.query, "Naruto");
        assert_eq!(groups[1].query, "Mob Psycho 100 season 2");
        assert_eq!(groups[1].season, Some(2));
        assert_eq!(groups[0].season, None);
    }

    #[tokio::test]
    async fn store_group_keeps_current_ticks_and_the_reverted_numbering() {
        use crate::test_support::{build_test_app_state, in_memory_pool};
        let state = build_test_app_state(in_memory_pool().await, None);
        // The slot as the page sees it: file 1 unticked by the user,
        // renumbered by a resolver.
        let mut slot_files = vec![
            cf("Show/01.mkv", Some("Show"), None, Some(1), None),
            cf("Show/02.mkv", Some("Show"), None, Some(14), None),
        ];
        slot_files[1].selected = false;
        slot_files[1].source_episode = Some(2);
        let mut s = ImportSession::new(
            session::mint_id(),
            PathBuf::from("/src"),
            ImportMode::Hardlink,
            false,
            false,
        );
        s.status = SessionStatus::Ready;
        let (mut groups, _) = group_files(slot_files);
        groups[0].resolved_by_id = true;
        s.groups = groups;
        let sid = s.id.clone();
        session::insert(&state.import_sessions, s);

        // A re-resolved clone (as research/pick build it): ticks stale
        // (both true), numbering reverted.
        let mut clone = session::get(&state.import_sessions, &sid).unwrap().groups[0].clone();
        clone.files.iter_mut().for_each(|f| f.selected = true);
        revert_renumbering(&mut clone);
        store_group(&state, &sid, 0, clone).unwrap();

        let stored = session::get(&state.import_sessions, &sid).unwrap();
        let g = &stored.groups[0];
        assert!(!g.files[1].selected, "the slot's tick survives");
        assert_eq!(g.files[1].episode, Some(2), "renumbering reverted");
        assert_eq!(g.files[1].source_episode, None);
        assert!(!g.resolved_by_id);
        assert!(
            store_group(&state, &sid, 9, g.clone()).is_err(),
            "out-of-range index errors"
        );
    }

    #[test]
    fn alternatives_exclude_the_pick_and_cap() {
        let mk = |id: i64| AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("T{id}"),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: String::new(),
            status_display: String::new(),
            episodes: None,
            season_year: None,
            source: String::new(),
            average_score: None,
        };
        let g = SeriesGroup {
            key: "x".into(),
            parsed_title: "x".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "x".into(),
            files: Vec::new(),
            candidates: (1..=7).map(mk).collect(),
            pick: Some(2),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        };
        let alts: Vec<usize> = g.alternatives().into_iter().map(|(i, _)| i).collect();
        assert_eq!(alts, vec![0, 1, 3, 4]);
        assert_eq!(g.picked().unwrap().id, 3);
    }
}
