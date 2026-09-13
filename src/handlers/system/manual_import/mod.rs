//! Manual import wizard (#122).
//!
//! `GET /system/import` renders the wizard: the start form when no
//! session is in the URL, the scanning / failed states while the
//! preview job runs, and the per-series preview once it is ready.
//! `POST /system/import` validates the start form and kicks off
//! `services::manual_import::start_preview`, then redirects to the
//! session URL. The override controls on each preview card post to
//! `/system/import/{session}/group/{idx}` and get the re-rendered
//! card back (HTMX) or a redirect to the page (plain form POST).
//!
//! All filesystem and AniList work lives in `services::manual_import`;
//! this module shapes the session for the templates and maps actions.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::models::{config, series};
use crate::services::anilist::AnimeEntry;
use crate::services::manual_import::{
    self, ImportMode, ImportOptions, ImportReport, ImportSession, SessionStatus,
    import::{import_progress_id, start_import},
    preview::{self, FileView, GroupCounts, GroupView, ProjectionContext, SessionSummary},
    session,
    walk::WalkStats,
};
use crate::services::media;
use crate::services::recycle::human_bytes;

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub session: String,
}

#[derive(Deserialize, Default)]
pub struct StartForm {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub follow_symlinks: Option<String>,
    #[serde(default)]
    pub include_hidden: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct GroupActionForm {
    /// `skip` / `unskip` / `pick` / `unpick` / `research` /
    /// `toggle_file` / `select_all` / `select_none`.
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub candidate: Option<usize>,
    #[serde(default)]
    pub file: Option<usize>,
    #[serde(default)]
    pub query: String,
    /// `pick_id`: provider id (positive AniList, negative MAL).
    #[serde(default)]
    pub id: Option<i64>,
}

#[derive(Deserialize, Default)]
pub struct PickerQuery {
    #[serde(default)]
    pub q: String,
}

/// Start-form echo so a validation error keeps what was typed.
struct StartFormView {
    path: String,
    mode: String,
    follow_symlinks: bool,
    include_hidden: bool,
}

/// One provider candidate as the picker shows it.
struct CandidateView {
    #[allow(dead_code)]
    index: usize,
    id: i64,
    title: String,
    subtitle: String,
    cover_url: String,
    format: String,
    year: String,
    episodes: String,
    external_href: String,
    /// `AniList` / `MAL`.
    source_label: &'static str,
    /// The group's current pick.
    is_current: bool,
}

/// The picker's result list, swapped into the card by the
/// search-as-you-type input. `g` mirrors the fields the include reads
/// off a `GroupCard` so one partial serves both renders.
struct PickerCtx {
    idx: usize,
    query: String,
    picker_rows: Vec<CandidateView>,
    picker_error: String,
}

#[derive(Template)]
#[template(path = "partials/system/import_picker_results.html")]
struct PickerResultsPartial {
    session_id: String,
    g: PickerCtx,
}

/// Everything the group card partial needs. Built from a
/// [`manual_import::SeriesGroup`] plus its [`GroupView`] projection.
struct GroupCard {
    idx: usize,
    parsed_title: String,
    /// `Season 2` when the files carry a season past the first.
    season_label: String,
    year_label: String,
    /// The series name was read from a folder rather than filenames.
    title_from_folder: bool,
    query: String,
    file_count: usize,
    size: String,
    kind: &'static str,
    kind_label: &'static str,
    pick: Option<CandidateView>,
    /// Ranked candidates for the picker's initial list, current
    /// pick marked.
    picker_rows: Vec<CandidateView>,
    picker_error: String,
    low_confidence: bool,
    search_error: String,
    skipped: bool,
    existing_title: String,
    existing_anilist_id: i64,
    files: Vec<FileView>,
    counts: GroupCounts,
    /// Another group in this preview picked the same series.
    duplicate_of: String,
    /// Inline error from the last override action on this card.
    action_error: String,
}

/// A live session on the start page's "Recent scans" list.
struct RecentView {
    id: String,
    root: String,
    /// `Scanning` / `Ready to review` / `Importing` / `Imported` / `Failed`.
    status: &'static str,
}

fn recent_status(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Scanning => "Scanning",
        SessionStatus::Ready => "Ready to review",
        SessionStatus::Importing => "Importing",
        SessionStatus::Done(_) => "Imported",
        SessionStatus::Failed(_) => "Failed",
    }
}

