use super::*;

fn ep(template: &str, ctx: &NameContext) -> String {
    render(TemplateKind::EpisodeFile, template, ctx)
        .expect("renders")
        .name
}

// ── Defaults reproduce the pre-#124 layout ──────────────────────────

#[test]
fn defaults_render_the_previous_hardcoded_layout() {
    let sample = sample_context();
    let series = render(
        TemplateKind::SeriesFolder,
        DEFAULT_SERIES_FOLDER_FORMAT,
        &sample,
    )
    .unwrap();
    assert_eq!(series.name, "Sousou no Frieren");
    let season = render(
        TemplateKind::SeasonFolder,
        DEFAULT_SEASON_FOLDER_FORMAT,
        &sample,
    )
    .unwrap();
    assert_eq!(season.name, "Season 01");
    assert_eq!(
        ep(DEFAULT_EPISODE_FILE_FORMAT, &sample),
        "Sousou no Frieren - S01E07 - Like a Fairy Tale.mkv"
    );
    // No episode title: the ` - ` before it goes too, exactly like the
    // old conditional `dest_stem`.
    assert_eq!(
        ep(DEFAULT_EPISODE_FILE_FORMAT, &sparse_context()),
        "Sousou no Frieren - S01E07.mkv"
    );
}

#[test]
fn every_default_validates() {
    for kind in [
        TemplateKind::SeriesFolder,
        TemplateKind::SeasonFolder,
        TemplateKind::EpisodeFile,
    ] {
        validate(kind, kind.default_template()).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
    }
}

// ── Tokens ──────────────────────────────────────────────────────────

#[test]
fn each_token_renders_its_field() {
    let s = sample_context();
    assert_eq!(
        ep(
            "[{group}] {series.title} ({series.year}) - {episode.number:000} [{quality.full}]{ext}",
            &s
        ),
        "[SubsPlease] Sousou no Frieren (2023) - 007 [1080p WEB-DL].mkv"
    );
    assert_eq!(
        ep(
            "{quality.resolution} {quality.source} {episode.number}{ext}",
            &s
        ),
        "1080p WEB-DL 7.mkv"
    );
    assert_eq!(
        ep("{season.number} {episode.number} {episode.title}{ext}", &s),
        "1 7 Like a Fairy Tale.mkv"
    );
}

#[test]
fn zero_pad_spec_pads_to_its_width() {
    let mut s = sample_context();
    assert_eq!(ep("{episode.number:00}{ext}", &s), "07.mkv");
    assert_eq!(ep("{episode.number:000}{ext}", &s), "007.mkv");
    s.episode_number = 123;
    assert_eq!(ep("{episode.number:00}{ext}", &s), "123.mkv");
    assert_eq!(
        ep("S{season.number:00}E{episode.number:00}{ext}", &s),
        "S01E123.mkv"
    );
}

#[test]
fn ext_carries_its_own_dot() {
    let mut s = sample_context();
    s.ext = "mp4".to_string();
    assert_eq!(
        ep("{series.title} - {episode.number:00}{ext}", &s),
        "Sousou no Frieren - 07.mp4"
    );
    let stem = episode_file("{series.title} - {episode.number:00}{ext}", &s);
    assert_eq!(stem.stem, "Sousou no Frieren - 07");
    assert_eq!(stem.file_name, "Sousou no Frieren - 07.mp4");
}

// ── Empty-token cleanup ─────────────────────────────────────────────

#[test]
fn empty_values_take_their_separators_and_brackets_with_them() {
    let sparse = sparse_context();
    assert_eq!(
        ep(
            "[{quality.full}] {series.title} - {episode.number:00} [{group}]{ext}",
            &sparse
        ),
        "Sousou no Frieren - 07.mkv"
    );
    assert_eq!(
        ep(
            "{series.title} ({series.year}) - S{season.number:00}E{episode.number:00} - {episode.title}{ext}",
            &sparse
        ),
        "Sousou no Frieren - S01E07.mkv"
    );
    // Dot-style scene templates collapse the doubled dot.
    assert_eq!(
        ep(
            "{series.title}.{series.year}.S{season.number:00}E{episode.number:00}.{episode.title}{ext}",
            &sparse
        ),
        "Sousou no Frieren.S01E07.mkv"
    );
    // Leading empties leave no dangling separator either.
    assert_eq!(
        ep(
            "{group} - {series.title} - {episode.number:00}{ext}",
            &sparse
        ),
        "Sousou no Frieren - 07.mkv"
    );
}

