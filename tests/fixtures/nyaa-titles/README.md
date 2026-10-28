# Nyaa release-title corpus

Real Nyaa release titles, grouped by shape, each with a hand-verified
expectation for `services::release_title`. Used by
`tests/release_title_corpora.rs` and by the parser's own tests.

Scraped from Nyaa RSS (`?page=rss`) on 2026-10-28 at no more than one request
per 1.2 s. Each file records the queries it came from, and every case its
torrent page URL. `url: null` marks a hand-supplied title, or one seen on a
page outside the scraped feeds; those carry a note. Titles were picked
one per title shape and series, so the corpus covers shapes rather than
repeats.

**These expectations are ground truth, not snapshots.** Each one was written
by reading the title before the new parser existed. Where the parser
disagrees, the parser is wrong, or the case moves to a reviewed known-gap list
with a reason. Do not regenerate expectations from parser output.

Cases added later were promoted from `sample/` while reviewing the
differential against the old parsers (since deleted): each pins a rule
that review added, was written from the title, and carries a `note`.

## Buckets

| File | What it covers |
|---|---|
| `subsplease-weekly`, `subsplease-batch` | SubsPlease weeklies, `v2`, `S2 - 04`, `(01-12) [Batch]` |
| `erai-raws` | Erai-raws weeklies, `Part 2`, `01 ~ 03 [BATCH]`, `(REPACK)` |
| `eternalanimes` | Underscore titles with a bare episode (`_24_`), `2nd_Season_05`, `4th_04_V2`, `01-25_End`, `+OVA` (issue #296) |
| `numeric-titles` | Titles that end in a number: 86, Mob Psycho 100, Kaiju No. 8, Chihayafuru 2/3, Gundam 00, Steins;Gate 0, Zom 100, Cyborg 009, Lupin III |
| `roman-sequels` | Overlord IV, Goblin Slayer II, DanMachi IV, and Roman numerals that are titles |
| `bd-season-packs` | MTBB, Kaizoku, smol and other BD packs, movies among them |
| `multi-season-collections` | `S01-S04`, `S1+S2+S3`, `Seasons 1-3`, collections, year ranges |
| `long-running-absolute` | One Piece, Detective Conan, Boruto, Doraemon: 3- and 4-digit episodes and ranges |
| `cjk-bracketed` | `【喵萌奶茶屋】★10月新番★[...][03]`, `[08v2]`, `第二季`, `[01-12 Fin]`, VCB-Studio `[Fin]` packs |
| `cjk-slash` | `[LoliHouse] 中文 / Romaji - 03`, `[01-24(73-96) 合集]` |
| `scene-dotted` | Scene-style names with a trailing `-GROUP` |
| `specials-ova` | `S00Exx`, `OVA 01`, `- OVA`, `+ OVAs`, trailing `OVA` / `Specials` |
| `extras` | NCOP/NCED, creditless, previews, trailers |
| `half-recap` | `- 13.5`, `_12.5_`, `ep 13.5` |
| `revisions` | `v2`, `[v2]`, `V2`, `REPACK`, Title Case "Proper" |
| `ru-eu` | `[TV-1] [04 of 12]`, `Temporada`, `Saison`, `Saisons 1/2/3`, `Epi 1 - 4`, `Partie 2` |
| `pipe-dual-titles` | `Title \| Alt \| Season 2` |
| `year-in-title` | `(2024)`, `Red River 2026 S01E17`, `2.5 Dimensional Seduction` |
| `multi-episode` | `S01E01-E02`, `E01-E02`, `- 01-02`, `(01-02)`, `[01-02]`, `Vol 01-02` |
| `alt-title-parens-dashed-groups` | `[Naruto-Kun.Hu - UraharaShop]`, `(Alt Title) - 03 (S01E03)` |
| `movies` | Movie releases and movie packs |
| `dvd-old` | DVD-era packs, `Episode 27,69,76`, `- 1 - 152`, `(E001-E130)` |

`sample/listing-sample.json` holds about 2,000 unverified titles from the Nyaa
anime listing, used only to compare old and new parser output.

## Expectation fields

Each case is `{title, url, category, expect, series?, verified, note?}`.
Cases promoted from the sample have no `category` (the listing sample does not
record it).

`expect` (absent fields take the default shown):

| Field | Meaning | Default |
|---|---|---|
| `batch` | `is_batch()` | always present |
| `group` | release group (`null` = none) | not checked |
| `seasons` | explicit season numbers, one for a single season or several for a multi-season pack | none |
| `season_part` | `Part N` / `P2` / `Vol N` of a season | none |
| `episodes` | the episode claim: a list, or `{first, last}` for a range; `null` means not checked. A special, extra or half episode names no slot, so `release_episodes()` is empty for those even when a claim exists | none |
| `ambiguous` | a bare number after the title that only series context can place (`null` = none) | not checked |
| `special`, `includes_specials`, `extra`, `half_episode` | flags | false |
| `version` | release revision (`v2`, uppercase `REPACK` / `PROPER`) | 1 |
| `resolution` | vertical resolution | not checked |
| `year` | year token | not checked |

`series` (optional): `{aliases, episodes}`. `for_series(aliases)` must return
those episodes (`[]` = none). Used where the episode depends on knowing the
series name: a bare number right after it, or a title that ends in its own
number.

## Adding cases

Add the title to the bucket it fits and write the expectation by reading the
title, not by copying parser output. Note anything a reader might question.
Keep the scrape etiquette above if you fetch more.
