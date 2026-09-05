# templates/AGENTS.md — Askama + HTMX

Askama 0.16 (Jinja2-like, compiled into the binary at build time via proc-macro). Templates live here; handlers call `template.render()` and wrap in `Html(...)` themselves — there's no `axum`-integration crate (Askama 0.13 merged with rinja and dropped per-framework wrappers).

**htmx 4.0.0** is vendored under `static/vendor/` (issue #130, migrated from 2.0.9). **Body-wide `<body hx-boost:inherited="true">` is active** — every plain `<a>` and `<form>` runs through htmx by default. No extensions are loaded: the progress toast streams over native `EventSource`, and the `<head>` never changes across a boosted nav (all CSS is bundled in `base.html`; htmx 4 core carries `<title>` on its own), so the 2.x SSE and head-support extensions are gone.

## htmx 4 rules (what changed from 2.x, and what pins it)

- **Inheritance is explicit.** An attribute reaches descendants only with the `:inherited` suffix (`hx-boost:inherited`). A bare `hx-target` on a form is the form's alone; a link inside it boosts to the body. Don't add `:inherited` near a form unless every descendant should really use it. `hx-disinherit` no longer exists.
- **Event names are colon-separated**: `htmx:after:swap`, `htmx:after:settle`, `htmx:after:request`, `htmx:response:error`, `htmx:config:request`, `htmx:after:init` (was `htmx:load`). In `hx-on` that is `hx-on::after:request` / `hx-on::response:error`; the 2.x kebab spellings bind to events that never fire. There is no `event.detail.successful`; read `event.detail.ctx.response.status`.
- **`htmx:after:swap` fires on the request's source element**, not on the target; when the source was swapped out htmx re-points the event at the connected target, and only if both are gone does it land on `document`. Section re-bind listeners compare `window.ryokanSwapTargetId(ev)` (`base.js`; reads `ev.detail.ctx.target.id`) instead of `ev.target.id`. `htmx:after:settle` still fires on the target.
- **`htmx:confirm` fires only when the request has a confirm.** The `data-ryokan-confirm-*` bridge in `base.js` therefore stamps `ctx.confirm` from an `htmx:config:request` listener for opted-in source elements, then the `htmx:confirm` listener `preventDefault()`s and calls `issueRequest()` / `dropRequest()`. No template needs `hx-confirm`. Forms htmx drives (any `hx-*` verb, or boosted) go through the bridge; only `hx-boost="false"` forms use the native submit listener, decided at submit time via `elt._htmx.boosted`.
- **Renamed attributes**: `hx-disable` now means "disable these elements during the request" (was `hx-disabled-elt`); "skip htmx processing" is `hx-ignore` (was `hx-disable`). `hx-ext` is gone.
- **`htmx.ajax` options**: `push: 'true'` as a string (was `pushUrl`); the option takes the `hx-push-url` vocabulary, so a boolean `true` is stringified and pushes a page literally named `/true` (the calendar's monitored toggle did this). `htmx.onLoad` still exists (fires on `htmx:after:process`); `htmx.process` and `htmx.trigger` are unchanged.
- **Config meta** in `base.html`: `noSwap: [204, 304, "4xx", "5xx"]` (htmx 4 would otherwise swap error bodies; handlers return plain-text errors on those statuses and rely on the HX-Trigger toast) and `defaultTimeout: 0` (htmx 4 aborts at 60s by default; interactive search can run longer). History refetches on back / forward by default, which is what the old `historyEnableCache: false` did.
- **Server contract unchanged**: `HX-Request`, `HX-Trigger`, `HX-Redirect`, `HX-Refresh`, `HX-Retarget` behave as before; `HX-Trigger-Name` is gone (Ryokan never read it). `hx-vals='js:…'`, `hx-target="closest tr"` / `this`, `hx-swap-oob`, `hx-include="closest form"`, and `hx-trigger="keyup changed delay:400ms"` all parse as before.
- **Pinned by** `tests/htmx_foundation.rs` (vendored bundle shape, body attribute, meta config, and a vocabulary guard that fails on any 2.x attribute / event name / `pushUrl` / `detail.successful` in templates or JS) and the browser-e2e suite (`hx_on_handlers_fire_under_htmx_4_event_names`, the `indexers_delete_confirm_removes_row` rebind check, the confirm-bridge delete tests).

## Boot order

`templates/base.html` loads the htmx core as `defer` *before* `static/js/page_lifecycle.js` and `static/js/base.js` so any code referencing the `htmx.*` global sees it on first paint. `base.js` guards its modal IIFEs, so a page without the modal markup (test fixtures) still gets the toast helpers and the listeners defined after them; the confirm bridge registers too but needs the modal markup to actually confirm anything.

Per-element `hx-boost="false"` opt-outs:
- `/logout` links in `base.html` (avoids a swap-then-redirect race against session-clear)
- download links (`system.html`, `partials/system/backup.html`): a boosted click would swap the attachment's bytes into the page
- the API-key create form (`partials/settings/api_keys.html`): a JS `submit` listener owns that form

`target="_blank"` links and in-page anchors are not opted out; htmx handles both natively.

## Partial-rendering convention

Handlers needing both full-page and fragment responses branch on `axum_htmx::HxRequest(is_htmx)`. Fragments live under:

- `templates/partials/<page>/<area>.html` (per-area, e.g. settings tab body)
- `templates/partials/<page>/<thing>_row.html` (single repeating row, e.g. an indexer row swapped after upsert)

Pure-fragment routes (no full-page equivalent — e.g. conditional-field swap on a select change) are new handlers only hit by `hx-*` requests.

**Progressive enhancement is preserved**: every form-POST handler keeps its non-HTMX path (form data → write → redirect) so the page works without JS. The HxRequest branch is the optimization, not the only path.

## Patterns load-bearing for new migrations

Tested via `tests/htmx_browser_e2e*.rs`.

- **`Form<T>`, not `Json<T>`**, on handler extractors. `hx-vals` + `hx-include="closest form"` form-encode by default. New handlers take `Form<T>` with `#[serde(default)]` on every field if `hx-include` may pull extras the handler doesn't care about — serde silently drops unknown fields.
- **Always-200 for inline-result swaps.** The `noSwap` config in `base.html` makes htmx *skip the swap on 4xx/5xx* (htmx 2's built-in policy, opted back in under htmx 4) — a handler returning 502 on connection-test failure leaves the spinner up forever. Pattern: `templates/partials/settings/connection_test_result.html` + `ConnectionTestResultPartial::into_html_ok()` — render success/failure into the same partial (different inline color), always 200. Inverse for row-removal swaps: 5xx is the right signal so htmx skips the swap and the row stays put for the user to retry.
- **`htmx:confirm` bridge** in `static/js/base.js` wires `data-ryokan-confirm-*` attrs to the in-app confirm modal *for htmx-driven forms (any `hx-*` verb, or boosted)*. Load-bearing because htmx's submit listener fires before any per-form `submit` listener could (registration order — htmx loads first), so a custom listener calling `preventDefault()` runs after the AJAX is already in flight. **Only `hx-boost="false"` forms use the per-form submit-intercept pattern; under body-wide boost every other form is htmx-driven and goes through the bridge.** Adding a confirm modal to a new HTMX form is just adding the `data-ryokan-confirm-*` attrs.
- **`HX-Refresh: true`** for full-page reload after a state change a per-row swap can't represent. CF delete returns this when the table goes empty so the empty-state CTA renders (lives outside the `{% for %}` loop, can't be swapped in by per-row `outerHTML`). Don't overuse — full reload is heavy; per-row swap is default.
- **Wire legacy form fields through hidden inputs** even after they no longer drive runtime behavior. A user with a stale tab will blank them on save otherwise. Pattern in `handlers/settings/mod.rs::settings_submit` for `qbit_url` / `qbit_user` / `qbit_pass`.
- **`htmx_aware_redirect` for any `Redirect::to`.** Under hx-boost, htmx follows 3xx via `fetch` and inline-swaps the destination's HTML into the source page's `hx-target` — producing nested-page renders (a Settings response inside the prior page's body). Helper at `src/handlers/responses.rs` returns `200 OK` + `HX-Redirect` for HTMX callers (htmx triggers a real `window.location` nav) and a standard 303 for plain callers. `htmx_aware_redirect_from_req(req, url)` is the middleware-friendly variant. **`tests/htmx_redirect_audit.rs` is a CI-enforced lint** — every `Redirect::to` must route through the helper, sit inside `if !is_htmx { ... }`, or be in the documented exceptions table.

## Per-page JS lifecycle (`static/js/page_lifecycle.js`)

Module-scope `setInterval` started once at initial document load runs forever and accumulates copies on every boosted re-entry. The lifecycle helper exposes:

```js
ryokanRegisterPageInit(name, { check, mount, unmount });
```

Each registration runs `check()` on every `htmx.onLoad` firing, calls `mount()` when the page becomes active and `unmount()` when it leaves. Try/catch wraps each so a throwing registration can't break siblings.

**Use this for any new page that starts a poller or registers global listeners.** The legacy `if (document.getElementById('foo')) setInterval(...)` shape leaks under boost.

## Per-page `<script>` placement

**Per-page `<script>` tags belong in `{% block page_js %}`, not `{% block content %}`.**

Per-page scripts call `window.ryokanRegisterPageInit` / `ryokanProgressToast` / `ryokanToast` defined in `page_lifecycle.js` + `base.js`. With `defer`, scripts execute in DOM-tree order — scripts inside `{% block content %}` run *before* base.html's bottom-of-body scripts, so the per-page script runs before its dependencies are defined → TypeError → silent script abort.

Boost-nav users don't notice because the helpers stay loaded from a prior page; only direct-URL loads hit the bug. The `{% block page_js %}` placeholder is at end-of-body after `base.js`, so per-page scripts render LAST.

## Links inside forms that carry `hx-target`

Under htmx 2's implicit inheritance a plain `<a href>` inside a form with `hx-target="#some-region" hx-swap="outerHTML"` (the per-tab Settings subforms) was boosted **using the form's target and swap**, rendering the destination page nested inside that region with two sidebars overlapping. htmx 4 inherits only through `:inherited`, so this can't happen unless someone adds `hx-target:inherited` to a form; don't. The per-link `hx-boost="false"` opt-outs (download links, `/logout`, the API-key create form) are listed under Boot order.

## Per-page JS quirks under hx-boost

- **Use `var` at module scope, not `let` / `const`.** Top-level `let` / `const` throws "redeclaration" SyntaxError on body-swap re-execute.
- **Module-scope DOM snapshots go stale** after a body swap. Cache via a Proxy or re-query inside the handler.

## CSS gotcha: `background:` shorthand on `<select>`

`background:` shorthand resets every `background-*` longhand it doesn't mention, including `background-image`. **Never use it on `<select>` or anything that inherits select-styling.**

`forms.css` has a global `select { appearance: none; background-image: <SVG chevron>; }` rule that paints a CSS chevron in place of the native one (Firefox under hx-boost drops the native chevron after a body-swap; CSS-painted survives). Per-element rules like `.folder-select { background: var(--bg) }` silently clobber the chevron.

Use `background-color: var(--bg)` (longhand) instead. The global `select` rule has `!important` on its `background-*` properties as defense-in-depth, but that's a guard, not a license.

## HX-Trigger payloads must be ASCII

Non-ASCII bytes mojibake into Latin-1 (em-dash → `â\u{80}\u{94}`). Use ASCII punctuation in HX-Trigger JSON envelopes.

## Toast helpers (defined in `static/js/base.js`)

- `ryokanToast({ title, body, kind, category, sticky, duration, busy, log, actions })` — one options object; `kind` is `info` | `success` | `warn` | `error` (anything else coerces to `info`), `sticky: true` disables auto-dismiss, `busy: true` shows a spinner next to the title (`update({busy: false})` or `finalize()` hides it), `log: false` skips the System → Logs write.
- `ryokanProgressToast({ progressId, title, body, kind, category, onTerminal })` — sticky, busy toast driven by the `/api/progress/{id}` stream; throws without `progressId`, and `finalize()` on the returned handle turns it into a normal auto-dismissing toast.
- **Toasts follow the user across pages.** The live set lives on `window.__ryokanToastRuntime`, which survives base.js re-executing on a boosted swap: the toast elements are re-appended to the new stack with their timers, action buttons, and progress followers intact (the old follower keeps its EventSource; nothing re-subscribes). A full page load rebuilds the set from the `ryokanLiveToasts` sessionStorage record: transient toasts come back with their remaining time, progress toasts re-attach to their job by id (the stream replays the buffer from the start), action buttons are dropped. Consequences: a toast is never "gone with the page", so a page-specific `onTerminal` must tolerate firing on a different page (it does not survive a full load at all), and `ryokanQueueToast` is only needed for a toast fired in the same tick as a `location.reload()` / `location.href` assignment.

## XSS surface

Anything rendered with `|safe` must have been round-tripped through `services::html::{escape, sanitize}` (built on `ammonia`) or it's an XSS vector. AniList descriptions, Nyaa description bodies, and any user-controlled string go through sanitize.
