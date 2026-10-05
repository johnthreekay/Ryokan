---
name: add-download-client
description: Step-by-step workflow for adding a new DownloadClient impl (e.g., Aria2, NZBGet, putio). Walks the full integration: trait impl → wiremock tests → live_smoke env-gate → kind registration in rebuild_clients_cache → per-protocol mapping → settings UI form → AGENTS.md quirks block. Use this when implementing a new client; following it end-to-end produces a complete, CI-clean PR.
---

# Adding a new DownloadClient impl

## Decide the protocol

Currently two protocols are recognized: `torrent` (qBit / Deluge / Transmission / rtorrent) and `usenet` (SAB). The protocol determines per-grab routing fallback (`AppState::client_for_indexer_with_id`).

If the new client is one of these two, no protocol changes are needed. If it's a third (Direct Connect? Soulseek? You wouldn't, but…), you'll also need to extend `protocol_for_client_kind` and add a `default_<protocol>_id` field on `DownloadClientPool` plus the resolver fallback rules. Treat that as a separate planning task. Most additions don't need it.

## Step 1: Implement the trait

Create `src/services/download_client/<kind>/mod.rs`. Start with a thorough module-level docstring covering wire quirks (every existing impl has one, and `code-reviewer` will check for it). The docstring is the source of truth for "things that bit us during impl"; the matching block in `services/download_client/AGENTS.md` (`CLAUDE.md` is a symlink to it) is the deduped summary.

Implement `DownloadClient` from `src/services/download_client/mod.rs`:

```rust
#[async_trait]
impl DownloadClient for FooClient {
    async fn test(&self) -> Result<String, String>;
    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String>;
    async fn add_torrent_returning_id(&self, url: &str, info_hash: &str)
        -> Result<(AddOutcome, String), String>;  // BT: return info_hash; Usenet: return captured opaque id
    async fn add_torrent_paused(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String>;
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String>;
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String>;
    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String>;
    async fn pause(&self, info_hash: &str) -> Result<(), String>;
    async fn resume(&self, info_hash: &str) -> Result<(), String>;
    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String>;
    async fn set_file_wanted(&self, info_hash: &str, files: &[usize], wanted: bool) -> Result<(), String>;
    fn sonarr_impl_name(&self) -> &'static str;
    fn protocol(&self) -> &'static str;  // "torrent" or "usenet"
    async fn set_seed_rules(&self, info_hash: &str, rules: SeedRules) -> Result<(), String>;
}
```

Four of these have defaults in `download_client/mod.rs`: `add_torrent_returning_id` (returns `info_hash`; override it for usenet), `add_torrent_paused` (a plain add; override it for the file picker), `protocol` (`"torrent"`), and `set_seed_rules` (a no-op). Return `Err` from `set_seed_rules` when the client can't honor per-item rules, so the gap is logged (rtorrent does).

### Guards every impl must call

- **Release URLs**: every add method (`add_torrent`, `add_torrent_paused`, `add_torrent_with_file_filter`, any `*_returning_id` override) calls `super::check_release_url(url)?` before sending anything. It allows magnet / http(s) only. `test_helpers::assert_refuses_non_url_releases` checks it from your `add.rs` wiremock test.
- **Info-hashes**: run `super::check_info_hash(hash)?` before any call whose API could read a hash as a list or a wildcard (qBittorrent reads `hashes=all` as every torrent and `a|b` as two). An empty hash means a hashless grab and is a no-op on pause / resume / delete / seed rules, never "all".
- **Secrets in error text**: reqwest's error `Display` includes the request URL, which for many clients carries an API key or a passkey. Format request errors with `e.without_url()` wherever the URL can carry a credential, and pass any release URL you put into an error or a log line through `crate::services::sanitize::redact_url`. Those strings reach notifications, System → Logs and the poll-error column.

### Identity contract

- **BT impls**: `add_torrent_returning_id` returns the precomputed `info_hash` argument unchanged. Hashes reach the trait as 40 (v1) or 64 (v2) lowercase hex, normalized by `normalize_info_hash` at every source, or empty for a hashless grab; case-munge internally if your wire format wants uppercase.
- **Usenet impls**: `add_torrent_returning_id` captures whatever opaque id the wire returns (SAB's `nzo_id`, NZBGet's `NZBID`) and returns it. Callers persist it on `grabbed_torrents.hash`. Subsequent ops receive that string verbatim as `info_hash`.

### Per-client scoping (mandatory)

Every impl needs a "things Ryokan added" filter so `list_scoped` doesn't return torrents from other tooling. Pick the mechanism:

- HTTP API with categories/labels → use that (qBit `?category=`, Deluge Label plugin, Transmission native labels, SAB `cat=`)
- API supports neither → fall back to save-path prefix (legacy Transmission)
- API has a custom1-style free-form field → use it (rtorrent)

