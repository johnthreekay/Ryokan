# AnimeTosho release-title corpus

A large sample of real release titles and torrent file lists, for holding
`services::release_title` to the numbers it reached when it replaced the old
parsers. Read by `tests/release_title_corpora.rs`.

| File | What |
|---|---|
| `titles.jsonl.gz` | 29,393 distinct release titles |
| `file-lists.jsonl.gz` | file lists of 3,007 of those torrents |
| `labels/labels.jsonl` | 1,000 of the titles, labeled blind |
| `labels/GUIDE.md` | the instructions the labels follow |

## Where it came from

Scraped on 2026-10-28 by `scrape_animetosho_corpus` (an ignored test in
`tests/release_title_corpora.rs`) at one request per 1.5 s:

- AnimeTosho's JSON feed (`https://feed.animetosho.net/json`; `animetosho.org`
  is gone): its recent pages, plus searches for batch and BD groups, Chinese
  and Japanese groups, title shapes (`S01-S03`, `13.5`, `第二季`, `NCOP`, ...),
  long-runners and titles that end in a number. AnimeTosho mirrors Nyaa,
  nekoBT, TokyoTosho and AniDex; each row says which (`source`).
- Nyaa RSS for the raw and non-English categories (`source: "nyaa-rss"`).
- Sukebei entries are left out (Ryokan never searches it).
- File lists for a fixed pick of AnimeTosho entries: up to 1,800 Nyaa packs
  and 500 single files, 1,000 nekoBT packs and 300 single files, at most six
  packs per AniDB series. 570 of those torrents have no file list on
  AnimeTosho (`files: []`); their rows stay.

Rows keep only what the tests read. `titles.jsonl.gz`: `title`, `source`, and
where known `at_id`, `nyaa_id`, `num_files`, `size`, `anidb_aid`, `anidb_eid`,
`nyaa_category`. `file-lists.jsonl.gz`: `at_id`, `title`, `at_is_batch`
(AnimeTosho's own flag), `anidb_aid`, `tvdb_season`, and `files` as
`[path, size]` pairs.

**This is not ground truth.** The titles and files are what indexers carry,
not answers. The tests hold totals measured on 2026-10-28 as floors and
ceilings (see below), so a change shows up as a moved number with the titles
that moved.

## The labels

1,000 titles drawn at random from the corpus were labeled by four Claude
subagents, 250 each, following `labels/GUIDE.md`: batch, the episodes the
title names, the ambiguous bare number, season, special, extra, group and
resolution, plus an `unsure` note on judgment calls (230 have one). The
labelers never saw any parser's output. Nobody checked the labels line by line;
every disagreement with the parser was read when the floors were set:

- Batch, 74: 73 packs the parser does not flag, mostly titles that name no
  episodes (`[UTW] Angel Beats! [BD]`, offered only by the interactive batch
  list) and a few shapes it misses (`{Season 1-3}`, `ALL SEASONS`,
  `[TV 01-51+SP]`, `S04Part01 ... and S04Part02`); one movie read as a season-2
  pack (`Heaven's Feel II - lost butterfly`).
- Episodes, 17: 13 follow deliberate rules (a bare number after the title is
  left to the series' name, `Pocket Monsters (2023) 141`; a range over 200
  episodes lists none; a tag beats an absolute number; an extra names no
  episode; `Digimon Adventure 2020 1 - 3` reads the dash's number, as
  `Chihayafuru 2 - 05` must), and four are misses or debatable reads
  (`[TV 01-51+SP]`, a KonoSuba title listing three numberings, One Pace's
  chapter range, `Towanoquon Movie 1-6`).

A long range is written as its two ends with `"range"` in `unsure`.

## What the tests hold

| Test | Measured on 2026-10-28 |
|---|---|
| `readings_are_consistent_on_every_corpus_title` | the readings agree with each other on every title |
| `parser_agrees_with_the_blind_labels_at_least_as_before` | of 1,000: batch 926, episodes 983, ambiguous 991, season 993, special 993, extra 997, group 981, resolution 998 |
| `batch_flag_tracks_file_counts` | 67 of 19,594 one-file titles read as batches; 4,484 of 6,915 titles of 10+ files do |
| `file_lists_read_as_before` | 27,171 of 27,798 episode-like videos read; 67 packs claim one episode twice; no file named as an extra takes a slot |
| `default_episode_names_read_back_for_every_corpus_show` | Ryokan's default episode name reads back for every show name |

When a parser change moves a number: if it improved, raise the limit in the
test; if it is a deliberate trade, lower it and say why in the commit.
`release_title::corpus::big_corpus_dump` (ignored, `RT_OUT`) writes every
reading as JSON lines, so two runs diff to the titles and files that moved.

For the record, the same day: the old parsers this one replaced agreed with the
labels on 79.4 to 89.2% of batch flags, 89.5 to 91.8% of episodes and 85.1 to
87.4% of groups (two old code paths). Sonarr 4.0.20's parser, through its
`/api/v3/parse` with no series context, agreed on 85.7% of batch flags, 90.9% of
episodes, 88.5% of seasons, 83.3% of groups and 82.1% of resolutions. It could
not parse 168 of the 1,000 titles, and it named an episode on 52 titles that
name none (years, resolutions, title numbers).

## Rebuilding

```bash
RT_CACHE=~/.cache/ryokan-corpus cargo nextest run --features test-support \
  --test release_title_corpora scrape_animetosho_corpus --run-ignored only --no-capture
```

`RT_STEP` picks `listing`, `details` or `pack` (default: all three). From an
empty cache the scrape takes about two hours; every response is cached, so a
rerun fetches only what is missing, and `pack` from the same cache writes the
same bytes. A rebuilt corpus is a different sample: re-measure every limit
above and relabel (or keep) the labeled titles.
