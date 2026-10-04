# services/source/AGENTS.md

Multi-layer source classification pipeline. `source/mod.rs` is the aggregator and pre-download orchestrator; `source/types.rs` holds the primitive enums (`Source`, `WebKind`, `Resolution`, `Origin`, `DecisionRule`) and the `SourceEvidence` / `ClassificationResult` records (re-exported from `services::source` so callers keep the familiar path).

Sibling files `services/source_filename.rs`, `source_description.rs`, `source_temporal.rs`, `source_groups.rs`, `source_dir.rs`, `source_ffprobe.rs` are individual signal layers. Together they produce `(Source, Resolution, is_remux)` used by scoring and upgrade decisions.

## Source ordering

`Unknown < Tv < Hdtv < Dvd < Web < BluRay`

BD is the canonical reference and WEB sits a notch below, matching community preferences. **Don't reorder without understanding the upgrade-sweep consequences** — the upgrade task uses the strict `<` to decide whether to grab a replacement.

## Classifier confidence loop

`episode_quality_tags` carries `classification_confidence`, `needs_review`, and `manual_override` columns alongside the verdict. When the layers don't agree strongly enough, the row is written with `needs_review = 1` and shows up on `/library/review`.

Setting a manual override via `/api/library/manual-override` writes `manual_override = 1` and clears `needs_review`. **The upgrade sweep skips rows with `manual_override = 1`** — a pinned classification is never silently re-graded by a later reclassification pass.

## Release-group identity map (`group_source_map`)

Seeded at startup from a built-in table; seeded rows are re-upserted on every boot but **user-added or user-edited rows are preserved**. The Settings → Release Groups tab also surfaces a "Suggested Mappings" panel derived from repeat manual overrides (N episodes from the same group pinned to the same source), which the user can promote into the identity map.

When editing seeding logic: do not overwrite user edits. When touching the suggestion query: it reads from `episode_quality_tags` rows with `manual_override = 1`.

The `group_source_map` table owns its own `CREATE TABLE` next to its model module (not in `migrations.rs`) and uses a one-shot `schema_migrations` ledger entry for any data rewrite — see `models/group_source_map.rs`.

## CF `ReleaseGroupSpecification` is title-only, not source-inferring

The bundled `S-Tier BD groups` CF in `static/default_custom_formats.json` lists 18 groups regex-matched against the scraped `[Group]` prefix. Several of those groups (MTBB, smol, Vodes, Okay-Subs, Arid, LYS1TH3A, sam) are intentionally **absent** from `SEED_DEFAULTS` in `models/group_source_map.rs` because TRaSH lists them in both its BD and WEB tiers, so the group alone doesn't say the source; MegaMTBB is absent because TRaSH doesn't list it. MiniMTBB is deliberately not in the S-Tier: a good group, but a mini-encode one rather than one aiming for the highest quality (TRaSH puts it in its low-quality list, which is harsher than Ryokan wants), so its releases score on their own traits (BluRay source, 10-bit, audio) with neither the group bonus nor a penalty.

Not a contradiction: the CF applies a score bonus to the group identity, while `group_source_map` applies a BD-vs-WEB prior to source classification. **An S-Tier CF match does not imply BluRay** — source classification still comes from filename / ffprobe / temporal / dir layers.

Keep these two lists editable independently. Do not "sync" them.

## Finished-series mode

`services::quality::FinishedSeriesMode` (SameAsAiring / PreferBd / BdOnly) keys off `config.finished_series_mode`.

- `PreferBd` triggers a BD-probe pass using `bd_probe_queries` (appends ` bluray` / ` BD` / ` BDRip` / ` remux` per alias) before falling back to the normal search.
- `BdOnly` never falls back and skips the series if no BD candidate is found.
- `SameAsAiring` is the neutral default — finished/airing distinction doesn't affect scoring.

"Finished" comes from the AL status field at classification time, not from release dates.