/// A file with no series hint at all, listed under its own heading.
struct UnmatchedView {
    rel_path: String,
    episode_label: String,
    quality_label: String,
    size: String,
}

#[derive(Template)]
#[template(path = "partials/system/import_group.html")]
struct GroupPartial {
    session_id: String,
    g: GroupCard,
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportTemplate {
    page: String,
    /// Active sidebar entry for the shared System sidebar partial.
    tab: &'static str,
    title_language: String,
    media_root: String,
    /// `start` / `scanning` / `ready` / `importing` / `done` / `failed`.
    step: &'static str,
    /// Start-form validation error.
    error: String,
    /// Inline notice on the preview (e.g. a refused confirm).
    notice: String,
    /// Progress id the page watches while scanning or importing.
    progress_id: String,
    report: ImportReport,
    report_size: String,
    form: StartFormView,
    session_id: String,
    root: String,
    mode_label: &'static str,
    mode_past: &'static str,
    follow_symlinks: bool,
    include_hidden: bool,
    stats: WalkStats,
    total_size: String,
    summary: SessionSummary,
    write_size: String,
    /// Hardlink mode across filesystems: the import would copy.
    cross_fs_warning: bool,
    failed_message: String,
    groups: Vec<GroupCard>,
    unmatched: Vec<UnmatchedView>,
    /// Start page only: live sessions, newest first.
    recent: Vec<RecentView>,
    /// Always false on the page; the summary / confirm partials set
    /// it when rendered standalone for an out-of-band swap.
    oob: bool,
}

/// Summary strip, re-rendered out-of-band after every card action.
#[derive(Template)]
#[template(path = "partials/system/import_summary.html")]
struct SummaryStripPartial {
    session_id: String,
    root: String,
    mode_label: &'static str,
    stats: WalkStats,
    total_size: String,
    summary: SessionSummary,
    oob: bool,
}

/// Sticky confirm bar, re-rendered out-of-band after every card action.
#[derive(Template)]
#[template(path = "partials/system/import_confirm.html")]
struct ConfirmBarPartial {
    session_id: String,
    mode_label: &'static str,
    mode_past: &'static str,
    summary: SessionSummary,
    write_size: String,
    /// Why the last confirm was refused; shown in the bar.
    error: String,
    oob: bool,
}

/// The two out-of-band blocks for a Ready session, from a fresh
/// projection of every group (cheap: pure, no I/O beyond the render
/// context the caller already built).
fn oob_totals(session: &ImportSession, ctx: &RenderContext) -> String {
    let views = project_views(session, ctx);
    let summary = preview::summarize(session, &views);
    let write_size = human_bytes(summary.write_bytes);
    let strip = SummaryStripPartial {
        session_id: session.id.clone(),
        root: session.root.display().to_string(),
        mode_label: session.mode.label(),
        stats: session.stats.clone(),
        total_size: human_bytes(session.stats.total_bytes),
        summary: summary.clone(),
        oob: true,
    };
    let bar = ConfirmBarPartial {
        session_id: session.id.clone(),
        mode_label: session.mode.label(),
        mode_past: session.mode.past_tense(),
        summary,
        write_size,
        error: String::new(),
        oob: true,
    };
    format!(
        "{}{}",
        strip.render().unwrap_or_default(),
        bar.render().unwrap_or_default()
    )
}

fn format_display(format: &str) -> String {
    match format.to_ascii_uppercase().as_str() {
        "" => "TBA".to_string(),
        "TV_SHORT" => "TV Short".to_string(),
        "MOVIE" => "Movie".to_string(),
        "SPECIAL" => "Special".to_string(),
        "MUSIC" => "Music".to_string(),
        other => other.to_string(),
    }
}

fn candidate_view(index: usize, entry: &AnimeEntry, pref: &str) -> CandidateView {
    let title = preview::entry_title(entry, pref).to_string();
    let subtitle = [
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    ]
    .into_iter()
    .find(|s| !s.is_empty() && **s != title)
    .cloned()
    .unwrap_or_default();
    let external_href = if entry.id > 0 {
        format!("https://anilist.co/anime/{}", entry.id)
    } else if let Some(mal) = entry.id_mal {
        format!("https://myanimelist.net/anime/{mal}")
    } else {
        String::new()
    };
    CandidateView {
        index,
        id: entry.id,
        title,
        subtitle,
        cover_url: entry.cover_url.clone(),
        format: format_display(&entry.format),
        year: entry.season_year.map(|y| y.to_string()).unwrap_or_default(),
        episodes: match entry.episodes {
            Some(n) => format!("{n} ep"),
            None => "? ep".to_string(),
        },
        external_href,
        source_label: if entry.source == "mal" {
            "MAL"
        } else {
            "AniList"
        },
        is_current: false,
    }
}

fn picker_rows(group: &manual_import::SeriesGroup, pref: &str) -> Vec<CandidateView> {
    group
        .candidates
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut v = candidate_view(i, e, pref);
            v.is_current = group.pick == Some(i);
            v
        })
        .collect()
}

