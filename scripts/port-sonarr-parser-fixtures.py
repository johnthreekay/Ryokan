#!/usr/bin/env python3
"""Port Sonarr's parser test fixtures into tests/fixtures/sonarr-parser/.

Sonarr (https://github.com/Sonarr/Sonarr, GPL-3.0) pins its title parser with
NUnit `[TestCase(...)]` attributes. This script reads those attributes, maps
each test method's assertions onto a flat expectation object, and writes one
JSON file per fixture for `tests/sonarr_parser_corpus.rs`. Sonarr's expected
values are kept verbatim so a re-port at a newer commit diffs cleanly; where
Ryokan deliberately reads a title differently, the runner's divergence list
says why.

Usage:
    python3 -I scripts/port-sonarr-parser-fixtures.py --commit <sha> [--from-dir DIR]

Without --from-dir the fixtures are fetched with `gh api` at that commit.
Run with `-I` and keep downloaded sources outside the repository.
"""

import argparse
import json
import pathlib
import re
import subprocess
import sys

FIXTURE_DIR = "src/NzbDrone.Core.Test/ParserTests"

# Fixtures whose cases are anime unless a title says otherwise; the rest are
# scene-format fixtures whose anime cases are picked out by shape.
FIXTURES = {
    "AbsoluteEpisodeNumberParserFixture": "anime",
    "UnicodeReleaseParserFixture": "anime",
    "AnimeMetadataParserFixture": "anime",
    "AnimeVersionFixture": "anime",
    "IsPossibleSpecialEpisodeFixture": "scene",
    "SeasonParserFixture": "scene",
    "MultiEpisodeParserFixture": "scene",
    "SingleEpisodeParserFixture": "scene",
    "ReleaseGroupParserFixture": "scene",
}

CJK = re.compile(r"[぀-ヿ㐀-鿿가-힯＀-￯]")


def fetch(name, commit):
    path = f"repos/Sonarr/Sonarr/contents/{FIXTURE_DIR}/{name}.cs?ref={commit}"
    out = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github.raw", path],
        check=True,
        capture_output=True,
    )
    return out.stdout.decode("utf-8")


# ── C# literal parsing ──────────────────────────────────────────────────


class Cursor:
    def __init__(self, text):
        self.text = text
        self.i = 0

    def peek(self, n=1):
        return self.text[self.i : self.i + n]

    def skip_ws(self):
        while self.i < len(self.text) and self.text[self.i].isspace():
            self.i += 1

    def expect(self, s):
        self.skip_ws()
        if not self.text.startswith(s, self.i):
            raise ValueError(f"expected {s!r} at {self.text[self.i:self.i + 40]!r}")
        self.i += len(s)


ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", "\\": "\\", '"': '"', "'": "'"}


def parse_string(c):
    verbatim = False
    if c.peek() == "@":
        verbatim = True
        c.i += 1
    c.expect('"')
    out = []
    while True:
        ch = c.text[c.i]
        if verbatim:
            if ch == '"':
                if c.peek(2) == '""':
                    out.append('"')
                    c.i += 2
                    continue
                c.i += 1
                return "".join(out)
            out.append(ch)
            c.i += 1
            continue
        if ch == '"':
            c.i += 1
            return "".join(out)
        if ch == "\\":
            nxt = c.text[c.i + 1]
            if nxt == "u":
                out.append(chr(int(c.text[c.i + 2 : c.i + 6], 16)))
                c.i += 6
            elif nxt == "x":
                m = re.match(r"[0-9a-fA-F]{1,4}", c.text[c.i + 2 :])
                out.append(chr(int(m.group(0), 16)))
                c.i += 2 + len(m.group(0))
            else:
                out.append(ESCAPES[nxt])
                c.i += 2
            continue
        out.append(ch)
        c.i += 1