#[test]
fn cleanup_never_touches_a_value() {
    let mut s = sample_context();
    s.series_title = "Is It Wrong... to Pick Up [Girls]".to_string();
    s.episode_title = "Part 1 - Part 2".to_string();
    assert_eq!(
        ep(DEFAULT_EPISODE_FILE_FORMAT, &s),
        "Is It Wrong... to Pick Up [Girls] - S01E07 - Part 1 - Part 2.mkv"
    );
}

#[test]
fn path_illegal_characters_are_sanitized_in_values_and_literals() {
    let mut s = sample_context();
    s.series_title = "Re:Zero kara Hajimeru Isekai Seikatsu".to_string();
    s.episode_title = "What Is Lost?".to_string();
    assert_eq!(
        ep(DEFAULT_EPISODE_FILE_FORMAT, &s),
        "Re_Zero kara Hajimeru Isekai Seikatsu - S01E07 - What Is Lost_.mkv"
    );
    // A colon typed into the template itself is replaced too.
    assert_eq!(
        ep(
            "{series.title}: {episode.number:00}{ext}",
            &sample_context()
        ),
        "Sousou no Frieren_ 07.mkv"
    );
}

// ── Validation ──────────────────────────────────────────────────────

#[test]
fn episode_template_must_end_with_ext_exactly_once() {
    let err = validate(
        TemplateKind::EpisodeFile,
        "{series.title} - {episode.number:00}",
    )
    .unwrap_err();
    assert!(err.contains("must end with {ext}"), "{err}");
    let err = validate(
        TemplateKind::EpisodeFile,
        "{ext} {series.title} - {episode.number:00}{ext}",
    )
    .unwrap_err();
    assert!(err.contains("only once"), "{err}");
    let err = validate(
        TemplateKind::EpisodeFile,
        "{series.title} - {episode.number:00}{ext} ",
    );
    assert!(
        err.is_ok(),
        "trailing whitespace after {{ext}} is tolerated: {err:?}"
    );
}

#[test]
fn folder_templates_reject_ext_and_episode_tokens() {
    let err = validate(TemplateKind::SeriesFolder, "{series.title}{ext}").unwrap_err();
    assert!(
        err.contains("only belongs in the episode file template"),
        "{err}"
    );
    let err = validate(
        TemplateKind::SeriesFolder,
        "{series.title} {episode.number}",
    )
    .unwrap_err();
    assert!(
        err.contains("not available in the series folder template"),
        "{err}"
    );
    let err = validate(TemplateKind::SeriesFolder, "{series.title} {season.number}").unwrap_err();
    assert!(err.contains("not available"), "{err}");
    assert!(
        validate(
            TemplateKind::SeasonFolder,
            "S{season.number:00} {series.year}"
        )
        .is_ok()
    );
}

#[test]
fn path_separators_and_empty_templates_are_rejected() {
    for kind in [
        TemplateKind::SeriesFolder,
        TemplateKind::SeasonFolder,
        TemplateKind::EpisodeFile,
    ] {
        let err = validate(kind, "   ").unwrap_err();
        assert!(err.contains("is empty"), "{err}");
        let err = validate(kind, "a/b").unwrap_err();
        assert!(err.contains("cannot contain / or \\"), "{err}");
        let err = validate(kind, "a\\b").unwrap_err();
        assert!(err.contains("cannot contain / or \\"), "{err}");
    }
}

