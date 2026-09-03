use super::*;
use crate::services::anilist::AnimeDetail;

fn detail_with_titles(english: &str, romaji: &str) -> AnimeDetail {
    AnimeDetail {
        is_adult: false,
        id: 1,
        id_mal: None,
        title_romaji: romaji.to_string(),
        title_english: english.to_string(),
        title_native: String::new(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "MOVIE".to_string(),
        status: String::new(),
        status_display: String::new(),
        episodes: Some(1),
        duration: None,
        season: String::new(),
        season_year: None,
        end_year: None,
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

fn related(
    id: i64,
    english: &str,
    romaji: &str,
    relation_type: &str,
    episodes: Option<i32>,
) -> RelatedEntry {
    RelatedEntry {
        id,
        id_mal: None,
        title_romaji: romaji.to_string(),
        title_english: english.to_string(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: "FINISHED".to_string(),
        status_display: "Finished".to_string(),
        episodes,
        relation_type: relation_type.to_string(),
        season_year: None,
        media_type: "ANIME".to_string(),
    }
}

#[test]
fn detect_siblings_finds_named_seasons_in_jojo_pack() {
    // Parent: JoJo S1 (franchise root, no subtitle of its own).
    // Pack contains files for S1 (no subtitle), S3 Stardust
    // Crusaders, and S4 Diamond is Unbreakable. Detection should
    // return two sibling matches (Stardust + Diamond) with only
    // their own files; S1 files stay unclaimed.
    let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
    parent.id = 14719; // AL id
    parent.episodes = Some(26);
    parent.relations = vec![
        related(
            20800,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "JoJo no Kimyou na Bouken: Stardust Crusaders",
            "SEQUEL",
            Some(24),
        ),
        related(
            31292,
            "JoJo's Bizarre Adventure: Diamond is Unbreakable",
            "JoJo no Kimyou na Bouken: Diamond wa Kudakenai",
            "SEQUEL",
            Some(39),
        ),
    ];

    let files: Vec<String> = vec![
        // S1 files (unclaimed)
        "[Group] JoJo no Kimyou na Bouken - 01.mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - 02.mkv".to_string(),
        // Stardust Crusaders (24 eps, we include just 3 for brevity)
        "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01.mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 02.mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 03.mkv".to_string(),
        // Diamond is Unbreakable
        "[Group] JoJo no Kimyou na Bouken - Diamond is Unbreakable - 01.mkv".to_string(),
        "[Group] JoJo no Kimyou na Bouken - Diamond is Unbreakable - 02.mkv".to_string(),
    ];

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 2, "expected Stardust + Diamond matches");

    let stardust = siblings
        .iter()
        .find(|s| s.anilist_id == 20800)
        .expect("stardust sibling present");
    assert_eq!(stardust.file_indices, vec![2, 3, 4]);
    assert!(
        stardust
            .matched_subtitle
            .to_lowercase()
            .contains("stardust"),
        "matched_subtitle should reference Stardust, got {:?}",
        stardust.matched_subtitle
    );

    let diamond = siblings
        .iter()
        .find(|s| s.anilist_id == 31292)
        .expect("diamond sibling present");
    assert_eq!(diamond.file_indices, vec![5, 6]);
}

#[test]
fn detect_siblings_returns_empty_for_jikan_sourced_detail() {
    // Provenance gate: Jikan-sourced details have id < 0. Even
    // if relations look plausible, we must not run sibling
    // detection against them — MAL splits sagas AL merges, which
    // would duplicate library rows.
    let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
    parent.id = -1; // Jikan sentinel
    parent.relations = vec![related(
        -20800,
        "JoJo's Bizarre Adventure: Stardust Crusaders",
        "",
        "SEQUEL",
        Some(24),
    )];
    let files: Vec<String> =
        vec!["[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01.mkv".to_string()];
    assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
}

#[test]
fn detect_siblings_resolves_overlap_by_longest_subtitle() {
    // Two siblings whose subtitles form a prefix relationship. A
    // filename containing the longer subtitle matches both
    // normalized needles, but the longer one must win — otherwise
    // we'd double-count the file.
    let mut parent = detail_with_titles("Franchise", "Franchise");
    parent.id = 100;
    parent.relations = vec![
        related(201, "Franchise: Alpha", "", "SEQUEL", Some(12)),
        related(202, "Franchise: Alpha Prime", "", "SEQUEL", Some(12)),
    ];
    let files: Vec<String> = vec![
        "[Group] Franchise - Alpha Prime - 01.mkv".to_string(),
        "[Group] Franchise - Alpha Prime - 02.mkv".to_string(),
    ];
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1);
    assert_eq!(siblings[0].anilist_id, 202);
    assert_eq!(siblings[0].file_indices, vec![0, 1]);
}

#[test]
fn detect_siblings_skips_relations_without_own_subtitle() {
    // "Naruto Shippuden" has no trailing delimiter so
    // trailing_subtitle_of returns None and the sibling gets
    // silently dropped. This is intentional — without a
    // subtitle we can't safely narrow a filename list, so
    // conservative over-skipping is the right call.
    let mut parent = detail_with_titles("Naruto", "Naruto");
    parent.id = 20;
    parent.relations = vec![related(1735, "Naruto Shippuden", "", "SEQUEL", Some(500))];
    let files: Vec<String> = vec!["[Group] Naruto Shippuden - 01.mkv".to_string()];
    assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
}

#[test]
fn detect_siblings_rejects_episode_count_overshoot() {
    // A sibling with episodes=12 whose subtitle accidentally
    // matches 50 files in the pack. The episode-count cap
    // (×1.5 + 2 = 20) fires and drops the sibling entirely
    // rather than emitting a wildly-wrong routing.
    let mut parent = detail_with_titles("Franchise", "Franchise");
    parent.id = 100;
    parent.relations = vec![related(
        201,
        "Franchise: Alpha Beta",
        "",
        "SEQUEL",
        Some(12),
    )];
    let files: Vec<String> = (1..=50)
        .map(|i| format!("[Group] Franchise - Alpha Beta - {:02}.mkv", i))
        .collect();
    assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
}

#[test]
fn detect_siblings_filters_out_source_material_relations() {
    // ADAPTATION / SOURCE / COMPILATION / CONTAINS relations
    // point at manga / LN / book entries that will never appear
    // in an anime torrent. Even if one happened to share a
    // substring with a filename, the relation-type gate must
    // drop it before we waste cycles on string matching.
    let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "");
    parent.id = 14719;
    parent.relations = vec![related(
        2,
        "JoJo's Bizarre Adventure: Stardust Crusaders",
        "",
        "SOURCE",
        Some(1),
    )];
    let files: Vec<String> = vec!["[Group] JoJo - Stardust Crusaders - 01.mkv".to_string()];
    assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
}

#[test]
fn detect_siblings_ignores_non_anime_media_types() {
    // AL returns the parent manga via a relation edge with
    // media_type="MANGA". Never an anime torrent candidate.
    let mut parent = detail_with_titles("Show", "");
    parent.id = 10;
    let mut manga_rel = related(5, "Show: Spinoff Arc", "", "SIDE_STORY", Some(10));
    manga_rel.media_type = "MANGA".to_string();
    parent.relations = vec![manga_rel];
    let files: Vec<String> = vec!["[Group] Show - Spinoff Arc - 01.mkv".to_string()];
    assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
}

#[test]
fn detect_siblings_passes_through_spin_off_and_summary_relations() {
    // Niche relation types (SPIN_OFF, SUMMARY, CHARACTER,
    // ALTERNATIVE) are included in the filter — the subtitle
    // match and episode-count cap do the downstream filtering.
    let mut parent = detail_with_titles("Show", "");
    parent.id = 10;
    parent.relations = vec![
        related(11, "Show: Recap Arc", "", "SUMMARY", Some(4)),
        related(12, "Show: Extra Chapter", "", "SPIN_OFF", Some(6)),
    ];
    let files: Vec<String> = vec![
        "[Group] Show - Recap Arc - 01.mkv".to_string(),
        "[Group] Show - Recap Arc - 02.mkv".to_string(),
        "[Group] Show - Extra Chapter - 01.mkv".to_string(),
    ];
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 2);
    let recap = siblings
        .iter()
        .find(|s| s.anilist_id == 11)
        .expect("recap sibling present");
    assert_eq!(recap.file_indices, vec![0, 1]);
    let extra = siblings
        .iter()
        .find(|s| s.anilist_id == 12)
        .expect("extra sibling present");
    assert_eq!(extra.file_indices, vec![2]);
}

