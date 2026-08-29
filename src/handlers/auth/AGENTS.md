# handlers/auth/AGENTS.md

Cookie-based sessions for the web UI. `require_auth` middleware on protected routes redirects to `/login`; first-run setup at `/setup` creates the admin account. Tests live in `tests/` (topic-split: `throttle.rs`, `csrf.rs`, `sessions.rs`, `proxy_headers.rs`, `setup.rs`, `timing_equalization.rs`, `forgot_password.rs`, `sanitize.rs`).

## Session cookie

`session=<hex-token>; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800`

- 7-day TTL, `HttpOnly` (not JS-readable).
- `SameSite=Lax` deliberately, **not `Strict`** — `Strict` blocks the cookie on top-level form POSTs from external referrers, which breaks Seerr-style "click link to sign in" flows.
- `Secure` is decided per request by `cookie_secure_for`: forced by `RYOKAN_COOKIE_SECURE`, else inferred from `X-Forwarded-Proto: https` (leftmost hop) **only while `RYOKAN_TRUSTED_PROXY` is on**. Same rule as Sonarr's cookie auth (`CookieSecurePolicy.SameAsRequest` behind its Trusted Networks). Never inferred without proxy trust: any client can send the header, and a `Secure` cookie handed out over plain HTTP is never sent back, which locks the user out. Default off so `cargo run` on HTTP localhost works.
- The logout `Set-Cookie` uses `Max-Age=0` and **echoes the same `Secure` attribute as the set path** — some browsers refuse to clear a `Secure` cookie from a non-`Secure` response; the reverse is safe.
- Session tokens are hex-encoded random bytes from `rand`, **stored verbatim in the `sessions` table** (no additional hashing — the cookie value *is* the DB lookup key, so a DB dump is already a session-hijack vector and re-hashing wouldn't change that threat model).

## Timing-equalized login

`models::user::authenticate` bcrypt-verifies against a warmed dummy hash (`DUMMY_BCRYPT_HASH`) on the missing-user path so failed logins take the same ~50ms as real ones. `main()` forces the `LazyLock` to initialize via `warm_timing_equalizer` at startup, otherwise the very first probe would be a one-shot timing oracle for username enumeration.

bcrypt cost is **10**. `models::user::register` pushes `bcrypt::hash` into `tokio::task::spawn_blocking` so the ~50ms CPU cost doesn't stall a runtime worker. The dummy hash on the authenticate path is pre-computed at the same cost so the equalizer comparison is apples-to-apples.

## CSRF (Origin-based, not token-based)

OWASP "Verifying Origin With Standard Headers" — `verify_same_origin` (and `verify_same_origin_with_trust`) prefers `Origin` (always set by browsers on unsafe methods, including cross-origin form submissions) and falls back to `Referer` when Origin is absent.

Acceptable-hosts set is built from the `Host` header plus, when `RYOKAN_TRUSTED_PROXY` is on, `X-Forwarded-Host` (covers reverse-proxy-terminates-the-public-host case where browser sends public host in Origin but backend sees proxy's internal host).

Two layers run the check:
- `require_auth` applies it to state-changing methods on authenticated routes.
- `csrf_public` wraps unauthenticated `/login` and `/setup` POSTs so those endpoints aren't cross-origin-forgeable either.

**Missing both Origin and Referer → reject.**

## Per-IP login throttle

In-memory `LOGIN_FAILURES: Mutex<HashMap<String, Vec<Instant>>>` keyed by client IP. Failed login attempts push timestamps; middleware rejects when the per-window count exceeds the cap.

`sweep_login_failures()` runs from the `cleanup` background task every hour and prunes expired timestamps so a probe storm can't grow the map unbounded.

Client IP comes from `client_ip_from_request()`, which honors `X-Forwarded-For` / `X-Real-IP` **only when `RYOKAN_TRUSTED_PROXY` is set**. Otherwise the TCP peer is ground truth — direct-exposure deploys can't be bypassed by header spoofing.

Usernames are passed through `sanitize_for_log()` (strip control chars, cap at 64 bytes) before embedding in log lines so a probe can't smuggle terminal escapes or multi-KB garbage into `tracing` output.

`LOGIN_FAILURES` deliberately uses `.lock().unwrap()` — security-adjacent state should crash-loop on programmer error, not silently continue with half-mutated state.

## `users_exist` first-run cache

`AppState.users_exist: Arc<AtomicBool>` is a flip-to-true-once cache so `require_auth` can skip a `SELECT COUNT(*) FROM users` on every protected request once setup is complete. `models::user::register` flips it after successful registration.

## Sonarr/Radarr shim auth (out of scope)

The `sonarr_compat` / `radarr_compat` routers are merged **outside** the cookie-auth layer and use `arr_auth::check_api_key`. See `src/handlers/sonarr_compat/AGENTS.md` if it exists, otherwise `arr_auth.rs` directly.
