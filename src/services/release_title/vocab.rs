//! Step three: what a word means.
//!
//! One table classifies every word token: metadata (resolution, codec,
//! audio, source, platform, language), structure (season, part, episode
//! and special markers, tags like `S01E05`), and the keywords that make a
//! release a pack. Rules read the class, never the raw text, so a keyword
//! is recognised the same way everywhere.

use super::lexer::Tok;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    Plain,
    Resolution(u32),
    /// Codec, bit depth, audio, source, platform, language, container:
    /// release metadata with nothing to say about episodes.
    Meta,
    /// A standalone `v2`.
    Version(u32),
    /// Uppercase `PROPER` / `REPACK` / `RERIP`.
    Revision,
    /// `Batch`, `Complete`, `全集`, `合集`.
    Batch,
    /// `Collection`: a batch keyword only in compounds (`Complete
    /// Collection`), since it is also part of titles (`Kantai Collection`).
    Collection,
    /// `END`, `Fin`, `Final`, `完`.
    End,
    /// `OVA`, `OAD`, `Special(s)`.
    Special,
    /// `SP`: a special only in `SP01`-style shapes; `[SP]` is a subtitle tag.
    Sp,
    /// NCOP / NCED / creditless / OP / ED / PV / CM / preview / trailer.
    Extra,
    /// `Movie`, `Film`, `Gekijouban`, `劇場版`, `映画`.
    Movie,
    /// `Movies`, `Films`: a collection.
    Movies,
    /// `Season`, `Saison`, `Temporada`, `Stagione`, `Sezon` and plurals.
    SeasonWord,
    /// `Part`, `Partie`, `Parte`, `Cour`.
    PartWord,
    /// `Vol`, `Volume`.
    VolWord,
    /// `Episode`, `Episodes`, `Episodio`, `Épisode`, `Epi`, `Ep`, `Eps`.
    EpisodeWord,
    /// `2nd`, `3rd`, `4th`, `2a`, `1ª`.
    Ordinal(u32),
    /// `S2`, `S01`.
    SeasonShort(u32),
    /// `S01-S04`, `S01-04`.
    SeasonRange(u32, u32),
    /// `S01E05`, `S01E01-E02`, `S00E01v2`, `1x05`.
    Tag {
        season: u32,
        first: u32,
        last: u32,
        version: Option<u32>,
    },
    /// `S01E07.5`: a half episode in a tag.
    HalfTag(u32),
    /// `E05`, `EP05`, `EP1157`, `E01-E02`, `E001-E130`.
    EpPrefix(u32, u32),
    /// `OVA01`, `SP01`, `OAD1`.
    SpecialNum(u32),
    /// `TV-1` (Russian releases' season marker).
    TvSeason(u32),
    /// `P2` after a season marker.
    PartShort(u32),
    /// Uppercase `II`..`IX`.
    Roman(u32),
    /// `One`..`Ten` (`Season One`).
    WordNumber(u32),
    /// `第三季`.
    CjkSeason(u32),
    /// `第`, the CJK ordinal prefix (`第01话`, `第3季`).
    CjkOrdinal,
    /// `话` / `話` / `集`: CJK episode counter.
    CjkEpisode,
    /// `季`: CJK season counter.
    CjkSeasonCounter,
    /// `of` (`04 of 12`).
    Of,
}

impl Class {
    /// Release metadata: a dash number followed by one of these is an
    /// episode, and a bare trailing number before one is not title text.
    pub fn is_meta(self) -> bool {
        matches!(
            self,
            Class::Resolution(_) | Class::Meta | Class::Version(_) | Class::Revision
        )
    }
}