/// Library state the projection reads, gathered once per render.
struct RenderContext {
    media_root: String,
    title_pref: String,
    series_folder_format: String,
    season_folder_format: String,
    owned_folders: HashSet<String>,
    disk_folders: HashSet<String>,
}

async fn render_context(state: &AppState) -> RenderContext {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let media_root = cfg.media_root.trim().to_string();
    let title_pref = if cfg.title_language.is_empty() {
        "english".to_string()
    } else {
        cfg.title_language
    };
    let series_folder_format = cfg.series_folder_format;
    let season_folder_format = cfg.season_folder_format;
    let owned_folders: HashSet<String> = series::get_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.folder_name)
        .filter(|f| !f.is_empty())
        .collect();
    let disk_folders: HashSet<String> =
        media::list_media_folders(&media_root).into_iter().collect();
    RenderContext {
        media_root,
        title_pref,
        series_folder_format,
        season_folder_format,
        owned_folders,
        disk_folders,
    }
}

impl RenderContext {
    fn projection(&self) -> ProjectionContext<'_> {
        ProjectionContext {
            media_root: &self.media_root,
            owned_folders: &self.owned_folders,
            disk_folders: &self.disk_folders,
            title_pref: &self.title_pref,
            series_folder_format: &self.series_folder_format,
            season_folder_format: &self.season_folder_format,
        }
    }
}

fn build_card(
    idx: usize,
    group: &manual_import::SeriesGroup,
    view: GroupView,
    pref: &str,
    duplicate_of: String,
    action_error: String,
) -> GroupCard {
    GroupCard {
        idx,
        parsed_title: group.parsed_title.clone(),
        season_label: group
            .season
            .filter(|s| *s > 1)
            .map(|s| format!("Season {s}"))
            .unwrap_or_default(),
        year_label: group.year.map(|y| y.to_string()).unwrap_or_default(),
        title_from_folder: group
            .files
            .iter()
            .any(|f| f.title_source == manual_import::parse::TitleSource::ParentFolder),
        query: group.query.clone(),
        file_count: group.files.len(),
        size: human_bytes(group.total_bytes()),
        kind: view.kind.as_str(),
        kind_label: view.kind.label(),
        pick: group
            .pick
            .and_then(|i| group.candidates.get(i).map(|e| candidate_view(i, e, pref))),
        picker_rows: picker_rows(group, pref),
        picker_error: String::new(),
        low_confidence: group.low_confidence,
        search_error: group.search_error.clone().unwrap_or_default(),
        skipped: group.skipped,
        existing_title: group
            .existing
            .as_ref()
            .map(|e| e.title.clone())
            .unwrap_or_default(),
        existing_anilist_id: group.existing.as_ref().map(|e| e.anilist_id).unwrap_or(0),
        files: view.files,
        counts: view.counts,
        duplicate_of,
        action_error,
    }
}