def parse_value(c):
    c.skip_ws()
    if c.peek() in ('"', "@"):
        return parse_string(c)
    if c.text.startswith("new", c.i):
        m = re.compile(r"new\s*(?:\w+\s*)?\[\s*\]\s*\{").match(c.text, c.i)
        if not m:
            raise ValueError(f"unknown new-expression at {c.text[c.i:c.i + 40]!r}")
        c.i = m.end()
        items = []
        c.skip_ws()
        while c.peek() != "}":
            items.append(parse_value(c))
            c.skip_ws()
            if c.peek() == ",":
                c.i += 1
                c.skip_ws()
        c.i += 1
        return items
    for word, value in (("null", None), ("true", True), ("false", False)):
        if re.compile(rf"{word}\b").match(c.text, c.i):
            c.i += len(word)
            return value
    m = re.compile(r"-?\d+(?:\.\d+)?[mMdDfF]?").match(c.text, c.i)
    if not m:
        raise ValueError(f"unknown literal at {c.text[c.i:c.i + 40]!r}")
    c.i = m.end()
    raw = m.group(0).rstrip("mMdDfF")
    return float(raw) if "." in raw else int(raw)


def parse_args(text):
    c = Cursor(text)
    values = []
    c.skip_ws()
    while c.i < len(c.text):
        values.append(parse_value(c))
        c.skip_ws()
        if c.peek() == ",":
            c.i += 1
            c.skip_ws()
    return values


# ── Attribute and method extraction ─────────────────────────────────────

METHOD = re.compile(r"public\s+void\s+(\w+)\s*\(([^)]*)\)")


def attribute_body(src, start):
    """Return (args text, end index) for `[TestCase(` starting at `start`."""
    i = src.index("(", start) + 1
    depth, args_start = 1, i
    in_str = verbatim = False
    while depth:
        ch = src[i]
        if in_str:
            if verbatim:
                if ch == '"':
                    if src[i + 1] == '"':
                        i += 2
                        continue
                    in_str = False
            elif ch == "\\":
                i += 2
                continue
            elif ch == '"':
                in_str = False
        elif ch == '"':
            in_str, verbatim = True, src[i - 1] == "@"
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        i += 1
    return src[args_start : i - 1], i


def extract(src):
    """Yield (method, param names, xml_clean, [args...]) per test method."""
    pending = []
    pos = 0
    lines_commented = set()
    for m in re.finditer(r"^[ \t]*//.*$", src, re.M):
        lines_commented.update(range(m.start(), m.end()))
    token = re.compile(r"\[TestCase\(|public\s+void\s+\w+\s*\(")
    while True:
        m = token.search(src, pos)
        if not m:
            break
        if m.start() in lines_commented:
            pos = m.end()
            continue
        if m.group(0).startswith("[TestCase"):
            body, end = attribute_body(src, m.start())
            pending.append(parse_args(body))
            pos = end
            continue
        sig = METHOD.match(src, m.start())
        name = sig.group(1)
        params = [p.strip().split()[-1] for p in sig.group(2).split(",") if p.strip()]
        body_end = src.find("\n        }", sig.end())
        xml_clean = "XmlCleaner.ReplaceUnicode" in src[sig.end() : body_end]
        if pending:
            yield name, params, xml_clean, pending
        pending = []
        pos = sig.end()


# ── Assertion mapping, one entry per Sonarr test method ─────────────────


def span(first, last):
    return list(range(first, last + 1))


def nonzero(ep):
    return [ep] if ep else []


