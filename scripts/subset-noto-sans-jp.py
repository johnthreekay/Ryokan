#!/usr/bin/env python3
"""Cut the Noto Sans JP variable font down to the Japanese subset Ryokan ships.

Usage:
    pip install fonttools brotli
    curl -L -o 'NotoSansJP[wght].ttf' \
      'https://raw.githubusercontent.com/google/fonts/main/ofl/notosansjp/NotoSansJP%5Bwght%5D.ttf'
    python3 scripts/subset-noto-sans-jp.py 'NotoSansJP[wght].ttf' static/fonts/noto-sans-jp-japanese.woff2

The subset is every code point inside the Japanese `unicode-range` that
`static/css/base.css` declares for the body face (the same ranges the
Murecho display face uses) that Python's `cp932` codec can encode: JIS X
0208 levels 1 and 2, kana, JIS X 0201 half-width kana, full-width forms,
and the NEC / IBM extension kanji that show up in people's names. That is
the set Japanese web text is written in, defined by a codec table rather
than a list file, so the build is reproducible from the source font alone.
The weight axis is kept, so one file serves every `font-weight`.
"""
import sys

from fontTools import subset
from fontTools.ttLib import TTFont

# Mirrors the `unicode-range` of the Japanese @font-face rules in base.css.
JP_RANGES = [
    (0x3000, 0x30FF), (0x3131, 0x318E), (0x31F0, 0x31FF), (0x3200, 0x32FF),
    (0x3400, 0x4DBF), (0x4E00, 0x9FAF), (0xA960, 0xA97F), (0xAC00, 0xD7AF),
    (0xF900, 0xFAFF), (0xFE30, 0xFE4F), (0xFF00, 0xFFEF),
]


def encodable(cp: int) -> bool:
    try:
        chr(cp).encode("cp932")
    except UnicodeEncodeError:
        return False
    return True


def main(src: str, dst: str) -> None:
    font = TTFont(src)
    cmap = font.getBestCmap()
    keep = sorted(
        cp for lo, hi in JP_RANGES for cp in range(lo, hi + 1)
        if cp in cmap and encodable(cp)
    )
    kanji = sum(1 for cp in keep if 0x4E00 <= cp <= 0x9FFF or 0xF900 <= cp <= 0xFAFF)
    opts = subset.Options()
    opts.flavor = "woff2"
    opts.name_IDs = ["*"]   # keep the copyright and license notices inside the file
    opts.name_legacy = True
    opts.notdef_outline = True
    opts.hinting = False
    subsetter = subset.Subsetter(opts)
    subsetter.populate(unicodes=keep)
    subsetter.subset(font)
    font.flavor = "woff2"
    font.save(dst)
    print(f"{dst}: {len(keep)} code points ({kanji} kanji), "
          f"version {font['name'].getDebugName(5)}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
