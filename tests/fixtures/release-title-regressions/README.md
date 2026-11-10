# Release-title regression literals

Every title that Ryokan's old release-title parsers pinned in their inline
tests, with the outcome each test asserted, so the cases outlive the code
when `services::release_title` replaces those parsers.

- `batch-detectors.json`: `nyaa::scraper::detect_batch`,
  `rss::feed::detect_batch`, `indexers::detect_batch_from_title`.
- `group-resolution.json`: the feed and indexer group / resolution extractors.
- `seasons.json`: `parse_release_season`, `season_mismatch`, RSS
  `parse_release` season hints.
- `episodes.json`: `parse_release_numbers`, RSS `parse_release` episode sets,
  `grab::held_episodes`.
- `series-anchors.json`: the issue #296 stopgap's series-aware readings
  (episode right after the series name), which never shipped; they were
  folded into the new parser instead.

Each case is `{fn, title, <args>, expect, from}`:

- `fn` names the old function, plus a suffix such as `.contains` when the test
  only asserted part of the result.
- The extra arguments (`expected_season`, `episode`, `aliases`) are what the
  old function was called with.
- `from` is `dev 966695a` (the base these were extracted from) or the #296
  stopgap.

The new parser's runner maps each `fn` onto the matching reading. Where the
new parser deliberately answers differently (for example `S2 05` now reads as
season 2 episode 5 instead of nothing), the runner's divergence list says why.
These files are not edited to match.

Extracted on 2026-10-28. The batch, group and resolution cases came from a
script that matches `assert!(detect_batch("..."))` and
`assert_eq!(extract_*("..."), "...")`; the rest were transcribed by hand from
the test bodies.