#[test]
fn syntax_errors_are_named() {
    let err = validate(TemplateKind::SeriesFolder, "{series.titel}").unwrap_err();
    assert!(err.contains("{series.titel} is not a known token"), "{err}");
    let err = validate(TemplateKind::SeriesFolder, "{series.title").unwrap_err();
    assert!(err.contains("never closed"), "{err}");
    let err = validate(TemplateKind::SeriesFolder, "series.title}").unwrap_err();
    assert!(err.contains("no matching '{'"), "{err}");
    let err = validate(TemplateKind::EpisodeFile, "{episode.number:xx}{ext}").unwrap_err();
    assert!(err.contains("not a supported format"), "{err}");
}

#[test]
fn episode_template_must_carry_the_episode_number() {
    let err = validate(
        TemplateKind::EpisodeFile,
        "{series.title} - {episode.title}{ext}",
    )
    .unwrap_err();
    assert!(err.contains("must include {episode.number}"), "{err}");
}

#[test]
fn episode_template_must_parse_back_to_the_episode() {
    // `Title 7.mkv` has no marker the scanner recognizes.
    let err = validate(
        TemplateKind::EpisodeFile,
        "{series.title} {episode.number}{ext}",
    )
    .unwrap_err();
    assert!(err.contains("cannot read the episode number back"), "{err}");
    assert!(
        validate(
            TemplateKind::EpisodeFile,
            "{series.title} - {episode.number:00}{ext}"
        )
        .is_ok()
    );
    assert!(
        validate(
            TemplateKind::EpisodeFile,
            "{series.title} E{episode.number:00}{ext}"
        )
        .is_ok()
    );
    assert!(
        validate(
            TemplateKind::EpisodeFile,
            "{series.title}.S{season.number:00}E{episode.number:00}.{quality.full}{ext}"
        )
        .is_ok()
    );
}

#[test]
fn templates_that_go_empty_without_optional_details_are_rejected() {
    let err = validate(TemplateKind::SeasonFolder, "{series.year}").unwrap_err();
    assert!(err.contains("optional details"), "{err}");
    // The title is never optional, so this one is fine.
    assert!(validate(TemplateKind::SeriesFolder, "{series.title} ({series.year})").is_ok());
}

// ── Truncation ──────────────────────────────────────────────────────

#[test]
fn long_series_title_is_truncated_first_and_the_episode_number_survives() {
    let mut s = sample_context();
    s.series_title = "A".repeat(300);
    let r = render(TemplateKind::EpisodeFile, DEFAULT_EPISODE_FILE_FORMAT, &s).unwrap();
    assert!(r.truncated);
    assert!(r.name.len() <= MAX_COMPONENT_BYTES, "{}", r.name.len());
    assert!(
        r.name.ends_with(" - S01E07 - Like a Fairy Tale.mkv"),
        "{}",
        r.name
    );
    assert!(r.name.contains('\u{2026}'));
}

#[test]
fn truncation_respects_char_boundaries() {
    let mut s = sample_context();
    s.series_title = "葬送のフリーレン".repeat(20);
    let r = render(TemplateKind::EpisodeFile, DEFAULT_EPISODE_FILE_FORMAT, &s).unwrap();
    assert!(r.truncated);
    assert!(r.name.len() <= MAX_COMPONENT_BYTES);
    assert!(r.name.ends_with(" - S01E07 - Like a Fairy Tale.mkv"));
}

#[test]
fn short_names_are_never_marked_truncated() {
    let r = render(
        TemplateKind::EpisodeFile,
        DEFAULT_EPISODE_FILE_FORMAT,
        &sample_context(),
    )
    .unwrap();
    assert!(!r.truncated);
}

// ── Fallbacks and helpers ───────────────────────────────────────────

#[test]
fn render_or_default_falls_back_to_the_default_template() {
    let s = sample_context();
    let r = render_or_default(TemplateKind::EpisodeFile, "{broken", &s);
    assert_eq!(r.name, "Sousou no Frieren - S01E07 - Like a Fairy Tale.mkv");
    let r = render_or_default(TemplateKind::SeasonFolder, "", &s);
    assert_eq!(r.name, "Season 01");
}

