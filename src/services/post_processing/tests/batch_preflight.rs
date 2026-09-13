use crate::services::download_client::DownloadFile;
use crate::services::post_processing::{
    ready_wanted_video_indices, requires_episode_map_preflight, span_from_grab_episodes,
    validate_batch_episode_map,
};

#[test]
fn accepts_unique_dot_delimited_complete_series_batch() {
    let files = vec![
        (
            0,
            42,
            None,
            0,
            "[SoM] Dragon.Ball.001.V2.DVD.480p.mkv".to_string(),
        ),
        (
            1,
            42,
            None,
            0,
            "[SoM] Dragon.Ball.002.V2.DVD.480p.mkv".to_string(),
        ),
        (
            2,
            42,
            None,
            0,
            "[SoM] Dragon.Ball.153.DVD.480p.mkv".to_string(),
        ),
    ];
    assert!(validate_batch_episode_map(&files).is_ok());
}

#[test]
fn unparseable_extras_are_skipped_without_failing_the_batch() {
    // Real packs routinely ship NCOP/NCED/PV/CM/menu videos that parse to
    // `None` by design. They must be absent from the plan (the import loop
    // warn-skips them) without stranding the parseable siblings.
    let files = vec![
        (0, 42, None, 0, "Dragon.Ball.001.mkv".to_string()),
        (1, 42, None, 0, "Dragon.Ball.movie.mkv".to_string()),
        (
            2,
            42,
            None,
            0,
            "[Moozzi2] Anne of Green Gables [SP01] NCOP (BD 1440x1080 x.265 Flac).mkv".to_string(),
        ),
        (3, 42, None, 0, "Dragon.Ball.002.mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(plan.slots.contains_key(&0));
    assert!(plan.slots.contains_key(&3));
    assert!(!plan.slots.contains_key(&1));
    assert!(!plan.slots.contains_key(&2));
}

#[test]
fn non_positive_resolved_episode_is_skipped_without_failing_the_batch() {
    // E00 specials (and files landing on a sibling whose offset exceeds
    // their own number) resolve non-positive; skip them, keep the rest.
    let files = vec![
        (0, 42, None, 0, "Show - 00 - Special.mkv".to_string()),
        (1, 42, None, 0, "Show - 01.mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(!plan.slots.contains_key(&0));
    assert!(plan.slots.contains_key(&1));
}

#[test]
fn rejects_duplicate_destination_within_batch() {
    let files = vec![
        (0, 42, None, 0, "Show.S01E01.first.mkv".to_string()),
        (1, 42, None, 0, "Show.S01E01.second.mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("mapped both"));
    assert!(err.contains("series 42 episode 1"));
    assert!(err.contains("no files were changed"));
}

#[test]
fn misclassified_multi_video_grab_still_rejects_duplicate_destination() {
    let is_batch = false;
    let files = vec![
        (0, 42, None, 0, "Show.S01E01.first.mkv".to_string()),
        (1, 42, None, 0, "Show.S01E01.second.mkv".to_string()),
    ];

    assert!(!is_batch);
    assert!(requires_episode_map_preflight(is_batch, files.len()));
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("mapped both"));
    assert!(err.contains("series 42 episode 1"));
}

#[test]
fn same_episode_is_valid_when_routed_to_different_series() {
    let files = vec![
        (0, 42, None, 0, "Parent.S01E01.mkv".to_string()),
        (1, 43, None, 0, "Sibling.S01E01.mkv".to_string()),
    ];
    assert!(validate_batch_episode_map(&files).is_ok());
}

#[test]
fn route_offsets_are_checked_against_final_episode_slot() {
    let files = vec![
        (0, 43, Some(12), 0, "Arc.S01E13.first.mkv".to_string()),
        (1, 43, Some(13), 0, "Arc.S01E14.second.mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("series 43 episode 1"));
}

#[test]
fn cumulative_offset_collision_is_rejected_before_import() {
    let files = vec![
        (0, 42, None, 12, "Show.S01E01.mkv".to_string()),
        (1, 42, None, 12, "Show.013.mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("series 42 episode 1"));
}

#[test]
fn incomplete_wanted_video_blocks_the_entire_batch() {
    let files = vec![
        DownloadFile {
            name: "Show.001.mkv".to_string(),
            size: 100,
            progress: 1.0,
            wanted: true,
        },
        DownloadFile {
            name: "Show.002.mkv".to_string(),
            size: 100,
            progress: 0.5,
            wanted: true,
        },
    ];
    let err = ready_wanted_video_indices(&files).unwrap_err();
    assert!(err.contains("1 of 2 wanted video files are incomplete"));
}

#[test]
fn complete_unwanted_video_is_excluded_from_import_plan() {
    let files = vec![
        DownloadFile {
            name: "Show.001.mkv".to_string(),
            size: 100,
            progress: 1.0,
            wanted: true,
        },
        DownloadFile {
            name: "Show.002.mkv".to_string(),
            size: 100,
            progress: 1.0,
            wanted: false,
        },
    ];
    assert_eq!(ready_wanted_video_indices(&files).unwrap(), vec![0]);
}

// ── Release-version preference (issue #204) ─────────────────────────

#[test]
fn higher_release_version_wins_the_slot_and_supersedes_the_rest() {
    let files = vec![
        (0, 42, None, 0, "Show - 05 (720p).mkv".to_string()),
        (1, 42, None, 0, "Show - 05v2 (1080p).mkv".to_string()),
        (2, 42, None, 0, "Show - 06 (1080p).mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert_eq!(plan.slots.get(&1).map(|r| r.episode), Some(5));
    assert!(!plan.slots.contains_key(&0));
    assert_eq!(plan.superseded.get(&0), Some(&1));
    assert!(plan.slots.contains_key(&2));
    assert_eq!(plan.superseded.len(), 1);
}

#[test]
fn version_order_does_not_depend_on_file_order() {
    let files = vec![
        (0, 42, None, 0, "Show - 05v3 (1080p).mkv".to_string()),
        (1, 42, None, 0, "Show - 05 (1080p).mkv".to_string()),
        (2, 42, None, 0, "Show - 05v2 (1080p).mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(plan.slots.contains_key(&0));
    assert_eq!(plan.superseded.get(&1), Some(&0));
    assert_eq!(plan.superseded.get(&2), Some(&0));
}

#[test]
fn equal_versions_in_one_slot_still_fail_closed() {
    // Two v2s tie; the v1 underneath doesn't rescue the slot.
    let files = vec![
        (0, 42, None, 0, "Show - 05v2 (720p).mkv".to_string()),
        (1, 42, None, 0, "Show - 05v2 (1080p).mkv".to_string()),
        (2, 42, None, 0, "Show - 05 (480p).mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("mapped both"));
    assert!(err.contains("Show - 05v2 (720p).mkv"));
    assert!(err.contains("Show - 05v2 (1080p).mkv"));
    assert!(err.contains("no files were changed"));
}

#[test]
fn unversioned_duplicates_still_fail_closed() {
    let files = vec![
        (0, 42, None, 0, "Show - 05 (720p).mkv".to_string()),
        (1, 42, None, 0, "Show - 05 (1080p).mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("series 42 episode 5"));
}

#[test]
fn sxxexx_version_suffix_is_read_as_a_version() {
    let files = vec![
        (0, 42, None, 0, "Show.S01E10.1080p.mkv".to_string()),
        (1, 42, None, 0, "Show.S01E10v2.1080p.mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(plan.slots.contains_key(&1));
    assert_eq!(plan.superseded.get(&0), Some(&1));
}

// ── Non-episodic extras (issue #203) ────────────────────────────────

#[test]
fn corpus_pack_extras_are_skipped_without_failing_the_batch() {
    // The two real packs from the pinned corpus that used to fail closed:
    // Erai-raws' `SP` / `07_5` recap and Moozzi2's `PV` files collided
    // with the E02 / E07 / E01 slots.
    let files = vec![
        (
            0,
            42,
            None,
            0,
            "[Erai-raws] 86 Eighty-Six Part 2 - 02 [480p][Multiple Subtitle][2E7DEB0E].mkv"
                .to_string(),
        ),
        (
            1,
            42,
            None,
            0,
            "[Erai-raws] 86 Eighty-Six Part 2 - 07 [480p][Multiple Subtitle][E9DFD7E8].mkv"
                .to_string(),
        ),
        (
            2,
            42,
            None,
            0,
            "[Erai-raws] 86 Eighty-Six Part 2 - 07_5 [480p][Multiple Subtitle][3CC7C577].mkv"
                .to_string(),
        ),
        (
            3,
            42,
            None,
            0,
            "[Erai-raws] 86 Eighty-Six Part 2 - SP [480p][Multiple Subtitle][FDBE49E5].mkv"
                .to_string(),
        ),
        (
            4,
            43,
            None,
            0,
            "[npz-Moozzi2] Runway De Waratte - 01 (US BD, sub-only, 1080p) [C30B6E41].mkv"
                .to_string(),
        ),
        (
            5,
            43,
            None,
            0,
            "[npz-Moozzi2] Runway De Waratte - Character PV 1 (US BD, 1080p) [E392C1D3].mkv"
                .to_string(),
        ),
        (
            6,
            43,
            None,
            0,
            "[npz-Moozzi2] Runway De Waratte - PV 1 (US BD, 1080p) [746B23B1].mkv".to_string(),
        ),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    let mut kept: Vec<usize> = plan.slots.keys().copied().collect();
    kept.sort_unstable();
    assert_eq!(kept, vec![0, 1, 4]);
    assert!(plan.superseded.is_empty());
}

// ── Multi-episode files (issue #246) ────────────────────────────────

#[test]
fn multi_episode_file_takes_every_slot_in_its_span() {
    let files = vec![
        (0, 42, None, 0, "Show - 01 (1080p).mkv".to_string()),
        (1, 42, None, 0, "Show - 02-03 (1080p).mkv".to_string()),
        (2, 42, None, 0, "Show - S01E04-E05.mkv".to_string()),
        (3, 42, None, 0, "Show - 06 (1080p).mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    let span = plan.slots.get(&1).expect("dash range planned");
    assert_eq!((span.episode, span.episode_last), (2, 3));
    let span = plan.slots.get(&2).expect("SxxExx range planned");
    assert_eq!((span.episode, span.episode_last), (4, 5));
    assert_eq!(plan.slots.len(), 4);
    assert!(plan.superseded.is_empty());
}

#[test]
fn multi_episode_file_ties_with_a_single_on_a_shared_slot() {
    // `02-03` and a lone `03` both claim episode 3 at the same version:
    // the same ambiguity as two `03` files, so the batch fails closed.
    let files = vec![
        (0, 42, None, 0, "Show - 02-03 (1080p).mkv".to_string()),
        (1, 42, None, 0, "Show - 03 (1080p).mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("episode 3"), "{err}");
}

#[test]
fn multi_episode_file_that_loses_one_slot_is_superseded_whole() {
    // A v2 single for episode 3 beats the range on that slot; the
    // range is then not imported for episode 2 either, since a partial
    // import of one file has no safe shape.
    let files = vec![
        (0, 42, None, 0, "Show - 02-03 (1080p).mkv".to_string()),
        (1, 42, None, 0, "Show - 03v2 (1080p).mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(plan.slots.contains_key(&1));
    assert!(!plan.slots.contains_key(&0));
    assert_eq!(plan.superseded.get(&0), Some(&1));
}

#[test]
fn route_offset_shifts_the_whole_span() {
    // A sibling route with offset 12: the pack's `13-14` lands as the
    // sibling's E01-E02.
    let files = vec![(0, 43, Some(12), 0, "Show - 13-14 (1080p).mkv".to_string())];
    let plan = validate_batch_episode_map(&files).unwrap();
    let span = plan.slots.get(&0).unwrap();
    assert_eq!((span.episode, span.episode_last), (1, 2));
}

#[test]
fn grab_episode_list_becomes_a_span_only_when_contiguous_and_short() {
    let span = span_from_grab_episodes(&[5, 6]).unwrap();
    assert_eq!((span.first, span.last), (5, 6));
    let span = span_from_grab_episodes(&[6, 5]).unwrap();
    assert_eq!((span.first, span.last), (5, 6));
    // A gap is not a span: the first episode alone, as before #246.
    let span = span_from_grab_episodes(&[5, 7]).unwrap();
    assert_eq!((span.first, span.last), (5, 5));
    // Longer than one cour is a pack, not a file.
    let span = span_from_grab_episodes(&(1..=13).collect::<Vec<_>>()).unwrap();
    assert_eq!((span.first, span.last), (1, 1));
    assert_eq!(
        span_from_grab_episodes(&[9]).map(|s| (s.first, s.last)),
        Some((9, 9))
    );
    assert!(span_from_grab_episodes(&[]).is_none());
}
