---
name: add-indexer
description: Step-by-step workflow for adding a new Indexer impl beyond torznab/newznab (e.g., a private-tracker REST shape, a hypothetical anime-specific protocol). Walks the trait impl → caps fetcher → IndexerCache wiring → settings UI → AGENTS.md notes. Skip this skill for adding a new torznab/newznab *deployment* (e.g., a new Prowlarr/Jackett indexer row), that's just a Settings UI add. This skill is for adding a new *protocol*.
---

# Adding a new Indexer protocol impl

The `Indexer` trait + `IndexerCache` machinery is what runs **alongside** the direct Nyaa scraper. Nyaa stays out-of-band per plan decision #1. Never refactor Nyaa into this trait. The search pipeline dispatches to Nyaa-direct + fans out to `Indexer` impls in parallel and merges.

Currently one impl: `torznab` (which doubles as `newznab`: the wire format is identical, and they differ only in download URL semantics and category mapping). If you're adding a fundamentally new wire shape (e.g., a JSON-API private tracker, a SOAP interface), this is your skill.

## When this skill does NOT apply

- **Adding a new Prowlarr/Jackett indexer row**: that's a Settings → Indexers UI add, no code change. The torznab impl handles the wire.
- **Adding a category alias / search-mode tweak to torznab**: edit `src/services/indexers/torznab/`, not a new protocol.
- **Adding a generic RSS feed source (e.g., SubsPlease)**: that's `direct_rss_feeds`, separate from indexers. See `src/handlers/settings/direct_rss_feeds.rs`.

## Step 1: Implement the trait

Create `src/services/indexers/<kind>/mod.rs`. Read `src/services/indexers/mod.rs` for the trait surface: the `Indexer` trait, `Release` / `SearchQuery` / `IndexerCaps` data model.

The trait:

```rust
#[async_trait]
impl Indexer for FooIndexer {
    fn id(&self) -> i64;
    fn name(&self) -> &str;
    fn priority(&self) -> i32;
    fn is_private_tracker(&self) -> bool;
    fn download_client_id(&self) -> Option<i64>;  // default None: use the protocol's default client
    fn kind(&self) -> &str;             // default "torznab"; override it ("foo"), it drives protocol routing

    async fn caps(&self) -> Result<IndexerCaps, String>;
    async fn search(&self, query: &SearchQuery) -> Result<Vec<Release>, String>;
}
```

### `search()` contract

- RSS sync calls `search` with an **empty `q`** and expects the newest items (`fetch_indexer_rss`); make the empty query mean "latest", not "nothing".
- Drop releases below the row's `min_seeders` (torznab filters in `torznab/client.rs`).
- Stamp `indexer_id`, `indexer_priority` and `indexer_name` on every `Release` (see below).

### Hardening (as torznab does it)

- Normalize every info-hash through `download_client::normalize_info_hash` (40 or 64 hex, else `""`): qBittorrent reads `all` as every torrent (`torznab/parser.rs`).
- Cap the response body: `rss::feed::read_capped_body` (10 MB), never a bare `.text()`.
- Format reqwest errors with `e.without_url()`: the API key is in the URL, and the error text reaches notifications and System → Logs.
- Build the client with `user_agent("Ryokan/0.1")` and a timeout, once per indexer (not per request).

### Release identity contract

Snapshot `indexer_priority` and `indexer_name` at search time onto each `Release`. Do NOT read them live. A later DB edit (rename, priority change) shouldn't retroactively rewrite past `Release` records callers may have kept around. The dedup pass attributes each `(infohash, indexer)` pair to the lowest-priority-number indexer based on the snapshot.

### Source-classification degradation

The Nyaa-description-body signal is unavailable for non-Nyaa sources; classification degrades to four layers (filename + ffprobe + temporal + group-map). Ensure your `Release.title` is rich enough that the filename layer can do its job, anitomy parses the title for resolution, group, episode number, etc. If your wire format strips brackets or normalizes the title, the filename layer goes blind.

### Protocol-specific notes (any non-torznab impl)

- **URL opacity**: store the user-pasted base URL verbatim. Don't parse, don't reconstruct. The torznab wire-shape research notes in `src/services/indexers/AGENTS.md` are the canonical reference for why.
- **Error-as-200**: many indexer protocols return 200 + `<error>` body for failures. Parse the body before trusting the status code.
- **Categories**: the standard torznab anime category is `5070`. If your protocol has its own category space, document the mapping in the impl's docstring + `services/indexers/AGENTS.md`.
- **Rate limits**: indexers usually signal them as `429 Retry-After`. Use the shared per-indexer cooldown in `services/indexers/cooldown.rs`: on a 429 call `cooldown::record_429(self.id, retry_after_secs)`, and at the top of each request short-circuit on `cooldown::remaining(self.id)` with the `"Indexer rate-limited (cooldown Ns remaining)"` error prefix (torznab does both in `torznab/client.rs`).

## Step 2: Caps

`caps` returns the indexer's reported categories, search modes, and limits. `rebuild_cache` probes it in the background for rows whose `indexers.caps_json` is empty and stores the result with `models::indexers::update_caps`. Nothing refreshes stored caps later today (`CAPS_TTL_SECONDS` is defined but not enforced), and there is no manual refresh.

