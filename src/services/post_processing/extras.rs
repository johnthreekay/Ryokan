//! Extra files that travel with a video: subtitles, imported next to
//! the episode they belong to (Sonarr's "Import Extra Files").
//!
//! After a video lands, the download's own file list (the folder
//! walk when the client gave none) is searched for files whose
//! extension is in `config.extra_file_extensions`. A subtitle belongs
//! to the video when its name starts with the video's name, when its
//! own parsed episode span equals the video's, when its parent folder
//! is named that way (Erai-raws' `Subs/<episode name>/<lang>.ass`), or,
//! when nothing matched at all and the download holds a single video,
//! always.
//! It is written into the season folder as
//! `<episode file stem>[.N][.<lang>][.<tags>].<ext>` with the same
//! file operation as the video (hardlink, copy, or move), so the
//! recycle bin's `<stem>.` companion sweep retires it with the video.
//! The library scanner never reads these, and nothing is recorded in
//! the database for them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::services::media;

/// The default for `config.extra_file_extensions`: SubRip and the
/// ASS/SSA styled subtitles fansub groups ship.
pub const DEFAULT_EXTRA_FILE_EXTENSIONS: &str = "srt,ass";

/// Below this many videos in the download a subtitle that matches no
/// video by name still belongs to the one video there.
const SINGLE_VIDEO_FALLBACK_MAX: usize = 1;

/// How deep under the video's folder subtitles are looked for: a
/// pack's `Subs/` subfolder, and one level below it.
const MAX_DEPTH: u32 = 2;

/// Lowercase, dot-less, deduplicated extensions from the comma list
/// the settings page stores.
pub fn parse_extensions(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in list.split(',') {
        let ext = raw.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() || ext.contains(['/', '\\']) || out.contains(&ext) {
            continue;
        }
        // A video extension here would copy the pack's other episodes
        // into the season folder as phantom files.
        if super::is_video_file(&format!("x.{ext}")) {
            continue;
        }
        out.push(ext);
    }
    out
}

/// What one video's subtitle import did.
#[derive(Debug, Default)]
pub struct ExtrasReport {
    /// Destinations written.
    pub imported: Vec<PathBuf>,
    /// Matching files left alone because the destination already existed.
    pub skipped: usize,
    /// `source: error` lines.
    pub errors: Vec<String>,
}

/// Two-letter code for a language token in a subtitle file name, or
/// `None` for anything that is not a language.
fn language_code(token: &str) -> Option<&'static str> {
    Some(match token {
        "en" | "eng" | "english" => "en",
        "ja" | "jp" | "jpn" | "japanese" => "ja",
        "es" | "spa" | "spanish" | "es-la" | "es-es" | "es-419" | "lat" | "latino" => "es",
        "pt" | "por" | "portuguese" | "pt-br" | "pt-pt" | "ptbr" | "brazilian" => "pt",
        "fr" | "fre" | "fra" | "french" => "fr",
        "de" | "ger" | "deu" | "german" => "de",
        "it" | "ita" | "italian" => "it",
        "ru" | "rus" | "russian" => "ru",
        "ar" | "ara" | "arabic" => "ar",
        "zh" | "chi" | "zho" | "chinese" | "zh-cn" | "zh-tw" | "zh-hans" | "zh-hant" => "zh",
        "ko" | "kor" | "korean" => "ko",
        "nl" | "dut" | "nld" | "dutch" => "nl",
        "pl" | "pol" | "polish" => "pl",
        "tr" | "tur" | "turkish" => "tr",
        "id" | "ind" | "indonesian" => "id",
        "th" | "tha" | "thai" => "th",
        "vi" | "vie" | "vietnamese" => "vi",
        "sv" | "swe" | "swedish" => "sv",
        "da" | "dan" | "danish" => "da",
        "fi" | "fin" | "finnish" => "fi",
        "hu" | "hun" | "hungarian" => "hu",
        "cs" | "cze" | "ces" | "czech" => "cs",
        "el" | "gre" | "ell" | "greek" => "el",
        "he" | "heb" | "hebrew" => "he",
        "ms" | "may" | "msa" | "malay" => "ms",
        "ro" | "rum" | "ron" | "romanian" => "ro",
        "uk" | "ukr" | "ukrainian" => "uk",
        "hin" | "hindi" => "hi",
        "fil" | "tgl" | "filipino" | "tagalog" => "fil",
        _ => return None,
    })
}

