# handlers/auth/AGENTS.md

Cookie-based sessions for the web UI. `require_auth` middleware on protected routes redirects to `/login`; first-run setup at `/setup` creates the admin account. Tests live in `tests/` (topic-split: `throttle.rs`, `csrf.rs`, `sessions.rs`, `proxy_headers.rs`, `setup.rs`, `timing_equalization.rs`, `forgot_password.rs`, `sanitize.rs`).

## Session cookie

`session=<hex-token>; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800`

- 7-day TTL, `HttpOnly` (not JS-readable).
- `SameSite=Lax` deliberately, **not `Strict`** — `Strict` blocks the cookie on top-level form POSTs from external referrers, which breaks Seerr-style "click link to sign in" flows.
- `Secure` is decided per request by `cookie_secure_for`: forced by `RYOKAN_COOKIE_SECURE`, else inferred from `X-Forwarded-Proto: https` (leftmost hop) **only while `RYOKAN_TRUSTED_PROXY` is on**. Same rule as Sonarr's cookie auth (`CookieSecurePolicy.SameAsRequest` behind its Trusted Networks). Never inferred without proxy trust: any client can send the header, and a `Secure` cookie handed out over plain HTTP is never sent back, which locks the user out. Default off so `cargo run` on HTTP localhost works.
- The logout `Set-Cookie` uses `Max-Age=0` and **echoes the same `Secure` attribute as the set path** — some browsers refuse to clear a `Secure` cookie from a non-`Secure` response; the reverse is safe.
- Session tokens are hex-encoded random bytes from `rand`; the `sessions` table stores **the SHA-256 of the cookie value** (`models::session::token_hash`), never the value, so a copy of the database (a `ryokan.db.pre-restore-*` leftover, a readable file on a shared host) yields no working cookie. Every lookup hashes the presented cookie first. Rows from before the change were cleared once by the `sessions_hashed_v1` migration. The database files themselves are 0600 (`paths::make_db_private` at boot) and the Docker data dir 0700.

## Timing-equalized login

`models::user::authenticate` bcrypt-verifies against a warmed dummy hash (`DUMMY_BCRYPT_HASH`) on the missing-user path so failed logins take the same ~50ms as real ones. `main()` forces the `LazyLock` to initialize via `warm_timing_equalizer` at startup, otherwise the very first probe would be a one-shot timing oracle for username enumeration.

bcrypt cost is **10**. `models::user::hash_password` (behind `create_user` and `create_first_user`) pushes `bcrypt::hash` into `tokio::task::spawn_blocking` so the ~50ms CPU cost doesn't stall a runtime worker. The dummy hash on the authenticate path is pre-computed at the same cost so the equalizer comparison is apples-to-apples.

## CSRF (Origin-based, not token-based)

OWASP "Verifying Origin With Standard Headers" — `verify_same_origin` (and `verify_same_origin_with_trust`) prefers `Origin` (always set by browsers on unsafe methods, including cross-origin form submissions) and falls back to `Referer` when Origin is absent.

