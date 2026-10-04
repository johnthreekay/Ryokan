# services/indexers/AGENTS.md

Torznab/newznab indexer abstraction. The `Indexer` trait, `Release` / `SearchQuery` / `IndexerCaps` data model, dedup helpers, and concrete impls (`torznab/`). Lives **alongside** the direct Nyaa scraper in `services::nyaa`, not in place of it.

## Why Nyaa stays out-of-band

The search pipeline dispatches to Nyaa-direct + fans out to `Indexer` impls in parallel and merges. Conforming Nyaa to this trait would have meant adding `Release` fields like `nyaa_description: Option<String>` that only one impl populates — a noisy contract — and the source-classification pipeline reads Nyaa's description body directly. Pretending the sources are uniform would have hidden that coupling.

When adding new release sources: add an `Indexer` impl. Don't refactor Nyaa.

## Wire shape (torznab/newznab — research notes)

- **URL is opaque to Ryokan.** Prowlarr emits `http://host:9696/{N}/api?apikey={KEY}&t=...`; Jackett emits `http://host:9117/api/v2.0/indexers/{slug}/results/torznab/api?apikey={KEY}&t=...`. Both end in `/api` and accept torznab params after `?`. The user pastes the full base URL verbatim from each tool's "Copy Torznab Url" button. **Don't parse or reconstruct it.**
- **Errors come back as HTTP 200** with `<error code="N" description="..."/>` bodies. Real impls (Prowlarr, Jackett) also return non-200 in some paths (Prowlarr 401 on bad apikey before the torznab layer); both must be handled.
- **Anime category is `5070`** in the standard torznab namespace. AnimeTosho via Prowlarr historically mis-tagged anime as `5999` (Other) — title-parse fallback is required if the cat doesn't include `5070`.
- **Per-indexer rate limits live inside Prowlarr/Jackett**, not the indexer itself. They surface as `429 Retry-After`. The torznab client honors them via the per-id `cooldown` table in this module: on 429 it stamps `until = now + Retry-After` (capped at `cooldown::COOLDOWN_MAX = 300s`, defaulted to `cooldown::COOLDOWN_DEFAULT = 60s` when the header is missing) and subsequent calls for that indexer short-circuit at the top of `fetch()` until the window lifts. **Per-id rather than global**: a 429 on AB doesn't silence a healthy NZBGeek for the same window — each Prowlarr-fronted indexer has its own budget. Auto-search fan-outs see the cooldown error in the same string-prefix shape as Jikan's (`"Indexer rate-limited (cooldown Ns remaining)"`).
- **`tvsearch` with `cat=5070&q=<title>` is the right anime path.** `season`/`ep` params don't translate cleanly because anime trackers key on absolute episode numbers in titles, so `SearchQuery` deliberately omits them.

## Constants

- `TORZNAB_CAT_ANIME = 5070`
- `DEFAULT_REQUEST_TIMEOUT_SECS = 30` — overridable via `RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS`. Tighter than Sonarr's 100s default because Ryokan's interactive search needs lower user-perceived latency.
- `CAPS_TTL_SECONDS = 7 * 24 * 60 * 60` — indexer caps cache TTL, matches Sonarr's `NewznabCapabilitiesProvider.cs`. Not enforced yet: `rebuild_cache` probes caps in the background only for rows whose `caps_json` is empty (`models::indexers::update_caps`), so stored caps never refresh and there is no "Refresh caps" control.

## `Release` snapshot fields

`indexer_priority` and `indexer_name` are **snapshotted at search time** rather than read live, so a later DB edit (rename, priority change) can't retroactively rewrite past `Release` records that callers kept around. The dedup pass attributes each `(infohash, indexer)` pair to the lowest-priority-number indexer based on the snapshot.

`info_hash` may be empty (some private trackers omit it) — dedup falls back to `guid` (the `<guid>` element). `link` is a Prowlarr-proxied URL with the apikey appended; **stale on Prowlarr restart, so don't cache across days**.

`extra: HashMap<String, String>` is inspector-friendly only — the scoring path **must not** key off these fields. New first-class fields go on the struct.

## Source classification on torznab releases

The Nyaa-description-body signal is unavailable here; classification degrades to **four layers** (filename + ffprobe + temporal + group-map). When editing the source pipeline, don't assume `description` is always populated.

## `IndexerCache` on `AppState`

`Arc<RwLock<Arc<Vec<Arc<dyn Indexer>>>>>` — same swap-on-write shape as `CompiledCfCache`. Rebuilt by Settings → Indexers add/edit/delete handlers. The inner `Arc<Vec<_>>` is cheap-cloned out under read lock so the search hot path runs lock-free; this avoids rebuilding `reqwest::Client` instances per fan-out.

## Per-indexer download client pin

Each indexer row carries an optional `download_client_id`. Resolution chain at grab time (`AppState::client_for_indexer_with_id`): pin → per-protocol default (torznab → torrent default; newznab → usenet default) → torrent fallback. Deep dive in root AGENTS.md under "Download-client routing."
