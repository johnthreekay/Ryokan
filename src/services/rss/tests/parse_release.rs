use super::super::*;

fn item(title: &str) -> RssItem {
    RssItem {
        title: title.to_string(),
        link: String::new(),
        guid: String::new(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: extract_group(title),
        resolution: extract_resolution(title),
        is_batch: crate::services::release_title::parse(title).is_batch(),
        source: RssSource::Nyaa,
    }
}

#[test]
fn season_digit_is_not_parsed_as_absolute_episode() {
    // Regression: "[Kaizoku] Jujutsu Kaisen Season 3 (WEB 1080p HEVC
    // EAC-3) | The Culling Game Part 1" used to extract absolute
    // episode 3 from "season 3 (" via RE_ABSOLUTE's digit-before-
    // paren pattern. After the season-marker masking pass, "season
    // 3" and "part 1" are stripped from the absolute search window
    // so no spurious episode number survives.
    let parsed = parse_release(&item(
        "[Kaizoku] Jujutsu Kaisen Season 3 (WEB 1080p HEVC EAC-3) | The Culling Game Part 1",
    ));
    assert_eq!(parsed.season_hint, Some(3), "season_hint should be 3");
    assert!(
        parsed.absolute_eps.is_empty(),
        "absolute_eps should be empty, got {:?}",
        parsed.absolute_eps
    );
    assert!(
        parsed.season_relative_eps.is_empty(),
        "season_relative_eps should be empty, got {:?}",
        parsed.season_relative_eps
    );
}

#[test]
fn hyphen_space_episode_still_parses_after_mask() {
    // Sanity: the standard "[Group] Series - 01 (1080p)" shape
    // should still resolve to absolute episode 1 after the mask
    // pass. The mask strips optional season tokens; there are none
    // here so the title passes through unchanged.
    let parsed = parse_release(&item("[SubsPlease] Frieren - 01 (1080p) [ABCD1234].mkv"));
    assert!(parsed.absolute_eps.contains(&1));
}

#[test]
fn s3_prefix_does_not_leak_season_digit_to_episode() {
    // "[Group] Series S3 - 05 (1080p)" should extract season 3,
    // episode 5 — not both-3-and-5. Belongs to the season-dash
    // patterns, resolved before the absolute fallback runs, but
    // verify nothing regresses.
    let parsed = parse_release(&item("[Group] Cool Anime S3 - 05 (1080p)"));
    assert_eq!(parsed.season_hint, Some(3));
    assert!(
        parsed.season_relative_eps.contains(&5) || parsed.absolute_eps.contains(&5),
        "episode 5 should be resolved; got rel={:?} abs={:?}",
        parsed.season_relative_eps,
        parsed.absolute_eps
    );
}

#[test]
fn nrd_season_marker_masked() {
    // "3rd Season" should not leak its "3" as an absolute episode.
    let parsed = parse_release(&item("[Group] Series 3rd Season (WEB 1080p)"));
    assert_eq!(parsed.season_hint, Some(3));
    assert!(parsed.absolute_eps.is_empty());
}

#[test]
fn cour_marker_masked() {
    // "Cour 2" should not leak "2" to the absolute pass.
    let parsed = parse_release(&item("[Group] Series Cour 2 (WEB 1080p)"));
    assert!(parsed.absolute_eps.is_empty());
}
