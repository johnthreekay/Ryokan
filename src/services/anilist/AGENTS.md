# services/anilist/AGENTS.md

Primary metadata provider. `mod.rs` owns the GraphQL client, `DETAIL_CACHE`, and batch fetch helpers; `rate_limit.rs` owns the process-wide throttling state machine.

`RYOKAN_ANILIST_API_BASE` is re-read on every request rather than cached, so the `tests/external_sync_e2e.rs` wiremock fixture can flip it per-fixture without process restart.

## Request headers (the Referer gate)

Every GraphQL POST is built by `anilist_post(&client)`, which sets `User-Agent: Ryokan/0.1` and `Referer: https://github.com/johnthreekay/Ryokan` (`ANILIST_REFERER`). The Referer is load-bearing since 2026-09-07: AniList's "The AniList API has been temporarily disabled due to severe stability issues" 403 is a Referer gate, not an outage. A request with no Referer (the default for every scripted client) gets that body; a request with any Referer, project URL included, gets a normal answer with `X-RateLimit-*` headers. User-Agent and Origin play no part. The project URL is an honest identifier, not an anilist.co impersonation; if AniList ever narrows the gate to its own origin the 403 path and the MAL fallback behave exactly as they did during the outage. Build new AL requests through the helper (`handlers::oauth::fetch_anilist_viewer` does too) so the header is never forgotten; the bare-`client.post` shape has no test that fails, only a library that silently reads MAL.

## Rate-limit state machine

Behind `LazyLock<Mutex<_>>` in `rate_limit.rs`. Reads `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` from every AL response and adapts between two modes:

1. **Full headroom**: minimum inter-request spacing.
2. **Throttled**: wait until window-reset once `Remaining` drops below `REMAINING_HEADROOM_THRESHOLD` (3).

On 429 or 5xx, writes a cooldown-until timestamp using AL's `Retry-After` (capped at 5 minutes) or a 60s default, plus a mandatory **2-second safety margin past the window boundary**. Without the margin, waiting exactly the Retry-After value lands the next request at the boundary and trips a fresh 429.

Touch the safety margin, threshold, or taxonomy carefully — they're load-bearing for how the metadata chain behaves under throttle.

## Failure taxonomy

`classify_anilist_failure` returns three-way:

| Variant | Error-string prefix | Downstream policy |
|---|---|---|
| `RateLimited` | `"AniList rate-limited"` | Defer + retry. Falling back to Jikan would just move load and rapidly exhaust Jikan's 3 req/s budget too. |
| `Unavailable` | `"AniList unavailable"` | **Only this falls back to Jikan.** |
| `NotFound` | `"AniList not found"` | Stays not-found. |

Callers match on the **prefix string**, not HTTP codes. Adding new wordings requires updating the tag, not the policy.

## Jikan cooldown (the fallback side)

`services::jikan::JIKAN_COOLDOWN_UNTIL` is the Jikan equivalent of the AL rate-limit machine, simpler. When Jikan 429s, sets "unavailable until Instant" so subsequent calls return a clean cooldown error rather than hammering the API and piling up more 429s. **60s default, 300s max**, honors response `Retry-After` when present. Read `services/jikan.rs` if you're touching how the fallback chain handles rate limits — the AL→Jikan handoff assumes both sides have working cooldowns.

## `DETAIL_CACHE`

Per-AL-id memoization. Partial-recovery paths read from it after a failed batch fetch — `get_anime_details_batch` aborts on first Err but writes from completed chunks survive in the cache. Auto-expand's transitive neighbor-fetch fallback relies on this.

## Negative-ID sentinel

Series added via the Jikan fallback with no AL mapping store as `series.anilist_id = -mal_id`. **Every AL call site filters `id > 0`** (see `get_anime_details_batch` around `mod.rs:1326`) so synthetic ids don't leak into AL requests and instead route back through Jikan on refresh.

**User-visible consequence**: SeaDex is keyed by positive AL id, so series added this way are silently invisible to SeaDex-driven scoring (the SeaDex Custom Format and toggle never fire for them). New code that joins against AL ids must preserve the `> 0` filter or explicitly document the negated case.

## Airing schedule batch query

`Page.airingSchedules` is the batch-query shape for calendar use cases. **Rate limit degrades to 30/min** on this endpoint. Negative-AL-id series are blind to this query for the same reason as SeaDex.
