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

## The upgrade decision

- **The upgrade decision is `source::judge_upgrade`** (`services/source/upgrade_policy.rs`), Sonarr's `UpgradableSpecification` + `ProperSpecification` + `AnimeVersionUpgradeSpecification` in one function; `is_valid_upgrade` is only its quality half (rank tuple + the BDMV-crossing rule). Order: unclassified existing → never; higher rank → allowed while the file is below the quality cutoff, else "cutoff met" even when the Custom Format cutoff is unmet; lower rank → no; same rank + newer revision (`media::parse_release_revision`: `vN` case-insensitive, `PROPER` / `REPACK` / `RERIP` **uppercase only**, so a Title Case episode-title word in a renamed library file is not a v2) → allowed only from the **same release group** as the tag row's `release_group` (unknown on either side rejects), only under `proper_policy` `prefer_and_upgrade`, and through RSS only while the file is at most `PROPER_MAX_AGE_DAYS` (7) old by mtime (`EpisodeFile.modified_secs`); a revision bypasses both cutoffs; same rank + older revision → no; otherwise the CF total has to be strictly higher, the existing total under `custom_format_cutoff_score` (default 0, so a clean file meets it), and the gain at least `custom_format_upgrade_increment` (default 1). The existing side's revision, group and CF total come from the tag row's `release_title` / `release_group` (never the renamed file; a file with no row has no group and cannot take a revision), the CF total via `custom_formats::total_cf_score_for_release` with **no SeaDex hashes on either side** and size 0 on both sides in RSS (RssItem carries none); a file with no tag row has `cf_score = None` and takes no score upgrade (`FormatScoreUnknown`), a pinned row is `Pinned`, and a multi-episode file the release does not cover is `SpanNotCovered`. The RSS canonical history (`rss::CanonicalHistory`, grabbed titles plus the client's queue) keys each release by logical item and revision, so a `v2` is not swallowed as "already grabbed"; a lower revision of the same item still in history while the episode is not on disk (the `v1` still downloading when its `v2` posts) is judged by `rss::judge_queued_revision`, Sonarr's queue check: `prefer_and_upgrade` and the queued release's group, else rejected as already queued, since the file gate above sees disk files only; the within-sweep bucket key (`logical_bucket_key`) does not, so a `v1` and its `v2` in one feed window compete for one grab, and `compare_candidates` prefers the higher revision at equal score. Call sites: RSS (`rss::UpgradeGate` → `episode_is_upgradeable`, `from_rss = true`, the rejection reason lands in the decision record), the upgrade sweep and the auto-search handler's upgrade targets, the Wanted cutoff search included (both through `upgrade::judge_release_for_episode`, `from_rss = false`; the sweep's targets stay quality-only via `build_upgrade_targets`, as in Sonarr: a file at the quality cutoff with a low CF total is reached through RSS, since `find_best_for_target` returns one candidate and a higher-quality one is "cutoff met" for such a file, so sweeping it would burn a search per day that can never succeed), and the manual-import preview (`upgrade_policy::is_revision_upgrade`, no group rule, the user sees the outcome). Import still trusts the grab that reached it. Scoring adds a "Release Revision" line (+10 for v2, +5 per further version, cap +20; `scoring::revision_bonus`, mirrored in `rss::score_candidate`) gated by `SearchOptions.prefer_revisions`, which the auto-search contexts carry from `SeriesSearchCtx.prefer_revisions`; the SeaDex seed scorer passes `true`. Settings → Quality: Propers and Repacks, Upgrade Until Custom Format Score, Minimum Custom Format Score Increment. The series' **Upgrades** switch (`series.allow_upgrades`) is honored by the daily sweep, RSS (`UpgradeGate.series_allows_upgrades`, rejected as `UpgradeRejection::SeriesUpgradesOff` before any other rule) and the series auto-search (`upgrade_candidate_episodes`, so the Wanted searches and the failed-download re-search too); the single-episode search and interactive grabs ignore it on purpose, since the user picked the episode.