/// Tags that ride along after the language: forced, hearing-impaired
/// (`sdh`, `cc`, and Bazarr's / Plex's `hi`, which is therefore not
/// Hindi here).
fn is_subtitle_tag(token: &str) -> bool {
    matches!(token, "forced" | "sdh" | "cc" | "hi")
}

/// Language and tags read from the end of a subtitle's name, after
/// the video's own name when it carries it: `Show - 01.eng.forced.srt`
/// next to `Show - 01.mkv` reads `en` + `forced`. Only the trailing
/// tokens count, tags first and then one language, so a word inside a
/// title (`De Profundis`, the romaji `no`) is never read as one.
fn language_and_tags(subtitle_stem: &str, video_stem: &str) -> (Option<&'static str>, Vec<String>) {
    let lower = subtitle_stem.to_ascii_lowercase();
    let video_lower = video_stem.to_ascii_lowercase();
    let tail: &str = if !video_lower.is_empty() && lower.starts_with(&video_lower) {
        &lower[video_lower.len()..]
    } else {
        &lower
    };
    let tokens: Vec<&str> = tail
        .split(['.', '_', ' ', '[', ']', '(', ')'])
        .filter(|t| !t.is_empty())
        .collect();
    let mut tags: Vec<String> = Vec::new();
    let mut language = None;
    for token in tokens.iter().rev() {
        if is_subtitle_tag(token) {
            if !tags.iter().any(|t| t == token) {
                tags.insert(0, token.to_string());
            }
            continue;
        }
        language = language_code(token);
        break;
    }
    (language, tags)
}

/// Files under `dir` (up to [`MAX_DEPTH`]) with one of `extensions`,
/// for a download whose client gave no file list. Symlinks are not
/// followed: the walk stays inside the download.
pub fn walk_extras(dir: &Path, extensions: &[String]) -> Vec<PathBuf> {
    fn recurse(cur: &Path, depth: u32, extensions: &[String], out: &mut Vec<PathBuf>) {
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(cur) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                recurse(&path, depth + 1, extensions, out);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .is_some_and(|e| extensions.contains(&e))
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    recurse(dir, 0, extensions, &mut out);
    out.sort();
    out
}

/// True when `name` (a file stem or a folder name) names the video:
/// it starts with the video's stem, or parses to the same span.
fn names_video(name: &str, video_stem: &str, video_span: Option<media::EpisodeSpan>) -> bool {
    let lower = name.to_ascii_lowercase();
    if !video_stem.is_empty() && lower.starts_with(video_stem) {
        return true;
    }
    match (video_span, media::parse_episode_span(&lower)) {
        (Some(v), Some(s)) => {
            s.first == v.first && s.last == v.last && s.season.unwrap_or(1) == v.season.unwrap_or(1)
        }
        _ => false,
    }
}

/// The subtitle files among `candidates` (the download's own files
/// with a wanted extension) that belong to `video_src`: by the file's
/// name, by its parent folder's name (a pack's `Subs/<episode>/`), or,
/// when nothing matched and the download holds at most one video, all
/// of them. `video_span` is the span the video's own name parsed to.
pub fn find_subtitles(
    video_src: &Path,
    video_span: Option<media::EpisodeSpan>,
    candidates: &[PathBuf],
    videos_in_download: usize,
) -> Vec<PathBuf> {
    let video_stem = video_src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let video_dir = video_src.parent();
    let mut matched: Vec<PathBuf> = candidates
        .iter()
        .filter(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if names_video(stem, &video_stem, video_span) {
                return true;
            }
            // The parent folder names the episode (Erai-raws' Subs/
            // layout), and it is not the video's own folder.
            p.parent()
                .filter(|dir| Some(*dir) != video_dir)
                .and_then(|dir| dir.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|folder| names_video(folder, &video_stem, video_span))
        })
        .cloned()
        .collect();
    if matched.is_empty() && videos_in_download <= SINGLE_VIDEO_FALLBACK_MAX {
        matched = candidates.to_vec();
    }
    matched.sort();
    matched
}

