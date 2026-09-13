//! Outcome projection for the manual-import preview (#122): given a
//! matched [`SeriesGroup`] and the library's current state, what would
//! the import do to each file?
//!
//! Pure functions over the session. The wizard renders one card per
//! group from a [`GroupView`]; the override controls mutate the group
//! and re-project. Nothing here touches the disk beyond the
//! folder-collision check the caller feeds in.

use std::collections::HashSet;

use super::{ImportSession, SeriesGroup};
use crate::services::library_link::pick_title;
use crate::services::recycle::human_bytes;
use crate::services::{naming, post_processing, source};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    /// AL match, not in the library: the import creates the series.
    New,
    /// AL match that resolves to a tracked series.
    Merge,
    /// AL had nothing (or the search failed).
    NoMatch,
    /// The user excluded the whole group.
    Skipped,
}

impl GroupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Merge => "merge",
            Self::NoMatch => "nomatch",
            Self::Skipped => "skipped",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::New => "New series",
            Self::Merge => "Already in library",
            Self::NoMatch => "No match",
            Self::Skipped => "Skipped",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// Would be imported.
    Import,
    /// Ryokan already has this episode at equal or better quality.
    AlreadyPresent,
    /// A grab for this episode is in flight.
    Downloading,
    /// Ryokan has it at lower quality; the import would replace it.
    WouldReplace,
    /// The existing tag is a manual override; never touched.
    Pinned,
    /// No episode number could be read from the filename.
    NoEpisodeNumber,
    /// Unticked by the user.
    Deselected,
    /// The group has no AL match.
    Unmatched,
    /// The group is skipped.
    Skipped,
    /// A different file already sits at the destination path and
    /// Ryokan has no tag for it (a drop-in, or a folder scanned
    /// twice). Never overwritten: that would bypass the recycle bin.
    AlreadyOnDisk,
    /// Another file in this group lands on the same destination name.
    DuplicateName,
}

impl FileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::AlreadyPresent => "present",
            Self::Downloading => "downloading",
            Self::WouldReplace => "replace",
            Self::Pinned => "pinned",
            Self::NoEpisodeNumber => "no-episode",
            Self::Deselected => "deselected",
            Self::Unmatched => "unmatched",
            Self::Skipped => "skipped",
            Self::AlreadyOnDisk => "on-disk",
            Self::DuplicateName => "duplicate",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Import => "Import",
            Self::AlreadyPresent => "Already have",
            Self::Downloading => "Downloading",
            Self::WouldReplace => "Replace",
            Self::Pinned => "Pinned",
            Self::NoEpisodeNumber => "No episode number",
            Self::Deselected => "Excluded",
            Self::Unmatched => "No match",
            Self::Skipped => "Skipped",
            Self::AlreadyOnDisk => "Already on disk",
            Self::DuplicateName => "Duplicate name",
        }
    }

    /// Counts toward "will be written".
    pub fn writes(self) -> bool {
        matches!(self, Self::Import | Self::WouldReplace)
    }
}

