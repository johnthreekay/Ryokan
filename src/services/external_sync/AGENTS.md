# services/external_sync/AGENTS.md

AniList / MAL watch-list sync and its exclusions.

Keep this file under 40,000 characters (`wc -m`): past that, a Claude Code session with a 200K context window is given only its start. `tests/agents_md_audit.rs` enforces it.

## Layout

- `external_sync/` — AL/MAL watch-list sync (delta cursor, force-full-resync, removal detection, auth-rejection taxonomy). **Exclusions** (Sonarr's import-list exclusions): `models::sync_exclusions` (`external_sync_exclusions`, AniList id and/or MAL id; an AniList entry matches by its AniList id, or by MAL id only through a row recorded without one, while a MAL-only entry matches any row of its MAL id (#292: two AniList entries can share a MAL id, and excluding one must not exclude the other); the negative MAL sentinel never matches; `add` completes a one-id row with the other id (an AniList id completes only a MAL-only row), and the AniList merge loop passes the detail's `id_mal`, so a row recorded for a MAL-fallback series blocks the same show on the AniList list), written by `handlers::library::crud::record_sync_exclusion` when the remove dialog's checkbox (`RemoveSeriesForm.add_exclusion`, `BulkDeleteRequest.add_exclusion`) is set, read once per sync by both merge loops (`MergeOutcome.excluded`, the "excluded" count in the sync summary lines), and listed as Sync exclusions on Settings → Integrations (the tab the sidebar labels Connections) with an "Allow again" delete (`POST /settings/sync-exclusions/{id}/delete`); the list renders only while an account is linked (an empty-state line then) or a row exists, so an exclusion recorded before an unlink keeps its control. An excluded entry is skipped whole, monitor updates included, until the row is deleted, **unless the series is back in the library** (a hand re-add keeps syncing, and `add_series` clears matching rows through `delete_for_ids`). The exclusion is written only after the removal succeeded (`sync_exclusion_snapshot` before, `record_sync_exclusion` after). The "Allow again" control is a button with `hx-post`, not a form: the partial sits inside the Integrations `<form>`.

## The supervised task

- **`external_sync` quirks**: 1-min outer tick re-reads `config.external_sync_interval_minutes` (Settings change takes effect within 60s); `consecutive_errors`-driven exponential skip (2^errors intervals, capped at 5 → 32× multiplier, with 24h ceiling so a 7-day cadence can't get pushed seven months by five errors); `has_linked_account` early-out so no `scheduled_task_runs` row burns when no account is linked.