/// Import the subtitles that belong to `video_src` next to
/// `video_dest`, named after it. `mode` is `config.post_processing_mode`;
/// `candidates` are the download's files with a wanted extension (see
/// [`find_subtitles`]).
pub async fn import_subtitles(
    mode: &str,
    video_src: &Path,
    video_dest: &Path,
    video_span: Option<media::EpisodeSpan>,
    candidates: &[PathBuf],
    videos_in_download: usize,
) -> ExtrasReport {
    let mut report = ExtrasReport::default();
    if candidates.is_empty() {
        return report;
    }
    let found = find_subtitles(video_src, video_span, candidates, videos_in_download);
    if found.is_empty() {
        return report;
    }
    let Some(dest_dir) = video_dest.parent() else {
        return report;
    };
    let dest_stem = video_dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let src_stem = video_src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    // Group by (language, tags, extension) so several copies of the
    // same kind get a running number, the way Sonarr names them.
    struct Candidate {
        path: PathBuf,
        language: Option<&'static str>,
        tags: Vec<String>,
        ext: String,
    }
    let mut groups: HashMap<String, Vec<Candidate>> = HashMap::new();
    for path in found {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let (language, tags) = language_and_tags(&stem, &src_stem);
        let key = format!("{}|{}|{}", language.unwrap_or(""), tags.join("."), ext);
        groups.entry(key).or_default().push(Candidate {
            path,
            language,
            tags,
            ext,
        });
    }
    let mut keys: Vec<String> = groups.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let entries = groups.remove(&key).unwrap_or_default();
        let multiple = entries.len() > 1;
        for (copy, candidate) in entries.into_iter().enumerate() {
            let mut suffix = String::new();
            if multiple {
                suffix.push_str(&format!(".{}", copy + 1));
            }
            if let Some(code) = candidate.language {
                suffix.push('.');
                suffix.push_str(code);
            }
            for tag in &candidate.tags {
                suffix.push('.');
                suffix.push_str(tag);
            }
            let dest = dest_dir.join(format!("{dest_stem}{suffix}.{}", candidate.ext));
            if dest.exists() {
                report.skipped += 1;
                continue;
            }
            match super::do_file_op(mode, &candidate.path, &dest).await {
                Ok(()) => report.imported.push(dest),
                Err(e) => report
                    .errors
                    .push(format!("{}: {e}", candidate.path.display())),
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_parse_lowercase_and_dedupe() {
        assert_eq!(
            parse_extensions("srt, .ASS,ass, ,sub"),
            vec!["srt", "ass", "sub"]
        );
        assert!(parse_extensions("").is_empty());
        // A video extension never rides along.
        assert_eq!(parse_extensions("mkv,srt,mp4"), vec!["srt"]);
    }

    #[test]
    fn language_and_tags_read_the_trailing_tokens_only() {
        let (lang, tags) = language_and_tags("Show - 01.eng.forced", "Show - 01");
        assert_eq!(lang, Some("en"));
        assert_eq!(tags, vec!["forced"]);
        let (lang, tags) = language_and_tags("Show - 01", "Show - 01");
        assert_eq!(lang, None);
        assert!(tags.is_empty());
        let (lang, _) = language_and_tags("Show - 01.pt-BR", "Show - 01");
        assert_eq!(lang, Some("pt"));
        // `hi` is the hearing-impaired tag, not Hindi.
        let (lang, tags) = language_and_tags("Show - 01.en.hi", "Show - 01");
        assert_eq!(lang, Some("en"));
        assert_eq!(tags, vec!["hi"]);
        // A title word is never a language: only the trailing tokens
        // count, and a language has to sit right before the tags.
        let (lang, _) = language_and_tags("Show - 01 - De Profundis", "Show - 01");
        assert_eq!(lang, None);
        let (lang, _) = language_and_tags("Sousou no Frieren - 01 [ja]", "Frieren - 01");
        assert_eq!(lang, Some("ja"));
        let (lang, _) = language_and_tags("Sousou no Frieren - 01", "Frieren - 01");
        assert_eq!(lang, None);
        // Erai-raws' per-episode folder: the file is just the language.
        let (lang, _) = language_and_tags("eng", "[Erai-raws] Show - 01 [1080p]");
        assert_eq!(lang, Some("en"));
    }

    #[test]
    fn subtitles_match_by_name_folder_or_lone_video() {
        let root = Path::new("/downloads/pack");
        let video = root.join("[Group] Show - 01 (1080p).mkv");
        let span = media::parse_episode_span("[group] show - 01 (1080p).mkv");
        let candidates: Vec<PathBuf> = [
            "[Group] Show - 01 (1080p).eng.ass",
            "Subs/[Group] Show - 01 (1080p).jpn.ass",
            "Subs/Show - 01.srt",
            "Subs/Show - 02.srt",
            // Erai-raws: a folder named after the episode.
            "Subs/[Group] Show - 01 (1080p)/eng.ass",
            "Subs/[Group] Show - 02 (1080p)/eng.ass",
            "Subs/misc.srt",
        ]
        .iter()
        .map(|n| root.join(n))
        .collect();
        // Two videos in the download: only the matching ones.
        let found = find_subtitles(&video, span, &candidates, 2);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "Subs/Show - 01.srt".to_string(),
                "Subs/[Group] Show - 01 (1080p)/eng.ass".to_string(),
                "Subs/[Group] Show - 01 (1080p).jpn.ass".to_string(),
                "[Group] Show - 01 (1080p).eng.ass".to_string(),
            ]
        );
        // A lone video takes everything that matched nothing.
        let lone = Path::new("/downloads/one");
        let v = lone.join("Show - 01.mkv");
        let cands = vec![lone.join("english.srt")];
        assert_eq!(
            find_subtitles(&v, media::parse_episode_span("show - 01.mkv"), &cands, 1).len(),
            1
        );
        // The walk never follows a symlink.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.srt"), b"s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("a.srt"), dir.path().join("link.srt")).unwrap();
        let walked = walk_extras(dir.path(), &parse_extensions("srt"));
        assert_eq!(walked.len(), 1);
    }

    #[tokio::test]
    async fn subtitles_are_named_after_the_episode_file_with_language_and_copy() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let video = src_dir.path().join("[Group] Show - 01 (1080p).mkv");
        std::fs::write(&video, b"v").unwrap();
        for name in [
            "[Group] Show - 01 (1080p).eng.ass",
            "[Group] Show - 01 (1080p).eng.forced.ass",
            "[Group] Show - 01 (1080p).ass",
            "[Group] Show - 01 (1080p)[2].ass",
        ] {
            std::fs::write(src_dir.path().join(name), b"s").unwrap();
        }
        let dest = dest_dir.path().join("Show - S01E01 - Title.mkv");
        std::fs::write(&dest, b"v").unwrap();
        let candidates = walk_extras(src_dir.path(), &parse_extensions("ass"));
        let span = media::parse_episode_span("[group] show - 01 (1080p).mkv");
        let report = import_subtitles("copy", &video, &dest, span, &candidates, 1).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let mut names: Vec<String> = report
            .imported
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Show - S01E01 - Title.1.ass".to_string(),
                "Show - S01E01 - Title.2.ass".to_string(),
                "Show - S01E01 - Title.en.ass".to_string(),
                "Show - S01E01 - Title.en.forced.ass".to_string(),
            ]
        );
        // A second pass leaves the existing files alone.
        let again = import_subtitles("copy", &video, &dest, span, &candidates, 1).await;
        assert!(again.imported.is_empty());
        assert_eq!(again.skipped, 4);
    }
}