The label/category string comes from the `download_clients.label` row column (per-row, not global).

### File-priority gotchas (varies by client)

| Client | Wanted | Skip | Notes |
|---|---|---|---|
| qBit | 1 | 0 | 0/1/6/7 scale; we only write 0/1 |
| Deluge | 4 | 0 | 0/1/4/7. Writing `1` for "wanted" sets Low priority (bug) |
| Transmission | wanted=true | wanted=false | Separate from priority axis; don't touch priority |
| rtorrent | 1 | 0 | After setting, **MUST** call `d.update_priorities(<hash>)` or no effect |
| SAB | always wanted | n/a | NZBs are opaque blobs; `set_file_wanted` no-ops, returns `SelectiveOutcome::FullDownload` |

### Idempotency on retry

`add_torrent_with_file_filter` may be re-invoked on retry. Read each file's `wanted` flag back before changing it so a re-narrow doesn't clobber user edits made between attempts. The interactive picker also calls `set_file_wanted` directly via `grab_confirm`; if the user clicked Confirm twice (network blip), the second call should be a no-op for files already in the requested state.

### Duplicate-add detection

Each impl handles this differently:
- qBit: probe `/torrents/info?hashes=<hash>` **before** adding (4.x answers `Ok.` to a duplicate, so the add's own answer can't say who created it) and again after 5.x's `200 "Fails."` to tell a duplicate from a malformed magnet → `AddOutcome::AlreadyPresent`.
- Deluge: substring-match `"Torrent already in session"` / `"Torrent already being added"` (deluge-dev/#3507, error code fluctuates).
- Transmission: `torrent-duplicate` key inside `result: "success"` envelope (not an error).
- rtorrent: silent. `load.start_verbose` returns `0` either way. Pre-check by listing hashes.
- SAB: empty `nzo_ids` array; scan `mode=queue` for the URL.

Failing to detect duplicates → re-grabs hard-fail (RSS re-emission, upgrade-sweep collisions, post-crash regrabs). Reporting `Added` for a torrent that was already there is worse: the file picker's cancel deletes only what it added, so it would delete another grab's download with its data.

### State contracts

See `services/download_client/AGENTS.md` ("`Warning` vs `Errored`" and "Seed rules and `seeding_done`") for the per-client tables.

- `DownloadItemState::Errored` only for the client's own failed-download verdict: post-processing deletes the item with its data and blocklists the release. Anything recoverable (missing files, a dropped mount, a stopped item with a message) is `Warning`, which post-processing waits on.
- `seeding_done` only when the client itself stopped seeding by its ratio / time rule, never for a user pause.
- `list_scoped` returns `Err` when the listing fails. An empty `Ok` says the client holds nothing of Ryokan's, and the progress poll marks every grab on that client removed.

## Step 2: Wiremock tests

Create `src/services/download_client/<kind>/wiremock_tests/` with topic-split files. Existing convention (don't break it):

```
wiremock_tests/
├── mod.rs              # declares the topic modules
├── fixture.rs          # shared MockServer setup helpers
├── auth.rs             # connection / login / API-key handshake
├── add.rs              # add_torrent / add_torrent_paused / add_torrent_returning_id;
│                       #   calls test_helpers::assert_refuses_non_url_releases
├── list.rs             # list_scoped + scoping filter behavior
├── files.rs            # get_files + set_file_wanted
├── control.rs          # pause / resume / delete (hash guards, empty-hash no-ops)
├── seed_rules.rs       # set_seed_rules + seeding_done (torrent clients)
└── hash_case.rs        # uppercase-vs-lowercase hash handling at the wire (if applicable)
```

The directory is named `wiremock_tests/`, not `tests/`, to avoid colliding with the inline `#[cfg(test)] mod tests` block in the parent `mod.rs` (which holds pure-helper tests).

## Step 3: Live-smoke test

In `src/services/download_client/<kind>/mod.rs`, add an `#[ignore]`d test gated on `RYOKAN_<KIND>_E2E=1`:

```rust
#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore]  // run with --ignored AND env var set
    async fn live_smoke() {
        if std::env::var("RYOKAN_FOO_E2E").is_err() {
            eprintln!("skipping live_smoke: RYOKAN_FOO_E2E not set");
            return;
        }
        // exercise the full trait surface against localhost
        // (test → add_torrent → file filter → list_scoped → pause/resume → delete)
    }
}
```

CI never runs these; they're for hand-validation when touching the impl. Document any env vars (URL override, API key, default password) in:
- The env-vars table in root `AGENTS.md`
- The live-smoke table in `src/services/download_client/AGENTS.md`

## Step 4: Register the kind

Edit `src/services/download_client/mod.rs::rebuild_clients_cache` and add an arm to the `match row.kind.as_str()`:

```rust
"foo" if !row.url.is_empty() => Some(Arc::new(foo::FooClient::new(
    &row.url,
    &row.username,
    &row.password,
    &row.label,
))),
```

Then update `protocol_for_client_kind(kind: &str) -> Option<&'static str>` in the same file:

```rust
"foo" => Some("torrent"),  // or "usenet"
```

This is what routes new-kind grabs to the right protocol default when no per-indexer pin exists. The kind-to-protocol mapping is duplicated in three more places that must agree:

- `models::download_clients::protocol_for_kind` (`src/models/download_clients.rs`), plus the per-protocol kind lists in the same file (default-client selection and listing)
- `dcProtocolForKind` in `static/js/settings.js`

## Step 5: Settings UI

1. `src/handlers/settings/download_clients.rs`: add a `KIND_FOO` constant and the kind to `is_known_kind` (the upsert rejects unknown kinds), `kind_label` (the display name) and `copy_for_kind` (per-kind field labels and hints, e.g. SAB's API key living in the password field). `DC_KIND_COPY` in `static/js/settings.js` mirrors `copy_for_kind` for the live form; keep them in step.
2. The **Test** button builds its own client: add the kind to the constructor `match` in `settings_download_clients_test`, or Test fails for it while the saved row works.
3. Add an `<option>` for the kind in both `templates/partials/settings/download_clients/add_form_body.html` and `edit_form_body.html`.
4. `client_kind_display` in `src/handlers/downloads.rs` names the client on the Downloads page.
5. For a **usenet** kind, the templates that special-case `kind == "sabnzbd"` assume SAB is the only one: `download_clients/list.html`, `download_clients/edit_form_body.html`, and the indexer `add_form_body.html` / `edit_form_body.html` / `nyaa_form_body.html` pickers. Generalize those to the protocol.

## Step 6: DB schema

The `download_clients` table is generic. New kinds usually fit without a schema change. If you need a new column (rare: most fields fold into `username` / `password` / `label`), add an idempotent migration in `src/models/migrations/mod.rs`:

```rust
sqlx::query("ALTER TABLE download_clients ADD COLUMN foo_specific_field TEXT NOT NULL DEFAULT ''")
    .execute(&pool)
    .await
    .ok();  // ignore "duplicate column name" on re-run
```

For one-shot data backfills, use the `schema_migrations` ledger pattern (see `models/group_source_map.rs`).

## Step 7: Documentation

1. **`src/services/download_client/AGENTS.md`**: add a new `## Foo quirks (`<kind>/mod.rs`)` section. Mirror the existing impls' shape: bulleted gotchas, each with the *symptom* + *what we do about it* + a pointer to the impl for full detail. Don't duplicate the impl's docstring verbatim, the AGENTS.md is the deduped summary. Add the client's row to the "`Warning` vs `Errored`" and "Seed rules and `seeding_done`" tables.
2. **Live-smoke table**: add the env var + default config row.
3. **Root `AGENTS.md`**:
   - Update the Project Overview's client count if the protocol changed.
   - Add the live-smoke env var to the Environment Variables table.
4. If the protocol is new (third option beyond torrent/usenet), update the Download-client routing section.

## Step 8: Run the verification chain

Before commit:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-support -- -D warnings
cargo nextest run --workspace --features test-support       # all wiremock_tests should pass
RYOKAN_FOO_E2E=1 cargo nextest run live_smoke -- --ignored  # against a real localhost daemon
```

The `code-reviewer` agent encodes most of the conventions this skill walks through; running it against the diff catches anything missed.

## Files you'll touch (summary)

```
src/services/download_client/<kind>/mod.rs                      [NEW]
src/services/download_client/<kind>/wiremock_tests/*.rs         [NEW]
src/services/download_client/AGENTS.md                          [edit: quirks section + state tables]
src/services/download_client/mod.rs                             [edit: rebuild_clients_cache arm + protocol_for_client_kind]
src/models/download_clients.rs                                  [edit: protocol_for_kind + per-protocol kind lists]
src/handlers/settings/download_clients.rs                       [edit: KIND_*, is_known_kind, kind_label, copy_for_kind, Test constructor]
src/handlers/downloads.rs                                       [edit: client_kind_display]
static/js/settings.js                                           [edit: dcProtocolForKind, DC_KIND_COPY]
templates/partials/settings/download_clients/{add,edit}_form_body.html  [edit: kind <option>]
src/models/migrations/mod.rs                                    [edit: only if new column needed]
AGENTS.md (root)                                                [edit: env vars + project overview if applicable]
```