#[test]
fn detect_siblings_ignores_non_media_files_in_match_set() {
    // Subtitles, NFOs, samples etc. must not count toward the
    // episode cap or get routed. Only .mkv/.mp4/... files pass
    // through is_media_filename.
    let mut parent = detail_with_titles("Show", "");
    parent.id = 10;
    parent.relations = vec![related(11, "Show: Alpha Beta", "", "SEQUEL", Some(12))];
    let files: Vec<String> = vec![
        "[Group] Show - Alpha Beta - 01.mkv".to_string(),
        "[Group] Show - Alpha Beta - 01.srt".to_string(),
        "[Group] Show - Alpha Beta - readme.nfo".to_string(),
    ];
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1);
    // Only the .mkv file routes — the .srt and .nfo are filtered
    // out by is_media_filename before they can inflate the match
    // set.
    assert_eq!(siblings[0].file_indices, vec![0]);
}

// ── Layer 2: episode-range fallback ────────────────────────────

#[test]
fn detect_siblings_fallback_catches_bare_number_pack_single_word_arc() {
    // 48-file continuation pack using bare space-delimited episode
    // numbers followed by a quality bracket (no `S01E01`, no
    // `- 25`), where the sibling's trailing subtitle is a single
    // word and thus rejected by `trailing_subtitle_of`'s ≥2-token
    // rule. The subtitle path produces zero matches and files
    // 25-48 must come through the episode-range fallback, which
    // attributes them to the sibling with
    // `episode_offset = parent_cap`.
    //
    // Filenames here are synthetic token strings — the only thing
    // the test cares about is the bare-digit + quality-bracket
    // shape, since that's what `parse_episode_number`'s new
    // RE_BARE_NUM_BRACKET branch keys on.
    let mut parent = detail_with_titles("Parent Show", "Parent Show Romaji");
    parent.id = 20474;
    parent.episodes = Some(24);
    parent.relations = vec![related(
        20799,
        "Parent Show - Coda",
        "Parent Show - Coda",
        "SEQUEL",
        Some(24),
    )];

    let mut files: Vec<String> = Vec::new();
    for n in 1..=48 {
        files.push(format!(
            "fixture-parent-show {:02} (bd-1080p) [hash].mkv",
            n
        ));
    }

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1, "fallback should find one sibling");
    let s = &siblings[0];
    assert_eq!(s.anilist_id, 20799);
    // Sibling claims files 24..48 (indices 24..=47 → eps 25..=48).
    assert_eq!(s.file_indices.len(), 24);
    assert_eq!(*s.file_indices.first().unwrap(), 24);
    assert_eq!(*s.file_indices.last().unwrap(), 47);
    assert!(s.matched_subtitle.starts_with("episode-range fallback"));
    // Absolute numbering → offset = parent cap (24).
    assert_eq!(s.episode_offset, 24);
}

