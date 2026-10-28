# Nyaa filename corpus

Real-world video filenames scraped from Nyaa torrent file lists on
2026-08-28 and 2026-10-28 (the second batch: packs picked from the
release-title corpus in `../nyaa-titles/`, video files only), one JSON per
torrent, used by `tests/nyaa_filename_corpus.rs` as a regression corpus for
`services::media::parse_episode_number`.

Each fixture records the torrent's category bucket, release title, source
URL, and every contained video filename alongside the `(season, episode)`
the parser returned at capture time (`null` episode = parser returned
`None`). Fixtures titled "Hand-supplied" were contributed directly rather
than scraped and carry `url: null`; the [SoM] DBox and Bleach entries
specifically exercise the dot-delimited episode branch and its codec-token
guard (PR #198).

**These are characterization snapshots, not ground truth.** Captured
expectations may include known mis-parses; NC/PV/CM, bare `- SP`, and
half-episode (`07_5`) extras deliberately parse to `None` (issue #203
turned the last three from mis-parses into expectations). The test pins
today's behavior so any parser change that shifts a real-world outcome
shows up explicitly in review; updating an expectation alongside a
deliberate parser fix is normal and
expected. A changed expectation carries a `note` with its reason; the
switch to `release_title`'s file reading changed 46 files that way
(bare numbers after the title, CJK brackets, extras that read as the
title's own number).

Category buckets:

- `seasonal-single` — modern per-episode releases (SubsPlease / Erai-raws).
- `batch-modern` — modern season batch packs.
- `bd-pack-extras` — BD packs shipping NCOP/NCED/SP/PV/CM extras (Moozzi2).
- `dvd-underscore` — underscore-delimited old-school DVD rips (Exiled-Destiny).
- `dvd-dot` — DVD complete-series era releases.
- `movie` — movie releases and a movies+OVA+seasons ultimate collection.
- `numeric-title`: numeric series titles (86 Eighty-Six, Mob Psycho 100,
  Steins;Gate 0, Gundam 00).
- `eternalanimes`: underscore-delimited EternalAnimes packs with a bare
  episode number after the title (`[EA]Dark_Gathering_24_[...]`, issue #296).
- `cjk-pack`: Chinese fansub and VCB-Studio packs (`[01-12 合集]`, BDRip
  `[Fin]` releases with SP / menu / CM files).
- `webdl-dotted-codec` — hand-supplied WEB-DL scene names carrying dotted
  `AAC2.0.H.264` codec tokens (regression guard for dot-token episode
  parsing: `SxxEyy` must keep outranking any bare dot-delimited number).

Regenerate by re-scraping (throttle >= 1s between requests; Nyaa tarpits
scrape-looking bursts) and re-running the parser over the collected names.
