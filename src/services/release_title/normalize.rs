//! Step one: turn a raw title into the text the lexer reads.
//!
//! Full-width forms become ASCII, CJK brackets become `[` / `]`, dashes and
//! wave dashes become `-` / `~`, a `★...★` decoration is dropped, and a
//! trailing video or subtitle extension is cut off (a video one marks the
//! title as naming a single file). Case is kept: the uppercase-only rules
//! (Roman numerals, PROPER / REPACK) read it. HTML entities are not decoded
//! here; every ingress already decodes them once.

/// Longest title read, cut on a char boundary. Matches the cap the file
/// parser uses (`media::MAX_RELEASE_TITLE_BYTES`).
const MAX_TITLE_BYTES: usize = 1024;

/// Video containers: a title ending in one names a single file.
pub(crate) const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "wmv", "flv", "webm", "m4v", "ts", "m2ts", "mov", "strm",
];

/// Side files whose extension is cut off but says nothing about the release.
const SIDE_EXTENSIONS: &[&str] = &["ass", "srt", "ssa", "sup", "idx", "sub", "nfo", "txt"];

pub(super) struct Normalized {
    pub text: String,
    pub single_file: bool,
}

pub(super) fn normalize(input: &str) -> Normalized {
    let mut end = input.len().min(MAX_TITLE_BYTES);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = String::with_capacity(end);
    for c in input[..end].chars() {
        text.push(map_char(c));
    }
    let text = drop_star_decoration(&text);
    let (text, single_file) = cut_extension(text.trim());
    Normalized { text, single_file }
}

fn map_char(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        '【' | '〔' | '〖' => '[',
        '】' | '〕' | '〗' => ']',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{301C}' | '\u{223C}' => '~',
        // Armenian full stop and modifier colon stand in for `:` in a
        // few titles (`Mob Psycho 100։`, `86꞉ Eighty Six`).
        '\u{0589}' | '\u{A789}' => ':',
        c if c.is_control() => ' ',
        c => c,
    }
}

/// Drops `★10月新番★`-style decoration: a short CJK note between a pair
/// of stars, or after one star up to the next bracket when it is a season
/// note (`★01月新番[Title][01]`). Stars in a title (`夜露死駆★少女`,
/// `KERORO☆ ... Gunsou☆`) stay.
fn drop_star_decoration(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(['★', '☆']) {
        let after = &rest[open + '★'.len_utf8()..];
        let upto = after
            .find(['[', '【', '(', '★', '☆'])
            .unwrap_or(after.len());
        let season_note = &after[..upto];
        if season_note.contains("新番") && season_note.trim().chars().count() <= 12 {
            out.push_str(&rest[..open]);
            out.push(' ');
            rest = after[upto..].trim_start_matches(['★', '☆']);
            continue;
        }
        let Some(close) = after.find(['★', '☆']) else {
            break;
        };
        let note = &after[..close];
        if note.chars().count() <= 12 && !note.is_ascii() && !note.contains(' ') {
            out.push_str(&rest[..open]);
            out.push(' ');
            rest = &after[close + '★'.len_utf8()..];
        } else {
            out.push_str(&rest[..open + '★'.len_utf8()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Cuts a known extension off the end. Also handles a trailing note after
/// the extension (`[3F752475].mkv (KanColle the Movie)`), which a few
/// uploaders append.
fn cut_extension(text: &str) -> (String, bool) {
    let (body, note) = match text.strip_suffix(')') {
        Some(inner) => match inner.rfind(" (") {
            Some(at) => (&text[..at], &text[at..]),
            None => (text, ""),
        },
        None => (text, ""),
    };
    if let Some(dot) = body.rfind('.') {
        let ext = body[dot + 1..].to_ascii_lowercase();
        let video = VIDEO_EXTENSIONS.contains(&ext.as_str());
        if video || SIDE_EXTENSIONS.contains(&ext.as_str()) {
            let mut out = body[..dot].to_string();
            out.push_str(note);
            return (out, video);
        }
    }
    (text.to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_width_and_cjk_brackets_become_ascii() {
        let n = normalize("【喵萌奶茶屋】★10月新番★[Title：Sub][03]");
        assert_eq!(n.text, "[喵萌奶茶屋] [Title:Sub][03]");
        assert!(!n.single_file);
    }

    #[test]
    fn a_video_extension_marks_a_single_file() {
        let n = normalize("[SubsPlease] Show - 05 (1080p) [ABCD1234].mkv");
        assert_eq!(n.text, "[SubsPlease] Show - 05 (1080p) [ABCD1234]");
        assert!(n.single_file);
        let n = normalize("[o7] Gekijouban Kantai Collection [3F752475].mkv (KanColle the Movie)");
        assert_eq!(
            n.text,
            "[o7] Gekijouban Kantai Collection [3F752475] (KanColle the Movie)"
        );
        assert!(n.single_file);
        assert!(!normalize("Show.S01.1080p.WEB").single_file);
    }

    #[test]
    fn an_unpaired_star_season_note_is_dropped() {
        let n = normalize("【动漫国字幕组】★01月新番[Anime Series Title～！][01][1080P]");
        assert!(!n.text.contains("新番"), "{}", n.text);
    }

    #[test]
    fn stars_in_a_title_stay() {
        assert_eq!(normalize("夜露死駆★少女").text, "夜露死駆★少女");
        assert_eq!(
            normalize("[AnoZu] KERORO☆ 2026 S01E04 1080p | Keroro Gunsou☆").text,
            "[AnoZu] KERORO☆ 2026 S01E04 1080p | Keroro Gunsou☆"
        );
    }

    #[test]
    fn dashes_and_wave_dashes_unify() {
        assert_eq!(
            normalize("Rayearth OVA - 01–03").text,
            "Rayearth OVA - 01-03"
        );
        assert_eq!(normalize("～A〜B～").text, "~A~B~");
    }
}