#[test]
fn detect_siblings_fallback_rejects_ambiguous_two_sequels() {
    // Parent with two SEQUEL relations that both fit the overflow
    // range ambiguously. Neither is title-prefix matched, so the
    // tiebreaker doesn't save us → bail, fallback returns nothing.
    let mut parent = detail_with_titles("Parent Show", "");
    parent.id = 1;
    parent.episodes = Some(12);
    parent.relations = vec![
        related(2, "Unrelated Sequel One", "", "SEQUEL", Some(12)),
        related(3, "Unrelated Sequel Two", "", "SEQUEL", Some(12)),
    ];
    let mut files: Vec<String> = Vec::new();
    for n in 1..=24 {
        files.push(format!("[Group] Parent Show - {:02}.mkv", n));
    }
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert!(siblings.is_empty(), "ambiguous sequels must bail");
}

#[test]
fn detect_siblings_fallback_title_prefix_beats_strict_sequel() {
    // Owarimonogatari scenario: direct AniList SEQUEL is
    // Tsukimonogatari (not a continuation of the same title),
    // but the actual same-title continuation is Owarimonogatari
    // Second Season. Range-fit alone can't distinguish if both
    // candidates pass, so title-prefix wins as the tiebreaker.
    //
    // Here we give Tsuki an incompatible ep count (can't fit the
    // overflow) so it's rejected by range-fit first, and Owari S2
    // is the only survivor — validating the primary path.
    let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
    parent.id = 21320;
    parent.episodes = Some(13);
    parent.relations = vec![
        // Direct SEQUEL relation, wrong continuation — only 4 eps
        // so it cannot fit a 7-file overflow.
        related(
            20787,
            "Tsukimonogatari",
            "Tsukimonogatari",
            "SEQUEL",
            Some(4),
        ),
        // Same-title continuation; AniList may type this as a
        // SIDE_STORY so we must admit it via title-prefix.
        related(
            21860,
            "Owarimonogatari Second Season",
            "Owarimonogatari Second Season",
            "SIDE_STORY",
            Some(7),
        ),
    ];
    let mut files: Vec<String> = Vec::new();
    for n in 1..=20 {
        files.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari.mkv",
            n
        ));
    }

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1);
    let s = &siblings[0];
    assert_eq!(
        s.anilist_id, 21860,
        "must pick Owari S2, not Tsukimonogatari"
    );
    assert_eq!(s.file_indices.len(), 7);
    assert_eq!(
        s.episode_offset, 13,
        "absolute numbering → offset = parent cap"
    );
}