#[derive(Clone, Debug)]
pub struct FileView {
    pub idx: usize,
    pub rel_path: String,
    pub file_name: String,
    /// `E07`, or `-` when no episode number parsed.
    pub episode_label: String,
    /// `was E18` when the TMDB mapping renumbered the file. Empty
    /// otherwise.
    pub episode_note: String,
    pub quality_label: String,
    /// What Ryokan holds for this episode already, for the Replace /
    /// Already-have rows. Empty otherwise.
    pub existing_quality: String,
    pub status: FileStatus,
    pub status_class: &'static str,
    pub status_label: &'static str,
    /// Projected destination under the media root. Empty when nothing
    /// would be written.
    pub dest: String,
    pub selected: bool,
    pub size: String,
    pub size_bytes: u64,
    pub title_from_folder: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupCounts {
    pub import: usize,
    pub present: usize,
    pub downloading: usize,
    pub replace: usize,
    pub pinned: usize,
    pub no_episode: usize,
    pub deselected: usize,
    /// Untagged file already at the destination, left alone.
    pub on_disk: usize,
    /// Second file in the group with the same destination name.
    pub duplicate: usize,
    /// Bytes across the files that would be written.
    pub write_bytes: u64,
}

impl GroupCounts {
    pub fn writes(&self) -> usize {
        self.import + self.replace
    }
}

#[derive(Clone, Debug)]
pub struct GroupView {
    pub kind: GroupKind,
    /// Folder under the media root the files would land in. For a new
    /// series this is the auto-generated name (suffixed on collision).
    pub folder_name: String,
    /// A folder of the plain name already exists under the media root
    /// and no series row owns it, so the import would suffix.
    pub folder_collision: bool,
    pub files: Vec<FileView>,
    pub counts: GroupCounts,
}

/// Library state the projection needs, gathered once per render.
pub struct ProjectionContext<'a> {
    pub media_root: &'a str,
    /// `series.folder_name` for every tracked series.
    pub owned_folders: &'a HashSet<String>,
    /// Top-level directories currently under the media root.
    pub disk_folders: &'a HashSet<String>,
    /// `config.title_language`.
    pub title_pref: &'a str,
    /// Naming templates (#124): the folder a new series gets and the
    /// season folder every file lands in.
    pub series_folder_format: &'a str,
    pub season_folder_format: &'a str,
}

/// The folder name `series::upsert` would generate for an AL entry:
/// the series-folder template in the preferred title language, so the
/// preview shows the folder the import actually creates.
pub fn default_folder_name(
    entry: &crate::services::anilist::AnimeEntry,
    ctx: &ProjectionContext<'_>,
) -> String {
    naming::series_folder(
        ctx.series_folder_format,
        ctx.title_pref,
        &naming::SeriesNames::from_entry(entry),
    )
}

/// The season folder a group's files land in (#124).
pub fn season_folder_name(group: &SeriesGroup, ctx: &ProjectionContext<'_>) -> String {
    let names = match (&group.existing, group.picked()) {
        (Some(existing), _) => naming::SeriesNames {
            title: &existing.title,
            romaji: &existing.title_romaji,
            english: &existing.title_english,
            native: &existing.title_native,
            year: existing.season_year,
        },
        (None, Some(entry)) => naming::SeriesNames::from_entry(entry),
        (None, None) => naming::SeriesNames::default(),
    };
    naming::season_folder(ctx.season_folder_format, ctx.title_pref, &names, 1)
}

/// A folder name that collides with nothing: the plain name when it is
/// free (or owned by a series row, which is a merge not a collision),
/// otherwise ` (2)`, ` (3)`, ... Returns whether a suffix was needed.
pub fn unique_folder_name(base: &str, ctx: &ProjectionContext<'_>) -> (String, bool) {
    let taken = |name: &str| ctx.disk_folders.contains(name) && !ctx.owned_folders.contains(name);
    if !taken(base) {
        return (base.to_string(), false);
    }
    for n in 2..1000 {
        let candidate = format!("{base} ({n})");
        if !taken(&candidate) {
            return (candidate, true);
        }
    }
    (format!("{base} ({})", ctx.disk_folders.len() + 2), true)
}

/// `E18`, `E05-E06` for a file holding several episodes (issue #246),
/// or `-` with no episode number. No season in the label: each AniList
/// season is its own series in Ryokan with its own E1..En, and the
/// card's season chip says which one this group is.
pub fn episode_label(episode: Option<i32>, count: i32) -> String {
    match episode {
        Some(e) if count > 1 => format!("E{:02}-E{:02}", e, e + count - 1),
        Some(e) => format!("E{:02}", e),
        None => "-".to_string(),
    }
}

/// Destination for the preview, relative to the media root: every
/// file lands under it, so repeating the root on every row is noise.
/// The import job builds the absolute path itself.
fn dest_for(media_root: &str, folder: &str, season_folder: &str, file_name: &str) -> String {
    if media_root.is_empty() || folder.is_empty() {
        return String::new();
    }
    format!("{folder}/{season_folder}/{file_name}")
}

