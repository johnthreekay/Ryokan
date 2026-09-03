//! The file-list verdict: does what a download client says is inside a
//! grab actually belong to the series it was grabbed for?
//!
//! This is the second line of defense behind the title gate. The gate
//! judges a release *title*; this judges the *file names*, which an
//! indexer cannot rename and a misleading title cannot hide. It is
//! deliberately conservative in the other direction from the gate:
//! matching here is permissive (any own or sibling alias, verbatim or
//! fuzzy, on any single media file) because a match means "leave it
//! alone", and a misgrab verdict requires at least one file that
//! clearly names *something* (title signal) and no file that names
//! *us*. Files with no title signal at all (`01.mkv`, `S01E01.mkv`
//! without a titled folder) are unverifiable, never a misgrab.

use std::collections::HashSet;

use crate::models::grabbed_torrents::VerificationDetail;
use crate::services::auto_search::{
    distinctive_overlap_ratio, is_generic_title_token, is_media_filename, normalize_title,
    parse_release_season, token_set,
};

/// A media file "names something" when this many content tokens survive
/// the noise filters.
pub const TITLE_SIGNAL_MIN_TOKENS: usize = 2;
/// How many file names the stored detail keeps for the review tab.
pub const SAMPLE_LIMIT: usize = 5;
/// Share of an alias's distinctive tokens a file must carry to count as
/// naming that alias. Same figure the title gate uses.
pub const FILE_MATCH_RATIO: f32 = 0.6;

/// Release-side words that appear in file names without saying which
/// series the file belongs to. `normalize_title` already drops bracket
/// groups, resolutions, and codecs; this list covers what survives.
pub(crate) const FILE_NOISE_TOKENS: &[&str] = &[
    "bd",
    "bdrip",
    "bdmv",
    "bluray",
    "remux",
    "dvd",
    "dvdrip",
    "webrip",
    "webdl",
    "hdtv",
    "flac",
    "opus",
    "aac",
    "ac3",
    "eac3",
    "dts",
    "truehd",
    "10bit",
    "8bit",
    "hi10p",
    "hi10",
    "h264",
    "h265",
    "x264",
    "x265",
    "av1",
    "hevc",
    "avc",
    "batch",
    "complete",
    "uncensored",
    "uncen",
    "censored",
    "raw",
    "raws",
    "sub",
    "subs",
    "subbed",
    "dub",
    "dubbed",
    "eng",
    "jpn",
    "jap",
    "multi",
    "dual",
    "ncop",
    "nced",
    "op",
    "ed",
    "pv",
    "cm",
    "extras",
    "extra",
    "specials",
    "special",
    "sp",
    "sample",
    "season",
    "ep",
    "eps",
    "vol",
    "volume",
    "disc",
    "disk",
    "final",
    "end",
    "v2",
    "v3",
    "v4",
    "mkv",
    "mp4",
    "avi",
    "mka",
    "ass",
    "srt",
    "web",
    "bluray",
];

/// The outcome of checking a grab's file list.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// At least one media file names the series or a related entry.
    Verified {
        matched_file: String,
        matched_alias: String,
        notes: Vec<String>,
    },
    /// At least one media file clearly names something, and none of
    /// them name the series.
    Misgrab {
        sample: Vec<String>,
        notes: Vec<String>,
    },
    /// Nothing to judge: no media files, or names with no title in them.
    Unverifiable { reason: &'static str },
}

impl Verdict {
    /// The `grabbed_torrents.verification` value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Verified { .. } => "verified",
            Verdict::Misgrab { .. } => "misgrab",
            Verdict::Unverifiable { .. } => "unverifiable",
        }
    }

    pub fn is_misgrab(&self) -> bool {
        matches!(self, Verdict::Misgrab { .. })
    }

    /// What gets stored alongside the verdict.
    pub fn detail(&self, filenames: &[String]) -> VerificationDetail {
        let file_count = filenames.iter().filter(|f| is_media_filename(f)).count();
        let files: Vec<String> = filenames
            .iter()
            .filter(|f| is_media_filename(f))
            .take(SAMPLE_LIMIT)
            .cloned()
            .collect();
        match self {
            Verdict::Verified {
                matched_file,
                matched_alias,
                notes,
            } => VerificationDetail {
                files,
                file_count,
                matched: Some(matched_file.clone()),
                reason: format!("file names {matched_alias:?}"),
                notes: notes.clone(),
            },
            Verdict::Misgrab { sample, notes } => VerificationDetail {
                files: sample.clone(),
                file_count,
                matched: None,
                reason: "no media file names the series or a related entry".to_string(),
                notes: notes.clone(),
            },
            Verdict::Unverifiable { reason } => VerificationDetail {
                files,
                file_count,
                matched: None,
                reason: (*reason).to_string(),
                notes: Vec::new(),
            },
        }
    }
}

