//! What the differential against the old parsers settled as fixes, one
//! named test per reason (the triage notes, kept when `legacy.rs` and the
//! differential went in phase 13). Each asserts a few of the titles the
//! old readers got wrong; the corpus snapshot pins the rest.

use super::parse;

fn episodes(title: &str) -> Vec<i32> {
    let mut eps: Vec<i32> = parse(title).release_episodes().into_iter().collect();
    eps.sort_unstable();
    eps
}

/// An airing span with no episode claim is a whole-run pack.
#[test]
fn airing_span_pack() {
    assert!(parse(r###"[1983-1985] Dallos_ダロス_OVA (世界初のOVA)"###).is_batch());
    assert!(parse(r###"[1983-1984] Albegas_光速電神(光速電神アルベガス)_TV"###).is_batch());
    assert!(parse(r###"[1973-2011] Enma_咚隆隆炎魔君(ドロロンえん魔くん)_TV.OVA"###).is_batch());
}

/// [BD] alone is not a batch keyword; single BD episodes carry it too.
#[test]
fn bd_not_batch() {
    assert!(
        !parse(r###"[BD_SUBS] [BD][1080p][PT-BR] Mahou Shoujo Lyrical Nanoha A's"###).is_batch()
    );
    assert!(
        !parse(r###"[BakeSubs] Bakusou Kyoudai Let's & Go!! - 34 [BD][1080p][5026029B].mkv"###)
            .is_batch()
    );
}

/// The old reader took the codec number (H.264 / x.265).
#[test]
fn codec_number() {
    assert!(episodes(r###"[T3KASHi] Tomb Raider King S01 MULTi 1080p CR WEBRiP AAC 2.0 x.265 (Dogulwang, VF, VOSTFR)"###).is_empty());
    assert!(episodes(r###"[T3KASHi] The Drops of God S01 MULTi 1080p CR WEBRiP AAC 2.0 x.265 (Kami no Shizuku, VF, VOSTFR)"###).is_empty());
}

/// The old reader missed the episode (CJK bracket, #N, 第N話, EPnn, N of M, a bracketed or bare range).
#[test]
fn episode_missed() {
    assert_eq!(
        episodes(r###"Sailor Moon German Restoration Ep 005"###),
        [5]
    );
    assert_eq!(
        episodes(r###"Sailor Moon German Restoration Ep 006"###),
        [6]
    );
    assert_eq!(
        episodes(r###"[AI-Raws] .hack//Roots #01 BDRip 1080p MKV"###),
        [1]
    );
}

/// The old extractor missed this group (a trailing scene group, a 【】 group, a round-bracket group, or a last-bracket group).
#[test]
fn group_missed() {
    assert_eq!(
        parse(r###"Psyren.S01E04.VOSTFR.1080p.WEBRiP.x265-KAF"###)
            .group
            .unwrap_or_default(),
        "KAF"
    );
    assert_eq!(
        parse(r###"Ao.Ashi.S02E04.VOSTFR.1080p.WEBRiP.x265-KAF"###)
            .group
            .unwrap_or_default(),
        "KAF"
    );
    assert_eq!(
        parse(r###"Psyren.S01E01.MULTi.AD.1080p.WEBRiP.x265-KAF"###)
            .group
            .unwrap_or_default(),
        "KAF"
    );
}

/// The old extractor took a year, resolution, language, platform or season bracket as the group.
#[test]
fn group_not_a_group() {
    assert_eq!(
        parse(r###"[1973] Chargeman_チャージマン研!_TV"###)
            .group
            .unwrap_or_default(),
        ""
    );
    assert_eq!(
        parse(r###"[1983-1985] Dallos_ダロス_OVA (世界初のOVA)"###)
            .group
            .unwrap_or_default(),
        ""
    );
    assert_eq!(
        parse(r###"[BDMV] Gals Can't be Kind to Otaku!?"###)
            .group
            .unwrap_or_default(),
        ""
    );
}

/// The old extractor took a title, metadata or year bracket instead of the group.
#[test]
fn group_wrong_bracket() {
    assert_eq!(
        parse(r###"(shiteater) Dr. Slump & Arale-chan 191 [FB507024]"###)
            .group
            .unwrap_or_default(),
        "shiteater"
    );
    assert_eq!(
        parse(r###"Your Letter (2025) [1080p CR WEB-DL DDP 2.0] [Chika]"###)
            .group
            .unwrap_or_default(),
        "Chika"
    );
    assert_eq!(parse(r###"【喵萌奶茶屋】★10月新番★[你好，我是受心上人所托来做恋爱药的魔女。 / Horemajo][04][1080p][简日双语]"###).group.unwrap_or_default(), "喵萌奶茶屋");
}

/// A multi-season pack names no single season.
#[test]
fn multi_season_no_hint() {
    assert_eq!(
        parse(r###"[Xspitfire911] Kingdom S01 > S06 BDRIP 1080p X265 10bit VOSTFR"###)
            .season_hint(),
        None
    );
    assert_eq!(parse(r###"[LuRenDian] Fabulous Beasts (You Shou Yan) S01-S06 (EP01-72) 1080p WEB-DL HEVC AAC [subs in 7 languages]"###).season_hint(), None);
}

/// The title names a pack (an episode range, a season with no episode, or a pack keyword) the old detector missed.
#[test]
fn pack_missed() {
    assert!(parse(r###"[Sosuke] Kaleido Star S1 [1080p] [Esp LA]"###).is_batch());
    assert!(parse(r###"Dragon Ball Z [TV] [01-240] Rus (rus, eng. sub.)"###).is_batch());
    assert!(parse(r###"[Salieri] Spy x Family - S3 - BD (1080p) (HDR) [Dual Audio]"###).is_batch());
}

/// A range names every episode in it.
#[test]
fn range_expanded() {
    assert_eq!(
        episodes(r###"[WZF]Mawaru_Penguindrum_-_01~24[BDRip][X264-AAC][1280x720][Sub_Esp][MP4]"###),
        (1..=24).collect::<Vec<i32>>()
    );
    assert_eq!(
        episodes(
            r###"[WZF]Demashitaa!_Powerpuff_Girls_Z_-_Capitulo_01~15[X264-AAC][784x576][Sub_Esp]"###
        ),
        (1..=15).collect::<Vec<i32>>()
    );
    assert_eq!(
        episodes(
            r###"[fig] Soul Eater S01 E01-E26 (BluRay 1080p HEVC Opus) [Dual-Audio] |  (Season 1)"###
        ),
        (1..=26).collect::<Vec<i32>>()
    );
}

/// The old extractor knew only 480p / 720p / 1080p / 2160p; WxH, 540p, 576, 900p and 4K read now.
#[test]
fn resolution_forms() {
    assert_eq!(
        parse(r###"Aikatsu! Encore - 001 (TVer 1920x1080 AVC AAC)"###).resolution,
        Some(1080)
    );
    assert_eq!(
        parse(r###"Aikatsu! Encore - 002 (TVer 1920x1080 AVC AAC)"###).resolution,
        Some(1080)
    );
}

/// The old reader missed this season marker (第N季, a bracketed Season NN, ТВ-N, Stagione, Temporada, Season One).
#[test]
fn season_marker_missed() {
    assert_eq!(
        parse(r###"Mairimashota! Iruma-kun Season One OP-ED Lyrics.txt"###).season_hint(),
        Some(1)
    );
    assert_eq!(
        parse(
            r###"[ANi]  黑化吧！聖女大人 第二季 - 04 [1080P][Baha][WEB-DL][AAC AVC][CHT][MP4]"###
        )
        .season_hint(),
        Some(2)
    );
    assert_eq!(parse(r###"[ANi]  重生者的魔法一定要特別 第二季 - 15 [1080P][Baha][WEB-DL][AAC AVC][CHT][MP4]"###).season_hint(), Some(2));
}

/// A sequel number in the title is not an episode.
#[test]
fn sequel_number_not_episode() {
    assert!(episodes(r###"The Legend of Hei 2 (2025) SUBFRENCH 720p ADN WEB-DL AAC2.0 x264-Tsundere-Raws (VOSTFR, Multi Subs, Luo Xiaohei Zhanji 2, The Legend of Hei II)"###).is_empty());
    assert!(episodes(r###"[H-Enc] 关于邻家的天使大人不知不觉把我惯成了废人这档子事 第二季 / Otonari no Tenshi-sama ni Itsunomanika Dame Ningen ni Sareteita Ken 2 (BDRip 1080p HEVC FLAC) | The Angel Next Door Spoils Me Rotten 2"###).is_empty());
}

/// The old reader took the title's sequel number over the bracketed episode.
#[test]
fn sequel_over_bracket() {
    assert_eq!(
        episodes(
            r###"[綠茶字幕組] 賽博朋克：邊緣行者 2 / Cyberpunk - Edgerunners 2 [01][WebRip][1080p][繁日內嵌]"###
        ),
        [1]
    );
    assert_eq!(
        episodes(
            r###"[绿茶字幕组] 赛博朋克：边缘行者 2 / Cyberpunk - Edgerunners 2 [01][WebRip][1080p][简日内嵌]"###
        ),
        [1]
    );
}

/// The title names one episode; the old detector read a title, season or year number as a range.
#[test]
fn single_episode() {
    assert!(
        !parse(r###"[綠茶字幕組] 青之箱 第二季 / Ao no Hako S2 [03][WebRip][1080p][繁日內嵌]"###)
            .is_batch()
    );
    assert!(
        !parse(r###"[绿茶字幕组] 青之箱 第二季 / Ao no Hako S2 [03][WebRip][1080p][简日内嵌]"###)
            .is_batch()
    );
    assert!(
        !parse(r###"[绿茶字幕组] 青之箱 第二季 / Ao no Hako S2 [02][WebRip][1080p][简繁日内封]"###)
            .is_batch()
    );
}

/// The old reader added a title, season or part number (`Part 2 - 03`, `Season 2 - 04`, `Ranma 1-2`).
#[test]
fn title_number_added() {
    assert_eq!(
        episodes(
            r###"[ANi]  冰劍的魔術師將要統一世界 2 - 03 [1080P][Baha][WEB-DL][AAC AVC][CHT][MP4]"###
        ),
        [3]
    );
    assert_eq!(
        episodes(
            r###"[Erai-raws] Ao Ashi Season 2 - 04 [480p CR WEB-DL AVC AAC][MultiSub][7849AABF]"###
        ),
        [4]
    );
}

/// A volume number or range is not an episode.
#[test]
fn volume_not_episode() {
    assert!(
        episodes(r###"[Spartanoah] Steins;Gate S1 - Vol 1-9 [BDMV 1080p AVC PCM]"###).is_empty()
    );
    assert!(episodes(r###"[ReinForce] Otonari no Tenshi-sama ni Itsunomanika Dame Ningen ni Sareteita Ken 2 Vol 4 (BDRip 1920x1080 x264 FLAC)"###).is_empty());
}

/// A year (or a `99 year abbreviation) is not an episode.
#[test]
fn year_not_episode() {
    assert!(episodes(r###"Master Mosquiton `99 (1999, eng softsubs)"###).is_empty());
    assert_eq!(
        episodes(r###"[FSP] Douluo Dalu II - Soul Land 2 - 176 [1080p]"###),
        [176]
    );
}

/// A thousands separator keeps the number in the show's name (`Saving
/// 80,000 Gold`, cut at `Saving` before, so the search gate's extra-words
/// check had nothing to compare).
#[test]
fn thousands_in_title() {
    assert_eq!(
        parse(r###"[sandoe41] Saving 80,000 Gold in Another World for My Retirement [Season 01v2][1080P][BDRip][HEVC-10bit][FLAC]"###)
            .series_title
            .as_deref(),
        Some("Saving 80,000 Gold in Another World for My Retirement")
    );
    // A list of episodes is not a number.
    assert_eq!(
        parse(r###"[ARR] Magnerobo Ga-Keen 13,15-26,28-39 [640p DVD AC3 x264]"###)
            .series_title
            .as_deref(),
        Some("Magnerobo Ga-Keen")
    );
}

/// Two range shapes from the 29k-title corpus that AnimeTosho's file
/// lists showed were packs: a tag range that writes the season again, and
/// `to` after an episode marker.
#[test]
fn tag_and_to_ranges() {
    let t = parse("Ninjala S01E138-S01E198 (WEBDL 1080p x264 AAC) [S1PH3R] + EP110");
    assert!(t.is_batch());
    assert_eq!(
        t.episodes.as_ref().map(|c| (c.first, c.last)),
        Some((138, 198))
    );
    let t = parse("Qin's Moon S1 ep. 1 to 11 (Remastered)");
    assert!(t.is_batch());
    assert_eq!(
        episodes("Show Episode 1 to 12 [BD]"),
        (1..=12).collect::<Vec<_>>()
    );
    // A different season written after the dash is not this tag's range.
    assert_eq!(episodes("Show S01E12-S02E01 [1080p]"), vec![12]);
}