pub fn project_group(group: &SeriesGroup, ctx: &ProjectionContext<'_>) -> GroupView {
    let picked = group.picked();
    let kind = if group.skipped {
        GroupKind::Skipped
    } else if picked.is_none() {
        GroupKind::NoMatch
    } else if group.existing.is_some() {
        GroupKind::Merge
    } else {
        GroupKind::New
    };

    let (folder_name, folder_collision) = match (&group.existing, picked) {
        (Some(existing), _) => (existing.folder_name.clone(), false),
        (None, Some(entry)) => unique_folder_name(&default_folder_name(entry, ctx), ctx),
        (None, None) => (String::new(), false),
    };
    let season_folder = season_folder_name(group, ctx);

    // Only a folder that exists can hold a stranger at a destination
    // path; a new series' folder doesn't, so no stat calls for those.
    let folder_on_disk = !folder_name.is_empty() && ctx.disk_folders.contains(&folder_name);
    let mut names_taken: HashSet<&str> = HashSet::new();
    let mut counts = GroupCounts::default();
    let files = group
        .files
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let mut existing_quality = String::new();
            let mut status = match kind {
                GroupKind::Skipped => FileStatus::Skipped,
                GroupKind::NoMatch => FileStatus::Unmatched,
                GroupKind::New | GroupKind::Merge => {
                    if !f.selected {
                        FileStatus::Deselected
                    } else if f.episode.is_none() {
                        FileStatus::NoEpisodeNumber
                    } else {
                        let tag = group
                            .existing
                            .as_ref()
                            .and_then(|e| e.tags.get(&f.episode.unwrap_or_default()));
                        match tag {
                            None => FileStatus::Import,
                            Some(t) => {
                                existing_quality = t.quality_label.clone();
                                if t.manual_override {
                                    FileStatus::Pinned
                                } else if t.state == "grabbed" {
                                    FileStatus::Downloading
                                } else if t.state != "completed" {
                                    // A failed / cleared tag holds no
                                    // file; import as new.
                                    FileStatus::Import
                                } else {
                                    let incoming =
                                        source::classify_release_sync(&f.file_name, None);
                                    if source::is_valid_upgrade(&t.classification, &incoming) {
                                        FileStatus::WouldReplace
                                    } else {
                                        FileStatus::AlreadyPresent
                                    }
                                }
                            }
                        }
                    }
                }
            };
            if status == FileStatus::Import && folder_on_disk {
                let dest = std::path::Path::new(ctx.media_root)
                    .join(&folder_name)
                    .join(&season_folder)
                    .join(&f.file_name);
                if dest.exists() && !post_processing::files_share_inode(&f.path, &dest) {
                    status = FileStatus::AlreadyOnDisk;
                }
            }
            if status.writes() && !names_taken.insert(f.file_name.as_str()) {
                status = FileStatus::DuplicateName;
            }
            match status {
                FileStatus::Import => counts.import += 1,
                FileStatus::AlreadyPresent => counts.present += 1,
                FileStatus::Downloading => counts.downloading += 1,
                FileStatus::WouldReplace => counts.replace += 1,
                FileStatus::Pinned => counts.pinned += 1,
                FileStatus::NoEpisodeNumber => counts.no_episode += 1,
                FileStatus::Deselected => counts.deselected += 1,
                FileStatus::AlreadyOnDisk => counts.on_disk += 1,
                FileStatus::DuplicateName => counts.duplicate += 1,
                FileStatus::Unmatched | FileStatus::Skipped => {}
            }
            if status.writes() {
                counts.write_bytes += f.size_bytes;
            }
            let dest = if status.writes() {
                dest_for(ctx.media_root, &folder_name, &season_folder, &f.file_name)
            } else {
                String::new()
            };
            FileView {
                idx,
                rel_path: f.rel_path.clone(),
                file_name: f.file_name.clone(),
                episode_label: episode_label(f.episode, f.episode_count),
                episode_note: f
                    .source_episode
                    .map(|e| format!("was E{e:02}"))
                    .unwrap_or_default(),
                quality_label: f.quality_label.clone(),
                existing_quality,
                status,
                status_class: status.as_str(),
                status_label: status.label(),
                dest,
                selected: f.selected,
                size: human_bytes(f.size_bytes),
                size_bytes: f.size_bytes,
                title_from_folder: f.title_source == super::parse::TitleSource::ParentFolder,
            }
        })
        .collect();

    GroupView {
        kind,
        folder_name,
        folder_collision,
        files,
        counts,
    }
}