#[test]
fn preferred_title_follows_the_language_with_fallbacks() {
    let names = SeriesNames {
        title: "Fallback",
        romaji: "Sousou no Frieren",
        english: "Frieren: Beyond Journey's End",
        native: "葬送のフリーレン",
        year: Some(2023),
    };
    assert_eq!(
        names.preferred_title("english"),
        "Frieren: Beyond Journey's End"
    );
    assert_eq!(names.preferred_title("romaji"), "Sousou no Frieren");
    assert_eq!(names.preferred_title("native"), "葬送のフリーレン");
    let only_title = SeriesNames {
        title: "Fallback",
        ..Default::default()
    };
    assert_eq!(only_title.preferred_title("romaji"), "Fallback");
    // The folder helper sanitizes the colon the English title carries.
    assert_eq!(
        series_folder(DEFAULT_SERIES_FOLDER_FORMAT, "english", &names),
        "Frieren_ Beyond Journey's End"
    );
    assert_eq!(
        series_folder("{series.title} ({series.year})", "romaji", &names),
        "Sousou no Frieren (2023)"
    );
    assert_eq!(
        season_folder("Season {season.number}", "romaji", &names, 1),
        "Season 1"
    );
}

#[test]
fn quality_labels_compose() {
    assert_eq!(quality_source_label("BluRay", true, ""), "BluRay Remux");
    assert_eq!(quality_source_label("BluRay", false, ""), "BluRay");
    assert_eq!(quality_source_label("Web", false, "WEBRip"), "WEBRip");
    assert_eq!(quality_source_label("Web", false, "WEB-DL"), "WEB-DL");
    assert_eq!(quality_source_label("Web", false, ""), "WEB");
    assert_eq!(quality_source_label("DVD", false, ""), "DVD");
    assert_eq!(quality_source_label("Unknown", false, ""), "");
    assert_eq!(quality_full("1080p", "BluRay Remux"), "1080p BluRay Remux");
    assert_eq!(quality_full("", "WEB-DL"), "WEB-DL");
    assert_eq!(quality_full("720p", ""), "720p");
    assert_eq!(quality_full("", ""), "");
}

#[test]
fn preview_reports_validation_errors_and_renders_the_sample() {
    let ok = preview(TemplateKind::EpisodeFile, DEFAULT_EPISODE_FILE_FORMAT).unwrap();
    assert_eq!(
        ok.name,
        "Sousou no Frieren - S01E07 - Like a Fairy Tale.mkv"
    );
    assert!(preview(TemplateKind::EpisodeFile, "{series.title}").is_err());
}

#[test]
fn kind_keys_round_trip() {
    for kind in [
        TemplateKind::SeriesFolder,
        TemplateKind::SeasonFolder,
        TemplateKind::EpisodeFile,
    ] {
        assert_eq!(TemplateKind::from_key(kind.key()), Some(kind));
    }
    assert_eq!(TemplateKind::from_key("nope"), None);
}

#[tokio::test]
async fn series_upsert_applies_the_series_folder_template_once() {
    use crate::models::{config, series};
    use crate::test_support::in_memory_pool;

    let db = in_memory_pool().await;
    let mut cfg = config::Config {
        title_language: "romaji".to_string(),
        series_folder_format: "{series.title} ({series.year})".to_string(),
        ..Default::default()
    };
    config::save_config(&db, &cfg).await.unwrap();

    let core = || series::SeriesCore {
        anilist_id: 154587,
        mal_id: None,
        title: "Frieren: Beyond Journey's End",
        title_romaji: "Sousou no Frieren",
        title_english: "Frieren: Beyond Journey's End",
        title_native: "葬送のフリーレン",
        cover_url: "",
        format: "TV",
        status: "FINISHED",
        episodes: Some(28),
        season_year: Some(2023),
        end_year: Some(2024),
    };
    let (id, created) = series::upsert(&db, core()).await.unwrap();
    assert!(created);
    let row = series::get_by_id(&db, id).await.unwrap().unwrap();
    assert_eq!(row.folder_name, "Sousou no Frieren (2023)");

    // A later template change never renames an existing series.
    cfg.series_folder_format = "{series.title}".to_string();
    config::save_config(&db, &cfg).await.unwrap();
    let (again, created) = series::upsert(&db, core()).await.unwrap();
    assert_eq!(again, id);
    assert!(!created);
    let row = series::get_by_id(&db, id).await.unwrap().unwrap();
    assert_eq!(row.folder_name, "Sousou no Frieren (2023)");
}

