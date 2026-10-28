# Sonarr parser fixtures

Release titles and expected parse results ported from Sonarr's parser test
suite, used by `tests/sonarr_parser_corpus.rs` as a reference corpus for
`services::release_title`.

- Source: <https://github.com/Sonarr/Sonarr>, `src/NzbDrone.Core.Test/ParserTests/`
- Commit: `8f8226786bad9804486368768a108ae3aec17101` (2026-10-21)
- Ported: 2026-10-28, converted from NUnit `[TestCase(...)]` attributes to
  JSON by `scripts/port-sonarr-parser-fixtures.py`. Commented-out cases are
  skipped. This is a modified form of the original files.
- License: GPL-3.0-only (`LICENSE.sonarr.md`). Ryokan is GPL-3.0-or-later, so
  the combined work is distributed under GPLv3. Test data only; nothing here
  ships in the binary or the Docker image.

## Format

One JSON per Sonarr fixture file:

```json
{
  "source": "src/NzbDrone.Core.Test/ParserTests/AbsoluteEpisodeNumberParserFixture.cs",
  "repository": "https://github.com/Sonarr/Sonarr",
  "commit": "8f8226786bad9804486368768a108ae3aec17101",
  "license": "GPL-3.0-only",
  "cases": [
    {
      "method": "should_parse_absolute_numbers",
      "title": "[SubDESU]_Show_One_07_(1280x720_x264-AAC)_[6B7FD717]",
      "kind": "release",
      "scope": "anime",
      "sonarr": {"series_title": "Show One", "absolute": [7], "seasons": [0], "episodes": [], "full_season": false}
    }
  ]
}
```

- `method` is the Sonarr test method the case came from.
- `kind` is `release` for a post title, or `file` for a `Parser.ParsePath`
  case, which also carries the release folder name in `release`.
- `scope` is `anime` for anime-shaped titles (a leading bracket group or CJK
  text, or any case in the anime fixtures) and `scene` otherwise. Scene cases
  are asserted on season and pack shape only.
- `xml_clean` marks cases Sonarr runs through `XmlCleaner.ReplaceUnicode`
  first (it strips control characters and surrogates, a no-op for Rust
  strings).
- `sonarr` holds exactly the fields that method asserts, with Sonarr's own
  values. Absent fields are not asserted. Field names: `series_title`,
  `absolute`, `special_absolute`, `seasons`, `episodes`, `full_season`,
  `partial_season`, `multi_season`, `season_extra`, `special`,
  `split_episode`, `group` (`null` means no group), `hash` (`null` means no
  hash), `version`, `possible_special`.

Sonarr's expectations are kept verbatim, even where Ryokan deliberately reads
a title differently. Those cases are listed with a reason in the runner's
divergence table rather than edited here, so a re-port at a newer commit
diffs cleanly.

## Regenerating

```sh
python3 -I scripts/port-sonarr-parser-fixtures.py --commit <sha>
```

The script fetches each fixture with `gh api` at that commit (or reads a
local copy with `--from-dir`) and rewrites the JSON files here. Update the
commit and date above when you do.