/// Display title for an AL entry in the user's preferred language.
pub fn entry_title<'a>(entry: &'a crate::services::anilist::AnimeEntry, pref: &str) -> &'a str {
    pick_title(
        pref,
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    )
}

/// Roll-up across the whole preview for the summary strip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSummary {
    pub groups: usize,
    pub new_series: usize,
    pub merge: usize,
    pub no_match: usize,
    pub skipped: usize,
    pub files: usize,
    pub writes: usize,
    pub present: usize,
    pub replace: usize,
    pub no_episode: usize,
    pub unmatched_files: usize,
    pub write_bytes: u64,
    /// Series that need the user's eye: low-confidence matches and
    /// search errors on non-skipped groups.
    pub needs_attention: usize,
}

pub fn summarize(session: &ImportSession, views: &[GroupView]) -> SessionSummary {
    let mut s = SessionSummary {
        groups: session.groups.len(),
        files: session.stats.files,
        unmatched_files: session.unmatched_files.len(),
        ..Default::default()
    };
    for (g, v) in session.groups.iter().zip(views) {
        match v.kind {
            GroupKind::New => s.new_series += 1,
            GroupKind::Merge => s.merge += 1,
            GroupKind::NoMatch => s.no_match += 1,
            GroupKind::Skipped => s.skipped += 1,
        }
        s.writes += v.counts.writes();
        s.present += v.counts.present;
        s.replace += v.counts.replace;
        s.no_episode += v.counts.no_episode;
        s.write_bytes += v.counts.write_bytes;
        if !g.skipped && (g.low_confidence || g.search_error.is_some()) {
            s.needs_attention += 1;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anilist::AnimeEntry;
    use crate::services::manual_import::{
        CandidateFile, ExistingSeries, ExistingTag, parse::TitleSource,
    };
    use crate::services::source::{ClassificationResult, Resolution, Source};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn entry(id: i64, english: &str, romaji: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: romaji.into(),
            title_english: english.into(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    fn file(name: &str, episode: Option<i32>) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from(name),
            rel_path: format!("Show/{name}"),
            file_name: name.into(),
            size_bytes: 100,
            parsed_title: Some("Show".into()),
            title_source: TitleSource::Filename,
            season: None,
            episode,
            year: None,
            group: None,
            quality_label: source::classify_release_sync(name, None).label(),
            selected: true,
            episode_count: 1,
            source_episode: None,
        }
    }

    fn group(
        files: Vec<CandidateFile>,
        candidates: Vec<AnimeEntry>,
        pick: Option<usize>,
    ) -> SeriesGroup {
        SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files,
            candidates,
            pick,
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            resolved_by_id: false,
            mapping_note: None,
            search_results: Vec::new(),
        }
    }

    fn stored(source: Source, res: Resolution, state: &str, pinned: bool) -> ExistingTag {
        let classification = ClassificationResult {
            source,
            resolution: res,
            ..source::classify_release_sync("", None)
        };
        ExistingTag {
            quality_label: classification.label(),
            state: state.into(),
            manual_override: pinned,
            classification,
        }
    }

    fn ctx<'a>(owned: &'a HashSet<String>, disk: &'a HashSet<String>) -> ProjectionContext<'a> {
        ProjectionContext {
            media_root: "/media",
            owned_folders: owned,
            disk_folders: disk,
            title_pref: "english",
            series_folder_format: naming::DEFAULT_SERIES_FOLDER_FORMAT,
            season_folder_format: naming::DEFAULT_SEASON_FOLDER_FORMAT,
        }
    }

    #[test]
    fn new_series_projects_import_rows_with_dest_and_folder() {
        let owned = HashSet::new();
        let disk = HashSet::new();
        let g = group(
            vec![
                file("[G] Show - 01 [1080p].mkv", Some(1)),
                file("[G] Show - NCOP.mkv", None),
            ],
            vec![entry(1, "Show: The Series", "Show")],
            Some(0),
        );
        let v = project_group(&g, &ctx(&owned, &disk));
        assert_eq!(v.kind, GroupKind::New);
        assert_eq!(v.folder_name, "Show_ The Series");
        assert!(!v.folder_collision);
        assert_eq!(v.files[0].status, FileStatus::Import);
        assert_eq!(v.files[0].episode_label, "E01");
        assert_eq!(
            v.files[0].dest,
            "Show_ The Series/Season 01/[G] Show - 01 [1080p].mkv"
        );
        assert_eq!(v.files[1].status, FileStatus::NoEpisodeNumber);
        assert!(v.files[1].dest.is_empty());
        assert_eq!(v.counts.import, 1);
        assert_eq!(v.counts.no_episode, 1);
        assert_eq!(v.counts.write_bytes, 100);
    }

    #[test]
    fn folder_collision_suffixes_unless_owned() {
        let mut disk = HashSet::new();
        disk.insert("Show".to_string());
        let owned = HashSet::new();
        let (name, collided) = unique_folder_name("Show", &ctx(&owned, &disk));
        assert_eq!(name, "Show (2)");
        assert!(collided);

        let mut owned = HashSet::new();
        owned.insert("Show".to_string());
        let (name, collided) = unique_folder_name("Show", &ctx(&owned, &disk));
        assert_eq!(name, "Show");
        assert!(!collided);
    }

    #[test]
    fn merge_projects_present_replace_pinned_downloading() {
        let owned = HashSet::new();
        let disk = HashSet::new();
        let mut tags = HashMap::new();
        tags.insert(
            1,
            stored(Source::BluRay, Resolution::R1080p, "completed", false),
        );
        tags.insert(
            2,
            stored(Source::Web, Resolution::R720p, "completed", false),
        );
        tags.insert(3, stored(Source::Web, Resolution::R720p, "completed", true));
        tags.insert(4, stored(Source::Web, Resolution::R1080p, "grabbed", false));
        tags.insert(5, stored(Source::Web, Resolution::R1080p, "failed", false));
        let mut g = group(
            vec![
                file("[G] Show - 01 [WEB 1080p].mkv", Some(1)),
                file("[G] Show - 02 [BD 1080p].mkv", Some(2)),
                file("[G] Show - 03 [BD 1080p].mkv", Some(3)),
                file("[G] Show - 04 [BD 1080p].mkv", Some(4)),
                file("[G] Show - 05 [BD 1080p].mkv", Some(5)),
                file("[G] Show - 06 [BD 1080p].mkv", Some(6)),
            ],
            vec![entry(1, "Show", "Show")],
            Some(0),
        );
        g.existing = Some(ExistingSeries {
            id: 7,
            anilist_id: 1,
            title: "Show".into(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            season_year: None,
            folder_name: "Show Folder".into(),
            tags,
        });
        let v = project_group(&g, &ctx(&owned, &disk));
        assert_eq!(v.kind, GroupKind::Merge);
        assert_eq!(v.folder_name, "Show Folder");
        let statuses: Vec<FileStatus> = v.files.iter().map(|f| f.status).collect();
        assert_eq!(
            statuses,
            vec![
                FileStatus::AlreadyPresent,
                FileStatus::WouldReplace,
                FileStatus::Pinned,
                FileStatus::Downloading,
                FileStatus::Import,
                FileStatus::Import,
            ]
        );
        assert_eq!(v.files[0].existing_quality, "BD-1080p");
        assert_eq!(
            v.files[1].dest,
            "Show Folder/Season 01/[G] Show - 02 [BD 1080p].mkv"
        );
        assert_eq!(v.counts.present, 1);
        assert_eq!(v.counts.replace, 1);
        assert_eq!(v.counts.pinned, 1);
        assert_eq!(v.counts.downloading, 1);
        assert_eq!(v.counts.import, 2);
        assert_eq!(v.counts.writes(), 3);
    }

    #[test]
    fn stranger_at_the_destination_and_duplicate_names_are_never_written() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let season = media.join("Show Folder").join("Season 01");
        std::fs::create_dir_all(&season).unwrap();
        // An untagged file already at the destination name (a drop-in
        // the classify sweep hasn't seen), and a source file that is
        // a hardlink of its destination (a prior import: fine).
        std::fs::write(season.join("[G] Show - 01 [BD 1080p].mkv"), b"stranger").unwrap();
        let src_linked = tmp.path().join("src").join("[G] Show - 02 [BD 1080p].mkv");
        std::fs::create_dir_all(src_linked.parent().unwrap()).unwrap();
        std::fs::write(&src_linked, b"same").unwrap();
        std::fs::hard_link(&src_linked, season.join("[G] Show - 02 [BD 1080p].mkv")).unwrap();

        let mut f1 = file("[G] Show - 01 [BD 1080p].mkv", Some(1));
        f1.path = tmp.path().join("src").join("[G] Show - 01 [BD 1080p].mkv");
        std::fs::write(&f1.path, b"incoming").unwrap();
        let mut f2 = file("[G] Show - 02 [BD 1080p].mkv", Some(2));
        f2.path = src_linked.clone();
        let mut f3 = file("[G] Show - 03 [BD 1080p].mkv", Some(3));
        f3.path = tmp
            .path()
            .join("src")
            .join("a")
            .join("[G] Show - 03 [BD 1080p].mkv");
        let mut f3b = file("[G] Show - 03 [BD 1080p].mkv", Some(4));
        f3b.path = tmp
            .path()
            .join("src")
            .join("b")
            .join("[G] Show - 03 [BD 1080p].mkv");

        let mut g = group(
            vec![f1, f2, f3, f3b],
            vec![entry(1, "Show", "Show")],
            Some(0),
        );
        g.existing = Some(ExistingSeries {
            id: 7,
            anilist_id: 1,
            title: "Show".into(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            season_year: None,
            folder_name: "Show Folder".into(),
            tags: HashMap::new(),
        });
        let owned: HashSet<String> = ["Show Folder".to_string()].into_iter().collect();
        let disk: HashSet<String> = ["Show Folder".to_string()].into_iter().collect();
        let media_s = media.to_string_lossy().into_owned();
        let ctx = ProjectionContext {
            media_root: &media_s,
            owned_folders: &owned,
            disk_folders: &disk,
            title_pref: "english",
            series_folder_format: naming::DEFAULT_SERIES_FOLDER_FORMAT,
            season_folder_format: naming::DEFAULT_SEASON_FOLDER_FORMAT,
        };
        let v = project_group(&g, &ctx);
        let statuses: Vec<FileStatus> = v.files.iter().map(|f| f.status).collect();
        assert_eq!(
            statuses,
            vec![
                FileStatus::AlreadyOnDisk,
                FileStatus::Import,
                FileStatus::Import,
                FileStatus::DuplicateName,
            ]
        );
        assert_eq!(v.counts.on_disk, 1);
        assert_eq!(v.counts.duplicate, 1);
        assert_eq!(v.counts.writes(), 2);
        assert!(v.files[0].dest.is_empty(), "nothing written for a stranger");
    }

    #[test]
    fn deselected_skipped_and_nomatch_rows() {
        let owned = HashSet::new();
        let disk = HashSet::new();
        let mut g = group(
            vec![
                file("Show - 01.mkv", Some(1)),
                file("Show - 02.mkv", Some(2)),
            ],
            vec![entry(1, "Show", "Show")],
            Some(0),
        );
        g.files[1].selected = false;
        let v = project_group(&g, &ctx(&owned, &disk));
        assert_eq!(v.files[1].status, FileStatus::Deselected);
        assert_eq!(v.counts.deselected, 1);
        assert_eq!(v.counts.import, 1);

        g.skipped = true;
        let v = project_group(&g, &ctx(&owned, &disk));
        assert_eq!(v.kind, GroupKind::Skipped);
        assert!(v.files.iter().all(|f| f.status == FileStatus::Skipped));
        assert_eq!(v.counts.writes(), 0);

        g.skipped = false;
        g.pick = None;
        let v = project_group(&g, &ctx(&owned, &disk));
        assert_eq!(v.kind, GroupKind::NoMatch);
        assert!(v.files.iter().all(|f| f.status == FileStatus::Unmatched));
        assert!(v.folder_name.is_empty());
    }
}
