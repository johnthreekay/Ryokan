# services/release_title/AGENTS.md

One parser for anime release titles, replacing the separate batch detectors, episode readers, season readers and group / resolution extractors that disagreed with each other (issue #296 and the plan behind it). Pure: no I/O, no regex in the hot path. Call sites move onto it one at a time, each move gated by the differential below. Moved so far: the search gates (`auto_search::classify_match`, the interactive filter, the TV-specials rejection, scoring, the Wanted episode-set selection).

## Shape

`parse(&str) -> ParsedTitle` needs no series knowledge. Callers then ask for a reading:

- `is_batch()`: a multi-episode pack. Replaces every batch detector.
- `release_episodes()`: the episodes the title itself names. Never a bare number after the title, never a special / extra / half episode.
- `season_hint()`, `conflicts_with_season(n)`.
- `for_series(&SeriesAnchors)`: the episodes for a known series. Keeps the title's own claim, except that a bare range or marker number held inside the series' name (`Tatsuki Fujimoto 17-26`) is vetoed; otherwise reads the number right after the longest alias found word for word (`[EA]Dark_Gathering_24_`), skipping one season marker. A number inside the alias is the name's (`Mob Psycho 100`).
- `is_special_for(&SeriesAnchors)`: a trailing `Special` / `OVA` that ends the series' own name (`A Returner's Magic Should Be Special`) is not a special.

`SeriesAnchors::new(aliases)` normalizes aliases the way titles are normalized; build it once per series, not per title (RSS loops items × series). `.with_specials_as_episodes(!media::is_tv_format(format))` carries the series' format: for an OVA / ONA / special / movie entry a special marker is part of the name, so `for_series` reads `OVA 01` as episode 1; for a TV series (the default) a special names no episode.

## Pipeline

Each file is one step and says what it does at the top:

1. `normalize.rs`: length cap, full-width to ASCII, CJK brackets to `[` / `]`, dash and wave-dash forms, `★10月新番★` decoration dropped (a short CJK note between two stars only; stars in a title like `KERORO☆` stay), a trailing extension cut (a video one sets `single_file`).
2. `lexer.rs`: tokens carry the separator before them and their bracket depth. This is what replaces `\b`, which fails beside `_` (#296).
3. `vocab.rs`: one table classifies every word. Rules read `Class`, never raw text.
4. `rules.rs`: the ordered rules, below.
5. `anchors.rs`: the series-aware reading.

## Rule order (load-bearing)

`rules::parse` runs, in order: leading group (marks its tokens skipped, so nothing else reads them), revision, resolution, year and airing span, CRC, half episode, the episode claim, the extra gate, seasons, included specials, the special flag, keywords, the ambiguous number (only with no claim), the trailing group (only with no leading group), a bare resolution, the title text, the anchor words.

The episode claim is the first of these that matches (doc comment at the top of `rules.rs`):

1. `SxxEyy` tags at any depth.
2. The number after ` - ` / `_-_`, only when a boundary follows it (bracket, metadata, another dash, the end). `Series - 100 Years Quest - 05` is 5; `- 39 Fanart Corner` claims nothing.
3. `E05` / `EP05` / `Ep 05`. Skipped when it follows a `+` after a season marker (`S1 + Ep. 0` is a pack with a bonus episode).
4. `Episode 05`, `#05`, `第05话`, `04 of 12` / `04 из 12`.
5. `OVA 01` / `SP01` (sets `special`).
6. A bracketed number or range (`[03]`, `[08v2]`, `(01-12)`).
7. A bare range in the title text (`01-25`, `1100-1104`).
8. A season marker then a bare number (`S2 05`, `2nd Season 05`, `II 05`).

Moving a rule earlier changes what wins on every title both rules match; run the corpora before and after.

## Deliberate rules (each pinned by a corpus case)

- A spaced range pairs equal widths, so a title's own number before ` - NN` is never a range start (`Chihayafuru 2 - 05`, `Season 2 - 05`); a zero-padded left number is accepted (`01 - 119`).
- Ranges ascend; `MAX_EXPANDED_RANGE` (200) caps expansion, not batch-ness.
- Years are never episodes; `[1983-1984]` with no claim is a whole-run pack (`year_range`).
- `1080` / `720` after a dash are episodes (One Piece); a resolution needs `p`, `i`, `WxH` (a trailing `p` is fine), `4K` / `UHD` / `FHD`, or sits beside metadata (`bare_resolution`).
- `07.5` is a half episode, never episode 7. NC / PV / CM / preview / trailer are extras: no claim, never a batch.
- `[SP]` alone is a subtitle tag; `SP01` is a special.
- `special` is a flag; what it means depends on the series' format (`media::is_tv_format`). A tag naming a real season (`S02E03`) beats a title that ends in `Special`.
- PROPER / REPACK and Roman `II`..`IX` are uppercase only. A Roman numeral is a season marker only at top level or alone in a bracket, and only where the title text ends (a bracket, dash, `|`, metadata, the end) or before a number: `Lupin III vs`, `Chibi Maruko-chan II 樱桃小丸子` and `(..., The Legend of Hei II)` are names.
- `Part` / `Cour` N ≥ 2 is a season hint (`seasons.hint_only`), not a pack, unless it sits at top level right before a bracket (`Series Cour 2 (WEB 1080p)`, `part_pack`).
- A title naming one video file is never a season pack. An extras release is never a batch.
- `[BD]` alone is not a batch keyword.
- A bare number right after the title is `ambiguous_number`, read only through `for_series`. The search for it stops at the first top-level metadata token, so `H.264 (Japanese Sub)` has none.
- Group: the first non-metadata, non-numeric, non-CRC, non-website bracket; else a scene tail after the longest metadata head (`H.264-VARYG`, `x264-Tsundere-Raws`), a lead-dash word at the end, a one-word last bracket (`[Lunar]`), or two plain words in a last bracket that follows another bracket (`(Dual Audio)[MICO XD]`). A first bracket made only of metadata words (`[BD_SUBS]`) is skipped.

## Tests

- `tests/release_title_corpora.rs`, all part of `cargo t`:
  - `tests/fixtures/nyaa-titles/`: hand-verified titles in buckets, every field checked. A new rule lands with a case here (promote the sample title that showed it), with a `note`.
  - `tests/fixtures/sonarr-parser/`: Sonarr's anime parser cases; every disagreement is listed with a reason in `ryokan-divergences.json`.
  - `tests/fixtures/release-title-regressions/`: every title the old inline tests asserted, with `divergences.json` for the deliberate changes.
  - A consistency check over every corpus title (the sample included): a single-episode claim is a batch only with a keyword; a special / extra / half episode names no episodes; `ambiguous_number` and the year never appear in `release_episodes`.
- `tests/proptest_invariants.rs`: never panics; deterministic; bracketed metadata around a weekly title does not move its episode; a number inside the series' name never reads as an episode through `for_series`.
- `differential.rs` (test only, deleted with `legacy.rs` at cleanup): frozen copies of the old parsers (`legacy.rs`, verbatim from `dev`) against the new one, per lens, over the unverified listing sample. Every difference needs an entry in `tests/fixtures/release-title-differential/triage.json` naming a reason; each reason has a verdict (`fix` / `accepted` / `regression`) and a note. The test fails on a missing entry (and prints it ready to paste, `"reason": ""`), a stale one, an unknown or unused reason, and a `regression` on a lens in `MIGRATED_LENSES`. When a call site moves, add its lens there first.
- Debugging a title: `RT_TITLES=$'title one\ntitle two' cargo nextest run --features test-support --lib release_title::differential::dump --run-ignored only --no-capture` prints each parse and its lens differences.