/// Everything the verdict looks at. Aliases are raw (un-normalized)
/// titles; the series' own titles and synonyms in `own_aliases`, the
/// titles of related entries in `sibling_aliases`.
pub struct VerdictInput<'a> {
    pub own_aliases: &'a [String],
    pub sibling_aliases: &'a [String],
    pub filenames: &'a [String],
    /// The season number the series maps to (0 when unknown); only
    /// used for an advisory note.
    pub expected_season: i32,
}

/// Flatten a relative path so the top-level folder counts as part of
/// the name, then normalize the way the title gate does.
pub(crate) fn normalize_path(path: &str) -> String {
    normalize_title(&path.replace(['/', '\\'], " "))
}

/// The tokens of a normalized path that could name a series.
pub(crate) fn content_tokens(normalized_path: &str) -> HashSet<String> {
    token_set(normalized_path)
        .into_iter()
        .filter(|t| !is_generic_title_token(t))
        .filter(|t| !FILE_NOISE_TOKENS.contains(&t.as_str()))
        .filter(|t| !is_structural_token(t))
        .collect()
}

/// Episode, season, version, resolution, and checksum tokens.
fn is_structural_token(t: &str) -> bool {
    if t.chars().count() < 2 {
        return true;
    }
    if t.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    let strip =
        |s: &str, prefix: &str| -> Option<String> { s.strip_prefix(prefix).map(|r| r.to_string()) };
    // s01, e07, s01e07, v2, 1080p, 10bit, 1920x1080
    if let Some(rest) = strip(&lower, "s") {
        let (digits, tail) = rest.split_at(
            rest.find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len()),
        );
        if !digits.is_empty()
            && (tail.is_empty()
                || tail
                    .strip_prefix('e')
                    .is_some_and(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())))
        {
            return true;
        }
    }
    for prefix in ["e", "v"] {
        if let Some(rest) = strip(&lower, prefix)
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    for suffix in ["p", "bit"] {
        if let Some(rest) = lower.strip_suffix(suffix)
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    if let Some((w, h)) = lower.split_once('x')
        && !w.is_empty()
        && !h.is_empty()
        && w.chars().all(|c| c.is_ascii_digit())
        && h.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    // CRC32 and similar hex runs. Six or more so ordinary words that
    // happen to be hex ("bead", "face") survive.
    if lower.len() >= 6 && lower.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    false
}

pub(crate) fn has_title_signal(tokens: &HashSet<String>) -> bool {
    tokens.len() >= TITLE_SIGNAL_MIN_TOKENS
}

/// An alias plus its multi-word segments split at a colon or dash, so a
/// file that carries only the subtitle ("Stardust Crusaders - 01")
/// still names "JoJo's Bizarre Adventure: Stardust Crusaders".
fn alias_forms(alias: &str) -> Vec<String> {
    let mut forms = vec![alias.to_string()];
    let split = alias
        .replace(['\u{2013}', '\u{2014}'], "|")
        .replace(": ", "|")
        .replace(" - ", "|");
    for part in split.split('|') {
        let part = part.trim();
        if part.len() >= 5 && part.split_whitespace().count() >= 2 && part != alias {
            forms.push(part.to_string());
        }
    }
    forms
}

/// The first alias the file names, verbatim or by distinctive-token
/// overlap. No surplus budget on purpose: a file that says our title
/// plus more words is still our file.
pub(crate) fn file_matches_any(
    normalized_path: &str,
    path_tokens: &HashSet<String>,
    aliases: &[String],
) -> Option<String> {
    for alias in aliases {
        for form in alias_forms(alias) {
            let normalized_alias = normalize_title(&form);
            if normalized_alias.is_empty() {
                continue;
            }
            if normalized_path.contains(&normalized_alias) {
                return Some(alias.clone());
            }
            let alias_tokens = token_set(&normalized_alias);
            if distinctive_overlap_ratio(path_tokens, &alias_tokens) >= FILE_MATCH_RATIO {
                return Some(alias.clone());
            }
        }
    }
    None
}

/// Every content token any alias contributes. A file that shares even
/// one of these is ambiguous (abbreviated fansub names like "JoJo
/// Stardust"), never a misgrab.
fn alias_content_tokens(aliases: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    for alias in aliases {
        out.extend(content_tokens(&normalize_title(alias)));
    }
    out
}

pub fn assess(input: &VerdictInput<'_>) -> Verdict {
    let media: Vec<&String> = input
        .filenames
        .iter()
        .filter(|f| is_media_filename(f))
        .collect();
    if media.is_empty() {
        return Verdict::Unverifiable {
            reason: "no media files in the download",
        };
    }

    let mut notes: Vec<String> = Vec::new();
    if input.expected_season > 0 {
        let explicit: Vec<i32> = media
            .iter()
            .map(|f| parse_release_season(f))
            .filter(|s| *s > 0)
            .collect();
        if !explicit.is_empty() && explicit.iter().all(|s| *s != input.expected_season) {
            notes.push(format!(
                "season mismatch: files say season {}, the series is season {}",
                explicit[0], input.expected_season
            ));
        }
    }

    let aliases: Vec<String> = input
        .own_aliases
        .iter()
        .chain(input.sibling_aliases.iter())
        .cloned()
        .collect();

    let known = alias_content_tokens(&aliases);
    let mut any_signal = false;
    let mut shares_a_word = false;
    for file in &media {
        let normalized = normalize_path(file);
        let path_tokens = token_set(&normalized);
        if let Some(alias) = file_matches_any(&normalized, &path_tokens, &aliases) {
            return Verdict::Verified {
                matched_file: (*file).clone(),
                matched_alias: alias,
                notes,
            };
        }
        let content = content_tokens(&normalized);
        if has_title_signal(&content) {
            any_signal = true;
        }
        if content.iter().any(|t| known.contains(t)) {
            shares_a_word = true;
        }
    }

    if !any_signal {
        Verdict::Unverifiable {
            reason: "file names carry no title to check",
        }
    } else if shares_a_word {
        // "JoJo Stardust - 01" against "JoJo's Bizarre Adventure:
        // Stardust Crusaders": too abbreviated to match, too related to
        // condemn.
        Verdict::Unverifiable {
            reason: "file names share words with the series but do not match it",
        }
    } else {
        Verdict::Misgrab {
            sample: media
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|f| (*f).clone())
                .collect(),
            notes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn run(own: &[&str], siblings: &[&str], files: &[&str], season: i32) -> Verdict {
        let own = strs(own);
        let siblings = strs(siblings);
        let files = strs(files);
        assess(&VerdictInput {
            own_aliases: &own,
            sibling_aliases: &siblings,
            filenames: &files,
            expected_season: season,
        })
    }

    const KOWAREMONO: &[&str] = &[
        "Kowaremono: Risa THE ANIMATION",
        "コワレモノ:璃沙 THE ANIMATION",
        "Risa THE ANIMATION",
    ];

    #[test]
    fn grisaia_pack_against_kowaremono_aliases_is_misgrab() {
        // Issue #219: file 2 of the batch the reporter's grab came from.
        let v = run(
            KOWAREMONO,
            &[
                "Kowaremono THE ANIMATION",
                "Kowaremono: Risa PLUS THE ANIMATION",
            ],
            &[
                "[Xonline] Grisaia Phantom Trigger The Animation/[Xonline] Grisaia Phantom Trigger The Animation - 01 (BD 1920p x.264-10Bit Flac) [2E112DAF].mkv",
                "[Xonline] Grisaia Phantom Trigger The Animation/[Xonline] Grisaia Phantom Trigger The Animation - 02 (BD 1920p x.264-10Bit Flac) [02964F5A].mkv",
            ],
            0,
        );
        match v {
            Verdict::Misgrab { sample, .. } => assert_eq!(sample.len(), 2),
            other => panic!("expected misgrab, got {other:?}"),
        }
    }

    #[test]
    fn legit_pack_with_titled_files_is_verified() {
        let v = run(
            KOWAREMONO,
            &[],
            &[
                "[H-Enc] Kowaremono Risa The Animation 01-02/Kowaremono Risa The Animation - 01 (BDRip 1080p HEVC AAC).mkv",
            ],
            0,
        );
        assert!(matches!(v, Verdict::Verified { .. }), "{v:?}");
    }

    #[test]
    fn generic_filenames_inside_titled_folder_are_verified() {
        let v = run(
            &["Sousou no Frieren"],
            &[],
            &[
                "[Group] Sousou no Frieren (BD 1080p)/01.mkv",
                "[Group] Sousou no Frieren (BD 1080p)/02.mkv",
                "[Group] Sousou no Frieren (BD 1080p)/fonts/readme.txt",
            ],
            0,
        );
        assert!(matches!(v, Verdict::Verified { .. }), "{v:?}");
    }

    #[test]
    fn terse_single_file_without_signal_is_unverifiable() {
        let v = run(&["Kimetsu no Yaiba"], &[], &["KnY - 01.mkv"], 0);
        assert_eq!(
            v,
            Verdict::Unverifiable {
                reason: "file names carry no title to check"
            }
        );
        let v = run(&["Kimetsu no Yaiba"], &[], &["S01E01.mkv"], 0);
        assert!(matches!(v, Verdict::Unverifiable { .. }), "{v:?}");
    }

    #[test]
    fn sibling_subtitle_files_are_verified() {
        // A JoJo pack whose files name the arc, not the root title.
        let v = run(
            &["JoJo no Kimyou na Bouken", "JoJo's Bizarre Adventure"],
            &["JoJo's Bizarre Adventure: Stardust Crusaders"],
            &["[Group] Stardust Crusaders - 01 (1080p).mkv"],
            0,
        );
        assert!(matches!(v, Verdict::Verified { .. }), "{v:?}");
    }

    #[test]
    fn abbreviated_filenames_sharing_a_title_word_are_unverifiable() {
        // Fansub abbreviations share a word with the title without
        // matching it. Related enough that removal would be wrong.
        let v = run(
            &["JoJo's Bizarre Adventure: Stardust Crusaders"],
            &["JoJo's Bizarre Adventure: Stardust Crusaders - Egypt-hen"],
            &["[HorribleSubs] JoJo Stardust - 01 [720p].mkv"],
            0,
        );
        assert_eq!(
            v,
            Verdict::Unverifiable {
                reason: "file names share words with the series but do not match it"
            }
        );
        // Sharing only generic words is not sharing.
        let v = run(
            &["Kowaremono: Risa THE ANIMATION"],
            &[],
            &["[G] Grisaia Phantom Trigger The Animation - 01.mkv"],
            0,
        );
        assert!(v.is_misgrab(), "{v:?}");
        // A title whose only distinctive word travels with a number
        // ("Persona 5") leaves one content token, below the signal
        // floor: unverifiable, not a misgrab. Conservative on purpose.
        let v = run(
            &["Kowaremono: Risa THE ANIMATION"],
            &[],
            &["[G] Persona 5 The Animation - 01.mkv"],
            0,
        );
        assert!(matches!(v, Verdict::Unverifiable { .. }), "{v:?}");
    }

    #[test]
    fn one_matching_file_verifies_a_mixed_pack() {
        let v = run(
            &["Sousou no Frieren"],
            &[],
            &[
                "[Group] Something Else Entirely - 01.mkv",
                "[Group] Sousou no Frieren - 01.mkv",
            ],
            0,
        );
        assert!(matches!(v, Verdict::Verified { .. }), "{v:?}");
    }

    #[test]
    fn no_media_files_is_unverifiable() {
        let v = run(&["Sousou no Frieren"], &[], &["readme.txt", "cover.jpg"], 0);
        assert_eq!(
            v,
            Verdict::Unverifiable {
                reason: "no media files in the download"
            }
        );
    }

    #[test]
    fn content_tokens_strip_episode_resolution_codec_and_crc() {
        let normalized = normalize_path(
            "[Xonline] Grisaia Phantom Trigger The Animation - 02 (BD 1920p x.264-10Bit Flac) [02964F5A].mkv",
        );
        let tokens = content_tokens(&normalized);
        let mut got: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
        got.sort_unstable();
        assert_eq!(got, vec!["grisaia", "phantom", "trigger"]);
        assert!(
            !content_tokens(&normalize_path(
                "S01E07 v2 1080p 10bit 1920x1080 deadbeef.mkv"
            ))
            .iter()
            .any(|_| true)
        );
    }

    #[test]
    fn season_mismatch_is_a_note_not_a_verdict() {
        let v = run(
            &["Sousou no Frieren"],
            &[],
            &["[Group] Sousou no Frieren S03E01 (1080p).mkv"],
            1,
        );
        match v {
            Verdict::Verified { notes, .. } => {
                assert_eq!(notes.len(), 1, "{notes:?}");
                assert!(notes[0].starts_with("season mismatch"), "{notes:?}");
            }
            other => panic!("expected verified with a note, got {other:?}"),
        }
    }

    #[test]
    fn detail_records_sample_and_reason() {
        let files = strs(&[
            "[G] Other Show - 01.mkv",
            "[G] Other Show - 02.mkv",
            "[G] Other Show - 03.mkv",
            "[G] Other Show - 04.mkv",
            "[G] Other Show - 05.mkv",
            "[G] Other Show - 06.mkv",
            "readme.txt",
        ]);
        let v = assess(&VerdictInput {
            own_aliases: &strs(&["Sousou no Frieren"]),
            sibling_aliases: &[],
            filenames: &files,
            expected_season: 0,
        });
        let d = v.detail(&files);
        assert_eq!(v.as_str(), "misgrab");
        assert_eq!(d.files.len(), SAMPLE_LIMIT);
        assert!(d.matched.is_none());
        assert!(d.reason.contains("no media file names"));
        let json = serde_json::to_string(&d).unwrap();
        let back: VerificationDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back.files, d.files);
    }
}