#[test]
fn detect_siblings_fallback_skips_when_no_overflow() {
    // 12-ep parent, 12 files numbered 01..12 — nothing exceeds the
    // parent cap, so the fallback must not synthesize siblings.
    let mut parent = detail_with_titles("Parent Show", "");
    parent.id = 1;
    parent.episodes = Some(12);
    parent.relations = vec![related(
        2,
        "Parent Show Second Season",
        "",
        "SEQUEL",
        Some(12),
    )];
    let mut files: Vec<String> = Vec::new();
    for n in 1..=12 {
        files.push(format!("[Group] Parent Show - {:02}.mkv", n));
    }
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert!(siblings.is_empty());
}

#[test]
fn detect_siblings_fallback_skipped_when_parent_episodes_unknown() {
    // Airing / unknown-length parent (episodes=None): we can't
    // safely attribute overflow. Fallback bails.
    let mut parent = detail_with_titles("Parent Show", "");
    parent.id = 1;
    parent.episodes = None;
    parent.relations = vec![related(
        2,
        "Parent Show Second Season",
        "",
        "SEQUEL",
        Some(12),
    )];
    let files: Vec<String> = (1..=12)
        .map(|n| format!("[Group] Parent Show - {:02}.mkv", n))
        .collect();
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert!(siblings.is_empty());
}

#[test]
fn detect_siblings_fallback_skipped_when_subtitle_path_found_something() {
    // Subtitle path hit at least one sibling → fallback is
    // suppressed. Parent has relation with a usable 2-token
    // subtitle AND files matching it, so subtitle path produces
    // results. Fallback won't run even if overflow files exist
    // (known limitation — fallback doesn't supplement partial
    // subtitle matches).
    let mut parent = detail_with_titles("Parent Show", "");
    parent.id = 1;
    parent.episodes = Some(12);
    parent.relations = vec![
        related(2, "Parent Show: Alpha Beta", "", "SEQUEL", Some(12)),
        related(3, "Parent Show Third Season", "", "SEQUEL", Some(12)),
    ];
    let files: Vec<String> = vec![
        "[Group] Parent Show - Alpha Beta - 01.mkv".to_string(),
        "[Group] Parent Show - Alpha Beta - 02.mkv".to_string(),
        // These would be overflow for a 12-ep parent but subtitle
        // path already produced matches, so fallback is suppressed.
        "[Group] Parent Show - 25.mkv".to_string(),
        "[Group] Parent Show - 26.mkv".to_string(),
    ];
    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1);
    assert_eq!(siblings[0].anilist_id, 2);
}

#[test]
fn detect_siblings_fallback_handles_absolute_numbered_smol_owari() {
    // Real-world: [smol] Monogatari batch uses continuous
    // absolute numbering (E13 Owarimonogatari, E14 Owarimonogatari
    // Second Season, ...). Subtitle detection can't fire —
    // "Owarimonogatari Second Season" has no ": " / " - "
    // delimiter AND its trailing portion is a generic-ordinal
    // phrase anyway — so the episode-range fallback is the only
    // path that reaches this release. It picks Owari S2 via
    // title-prefix matching and the per-sibling offset pass
    // applies offset=13 so post_processing renames E14..E20 to
    // E01..E07 of Owari S2.
    let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
    parent.id = 21320;
    parent.episodes = Some(13);
    parent.relations = vec![related(
        21860,
        "Owarimonogatari Second Season",
        "Owarimonogatari Second Season",
        "SIDE_STORY",
        Some(7),
    )];
    let mut files: Vec<String> = Vec::new();
    for n in 1..=13 {
        files.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
            n
        ));
    }
    for n in 14..=20 {
        files.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
            n
        ));
    }

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1, "fallback should find Owari S2");
    let s = &siblings[0];
    assert_eq!(s.anilist_id, 21860);
    // Overflow = files with E14..E20 (indices 13..=19).
    assert_eq!(s.file_indices.len(), 7);
    assert_eq!(*s.file_indices.first().unwrap(), 13);
    assert_eq!(*s.file_indices.last().unwrap(), 19);
    assert!(s.matched_subtitle.starts_with("episode-range fallback"));
    assert_eq!(
        s.episode_offset, 13,
        "absolute numbering → offset = parent cap"
    );
}