/// `picked AL id -> parsed title of the first non-skipped group that
/// picked it`, for the "also matched by" flag.
fn duplicate_picks(session: &ImportSession) -> HashMap<i64, String> {
    let mut first: HashMap<i64, String> = HashMap::new();
    for g in &session.groups {
        if g.skipped {
            continue;
        }
        if let Some(e) = g.picked() {
            first.entry(e.id).or_insert_with(|| g.parsed_title.clone());
        }
    }
    first
}

fn duplicate_for(firsts: &HashMap<i64, String>, group: &manual_import::SeriesGroup) -> String {
    if group.skipped {
        return String::new();
    }
    group
        .picked()
        .and_then(|e| firsts.get(&e.id))
        .filter(|t| **t != group.parsed_title)
        .cloned()
        .unwrap_or_default()
}

/// Every group's outcome projection; what the summary needs.
fn project_views(session: &ImportSession, ctx: &RenderContext) -> Vec<GroupView> {
    let proj = ctx.projection();
    session
        .groups
        .iter()
        .map(|g| preview::project_group(g, &proj))
        .collect()
}

fn build_cards(session: &ImportSession, ctx: &RenderContext) -> (Vec<GroupCard>, Vec<GroupView>) {
    let firsts = duplicate_picks(session);
    let views = project_views(session, ctx);
    let cards = session
        .groups
        .iter()
        .zip(views.iter())
        .enumerate()
        .map(|(idx, (g, v))| {
            build_card(
                idx,
                g,
                v.clone(),
                &ctx.title_pref,
                duplicate_for(&firsts, g),
                String::new(),
            )
        })
        .collect();
    (cards, views)
}

fn empty_form() -> StartFormView {
    StartFormView {
        path: String::new(),
        mode: "hardlink".to_string(),
        follow_symlinks: false,
        include_hidden: false,
    }
}

fn base_template(ctx: &RenderContext) -> ImportTemplate {
    ImportTemplate {
        // Reached from System → Import Library; highlight that nav entry.
        page: "system".to_string(),
        tab: "import",
        title_language: ctx.title_pref.clone(),
        media_root: ctx.media_root.clone(),
        step: "start",
        error: String::new(),
        notice: String::new(),
        progress_id: String::new(),
        report: ImportReport::default(),
        report_size: String::new(),
        form: empty_form(),
        session_id: String::new(),
        root: String::new(),
        mode_label: ImportMode::Hardlink.label(),
        mode_past: ImportMode::Hardlink.past_tense(),
        follow_symlinks: false,
        include_hidden: false,
        stats: WalkStats::default(),
        total_size: String::new(),
        summary: SessionSummary::default(),
        write_size: String::new(),
        cross_fs_warning: false,
        failed_message: String::new(),
        groups: Vec::new(),
        unmatched: Vec::new(),
        recent: Vec::new(),
        oob: false,
    }
}

fn recent_sessions(state: &AppState) -> Vec<RecentView> {
    session::list(&state.import_sessions)
        .into_iter()
        .map(|s| RecentView {
            id: s.id.clone(),
            root: s.root.display().to_string(),
            status: recent_status(&s.status),
        })
        .collect()
}

fn render(t: ImportTemplate) -> Html<String> {
    Html(t.render().unwrap_or_default())
}

fn session_url(id: &str) -> String {
    format!("/system/import?session={id}")
}

pub async fn page(State(state): State<AppState>, Query(q): Query<PageQuery>) -> Html<String> {
    render_session_page(&state, q.session.trim(), String::new()).await
}

