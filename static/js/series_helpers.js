// Shared helpers for the series page. Loaded BEFORE every other
// `series_*.js` file so the SD Proxy + escape/format helpers are
// guaranteed-available when the per-feature files run their own
// module-scope bootstrap code (the score-breakdown panel
// position-on-open IIFE in series_interactive_search.js, etc).
//
// Per CLAUDE.md "Per-page JS quirks under hx-boost": every top-level
// declaration is `var` (not `let` / `const`) because hx-boost re-
// executes this script on every body-swap nav-back. Top-level `let`
// / `const` would throw "redeclaration" SyntaxError on the second
// execution and silently kill the whole script.

// Per-series data lookup. Was previously a module-scope const that
// snapshotted `series-data`'s dataset at script-load time. That broke
// under body-wide hx-boost (PR 140): navigating from Series A to Series
// B swaps the body in place — and even though hx-boost re-runs this
// script on every nav-back, leaked `setInterval` callbacks (e.g. an
// older download-progress poller from a prior visit) close over the
// SD reference captured at THEIR script-load time, not the current
// dataset. With a `const` snapshot, those leaked callbacks would keep
// hitting Series A's API while the user was on Series B (cross-series
// grab history, wrong-target deletes, etc.).
//
// A Proxy that reads `document.getElementById('series-data')?.dataset`
// fresh on every property access keeps all the existing `SD.foo`
// callsites unchanged while making them safe regardless of which
// closure scope they were captured in. The element lookup is
// microsecond-fast and dataset returns a live DOMStringMap, so the
// performance cost is negligible compared to the network calls these
// values feed into. The poller leak itself is fixed in series.js by
// stashing the timer handle on `window`; this Proxy is defense-in-depth.
var SD = new Proxy({}, {
    get(_target, prop) {
        const el = document.getElementById('series-data');
        return el ? el.dataset[prop] : undefined;
    },
});

// `&by=anilist` for an `/api/series/${SD.id}/...` call when `SD.id` is
// an AniList id (the page of a series not in the library), else ''.
// Those endpoints read a bare id as a library id first, so an untracked
// page's search ran for whichever library series had that internal id.
// Append after a query string that already has a `?`.
function seriesIdKindParam() {
    return SD.idKind === 'anilist' ? '&by=anilist' : '';
}

// Title-language switching is handled entirely by CSS via the
// `html[data-title-language]` attribute set by the inline head script
// in base.html. No DOM walking here — doing it post-parse caused a
// visible flash of the english titles before they were swapped to
// romaji.

// HTML-escape via the textContent → innerHTML round-trip. Cheap, no
// allocation hot-path concerns at the call rates this page does
// (per-row table cells in modals, never per-keystroke). Used by every
// `series_*.js` file that renders user-controlled strings into
// innerHTML — release titles, group names, file paths, etc.
function escHtml(s) {
    // Quotes too: these strings land in attributes (`title="..."`), and
    // the old textContent -> innerHTML round trip left `"` alone, so a
    // release title could close the attribute and add a handler.
    if (s == null) return '';
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

// Per-file size renderer for grab-history rows + episode-detail "Size"
// column. Diverges from `formatSeasonSize` in series.js (which mirrors
// the server's `services::media::format_size` shape exactly so JS-driven
// updates look identical to a fresh render). This one is the older
// frontend convention used in the modal table; both end up reading
// "GiB" / "MiB" but with slightly different rounding rules and the
// difference is load-bearing for the snapshot tests on those rows.
// Don't unify them.
function formatBytes(bytes) {
    if (!bytes || bytes <= 0) return '';
    const gb = bytes / (1024 * 1024 * 1024);
    if (gb >= 1) return gb.toFixed(1) + ' GiB';
    const mb = bytes / (1024 * 1024);
    return Math.round(mb) + ' MiB';
}