const META: &[&str] = &[
    // codecs and encodes
    "x264",
    "x265",
    "h264",
    "h265",
    "hevc",
    "avc",
    "av1",
    "av2",
    "vp9",
    "xvid",
    "divx",
    "mpeg2",
    "mpeg-2",
    "hi10p",
    "hi10",
    "hi444pp",
    "hi444",
    "main444p10",
    "yuv420p10",
    "10bit",
    "8bit",
    "12bit",
    "10bits",
    "8bits",
    "10-bit",
    "8-bit",
    "12-bit",
    "10-bits",
    "veryslow",
    "crf16",
    "custom",
    "vfr",
    "remastered",
    // audio
    "aac",
    "ac3",
    "eac3",
    "e-ac-3",
    "eac-3",
    "ddp",
    "dd",
    "flac",
    "opus",
    "truehd",
    "dts",
    "dts-hd",
    "dtshdma",
    "pcm",
    "lpcm",
    "mp3",
    "dual",
    "dual-audio",
    "multi-audio",
    "atmos",
    "flacx2",
    "2xflac",
    "assx2",
    "2ass",
    "dd-ac3",
    "aac2.0",
    "ddp2.0",
    "ddp5.1",
    "flac2.0",
    "flac5.1",
    "opus2.0",
    "audio",
    "dubbed",
    "dub",
    // sources
    "bd",
    "bdrip",
    "bluray",
    "blu-ray",
    "brrip",
    "remux",
    "bdremux",
    "bdmv",
    "bd-raw",
    "bdiso",
    "web",
    "web-dl",
    "webdl",
    "webrip",
    "web-rip",
    "hdtv",
    "hdrip",
    "tvrip",
    "dvd",
    "dvdrip",
    "dvdiso",
    "pdtv",
    "vhs",
    "vhsrip",
    "ld",
    "j-dvd",
    "r2jp",
    "jpbd",
    "cam",
    "bdrip-web-dl",
    "dvd-encode",
    "dvd1",
    "dvd2",
    "bd-box",
    "uncensored",
    "reseed",
    "hd",
    // platforms
    "cr",
    "amzn",
    "nf",
    "dsnp",
    "hulu",
    "hidive",
    "hmax",
    "adn",
    "baha",
    "b-global",
    "bilibili",
    "bili",
    "iqiyi",
    "iq",
    "wetv",
    "abema",
    "funi",
    "funi-dl",
    "funidub",
    "viu",
    "ytb",
    "yt",
    "youtube",
    "lftlnet",
    "ov",
    "aniplus",
    "nbn",
    "ivi",
    // languages and subtitles
    "eng",
    "jpn",
    "jap",
    "jp",
    "chs",
    "cht",
    "big5",
    "gb",
    "gb_jp",
    "rus",
    "vostfr",
    "vf",
    "subfrench",
    "multi",
    "multisub",
    "multi-sub",
    "multi-subs",
    "multiple",
    "msubs",
    "sub",
    "subs",
    "subbed",
    "softsub",
    "softsubs",
    "hardsub",
    "hardsubs",
    "raw",
    "raws",
    "sub_esp",
    "esp",
    "spa",
    "ita",
    "fre",
    "ger",
    "por",
    "pt-br",
    "por-br",
    "spa-la",
    "ara",
    "english",
    "subtitle",
    "subtitles",
    "dual-subs",
    "eng-subs",
    // containers
    "mkv",
    "mp4",
    "avi",
];

const CJK_META: &[&str] = &[
    "简", "繁", "双语", "雙語", "内嵌", "內嵌", "内封", "內封", "外挂", "字幕", "中文", "繁中",
    "简体", "繁体", "简日", "繁日", "无修", "無修", "國語", "国语", "中配",
];

const EXTRA_WORDS: &[&str] = &[
    "ncop",
    "nced",
    "ncoped",
    "creditless",
    "textless",
    "op",
    "ed",
    "oped",
    "opening",
    "openings",
    "ending",
    "endings",
    "pv",
    "cm",
    "tv-cm",
    "preview",
    "previews",
    "trailer",
    "trailers",
    "teaser",
    "menu",
    "yokoku",
];

fn word_number(w: &str) -> Option<u32> {
    Some(match w {
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        _ => return None,
    })
}

/// Value of a Roman numeral `I` to `X`, in any case. Callers bound the
/// range they accept (a season marker is `II`..`IX`).
pub fn roman_value(s: &str) -> Option<u32> {
    Some(match s.to_ascii_uppercase().as_str() {
        "I" => 1,
        "II" => 2,
        "III" => 3,
        "IV" => 4,
        "V" => 5,
        "VI" => 6,
        "VII" => 7,
        "VIII" => 8,
        "IX" => 9,
        "X" => 10,
        _ => return None,
    })
}

/// `Second` in `Second Season`; only read before a season word.
pub(super) fn word_ordinal(w: &str) -> Option<u32> {
    Some(match w {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        "tenth" => 10,
        _ => return None,
    })
}