/// The wizard page for `session_id` in whatever state it is in.
/// `notice` renders above the preview (confirm refusals).
async fn render_session_page(state: &AppState, id: &str, notice: String) -> Html<String> {
    let ctx = render_context(state).await;
    let mut t = base_template(&ctx);
    if id.is_empty() {
        t.recent = recent_sessions(state);
        return render(t);
    }
    let session = if session::is_valid_id(id) {
        session::get(&state.import_sessions, id)
    } else {
        None
    };
    let Some(session) = session else {
        t.error = "That preview has expired. Start a new scan.".to_string();
        t.recent = recent_sessions(state);
        return render(t);
    };

    t.session_id = session.id.clone();
    t.root = session.root.display().to_string();
    t.mode_label = session.mode.label();
    t.mode_past = session.mode.past_tense();
    t.follow_symlinks = session.follow_symlinks;
    t.include_hidden = session.include_hidden;
    t.notice = notice;
    match &session.status {
        SessionStatus::Scanning => {
            t.step = "scanning";
            t.progress_id = session.id.clone();
        }
        SessionStatus::Importing => {
            t.step = "importing";
            t.progress_id = import_progress_id(&session.id);
        }
        SessionStatus::Done(report) => {
            t.step = "done";
            t.report_size = human_bytes(report.bytes_written);
            t.report = (**report).clone();
        }
        SessionStatus::Failed(msg) => {
            t.step = "failed";
            t.failed_message = msg.clone();
        }
        SessionStatus::Ready => {
            t.step = "ready";
            let (cards, views) = build_cards(&session, &ctx);
            let summary = preview::summarize(&session, &views);
            t.write_size = human_bytes(summary.write_bytes);
            t.summary = summary;
            t.groups = cards;
            t.stats = session.stats.clone();
            t.total_size = human_bytes(session.stats.total_bytes);
            t.cross_fs_warning =
                session.mode == ImportMode::Hardlink && session.cross_fs == Some(true);
            t.unmatched = session
                .unmatched_files
                .iter()
                .map(|f| UnmatchedView {
                    rel_path: f.rel_path.clone(),
                    episode_label: preview::episode_label(f.episode, f.episode_count),
                    quality_label: f.quality_label.clone(),
                    size: human_bytes(f.size_bytes),
                })
                .collect();
        }
    }
    render(t)
}

fn checkbox_on(v: &Option<String>) -> bool {
    v.as_deref()
        .is_some_and(|s| matches!(s, "1" | "on" | "true"))
}

/// Start-form validation. Returns the session to start or the message
/// to show. Path checks are a couple of `stat`s; fine on the request
/// path.
fn validate_start(form: &StartForm, media_root: &str) -> Result<ImportSession, String> {
    let path = form.path.trim();
    if media_root.is_empty() {
        return Err(
            "Set a media root under Settings before importing. Imported files are placed under it."
                .to_string(),
        );
    }
    if path.is_empty() {
        return Err("Enter the folder to scan.".to_string());
    }
    let root = PathBuf::from(path);
    if !root.is_absolute() {
        return Err("Enter an absolute path, for example /mnt/anime.".to_string());
    }
    if !root.is_dir() {
        return Err(format!(
            "{} is not a folder Ryokan can read. Check the path, and if Ryokan runs in Docker make sure the folder is mounted into the container.",
            root.display()
        ));
    }
    let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let canonical_media =
        std::fs::canonicalize(media_root).unwrap_or_else(|_| PathBuf::from(media_root));
    if canonical_root.starts_with(&canonical_media) {
        return Err(format!(
            "{} is inside your Ryokan media root ({}). Ryokan already tracks files there; point the wizard at a folder outside it.",
            root.display(),
            media_root
        ));
    }
    let mode = if form.mode.trim().is_empty() {
        ImportMode::Hardlink
    } else {
        ImportMode::parse(&form.mode).ok_or_else(|| "Unknown import mode.".to_string())?
    };
    Ok(ImportSession::new(
        session::mint_id(),
        root,
        mode,
        checkbox_on(&form.follow_symlinks),
        checkbox_on(&form.include_hidden),
    ))
}

pub async fn start(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<StartForm>,
) -> Response {
    let ctx = render_context(&state).await;
    match validate_start(&form, &ctx.media_root) {
        Ok(session) => {
            let id = session.id.clone();
            manual_import::start_preview(&state, session).await;
            htmx_aware_redirect(is_htmx, &session_url(&id))
        }
        Err(msg) => {
            let mut t = base_template(&ctx);
            t.error = msg;
            t.recent = recent_sessions(&state);
            t.form = StartFormView {
                path: form.path.trim().to_string(),
                mode: if ImportMode::parse(&form.mode).is_some() {
                    form.mode.trim().to_ascii_lowercase()
                } else {
                    "hardlink".to_string()
                },
                follow_symlinks: checkbox_on(&form.follow_symlinks),
                include_hidden: checkbox_on(&form.include_hidden),
            };
            render(t).into_response()
        }
    }
}

