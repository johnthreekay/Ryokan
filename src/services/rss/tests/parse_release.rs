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
    // episode 3 from "season 3 (". The season is a season; no episode
    // number survives.
    let parsed = parse_release(&item(
        "[Kaizoku] Jujutsu Kaisen Season 3 (WEB 1080p HEVC EAC-3) | The Culling Game Part 1",
    ));
    assert_eq!(parsed.season_hint, Some(3), "season_hint should be 3");
    assert!(
        parsed.title.release_episodes().is_empty(),
        "no episodes expected, got {:?}",
        parsed.title.release_episodes()
    );
}

#[test]
fn hyphen_space_episode_parses() {
    // The standard "[Group] Series - 01 (1080p)" shape is episode 1.
    let parsed = parse_release(&item("[SubsPlease] Frieren - 01 (1080p) [ABCD1234].mkv"));
    assert!(parsed.title.release_episodes().contains(&1));
    assert_eq!(parsed.parse_mode, "absolute_dash");
}

#[test]
fn s3_prefix_does_not_leak_season_digit_to_episode() {
    // "[Group] Series S3 - 05 (1080p)" is season 3 episode 5, the
    // claim bound to the season.
    let parsed = parse_release(&item("[Group] Cool Anime S3 - 05 (1080p)"));
    assert_eq!(parsed.season_hint, Some(3));
    assert!(parsed.season_bound);
    assert_eq!(
        parsed
            .title
            .release_episodes()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![5]
    );
}

#[test]
fn nrd_season_marker_is_not_an_episode() {
    // "3rd Season" does not leak its "3" as an episode.
    let parsed = parse_release(&item("[Group] Series 3rd Season (WEB 1080p)"));
    assert_eq!(parsed.season_hint, Some(3));
    assert!(parsed.title.release_episodes().is_empty());
}

#[test]
fn cour_marker_is_not_an_episode() {
    // "Cour 2" does not leak "2" as an episode.
    let parsed = parse_release(&item("[Group] Series Cour 2 (WEB 1080p)"));
    assert!(parsed.title.release_episodes().is_empty());
}

#[test]
fn a_number_in_the_series_name_is_not_an_episode() {
    // `Kaiju No. 8 - 10` used to read as episodes 8-10 and tag all three.
    let parsed = parse_release(&item(
        "[SubsPlease] Kaiju No. 8 - 10 (1080p) [ABCD1234].mkv",
    ));
    assert_eq!(
        parsed
            .title
            .release_episodes()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![10]
    );
}