#[test]
fn detect_siblings_fallback_partial_fit_multi_sibling_owari_with_zoku() {
    // Real-world progression of the smol Owari case: the pack has
    // 20 files (S07E01..=E20) but parent_cap = 12 (some AL data
    // sources report Owarimonogatari with 12 ep count), so
    // overflow = 8 files (eps 13..=20). The only graph-visible
    // sibling is Owarimonogatari Second Season (7 eps). The 8th
    // overflow file is Zoku Owarimonogatari, a 1-episode movie
    // that either isn't grafted or has no episodes count.
    //
    // Expected: packing loop picks Owari S2 for 7 of 8 overflow
    // files (partial fit, threshold = ceil(7*0.75) = 6), ep 20
    // falls out as unattributed. Emitting one partial-fit
    // sibling is better than bailing entirely, because the 7
    // files that DO fit cleanly are definitively Owari S2.
    let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
    parent.id = 21320;
    parent.episodes = Some(12);
    parent.relations = vec![
        // Direct SEQUEL is Tsukimonogatari — fits the first 4
        // overflow eps but not title-prefixed.
        related(
            20787,
            "Tsukimonogatari",
            "Tsukimonogatari",
            "SEQUEL",
            Some(4),
        ),
        // Same-title continuation, transitively grafted as
        // SIDE_STORY. Owari S2 takes precedence because it's
        // title-prefixed AND covers more of the overflow in
        // round 1.
        related(
            21860,
            "Owarimonogatari Second Season",
            "Owarimonogatari Second Season",
            "SIDE_STORY",
            Some(7),
        ),
    ];
    let mut files: Vec<String> = Vec::new();
    for n in 1..=12 {
        files.push(format!(
            "fixture-monogatari-s07e{:02}-owarimonogatari-bd-1080p.mkv",
            n
        ));
    }
    for n in 13..=19 {
        files.push(format!(
            "fixture-monogatari-s07e{:02}-owarimonogatari-second-season-bd-1080p.mkv",
            n
        ));
    }
    files.push("fixture-monogatari-s07e20-zoku-owarimonogatari-bd-1080p.mkv".to_string());

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1, "must emit Owari S2 as partial fit");
    let s = &siblings[0];
    assert_eq!(s.anilist_id, 21860);
    assert_eq!(
        s.file_indices.len(),
        7,
        "seven files in [13..=19] must be claimed"
    );
    assert_eq!(*s.file_indices.first().unwrap(), 12);
    assert_eq!(*s.file_indices.last().unwrap(), 18);
    // File index 19 (ep 20, Zoku) must NOT be claimed — it falls
    // outside Owari S2's range and no other candidate can cover it.
    assert!(
        !s.file_indices.contains(&19),
        "ep 20 Zoku file must remain unattributed"
    );
    assert_eq!(
        s.episode_offset, 12,
        "absolute numbering → offset = parent cap"
    );
}