/// Apply one override action. Errors are user-facing strings the card
/// shows inline.
async fn apply_group_action(
    state: &AppState,
    session_id: &str,
    idx: usize,
    form: &GroupActionForm,
) -> Result<(), String> {
    let set_group = |f: &dyn Fn(&mut manual_import::SeriesGroup)| -> Result<(), String> {
        session::update(&state.import_sessions, session_id, |s| {
            match s.groups.get_mut(idx) {
                Some(g) => {
                    f(g);
                    Ok(())
                }
                None => Err("Unknown series in this preview".to_string()),
            }
        })
        .unwrap_or_else(|| Err("Preview session not found".to_string()))
    };
    match form.action.as_str() {
        "skip" => set_group(&|g| g.skipped = true),
        "unskip" => set_group(&|g| g.skipped = false),
        "pick" => {
            let Some(candidate) = form.candidate else {
                return Err("Pick a series".to_string());
            };
            manual_import::pick_candidate(state, session_id, idx, Some(candidate)).await
        }
        "unpick" => manual_import::pick_candidate(state, session_id, idx, None).await,
        "pick_id" => {
            let Some(id) = form.id else {
                return Err("Pick a series".to_string());
            };
            manual_import::pick_by_id(state, session_id, idx, id).await
        }
        "research" => manual_import::research_group(state, session_id, idx, &form.query).await,
        "toggle_file" => {
            let Some(file) = form.file else {
                return Err("Unknown file".to_string());
            };
            set_group(&|g| {
                if let Some(f) = g.files.get_mut(file) {
                    f.selected = !f.selected;
                }
            })
        }
        "select_all" => set_group(&|g| g.files.iter_mut().for_each(|f| f.selected = true)),
        "select_none" => set_group(&|g| g.files.iter_mut().for_each(|f| f.selected = false)),
        other => Err(format!("Unknown action {other}")),
    }
}

pub async fn group_action(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path((session_id, idx)): Path<(String, usize)>,
    Form(form): Form<GroupActionForm>,
) -> Response {
    // A malformed id never reaches a URL (the plain-POST redirect
    // would panic on a control character); an expired one goes to
    // the page, which explains and offers a fresh start.
    if !session::is_valid_id(&session_id) {
        return htmx_aware_redirect(is_htmx, "/system/import");
    }
    if !session::exists(&state.import_sessions, &session_id) {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    }
    let action_error = apply_group_action(&state, &session_id, idx, &form)
        .await
        .err()
        .unwrap_or_default();

    if !is_htmx {
        return htmx_aware_redirect(
            false,
            &format!("{}#import-group-{}", session_url(&session_id), idx),
        );
    }
    let Some(session) = session::get(&state.import_sessions, &session_id) else {
        return htmx_aware_redirect(true, &session_url(&session_id));
    };
    let Some(group) = session.groups.get(idx) else {
        return htmx_aware_redirect(true, &session_url(&session_id));
    };
    let ctx = render_context(&state).await;
    let view = preview::project_group(group, &ctx.projection());
    let firsts = duplicate_picks(&session);
    let card = build_card(
        idx,
        group,
        view,
        &ctx.title_pref,
        duplicate_for(&firsts, group),
        action_error,
    );
    let partial = GroupPartial {
        session_id: session_id.clone(),
        g: card,
    };
    // The card, then the summary strip and confirm bar out-of-band so
    // the totals follow every skip / tick / pick.
    let html = format!(
        "{}{}",
        partial.render().unwrap_or_default(),
        oob_totals(&session, &ctx)
    );
    Html(html).into_response()
}