MAPPINGS = {
    # AbsoluteEpisodeNumberParserFixture
    "should_parse_absolute_numbers": lambda a: {
        "series_title": a["title"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "seasons": [a["seasonNumber"]],
        "episodes": nonzero(a["episodeNumber"]),
        "full_season": False,
    },
    "should_parse_absolute_specials": lambda a: {
        "series_title": a["title"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "seasons": [0],
        "episodes": [],
        "full_season": False,
        "special": True,
    },
    "should_parse_absolute_specials_without_absolute_number": lambda a: {
        "series_title": a["title"],
        "absolute": [],
        "seasons": [0],
        "episodes": [],
        "full_season": False,
        "special": True,
    },
    "should_parse_multi_episode_absolute_numbers": lambda a: {
        "series_title": a["title"],
        "absolute": span(a["firstAbsoluteEpisodeNumber"], a["lastAbsoluteEpisodeNumber"]),
        "full_season": False,
    },
    "should_parse_anime_season_packs": lambda a: {
        "series_title": a["title"],
        "absolute": [],
        "seasons": [a["seasonNumber"]],
        "full_season": True,
    },
    "should_handle_anime_recap_numbering": lambda a: {
        "series_title": a["title"],
        "absolute": [],
        "special_absolute": [a["specialEpisodeNumber"]],
        "full_season": False,
    },
    "should_handle_ambiguously_named_anime_files_in_batch_release": lambda a: {
        "series_title": a["title"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "seasons": [0],
        "episodes": [],
        "full_season": False,
    },
    "should_parse_absolute_followed_by_standard_as_standard": lambda a: {
        "series_title": a["title"],
        "seasons": [a["seasonNumber"]],
        "episodes": [a["episodeNumber"]],
        "absolute": [],
        "special_absolute": [a["specialEpisodeNumber"]],
        "full_season": False,
    },
    # AnimeMetadataParserFixture
    "should_parse_releasegroup_and_hash": lambda a: {
        "group": a["subGroup"],
        "hash": a["hash"] or None,
    },
    "should_parse_release_hashes_with_10b_or_8b": lambda a: {"hash": a["hash"] or None},
    # AnimeVersionFixture
    "should_be_able_to_parse_repack": lambda a: {"version": a["version"]},
    # IsPossibleSpecialEpisodeFixture
    "IsPossibleSpecialEpisode_should_be_true": lambda a: {"possible_special": True},
    "IsPossibleSpecialEpisode_should_be_true_if_e00_special": lambda a: {"possible_special": True},
    "IsPossibleSpecialEpisode_should_be_false_for_Special_in_title": lambda a: {
        "possible_special": False
    },
    # MultiEpisodeParserFixture
    "should_parse_multiple_episodes": lambda a: {
        "series_title": a["title"],
        "seasons": [a["season"]],
        "episodes": a["episodes"],
        "absolute": [],
        "full_season": False,
    },
    # ReleaseGroupParserFixture
    "should_parse_release_group": lambda a: {"group": a["expected"]},
    "should_parse_exception_release_group": lambda a: {"group": a["expected"]},
    "should_not_include_language_in_release_group": lambda a: {"group": a["expected"]},
    "should_not_include_repost_in_release_group": lambda a: {"group": a["expected"]},
    "should_parse_anime_release_groups": lambda a: {"group": a["expected"]},
    "should_not_parse_anime_hash_as_release_group": lambda a: {"group": None},
    "should_not_parse_episode_title_as_release_group": lambda a: {"group": None},
    # SeasonParserFixture
    "should_parse_full_season_release": lambda a: {
        "series_title": a["title"],
        "seasons": [a["season"]],
        "episodes": [],
        "absolute": [],
        "full_season": True,
    },
    "should_parse_season_extras": lambda a: {
        "series_title": a["title"],
        "seasons": [a["season"]],
        "episodes": [],
        "absolute": [],
        "full_season": True,
        "season_extra": True,
    },
    "should_parse_season_subpack": lambda a: {
        "series_title": a["title"],
        "seasons": [a["season"]],
        "episodes": [],
        "absolute": [],
        "full_season": True,
        "season_extra": True,
    },
    "should_parse_partial_season_release": lambda a: {
        "series_title": a["title"],
        "seasons": [a["season"]],
        "episodes": [],
        "absolute": [],
        "full_season": False,
        "partial_season": a["seasonPart"],
    },
    "should_parse_multi_season_release": lambda a: {
        "series_title": a["title"],
        "seasons": a["expectedSeasons"],
        "episodes": [],
        "absolute": [],
        "full_season": True,
        "multi_season": True,
    },
    "should_not_be_multi_season_for_a_single_season_release": lambda a: {
        "seasons": [a["season"]],
        "multi_season": False,
    },
    # SingleEpisodeParserFixture
    "should_parse_single_episode": lambda a: {
        "series_title": a["title"],
        "seasons": [a["seasonNumber"]],
        "episodes": [a["episodeNumber"]],
        "absolute": [],
        "full_season": False,
    },
    "should_parse_japanese_variety_show_format": lambda a: {
        "series_title": a["title"],
        "seasons": [a["seasonNumber"]],
        "episodes": [a["episodeNumber"]],
        "absolute": [],
        "full_season": False,
    },
    "should_parse_decimal_number_as_special": lambda a: {
        "series_title": a["title"],
        "seasons": [a["seasonNumber"]],
        "episodes": [a["episodeNumber"]],
        "absolute": [],
        "full_season": False,
        "special": True,
    },
    "should_parse_split_episode": lambda a: {
        "series_title": a["title"],
        "seasons": [a["seasonNumber"]],
        "episodes": [a["episodeNumber"]],
        "absolute": [],
        "full_season": False,
        "split_episode": True,
    },
    # UnicodeReleaseParserFixture
    "should_parse_chinese_anime_releases": lambda a: {
        "series_title": a["title"],
        "group": a["subgroup"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "full_season": False,
    },
    "should_parse_chinese_anime_season_episode_releases": lambda a: {
        "series_title": a["title"],
        "group": a["subgroup"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "full_season": False,
    },
    "should_parse_unbracketed_chinese_anime_releases": lambda a: {
        "series_title": a["title"],
        "group": a["subgroup"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "full_season": False,
    },
    "should_parse_chinese_multiepisode_releases": lambda a: {
        "series_title": a["title"],
        "group": a["subgroup"],
        "absolute": a["absoluteEpisodeNumbers"],
        "full_season": False,
    },
    "should_parse_gm_team_releases_and_files": lambda a: {
        "series_title": a["title"],
        "group": "GM-Team",
        "absolute": [a["absoluteEpisodeNumber"]],
        "full_season": False,
    },
    "should_parse_unicode_digits": lambda a: {
        "series_title": a["title"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "special_absolute": [],
        "full_season": False,
    },
    "should_parse_false_positive_chinese_anime_releases": lambda a: {
        "series_title": a["title"],
        "group": a["subgroup"],
        "absolute": [a["absoluteEpisodeNumber"]],
        "full_season": False,
    },
    "should_parse_full_season_releases": lambda a: {
        "series_title": a["title"],
        "group": a["releaseGroup"],
        "episodes": [],
        "absolute": [],
        "seasons": [a["season"]],
        "full_season": True,
    },
    "should_parse_multi_episode_release": lambda a: {
        "series_title": a["title"],
        "seasons": [a["season"]],
        "episodes": a["episodes"],
        "absolute": [],
        "full_season": False,
    },
}

# Methods whose first two arguments are a release folder and a file name
# (`Parser.ParsePath`) rather than a single post title.
PATH_METHODS = {"should_handle_ambiguously_named_anime_files_in_batch_release"}


def scope_of(title, default):
    if title.startswith(("[", "(", "【")) or CJK.search(title):
        return "anime"
    return default


def port(name, src, commit, default_scope):
    cases = []
    skipped = set()
    for method, params, xml_clean, rows in extract(src):
        mapping = MAPPINGS.get(method)
        if mapping is None:
            skipped.add(method)
            continue
        for row in rows:
            if len(row) != len(params):
                raise ValueError(f"{name}.{method}: {len(row)} args for {params}")
            args = dict(zip(params, row))
            case = {"method": method}
            if method in PATH_METHODS:
                case["release"] = args["releaseName"]
                case["title"] = args["filename"]
                case["kind"] = "file"
            else:
                case["title"] = row[0]
                case["kind"] = "release"
            case["scope"] = scope_of(case["title"], default_scope)
            if xml_clean:
                case["xml_clean"] = True
            case["sonarr"] = mapping(args)
            cases.append(case)
    return {
        "source": f"{FIXTURE_DIR}/{name}.cs",
        "repository": "https://github.com/Sonarr/Sonarr",
        "commit": commit,
        "license": "GPL-3.0-only",
        "cases": cases,
    }, sorted(skipped)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--commit", required=True)
    ap.add_argument("--from-dir", type=pathlib.Path)
    ap.add_argument(
        "--out",
        type=pathlib.Path,
        default=pathlib.Path("tests/fixtures/sonarr-parser"),
    )
    opts = ap.parse_args()
    opts.out.mkdir(parents=True, exist_ok=True)
    total = 0
    for name, default_scope in FIXTURES.items():
        if opts.from_dir:
            src = (opts.from_dir / f"{name}.cs").read_text(encoding="utf-8")
        else:
            src = fetch(name, opts.commit)
        data, skipped = port(name, src, opts.commit, default_scope)
        total += len(data["cases"])
        out = opts.out / f"{name}.json"
        out.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        note = f" (skipped methods without a mapping: {', '.join(skipped)})" if skipped else ""
        print(f"{name}: {len(data['cases'])} cases{note}", file=sys.stderr)
    print(f"total: {total} cases", file=sys.stderr)


if __name__ == "__main__":
    main()