#[test]
fn detect_siblings_fallback_filename_subtitle_corrects_bd_split_first_ep() {
    // Real-world case from the live [smol] Owarimonogatari grab
    // (reported 2026-04-15): the BD release splits the 48-min
    // first aired episode back into two ~24-min halves, so the
    // pack has 13 Owari 1 files (S07E01..=E13) followed by 7
    // Owari 2 files (S07E14..=E20). But AniList reports the
    // parent as 12 eps (it groups the merged broadcast ep 1 as
    // one). Forward-aligned numeric packing anchored at
    // parent_cap=12 would misroute S07E13 (Owari 1's last ep) to
    // Owari S2 as "ep 1" and leave S07E20 (the real Owari S2
    // finale) hanging under the parent.
    //
    // The filename subtitle pre-pass fixes this: S07E13's file-
    // name only contains "Owarimonogatari", matching the parent
    // title, so it's parent-pre-claimed. S07E14..=E20 contain
    // "Owarimonogatari Second Season", matching the sibling
    // title (longer → wins), so those 7 files are sibling-pre-
    // claimed. The episode offset comes out to 13 (min_ep - 1),
    // so post-processing renames S07E14..=E20 to Owari S2
    // E01..=E07 correctly.
    //
    // Filenames are taken directly from the user's grab; they
    // correspond to a specific real release of a real group.
    let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
    parent.id = 21262;
    parent.episodes = Some(12); // AL undercount — BD has 13 files for parent
    parent.relations = vec![
        related(
            20787,
            "Tsukimonogatari",
            "Tsukimonogatari",
            "SEQUEL",
            Some(4),
        ),
        related(
            21745,
            "Owarimonogatari Second Season",
            "Owarimonogatari Second Season",
            "SEQUEL",
            Some(7),
        ),
    ];
    let mut files: Vec<String> = Vec::new();
    for n in 1..=13 {
        files.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p HEVC Opus) [DEADBEEF].mkv",
            n
        ));
    }
    for n in 14..=20 {
        files.push(format!(
            "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p HEVC Opus) [DEADBEEF].mkv",
            n
        ));
    }

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(
        siblings.len(),
        1,
        "filename subtitle pre-pass should identify Owari S2"
    );
    let s = &siblings[0];
    assert_eq!(s.anilist_id, 21745);
    assert_eq!(
        s.file_indices.len(),
        7,
        "Owari S2 should claim exactly the 7 S07E14..=E20 files"
    );
    // File indices 13..=19 correspond to S07E14..=E20 (files
    // vec is 0-indexed).
    assert_eq!(*s.file_indices.first().unwrap(), 13);
    assert_eq!(*s.file_indices.last().unwrap(), 19);
    // Critically: file index 12 (S07E13) must NOT be claimed —
    // that's Owari 1's last ep, and misrouting it was the bug.
    assert!(
        !s.file_indices.contains(&12),
        "S07E13 (Owari 1 ep 13) must stay with parent"
    );
    // Offset: min_ep = 14, so offset = 13 (not parent_cap = 12).
    // S07E14 → 14 - 13 = Owari S2 ep 1. Correct.
    assert_eq!(
        s.episode_offset, 13,
        "offset must be min_ep - 1 = 13 so Owari S2 starts at local ep 1"
    );
}

// ── transitive_relation_graft ────────────────────────────────────

/// Build an `AnimeDetail` with a specific id, titles, episode
/// count, and a pre-populated relation list. Used by the
/// transitive-walk tests to construct the neighbor details that
/// the graft helper walks into.
fn detail_with_relations(
    id: i64,
    english: &str,
    romaji: &str,
    episodes: Option<i32>,
    relations: Vec<RelatedEntry>,
) -> AnimeDetail {
    let mut d = detail_with_titles(english, romaji);
    d.id = id;
    d.episodes = episodes;
    d.relations = relations;
    d.format = "TV".to_string();
    d
}