The indexer form's Categories field is comma-separated text, with chips built from the cached caps. If your protocol has no categories, return an empty `categories`: the form shows no chips, and `resolve_request_categories` sends the requested ids unchanged.

## Step 3: Tests

Same wiremock pattern as download clients. Create `src/services/indexers/<kind>/wiremock_tests/`:

```
wiremock_tests/
├── mod.rs            # declares the topic modules
├── fixture.rs        # MockServer setup + response-shape helpers
├── search.rs         # search() against various query shapes
├── rss.rs            # search() with an empty q (the RSS path)
├── caps.rs           # caps() round-trip
├── auth_failures.rs  # 200-with-error-body, 401, 5xx; errors never carry the API key
└── rate_limit.rs     # 429 + Retry-After and the cooldown short-circuit
```

No `live_smoke` for indexers (no `RYOKAN_<KIND>_E2E` gate exists). Wiremock coverage is the only verification. Point it at recorded fixtures from a real Prowlarr/Jackett response if the impl is for a known service.

## Step 4: Register the kind

`src/services/indexers/mod.rs::rebuild_cache` has no kind dispatch yet: it calls `torznab::TorznabIndexer::from_row_arc(&row)` for every row inside one `filter_map`. Replace that call with a `match row.kind.as_str()` that calls `foo::FooIndexer::from_row_arc(&row) -> Result<Arc<dyn Indexer>, String>` for your kind, inside the same `filter_map`, so a row whose client fails to build drops together with it (the comment there explains why the two must stay together).

Then update `protocol_for_indexer_kind(kind: &str) -> Option<&'static str>` in `src/services/download_client/mod.rs`:

```rust
"foo" => Some("torrent"),  // or "usenet" if your protocol delivers NZBs
```

This is what routes new-kind grabs to the right per-protocol download client default when no per-indexer pin is set.

## Step 5: Settings UI

1. `src/models/indexers.rs`: add a `KIND_FOO` constant beside `KIND_TORZNAB` / `KIND_NEWZNAB`.
2. `src/handlers/settings/indexers.rs`:
   - the upsert's kind `match` (it rejects unknown kinds)
   - the stateless Test check, and `build_transient_indexer`, which builds a `TorznabIndexer` directly: the Test button must build your type
   - `api_key_hint_for_kind` and `url_placeholder_for_kind` for per-kind form hints
3. Add an `<option>` to `templates/partials/settings/indexers/add_form_body.html` and `edit_form_body.html`.
4. Optional: presets in `src/services/indexer_catalog.rs`.

## Step 6: DB schema

The `indexers` table is generic. New kinds usually fit without a schema change. If you need a new column (rare), add an idempotent migration in `src/models/migrations/mod.rs`:

```rust
sqlx::query("ALTER TABLE indexers ADD COLUMN foo_specific TEXT NOT NULL DEFAULT ''")
    .execute(&pool)
    .await
    .ok();
```

## Step 7: Documentation

1. **`src/services/indexers/AGENTS.md`**: add a section under "Wire shape" describing the new protocol's quirks. Mirror the torznab section's bullet shape: URL opacity, error-as-200 behavior (or whatever your protocol does), categories, rate-limit handling, query-mode mapping.
2. **Root `AGENTS.md`**: update the Project Overview's "torznab/newznab indexer system" mention if the new protocol is broad enough to deserve a name change. Most additions don't qualify.

## Step 8: Verify

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-support -- -D warnings
cargo nextest run --workspace --features test-support
```

Run `code-reviewer` against the diff for convention checks (Result tag prefixes, hardcoded UA on the new HTTP client, log category for indexer-search events).

## Files you'll touch (summary)

```
src/services/indexers/<kind>/mod.rs                  [NEW]
src/services/indexers/<kind>/wiremock_tests/*.rs     [NEW]
src/services/indexers/AGENTS.md                      [edit: add protocol section]
src/services/indexers/mod.rs                         [edit: rebuild_cache kind match]
src/services/download_client/mod.rs                  [edit: protocol_for_indexer_kind]
src/models/indexers.rs                               [edit: KIND_* constant]
src/handlers/settings/indexers.rs                    [edit: upsert kind match, Test path, form hints]
templates/partials/settings/indexers/{add,edit}_form_body.html  [edit: kind <option>]
src/models/migrations/mod.rs                         [edit: only if new column needed]
AGENTS.md (root)                                     [edit: Project Overview if applicable]
```

## Reminder: Nyaa stays out-of-band

This is the most important rule and easiest to forget mid-impl. The search pipeline dispatches Nyaa + fans out `Indexer` impls **in parallel** and merges. Don't:

- Refactor Nyaa to implement `Indexer`. The trait was deliberately scoped to NOT include Nyaa-specific shapes (description-body URL, the scraped HTML quirks).
- Add Nyaa-only fields to `Release` (`nyaa_description: Option<String>`).
- Make the merge step "if Nyaa fails, treat as Indexer error". Nyaa is the protected hot path.

If you find yourself tempted to add Nyaa to `Indexer`, stop and re-read the "Why Nyaa stays out-of-band" section in `services/indexers/AGENTS.md`.