Acceptable-hosts set is built from the `Host` header plus, when `RYOKAN_TRUSTED_PROXY` is on, `X-Forwarded-Host` (covers reverse-proxy-terminates-the-public-host case where browser sends public host in Origin but backend sees proxy's internal host).

Two layers run the check:
- `require_auth` applies it to state-changing methods on authenticated routes.
- `csrf_public` wraps unauthenticated `/login` and `/setup` POSTs so those endpoints aren't cross-origin-forgeable either.

**Missing both Origin and Referer → reject.**

**Ports count when the Host header names one.** `SameSite` ignores ports, so another app on the same machine (`:8080` posting to Ryokan on `:8978`) gets the cookie sent; the Origin check is the only thing that tells them apart. `origin_matches` requires the Origin's port (the scheme default when omitted) to equal the Host header's, except that 80 and 443 match each other: browsers never write a default port into Host, so an explicit one comes from a proxy appending its listen port (`$host:$server_port`, :80 behind a TLS edge whose origin is 443), and it admits nothing a portless Host doesn't. A Host header without a port (a reverse proxy forwarding `$host`) and `X-Forwarded-Host` entries (proxies disagree on whether they carry their own port) compare hosts only. IPv6 literals are parsed by `split_authority`; splitting at the first `:` used to turn every one into `[`.

`handlers::security_headers` sends `Referrer-Policy: same-origin`. Keep it that way: under `no-referrer` browsers send `Origin: null` on every POST, and this check would reject all of them.

Safe methods (GET / HEAD / OPTIONS) skip the check, so nothing that changes state may be a GET. **`/logout` is a POST for this reason**: as a GET, any site could log the user out with a link, since `SameSite=Lax` sends the cookie on top-level navigations. `tests/sessions.rs` pins the cross-origin 403 and the `GET /logout` 405.

## Per-IP login throttle

In-memory `LOGIN_FAILURES: Mutex<HashMap<String, Vec<Instant>>>`, one bucket per username (`u:` + SHA-256 of the trimmed, lowercased name, so a 2 MB username costs 66 bytes) and one per client IP. `login_attempt` classifies and counts an attempt's buckets **as it starts**, under one lock, and a success clears its buckets; checking first and recording after the bcrypt await let a parallel burst all pass the check. Past the hard cap a bucket stops growing. Only an attempt every bucket allows creates a bucket; a throttled one gets no verdict, so it counts only against buckets that already exist, and a client sending a fresh username per request can't grow the map.

**Device cookies** (`models::login_device`, OWASP's lockout answer): a successful login or setup sets `ryokan_device` (random token, 400-day HttpOnly cookie; the table stores only its SHA-256). A login carrying a known device is throttled on that device's own bucket instead of the username's and the IP's, so someone else failing as `admin` can't lock out a browser that has logged in before, even behind a reverse proxy where every client shares one IP. A stranger's browser still faces both buckets. The lookup (`login_device::is_known`) is a read, since it runs before the throttle on every attempt; only a successful login marks the device used (`touch`). The hourly cleanup drops devices unused for 400 days.

`sweep_login_failures()` runs from the `cleanup` background task every hour and prunes expired timestamps so a probe storm can't grow the map unbounded.

Client IP comes from `client_ip_from_request()`, which honors `X-Forwarded-For` / `X-Real-IP` **only when `RYOKAN_TRUSTED_PROXY` is set**. Otherwise the TCP peer is ground truth — direct-exposure deploys can't be bypassed by header spoofing.

Usernames are passed through `sanitize_for_log()` (strip control chars, cap at 64 bytes) before embedding in log lines so a probe can't smuggle terminal escapes or multi-KB garbage into `tracing` output.

**API keys** share the map: `api_key_throttled` / `api_key_failed` keep a `key:<shim>:<ip>` bucket for wrong Sonarr / Radarr shim keys (`arr_auth::check_api_key`), one per shim since Seerr calls both from one address and a stale Radarr key must not lock out Sonarr, with the login window and soft cap; only failures count, and a throttled client gets 429 + `Retry-After: 60`. Those keys are whatever the user typed, so Settings refuses one under 20 characters (`MIN_SHIM_KEY_CHARS`). Log lines an unauthenticated client can repeat (a throttled login, a wrong scoped key) go through `logger::first_in_window`, one row per key per minute, since each is a database row. A throttled login is keyed on the buckets that tripped (fixed size), so a username guessed from many addresses is one line, and its line shows the client through `sanitize_for_log`.

**Cross-site GETs**: `refuse_cross_site_get` 403s a GET that does something (`/api/backup/download`, the OAuth `/start` routes) unless `Sec-Fetch-Site` is `same-origin` or `none` (typed, bookmarked) or absent (curl), since `SameSite=Lax` sends the session cookie on a top-level navigation. `same-site` is refused like `cross-site`: it ignores the port, so another app on the same host counts as same-site.

`LOGIN_FAILURES` deliberately uses `.lock().unwrap()` — security-adjacent state should crash-loop on programmer error, not silently continue with half-mutated state.

## `users_exist` first-run cache

`AppState.users_exist: Arc<AtomicBool>` is a flip-to-true-once cache so `require_auth` can skip a `SELECT COUNT(*) FROM users` on every protected request once setup is complete. `main.rs` primes it at boot, and `require_auth` promotes it the first time `has_users` reads true; setup itself never writes it.

## One admin, even under concurrent setup

`setup_submit` creates the account through `models::user::create_first_user`, an `INSERT ... SELECT ... WHERE NOT EXISTS (SELECT 1 FROM users)`, never `create_user`. Its `has_users` gate runs before the ~50ms bcrypt hash, so two submissions in flight at once both pass it; with a plain insert both landed and a second admin with a different username slipped in beside the first (`UNIQUE(username)` only catches a repeated name). The loser gets `Ok(None)`, an `Auth` warn line, and a redirect to `/login`. `create_user` stays for tests that seed users. `tests/setup.rs::concurrent_setup_posts_create_exactly_one_account` pins it.

## Sonarr/Radarr shim auth (out of scope)

The `sonarr_compat` / `radarr_compat` routers are merged **outside** the cookie-auth layer and use `arr_auth::check_api_key`. See `src/handlers/sonarr_compat/AGENTS.md` if it exists, otherwise `arr_auth.rs` directly.