#[test]
fn transitive_graft_pulls_in_second_hop_when_direct_relations_are_missing_edge() {
    // Parent has ONE direct PREQUEL neighbor. That neighbor's own
    // relations include a sibling that is NOT in the parent's
    // direct relations — the missing-edge case the walk exists to
    // fix. Graft should surface the missing sibling.
    let parent_id = 100;
    let neighbor_id = 200;
    let missing_sibling_id = 300;

    let parent = detail_with_relations(
        parent_id,
        "Parent Show",
        "Parent Show",
        Some(12),
        vec![related(
            neighbor_id,
            "Neighbor",
            "Neighbor",
            "PREQUEL",
            Some(26),
        )],
    );
    let neighbor = detail_with_relations(
        neighbor_id,
        "Neighbor",
        "Neighbor",
        Some(26),
        vec![
            // Back-edge to parent — must be deduped out.
            related(parent_id, "Parent Show", "Parent Show", "SEQUEL", Some(12)),
            // The sibling we want to surface.
            related(
                missing_sibling_id,
                "Parent Show Continuation",
                "Parent Show Continuation",
                "SEQUEL",
                Some(7),
            ),
        ],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(neighbor_id, neighbor);

    let graft = transitive_relation_graft(&parent, &neighbors);
    assert_eq!(graft.len(), 1, "back-edge to parent must be deduped");
    assert_eq!(graft[0].id, missing_sibling_id);
}

#[test]
fn transitive_graft_skips_non_walkable_relation_types() {
    // Parent has an ADAPTATION neighbor (manga). The walk must
    // NOT fetch into it even if we seed the map — is_pack_candidate
    // already blocks ADAPTATION as a direct sibling, and
    // is_transitive_walk_source must also block it as a walk
    // source.
    let parent = detail_with_relations(
        1,
        "Parent",
        "Parent",
        Some(12),
        vec![related(2, "Manga", "Manga", "ADAPTATION", None)],
    );
    // Seed neighbor map anyway — graft should ignore it because
    // the direct relation's type isn't walkable.
    let neighbor = detail_with_relations(
        2,
        "Manga",
        "Manga",
        None,
        vec![related(
            3,
            "Something Else",
            "Something Else",
            "SEQUEL",
            Some(13),
        )],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(2, neighbor);

    let graft = transitive_relation_graft(&parent, &neighbors);
    assert!(
        graft.is_empty(),
        "ADAPTATION direct relation must not be walked"
    );
}

#[test]
fn transitive_graft_dedupes_against_direct_relations() {
    // Parent already has a direct relation to id=5. Its neighbor
    // (reachable via PREQUEL) also lists id=5 as a sibling. The
    // graft must NOT return id=5 again — it's already in the
    // parent's direct list.
    let parent = detail_with_relations(
        1,
        "Parent",
        "Parent",
        Some(12),
        vec![
            related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26)),
            related(5, "Already Direct", "Already Direct", "SEQUEL", Some(7)),
        ],
    );
    let neighbor = detail_with_relations(
        2,
        "Neighbor",
        "Neighbor",
        Some(26),
        vec![
            related(5, "Already Direct", "Already Direct", "SEQUEL", Some(7)),
            // Also a truly new one.
            related(9, "Genuinely New", "Genuinely New", "SEQUEL", Some(12)),
        ],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(2, neighbor);

    let graft = transitive_relation_graft(&parent, &neighbors);
    assert_eq!(graft.len(), 1, "id=5 must be deduped against direct");
    assert_eq!(graft[0].id, 9);
}

#[test]
fn transitive_graft_filters_adaptation_hops() {
    // Parent's neighbor is a valid PREQUEL. But that neighbor's
    // OWN relations include a manga ADAPTATION. The hop filter
    // (is_pack_candidate_relation) must discard ADAPTATION hops
    // so they're never considered as siblings.
    let parent = detail_with_relations(
        1,
        "Parent",
        "Parent",
        Some(12),
        vec![related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26))],
    );
    let neighbor = detail_with_relations(
        2,
        "Neighbor",
        "Neighbor",
        Some(26),
        vec![
            {
                let mut r = related(3, "Manga Source", "Manga Source", "ADAPTATION", None);
                r.media_type = "MANGA".to_string();
                r
            },
            related(4, "Anime Sequel", "Anime Sequel", "SEQUEL", Some(12)),
        ],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(2, neighbor);

    let graft = transitive_relation_graft(&parent, &neighbors);
    assert_eq!(graft.len(), 1);
    assert_eq!(graft[0].id, 4);
}

#[test]
fn transitive_graft_returns_empty_when_parent_id_is_non_positive() {
    // Provenance gate: don't graft for Jikan-sourced details.
    let parent = detail_with_relations(
        -123,
        "Parent",
        "Parent",
        Some(12),
        vec![related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26))],
    );
    let neighbor = detail_with_relations(
        2,
        "Neighbor",
        "Neighbor",
        Some(26),
        vec![related(3, "Sibling", "Sibling", "SEQUEL", Some(12))],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(2, neighbor);

    let graft = transitive_relation_graft(&parent, &neighbors);
    assert!(graft.is_empty());
}

#[test]
fn transitive_graft_ignores_missing_neighbors_in_map() {
    // If the caller hit the fetch cap and didn't populate every
    // neighbor, graft must silently skip the un-fetched ones.
    let parent = detail_with_relations(
        1,
        "Parent",
        "Parent",
        Some(12),
        vec![
            related(
                2,
                "Fetched Neighbor",
                "Fetched Neighbor",
                "PREQUEL",
                Some(26),
            ),
            related(
                7,
                "Unfetched Neighbor",
                "Unfetched Neighbor",
                "SEQUEL",
                Some(26),
            ),
        ],
    );
    let neighbor_2 = detail_with_relations(
        2,
        "Fetched Neighbor",
        "Fetched Neighbor",
        Some(26),
        vec![related(5, "New Sibling", "New Sibling", "SEQUEL", Some(12))],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(2, neighbor_2);
    // Note: id=7 is intentionally NOT in the map.

    let graft = transitive_relation_graft(&parent, &neighbors);
    assert_eq!(graft.len(), 1);
    assert_eq!(graft[0].id, 5);
}

#[test]
fn expand_parent_with_transitive_relations_extends_relations_vec() {
    // Integration: the wrapper appends the graft onto the cloned
    // parent's relations vec. Previously-present relations stay.
    let parent = detail_with_relations(
        1,
        "Parent",
        "Parent",
        Some(12),
        vec![related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26))],
    );
    let neighbor = detail_with_relations(
        2,
        "Neighbor",
        "Neighbor",
        Some(26),
        vec![related(3, "Grafted", "Grafted", "SEQUEL", Some(13))],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(2, neighbor);

    let expanded = expand_parent_with_transitive_relations(&parent, &neighbors);
    assert_eq!(expanded.relations.len(), 2);
    let ids: Vec<i64> = expanded.relations.iter().map(|r| r.id).collect();
    assert!(ids.contains(&2), "direct relation must remain");
    assert!(ids.contains(&3), "grafted relation must be appended");
}