/// The picker's result list: the ranked candidates when `q` is empty,
/// a live provider search otherwise. Always 200 so the swap lands;
/// failures render inline.
pub async fn picker_candidates(
    State(state): State<AppState>,
    Path((session_id, idx)): Path<(String, usize)>,
    Query(q): Query<PickerQuery>,
) -> Html<String> {
    // One character fires a provider search against the shared
    // 30/min budget for nothing useful; treat it as "no query".
    let query = match q.q.trim() {
        t if t.chars().count() >= 2 => t.to_string(),
        _ => String::new(),
    };
    let ctx = render_context(&state).await;
    let group = if session::is_valid_id(&session_id) {
        session::get(&state.import_sessions, &session_id).and_then(|s| s.groups.get(idx).cloned())
    } else {
        None
    };
    let Some(group) = group else {
        let partial = PickerResultsPartial {
            session_id,
            g: PickerCtx {
                idx,
                query,
                picker_rows: Vec::new(),
                picker_error: "This preview has expired. Start a new scan.".to_string(),
            },
        };
        return Html(partial.render().unwrap_or_default());
    };
    let (rows, error) = if query.is_empty() {
        (picker_rows(&group, &ctx.title_pref), String::new())
    } else {
        match manual_import::live_search(&state, &session_id, idx, &query).await {
            Ok(hits) => {
                let current = group.picked().map(|e| e.id);
                (
                    hits.iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let mut v = candidate_view(i, e, &ctx.title_pref);
                            v.is_current = current == Some(e.id);
                            v
                        })
                        .collect(),
                    String::new(),
                )
            }
            Err(e) => (Vec::new(), format!("Search failed: {e}")),
        }
    };
    let partial = PickerResultsPartial {
        session_id,
        g: PickerCtx {
            idx,
            query,
            picker_rows: rows,
            picker_error: error,
        },
    };
    Html(partial.render().unwrap_or_default())
}

/// Confirm: start the import job for a Ready preview with something
/// to write. A refusal goes back into the confirm bar under HTMX (the
/// form targets it), or re-renders the page with the notice for a
/// plain POST, so the user always sees why nothing started.
pub async fn confirm(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(session_id): Path<String>,
) -> Response {
    if !session::is_valid_id(&session_id) {
        return htmx_aware_redirect(is_htmx, "/system/import");
    }
    let Some(session) = session::get(&state.import_sessions, &session_id) else {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    };
    if session.status != SessionStatus::Ready {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    }
    let ctx = render_context(&state).await;
    let views = project_views(&session, &ctx);
    let summary = preview::summarize(&session, &views);

    let refusal: Option<String> = if ctx.media_root.is_empty() {
        Some("Set a media root under Settings before importing.".to_string())
    } else if summary.writes == 0 {
        Some(
            "Nothing to import: every file is excluded, already present, or unmatched.".to_string(),
        )
    } else {
        start_import(&state, &session_id, ImportOptions::default())
            .await
            .err()
    };
    let Some(msg) = refusal else {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    };
    if is_htmx {
        let bar = ConfirmBarPartial {
            session_id: session.id.clone(),
            mode_label: session.mode.label(),
            mode_past: session.mode.past_tense(),
            write_size: human_bytes(summary.write_bytes),
            summary,
            error: msg,
            oob: false,
        };
        Html(bar.render().unwrap_or_default()).into_response()
    } else {
        render_session_page(&state, &session_id, msg)
            .await
            .into_response()
    }
}

/// Cancel a running import. The job stops between files; the page
/// shows the partial report when it lands.
pub async fn cancel(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(session_id): Path<String>,
) -> Response {
    if !session::is_valid_id(&session_id) {
        return htmx_aware_redirect(is_htmx, "/system/import");
    }
    session::update(&state.import_sessions, &session_id, |s| {
        if s.status == SessionStatus::Importing {
            s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    htmx_aware_redirect(is_htmx, &session_url(&session_id))
}

pub async fn discard(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(session_id): Path<String>,
) -> Response {
    if session::is_valid_id(&session_id) {
        session::remove(&state.import_sessions, &session_id);
    }
    htmx_aware_redirect(is_htmx, "/system/import")
}

#[cfg(test)]
mod tests;