fn cjk_number(s: &str) -> Option<u32> {
    let digit = |c: char| -> Option<u32> {
        Some(match c {
            '一' => 1,
            '二' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            _ => return None,
        })
    };
    let chars: Vec<char> = s.chars().collect();
    match chars.as_slice() {
        [d] if *d == '十' => Some(10),
        [d] => digit(*d),
        ['十', d] => digit(*d).map(|n| 10 + n),
        [d, '十'] => digit(*d).map(|n| n * 10),
        [d, '十', e] => Some(digit(*d)? * 10 + digit(*e)?),
        _ => None,
    }
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `1080p`, `480i`, `1920x1080`, `4k`, plus glued forms (`BD1080p`,
/// `1080p-FLAC`, `DVD-480p`).
pub(super) fn resolution(lower: &str) -> Option<u32> {
    fn whole(w: &str) -> Option<u32> {
        if let Some(n) = w.strip_suffix(['p', 'i']).and_then(digits)
            && (240..=4320).contains(&n)
        {
            return Some(n);
        }
        if let Some((a, b)) = w.split_once(['x', '*'])
            && a.len() >= 3
            && let (Some(width), Some(h)) = (digits(a), digits(b.strip_suffix('p').unwrap_or(b)))
            && width >= 320
            && width > h
            && (240..=4320).contains(&h)
        {
            return Some(h);
        }
        match w {
            "4k" | "uhd" => Some(2160),
            "fhd" => Some(1080),
            _ => None,
        }
    }
    if let Some(n) = whole(lower) {
        return Some(n);
    }
    for part in lower.split('-') {
        if let Some(n) = whole(part) {
            return Some(n);
        }
    }
    let trimmed = lower.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    if trimmed.len() < lower.len() {
        return whole(trimmed);
    }
    None
}

fn is_meta_word(lower: &str) -> bool {
    if META.contains(&lower) {
        return true;
    }
    // `简繁内封字幕` is subtitle metadata; `桜都字幕组` is a group name.
    if CJK_META.iter().any(|m| lower.contains(m)) && !lower.ends_with(['组', '組', '社']) {
        return true;
    }
    // `AAC2.0`, `DDP5.1`, `FLAC2.0`, `OPUS2.0`, `2.0x2`.
    for prefix in [
        "aac", "ac3", "eac3", "ddp", "dd", "flac", "opus", "dts", "truehd", "pcm",
    ] {
        if let Some(rest) = lower.strip_prefix(prefix)
            && !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == 'x')
        {
            return true;
        }
    }
    false
}

fn tag(lower: &str) -> Option<Class> {
    // S01E05, S00E01v2, S01E01-E02, S01E05-06, S2024E10, S01EP01, S06E01b.
    let rest = lower.strip_prefix('s')?;
    let e = rest.find('e')?;
    let season = digits(&rest[..e]).filter(|s| *s <= 2100)?;
    let mut after = &rest[e + 1..];
    after = after.strip_prefix('p').unwrap_or(after);
    if let Some((int, frac)) = after.split_once('.') {
        digits(int)?;
        if frac.starts_with('5') {
            return Some(Class::HalfTag(season));
        }
        after = int;
    }
    if let Some(stripped) = after.strip_suffix(['a', 'b', 'c', 'd'])
        && stripped.bytes().last().is_some_and(|b| b.is_ascii_digit())
    {
        after = stripped;
    }
    let mut parts = after.split('-');
    let first_part = parts.next()?;
    // `S01E01E02`, `S01E01E02E03`: glued further episodes.
    if first_part.contains('e') {
        let numbers: Vec<u32> = first_part.split('e').map(digits).collect::<Option<_>>()?;
        let (&a, &b) = (numbers.first()?, numbers.last()?);
        return Some(Class::Tag {
            season,
            first: a,
            last: b.max(a),
            version: None,
        });
    }
    let (first, version) = split_version(first_part)?;
    let mut last = first;
    let mut version = version;
    for part in parts {
        let part = part.strip_prefix('e').unwrap_or(part);
        let (n, v) = split_version(part)?;
        last = n;
        version = v.or(version);
    }
    if last < first {
        return None;
    }
    Some(Class::Tag {
        season,
        first,
        last,
        version,
    })
}

/// `1x05`, `1x1`, `2x04x05`, `1x01-x03`, `2010x15`.
fn nx_tag(lower: &str) -> Option<Class> {
    let mut parts = lower.split('x');
    let season_part = parts.next()?;
    let season = digits(season_part)?;
    if !(season_part.len() <= 2 || (1900..=2100).contains(&season)) {
        return None;
    }
    let mut eps = Vec::new();
    for part in parts {
        let part = part.trim_matches('-');
        // `0x01` is a special; `0x2` is the tail of `2.0x2` audio.
        if season == 0 && part.len() < 2 {
            return None;
        }
        eps.push(digits(part)?);
    }
    let first = *eps.first()?;
    let last = *eps.last()?;
    (last >= first).then_some(Class::Tag {
        season,
        first,
        last,
        version: None,
    })
}

fn split_version(s: &str) -> Option<(u32, Option<u32>)> {
    match s.split_once('v') {
        Some((n, v)) => Some((digits(n)?, Some(digits(v)?))),
        None => Some((digits(s)?, None)),
    }
}

fn ep_prefix(lower: &str) -> Option<Class> {
    // E05, EP05, EPS05, EPI05, Episode05, E01-E02, E1-E2-E3, E1E2, E001-E130, EP1157.
    let rest = lower
        .strip_prefix("episode")
        .or_else(|| lower.strip_prefix("eps"))
        .or_else(|| lower.strip_prefix("epi"))
        .or_else(|| lower.strip_prefix("ep"))
        .or_else(|| lower.strip_prefix('e'))?;
    let mut nums = Vec::new();
    for part in rest.split(['-', 'e']) {
        let part = part.trim_start_matches('p');
        if part.is_empty() {
            continue;
        }
        nums.push(split_version(part)?.0);
    }
    let first = *nums.first()?;
    let last = *nums.last()?;
    (last >= first).then_some(Class::EpPrefix(first, last))
}

/// Classifies a word token. Uppercase-only rules read `tok.text`.
pub(super) fn classify(tok: &Tok) -> Class {
    if !tok.is_word() {
        return Class::Plain;
    }
    let lower = tok.lower.as_str();
    let text = tok.text.as_str();
    if matches!(text, "PROPER" | "REPACK" | "RERIP") {
        return Class::Revision;
    }
    // Uppercase `II`..`IX` only: `V` and `X` are letters too often.
    if text.bytes().all(|b| b.is_ascii_uppercase())
        && let Some(n) = roman_value(text).filter(|n| (2..=9).contains(n) && *n != 5)
    {
        return Class::Roman(n);
    }
    match lower {
        "batch" | "complete" | "completa" | "completo" | "mini-batch" => return Class::Batch,
        "collection" => return Class::Collection,
        "end" | "fin" | "final" | "finale" | "完" | "終" | "完結" => return Class::End,
        "ova" | "ovas" | "oad" | "oads" | "oav" | "ovd" | "special" | "specials" => {
            return Class::Special;
        }
        "sp" => return Class::Sp,
        w if EXTRA_WORDS.contains(&w) => return Class::Extra,
        "movie" | "film" | "gekijouban" | "劇場版" | "映画" => return Class::Movie,
        "movies" | "films" => return Class::Movies,
        "season" | "seasons" | "saison" | "saisons" | "temporada" | "temporadas" | "stagione"
        | "sezon" => return Class::SeasonWord,
        "part" | "partie" | "parte" | "cour" => return Class::PartWord,
        "vol" | "volume" | "volumes" => return Class::VolWord,
        "episode" | "episodes" | "episodio" | "episodios" | "épisode" | "episódio" | "epi"
        | "ep" | "eps" => return Class::EpisodeWord,
        "of" | "из" => return Class::Of,
        "第" => return Class::CjkOrdinal,
        "话" | "話" | "集" => return Class::CjkEpisode,
        "季" => return Class::CjkSeasonCounter,
        "ª" | "º" => return Class::Ordinal(0),
        _ => {}
    }
    if lower.contains("全集") || lower.contains("合集") {
        return Class::Batch;
    }
    if let Some(c) = tag(lower) {
        return c;
    }
    // A tag inside a compound: `zoo-s01e11-1080p`, `S06E03-Unwrapped`.
    if lower.contains('-')
        && let Some(c) = lower.split('-').find_map(tag)
    {
        return c;
    }
    if let Some(n) = resolution(lower) {
        return Class::Resolution(n);
    }
    if is_meta_word(lower) {
        return Class::Meta;
    }
    if let Some(v) = lower.strip_prefix('v').and_then(digits)
        && lower.len() <= 3
    {
        return Class::Version(v);
    }
    if let Some(c) = tag(lower) {
        return c;
    }
    if let Some(c) = nx_tag(lower) {
        return c;
    }
    for word in ["season", "saison", "temporada"] {
        if let Some(n) = lower.strip_prefix(word).and_then(digits) {
            return Class::SeasonShort(n);
        }
    }
    if let Some(rest) = lower.strip_prefix('s') {
        if let Some(n) = digits(rest).filter(|n| rest.len() <= 2 || (1900..=2100).contains(n)) {
            return Class::SeasonShort(n);
        }
        // `S01-S04`, `S01-04`, `S1-S2-S3-S4`.
        let parts: Vec<Option<u32>> = rest
            .split('-')
            .map(|p| digits(p.trim_start_matches('s')).filter(|n| *n <= 99))
            .collect();
        if parts.len() >= 2
            && parts.iter().all(Option::is_some)
            && let (Some(Some(a)), Some(Some(b))) = (parts.first(), parts.last())
            && a < b
        {
            return Class::SeasonRange(*a, *b);
        }
    }
    if let Some(c) = ep_prefix(lower) {
        return c;
    }
    for marker in ["ova", "oad", "sp", "special"] {
        if let Some(n) = lower.strip_prefix(marker).and_then(digits) {
            return Class::SpecialNum(n);
        }
    }
    if let Some(n) = lower
        .strip_prefix("tv-")
        .or_else(|| lower.strip_prefix("тв-"))
        .and_then(digits)
    {
        return Class::TvSeason(n);
    }
    if let Some(n) = lower.strip_prefix('p').and_then(digits).filter(|n| *n < 10) {
        return Class::PartShort(n);
    }
    let num_end = lower.bytes().take_while(|b| b.is_ascii_digit()).count();
    if num_end > 0 && num_end <= 2 {
        let suffix = &lower[num_end..];
        if matches!(
            suffix,
            "st" | "nd" | "rd" | "th" | "a" | "ª" | "º" | "e" | "eme" | "ème" | "ere" | "ère"
        ) {
            return Class::Ordinal(lower[..num_end].parse().unwrap_or(0));
        }
    }
    if let Some(n) = word_number(lower) {
        return Class::WordNumber(n);
    }
    if let Some(inner) = lower.strip_prefix('第').and_then(|r| r.strip_suffix('季'))
        && let Some(n) = cjk_number(inner)
    {
        return Class::CjkSeason(n);
    }
    // `NCOP-NCED`, `OP-ED`.
    if lower.contains('-') && lower.split('-').all(|p| EXTRA_WORDS.contains(&p)) {
        return Class::Extra;
    }
    // Compounds made only of metadata (`HEVC-10bit`, `Dual-Audio`,
    // `BDRip-WEB-DL`) count as metadata.
    if lower.contains('-')
        && lower
            .split('-')
            .all(|p| !p.is_empty() && (is_meta_word(p) || resolution(p).is_some()))
    {
        return Class::Meta;
    }
    Class::Plain
}

/// Eight hex digits: a CRC, when it is a bracket's whole content.
pub(super) fn is_crc(text: &str) -> bool {
    text.len() == 8 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::release_title::lexer::lex;

    fn class_of(word: &str) -> Class {
        classify(&lex(word).toks[0])
    }

    #[test]
    fn resolutions() {
        assert_eq!(class_of("1080p"), Class::Resolution(1080));
        assert_eq!(class_of("1920x1080"), Class::Resolution(1080));
        assert_eq!(class_of("1920x1080p"), Class::Resolution(1080));
        assert_eq!(class_of("BD1080p"), Class::Resolution(1080));
        assert_eq!(class_of("480i"), Class::Resolution(480));
        assert_eq!(class_of("4K"), Class::Resolution(2160));
        assert_eq!(class_of("1080p-FLAC"), Class::Resolution(1080));
    }

    #[test]
    fn tags_and_prefixes() {
        assert_eq!(
            class_of("S01E07v3"),
            Class::Tag {
                season: 1,
                first: 7,
                last: 7,
                version: Some(3)
            }
        );
        assert_eq!(
            class_of("S00E22-E23"),
            Class::Tag {
                season: 0,
                first: 22,
                last: 23,
                version: None
            }
        );
        assert_eq!(class_of("EP1157"), Class::EpPrefix(1157, 1157));
        assert_eq!(class_of("E001-E130"), Class::EpPrefix(1, 130));
        assert_eq!(class_of("S01-S04"), Class::SeasonRange(1, 4));
        assert_eq!(class_of("S2"), Class::SeasonShort(2));
        assert_eq!(class_of("OVA01"), Class::SpecialNum(1));
        assert_eq!(class_of("4th"), Class::Ordinal(4));
        assert_eq!(class_of("第三季"), Class::CjkSeason(3));
    }

    #[test]
    fn uppercase_only_markers() {
        assert_eq!(class_of("IV"), Class::Roman(4));
        assert_eq!(class_of("iv"), Class::Plain);
        assert_eq!(class_of("REPACK"), Class::Revision);
        assert_eq!(class_of("Proper"), Class::Plain);
    }

    #[test]
    fn metadata_compounds_but_not_groups() {
        assert_eq!(class_of("HEVC-10bit"), Class::Meta);
        assert_eq!(class_of("Dual-Audio"), Class::Meta);
        assert_eq!(class_of("x265-KAF"), Class::Plain);
        assert_eq!(class_of("AAC2.0"), Class::Meta);
    }
}