#[test]
fn expand_parent_with_empty_graft_still_returns_clone() {
    // When the walk produces no graft (e.g. no walkable direct
    // relations), the wrapper still returns a cloned detail so
    // callers can pass a single owned AnimeDetail into sibling
    // detection regardless.
    let parent = detail_with_relations(1, "Parent", "Parent", Some(12), vec![]);
    let neighbors = std::collections::HashMap::new();
    let expanded = expand_parent_with_transitive_relations(&parent, &neighbors);
    assert_eq!(expanded.id, parent.id);
    assert!(expanded.relations.is_empty());
}

#[test]
fn detect_siblings_finds_grafted_relation_after_transitive_walk() {
    // End-to-end: parent has no direct edge to the sibling whose
    // episodes are in the pack, but a PREQUEL neighbor does. After
    // running the expand step, detect_sibling_entries_in_pack
    // should pick up the grafted sibling. This is the Monogatari
    // missing-edge case modeled structurally.
    let parent_id = 21262;
    let neighbor_id = 20899;
    let grafted_id = 99423;

    let parent = detail_with_relations(
        parent_id,
        "Parent Show",
        "Parent Show",
        Some(12),
        vec![related(
            neighbor_id,
            "Parent Show Franchise",
            "Parent Show Franchise",
            "PREQUEL",
            Some(26),
        )],
    );
    let neighbor = detail_with_relations(
        neighbor_id,
        "Parent Show Franchise",
        "Parent Show Franchise",
        Some(26),
        vec![related(
            grafted_id,
            "Parent Show: Continuation Arc",
            "Parent Show: Continuation Arc",
            "SEQUEL",
            Some(7),
        )],
    );
    let mut neighbors = std::collections::HashMap::new();
    neighbors.insert(neighbor_id, neighbor);

    let expanded = expand_parent_with_transitive_relations(&parent, &neighbors);

    // Subtitle path: filenames mention "Continuation Arc" as the
    // trailing subtitle. Use synthetic tokens — no real group or
    // real release title formatting claimed here.
    let files: Vec<String> = (1..=7)
        .map(|n| {
            format!(
                "fixture-parent-show continuation arc - {:02} (bd 1080p) [hash].mkv",
                n
            )
        })
        .collect();

    let siblings = detect_sibling_entries_in_pack(&files, &expanded);
    assert_eq!(
        siblings.len(),
        1,
        "grafted sibling should be detected after transitive walk"
    );
    assert_eq!(siblings[0].anilist_id, grafted_id);
    assert_eq!(siblings[0].file_indices.len(), 7);
}

#[test]
fn detect_siblings_subtitle_path_keeps_offset_zero_for_arc_local_numbering() {
    // Subtitle-matched sibling where filenames use arc-local
    // numbering (E01..Esib_cap within their own arc). The per-
    // sibling offset pass must leave offset=0 because
    // min_ep=1 ≤ parent_cap. Contrived parent with a 2-token,
    // non-ordinal trailing subtitle so the subtitle path fires
    // deterministically.
    let mut parent = detail_with_titles("Parent Show", "Parent Show");
    parent.id = 1;
    parent.episodes = Some(24);
    parent.relations = vec![related(
        2,
        "Parent Show: Alpha Beta",
        "Parent Show: Alpha Beta",
        "SEQUEL",
        Some(12),
    )];
    let files: Vec<String> = (1..=12)
        .map(|n| format!("[Group] Parent Show - Alpha Beta - {:02}.mkv", n))
        .collect();

    let siblings = detect_sibling_entries_in_pack(&files, &parent);
    assert_eq!(siblings.len(), 1);
    assert_eq!(siblings[0].anilist_id, 2);
    assert_eq!(siblings[0].file_indices.len(), 12);
    // min_ep = 1 ≤ parent_cap=24 → offset = 0.
    assert_eq!(siblings[0].episode_offset, 0);
    // And the match came from the subtitle path, not the fallback.
    assert!(
        !siblings[0]
            .matched_subtitle
            .starts_with("episode-range fallback")
    );
}
