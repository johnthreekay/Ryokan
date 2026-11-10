# Labeling anime release titles

You will read real torrent titles from Nyaa / nekoBT and label what each one
says, as a careful human who knows anime fansub and scene naming would. Read
each title on its own. Do not guess from outside knowledge of the show beyond
what the title itself says, except to recognize common group names, metadata
words and well-known title words.

Output one JSON object per line, in the same order as the input, with exactly
these keys:

```json
{"n": 17, "batch": false, "episodes": [5], "ambiguous": null, "season": 2, "special": false, "extra": false, "group": "SubsPlease", "resolution": 1080, "unsure": ""}
```

- `n`: the line number from the input.
- `batch` (bool): the release holds several episodes (a range like `01-12` or
  `01 ~ 25`, a whole season or series pack like `S01`, `Season 2 (BD)`,
  `Complete`, `[Batch]`, `S01-S04`, `Vol 1-6`, a BD/DVD box with no episode
  number). False for one episode, one movie, one special, one extras release.
  A title naming a single video file (it ends in `.mkv` / `.mp4` etc.) is never
  a batch, even if it holds two episodes (`- 05-06 (...).mkv`).
- `episodes` (list of ints): every episode number the title itself names as an
  episode. Expand ranges (`01-03` is `[1, 2, 3]`; for ranges longer than 30,
  write `[first, last]` and put `"range"` in `unsure`). `S02E05` is `[5]`.
  Empty when it names no episode. A special's or extra's number is not an
  episode (`OVA 02`, `NCOP 1`, `S00E03` give `[]`). A half episode (`07.5`)
  gives `[]`. Years, resolutions, CRCs, version suffixes and sequel numbers that
  are part of the show's name are not episodes (`Chihayafuru 2 - 05` is `[5]`;
  `Kaiju No. 8 - 09` is `[9]`).
- `ambiguous` (int or null): a bare number right after the show's name with no
  marker, that could be either the episode or part of the name, and that you
  did not put in `episodes` (`Mob Psycho 100 (BD)` → 100,
  `[EA]Dark_Gathering_24_[1080p]` → 24). Null otherwise. If a number is clearly
  the episode (after ` - `, after `E`, `Ep`, `Episode`, `#`, in its own bracket
  like `[03]`), it goes in `episodes`, not here.
- `season` (int or null): the one season the title names (`S2`, `S02E05`,
  `Season 2`, `2nd Season`, `Second Season`, `II` / `III` in capitals as a
  sequel marker, `第二季`, `Part 2` / `Cour 2` count as season 2). Null when it
  names none or several (`S01-S03`). `S00` is 0.
- `special` (bool): an OVA / OAD / ONA / special / SP release
  (`OVA 01`, `- OVA`, `S00E05`, `Special 2`). A show whose own name ends in
  "Special" is not one by that word alone.
- `extra` (bool): creditless OP/ED (NCOP/NCED), PV, CM, preview, trailer,
  teaser, menu, opening/ending video releases.
- `group` (string or null): the release group as written (`SubsPlease`,
  `Erai-raws`, `VARYG` from `...H.264-VARYG`, `喵萌奶茶屋&LoliHouse`). Null if
  none is named.
- `resolution` (int or null): the vertical resolution named (`1080p` → 1080,
  `1920x1080` → 1080, `4K` → 2160, `720p` → 720, `BD 540p` → 540). Several →
  the highest. Null if none.
- `unsure` (string): empty, or a short note when a field is a judgment call.

Work through every line; do not skip. Be consistent; when two readings are
reasonable, pick the one most people would and note it in `unsure`.