#[tokio::test]
async fn series_upsert_without_a_config_row_uses_the_default_layout() {
    use crate::models::series;
    use crate::test_support::in_memory_pool;

    let db = in_memory_pool().await;
    let core = series::SeriesCore {
        anilist_id: 1,
        mal_id: None,
        title: "Cowboy Bebop",
        title_romaji: "Cowboy Bebop",
        title_english: "Cowboy Bebop",
        title_native: "カウボーイビバップ",
        cover_url: "",
        format: "TV",
        status: "FINISHED",
        episodes: Some(26),
        season_year: Some(1998),
        end_year: Some(1999),
    };
    let (id, _) = series::upsert(&db, core).await.unwrap();
    let row = series::get_by_id(&db, id).await.unwrap().unwrap();
    assert_eq!(row.folder_name, "Cowboy Bebop");
}

// ── Multi-episode files (issue #246) ────────────────────────────────

#[test]
fn multi_episode_context_renders_a_range_that_parses_back() {
    let multi = multi_episode_sample_context();
    // The default template: Sonarr's prefixed range after the `E`.
    let name = ep(DEFAULT_EPISODE_FILE_FORMAT, &multi);
    assert_eq!(
        name,
        "Sousou no Frieren - S01E07-E08 - Like a Fairy Tale + The Land Where the Soul Rests.mkv"
    );
    let span = parse_episode_span(&name.to_lowercase()).expect("parses");
    assert_eq!((span.season, span.first, span.last), (Some(1), 7, 8));

    // The dash slot: a bare range.
    let name = ep(
        "{series.title} - {episode.number:00} [{group}]{ext}",
        &multi,
    );
    assert_eq!(name, "Sousou no Frieren - 07-08 [SubsPlease].mkv");
    let span = parse_episode_span(&name.to_lowercase()).expect("parses");
    assert_eq!((span.season, span.first, span.last), (None, 7, 8));

    // The letter copies the literal's case; padding applies to both.
    assert_eq!(
        ep("s{season.number:00}e{episode.number:000}{ext}", &multi),
        "s01e007-e008.mkv"
    );
    assert_eq!(ep("{episode.number}{ext}", &multi), "7-8.mkv");
}

#[test]
fn single_episode_context_is_unchanged_by_episode_last() {
    let mut s = sample_context();
    assert!(!s.is_multi_episode());
    assert_eq!(s.slot_label(), "S01E07");
    // `episode_last` equal to the first episode is still a single.
    s.episode_last = 7;
    assert!(!s.is_multi_episode());
    assert_eq!(
        ep(DEFAULT_EPISODE_FILE_FORMAT, &s),
        "Sousou no Frieren - S01E07 - Like a Fairy Tale.mkv"
    );
    assert_eq!(multi_episode_sample_context().slot_label(), "S01E07-E08");
}

#[test]
fn every_default_validates_for_a_multi_episode_file() {
    // `validate` renders the two-episode sample and parses it back; the
    // defaults and the common alternatives all survive.
    for template in [
        DEFAULT_EPISODE_FILE_FORMAT,
        "{series.title} - {episode.number:00}{ext}",
        "{series.title} - E{episode.number:00} - {episode.title}{ext}",
        "[{group}] {series.title} - {episode.number:00} [{quality.full}]{ext}",
    ] {
        validate(TemplateKind::EpisodeFile, template).unwrap_or_else(|e| panic!("{template}: {e}"));
    }
}

#[test]
fn fallback_name_carries_the_range() {
    let multi = multi_episode_sample_context();
    let r = render_or_default(TemplateKind::EpisodeFile, "{{broken", &multi);
    // The default template still renders, so the fallback is the
    // default's shape, not the bare stem.
    assert!(r.name.contains("S01E07-E08"), "{}", r.name);
}
