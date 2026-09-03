// ── localStorage search options persistence (always-present block) ─────
//
// Mounted via `ryokanRegisterPageInit` so the prefill + form-submit
// listener fire AFTER htmx commits the body swap. Pre-fix this was
// a bare IIFE that ran at script-load; under boost the script
// could finish loading before `search-form` was committed (per the
// relations-carousel diagnosis), the form lookup returned null,
// and the localStorage save-on-submit listener never bound → the
// user's category/filter choice didn't persist across boost-
// nav-driven searches until F5.
//
// Persists only the dropdown-shaped knobs (category, filter): those
// are global preferences the user sets once and reuses. The
// uploader field deliberately stays OUT of the persistence list —
// it's a per-search context, not a global preference, and stickying
// `SubsPlease` (or whatever) across every future search confused
// users on docker-fresh installs into thinking Ryokan was forcing
// the value. Per-series uploader overrides on the series page are
// the right surface for "always use this uploader for this show."
var bindSearchOptionsPersistence = function () {
    const fields = ['search-category', 'search-filter'];
    const KEY = 'nyaa_search_opts';
    const form = document.getElementById('search-form');
    if (!form) return;
    if (form.dataset.ryokanOptsBound === '1') return;
    form.dataset.ryokanOptsBound = '1';
    const saved = JSON.parse(localStorage.getItem(KEY) || '{}');
    // One-shot cleanup for users with the legacy `search-user` key
    // already in localStorage from an older Ryokan version that
    // persisted the uploader field. Without this, the stale value
    // would sit in localStorage forever (the rewrite below only
    // writes the current `fields` list, but legacy keys stick
    // around since `setItem` writes the whole object). Strip on
    // first visit post-upgrade so the value doesn't keep
    // reappearing if any future code path ever reads the raw
    // localStorage entry.
    if ('search-user' in saved) {
        delete saved['search-user'];
        localStorage.setItem(KEY, JSON.stringify(saved));
    }
    for (const id of fields) {
        if (saved[id] !== undefined && saved[id] !== '') {
            const el = document.getElementById(id);
            if (el) el.value = saved[id];
        }
    }
    form.addEventListener('submit', function () {
        const opts = {};
        for (const id of fields) {
            const el = document.getElementById(id);
            if (el) opts[id] = el.value;
        }
        localStorage.setItem(KEY, JSON.stringify(opts));
    });
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('search-options-persistence', {
        check: function () { return !!document.getElementById('search-form'); },
        mount: bindSearchOptionsPersistence,
    });
} else {
    bindSearchOptionsPersistence();
}

// ── Results-present block (load-more + grab) ────────────────────────────
//
// The original template rendered this block only when {% if searched %},
// and initialized `hasMore` / `totalResults` via Askama-templated number
// literals. The extracted version is always loaded; it reads the same
// server state from `window.searchState` (set inline in the template
// right before this file loads) and gates execution on the presence of
// the `#results-body` element.

var searchState = window.searchState || { hasMore: false, totalResults: 0, searched: false };
var nextPage = 2;
var hasMore = !!searchState.hasMore;
var totalResults = Number(searchState.totalResults) || 0;

// Handle prefill from library "Search Nyaa" button. Mounted via
// `ryokanRegisterPageInit` so the form lookup happens after htmx
// commits the swap. Pre-fix a deep link with `?prefill=Q` would
// silently no-op on boost-nav (the IIFE ran before `search-form`
// was in DOM), and only F5 fired the auto-search.
var bindSearchPrefill = function () {
    const params = new URLSearchParams(window.location.search);
    const prefill = params.get('prefill');
    if (!prefill) return;
    const form = document.getElementById('search-form');
    if (!form) return;
    if (form.dataset.ryokanPrefillBound === '1') return;
    form.dataset.ryokanPrefillBound = '1';
    const input = document.getElementById('search-query');
    if (input) input.value = prefill;
    if (!searchState.searched) {
        // Auto-submit so results load immediately.
        form.submit();
    }
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('search-prefill', {
        check: function () { return !!document.getElementById('search-form'); },
        mount: bindSearchPrefill,
    });
} else {
    bindSearchPrefill();
}

function getSearchParams() {
    return new URLSearchParams({
        query: document.getElementById('search-query').value,
        category: document.getElementById('search-category').value,
        filter: document.getElementById('search-filter').value,
        // Form field name is `uploader` (NOT `user`) to dodge
        // browser autofill heuristics that pool `name="user"`
        // across sites. The Rust `SearchForm` deserializes
        // `uploader`; Nyaa's URL param `?u=` is built server-side.
        uploader: document.getElementById('search-user').value,
    });
}

function loadMore() {
    if (!hasMore) return;

    const btn = document.getElementById('load-more-btn');
    const status = document.getElementById('load-more-status');
    btn.disabled = true;
    btn.textContent = `Loading page ${nextPage}...`;

    const params = getSearchParams();
    params.set('p', nextPage);

    fetch(`/api/search/page?${params}`)
        .then(r => r.json())
        .then(data => {
            const results = data.results || [];
            hasMore = data.has_next;

            if (results.length === 0) {
                hasMore = false;
                document.getElementById('load-more-area').style.display = 'none';
                status.textContent = `All ${totalResults} results loaded`;
                return;
            }

            const tbody = document.getElementById('results-body');
            const cards = document.getElementById('results-cards');
            for (const r of results) {
                let rowClass = '';
                if (r.is_batch && r.is_trusted) rowClass = 'is-batch is-trusted';
                else if (r.is_batch) rowClass = 'is-batch';
                else if (r.is_trusted) rowClass = 'is-trusted';

                let scoreClass = r.score >= 60 ? 'score-high' : r.score >= 30 ? 'score-mid' : 'score-low';
                let tags = '';
                if (r.is_batch) tags += '<span class="tag tag-batch">BATCH</span>';
                if (r.is_trusted) tags += '<span class="tag tag-trusted">TRUSTED</span>';
                if (r.group) tags += `<span class="tag tag-group">${escHtml(r.group)}</span>`;
                if (r.resolution) tags += `<span class="tag tag-res">${escHtml(r.resolution)}p</span>`;

                const grabUrl = r.magnet || r.torrent || '';
                const grabBtn = grabUrl ? `<button class="btn btn-grab" onclick="grabRelease('${escAttr(grabUrl)}', this)">Grab</button>` : '';

                const scoreBreakdownHtml = renderScoreBreakdown(r);
                // Mirror the server-rendered shape in templates/search.html:
                // a `[data-utc]` span carrying the raw UTC string. The
                // page-load + paginated-load passes through
                // `renderLocalDates()` to overwrite the textContent with
                // local time and `title` with the UTC marker. No
                // `data-ts` — the global relative-time renderer in
                // base.js otherwise mixes "5d ago" / absolute date.
                const dateCell = r.upload_date
                    ? `<span data-utc="${escAttr(r.upload_date)}">${escHtml(r.upload_date)}</span>`
                    : '—';

                // Table row (desktop).
                const tr = document.createElement('tr');
                if (rowClass) tr.className = rowClass;
                // data-* attrs mirror the server-rendered rows so the
                // client-side column sort picks up paginated rows too.
                tr.dataset.score = r.score;
                tr.dataset.name = r.title;
                tr.dataset.size = r.size_bytes;
                tr.dataset.sizeHuman = r.size;
                tr.dataset.date = r.upload_date || '';
                tr.dataset.seeders = r.seeders;
                tr.dataset.leechers = r.leechers;
                tr.dataset.downloads = r.downloads;
                tr.dataset.infoHash = r.info_hash || '';
                tr.dataset.group = r.group || '';
                if (r.indexer_id != null) tr.dataset.indexerId = r.indexer_id;
                tr.innerHTML = `
                    <td class="col-score">
                        <details class="score-details" name="score-breakdown">
                            <summary class="score-badge ${scoreClass}" title="Score breakdown">${r.score}</summary>
                            ${scoreBreakdownHtml}
                        </details>
                    </td>
                    <td class="col-name">
                        <a href="${escAttr(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>
                        <div class="result-tags">${tags}</div>
                    </td>
                    <td class="col-size">${escHtml(r.size)}</td>
                    <td class="col-date">${dateCell}</td>
                    <td class="col-seed"><span class="seed-count">${r.seeders}</span></td>
                    <td class="col-leech"><span class="leech-count">${r.leechers}</span></td>
                    <td class="col-dl"><span class="dl-count">${r.downloads}</span></td>
                    <td class="col-actions">${grabBtn}</td>
                `;
                tbody.appendChild(tr);

                // Card (mobile). Same data, different shape. Hidden above
                // --bp-phone via CSS; loadMore keeps both in sync so a
                // viewport resize post-load still renders correctly.
                if (cards) {
                    const card = document.createElement('div');
                    card.className = `result-card${rowClass ? ' ' + rowClass : ''}`;
                    card.dataset.name = r.title;
                    card.dataset.sizeHuman = r.size;
                    card.dataset.seeders = r.seeders;
                    card.dataset.infoHash = r.info_hash || '';
                    card.dataset.group = r.group || '';
                    if (r.indexer_id != null) card.dataset.indexerId = r.indexer_id;
                    card.innerHTML = `
                        <div class="result-card-header">
                            <details class="score-details" name="score-breakdown">
                                <summary class="score-badge ${scoreClass}" title="Score breakdown">${r.score}</summary>
                                ${scoreBreakdownHtml}
                            </details>
                            <a class="result-card-title" href="${escAttr(r.link)}" target="_blank" rel="noopener">${escHtml(r.title)}</a>
                        </div>
                        <div class="result-card-tags">${tags}</div>
                        <div class="result-card-footer">
                            <span class="result-card-meta">${escHtml(r.size)}</span>
                            <span class="result-card-meta"><span class="seed-count">${r.seeders}</span> S</span>
                            <span class="result-card-meta"><span class="leech-count">${r.leechers}</span> L</span>
                            ${grabBtn}
                        </div>
                    `;
                    cards.appendChild(card);
                }
            }

            // Render any new `[data-utc]` cells (date-only text,
            // full UTC datetime on hover). The page-1 render is
            // handled by the DOMContentLoaded pass; this is the
            // paginated-append pass.
            renderUploadDates();

            totalResults += results.length;
            nextPage++;
            document.getElementById('results-count').textContent = `${totalResults} results`;
            status.textContent = `${totalResults} results total`;

            if (hasMore) {
                btn.disabled = false;
                btn.textContent = `Load page ${nextPage}`;
            } else {
                document.getElementById('load-more-area').style.display = 'none';
                status.textContent = `All ${totalResults} results loaded`;
            }
        })
        .catch(err => {
            btn.disabled = false;
            btn.textContent = `Load page ${nextPage}`;
            console.error('Load more failed:', err);
        });
}

function grabRelease(url, btn) {
    // Pull the row's data-* attributes so the backend can link the
    // grab to a library series (#6d) and so we can feed the file-
    // picker modal a useful header when we route that way. Falls
    // back to a URL-only grab when the button wasn't mounted inside
    // a result row — e.g. a caller from a different template.
    const row = btn.closest('tr[data-score]') || btn.closest('.result-card');
    const isBatch = !!(row && row.classList.contains('is-batch'));

    // Issue #83 — batch releases open the interactive file picker
    // unless the user's `grab_preview_mode` config is `never`, in
    // which case the Grab button takes the direct /api/grab path
    // for 1.3.0-style one-click behavior. Single-file releases
    // always bypass the picker (nothing to pick). If the picker
    // isn't available on this page (no modal DOM, no info_hash
    // on the row), fall through to the direct grab so the button
    // keeps working.
    const previewMode = (window.searchState && window.searchState.grabPreviewMode) || 'batches_only';
    const canPicker = isBatch
        && previewMode !== 'never'
        && typeof window.openGrabPicker === 'function'
        && row && row.dataset.infoHash;
    if (canPicker) {
        // Capture the row's hash + URL for the post-picker Cancel
        // wire-up. The picker confirms via `/api/grab/confirm` (not
        // `/api/grab`), so the post-`/api/grab` Cancel logic below
        // wouldn't otherwise fire — we hand `onConfirm` to the
        // picker so the original row's Grab button still flips into
        // Cancel state once a batch grab lands. For BT releases
        // `row.dataset.infoHash` IS the canonical id qBit knows the
        // torrent by, so cancel via /api/downloads/delete with this
        // hash works the same as the direct-grab path.
        const cancelHashFromRow = row && row.dataset.infoHash || '';
        const grabUrlFromRow = url;
        window.openGrabPicker(url, {
            title: row.dataset.name || '',
            size: row.dataset.sizeHuman || '',
            seeders: Number(row.dataset.seeders) || 0,
            group: row.dataset.group || '',
            infoHash: row.dataset.infoHash || '',
            // Carry the search-hit's batch classification through to
            // the preview POST so the backend's grab-row write uses
            // the listing's flag instead of a file-count proxy (which
            // mis-flags .mkv+.ass+.srt single-episode releases).
            isBatch: isBatch,
            onConfirm: function () {
                if (!cancelHashFromRow) return;
                flipGrabButtonToCancel(btn, cancelHashFromRow, grabUrlFromRow);
            },
        });
        return;
    }

    btn.disabled = true;
    btn.textContent = '...';
    const payload = {url: url};
    if (row) {
        if (row.dataset.name) payload.title = row.dataset.name;
        if (row.dataset.infoHash) payload.info_hash = row.dataset.infoHash;
        payload.is_batch = isBatch;
        // Multi-client routing — round-trip the result's indexer_id so
        // the backend can dispatch through the per-indexer pin. Falls
        // through to the Nyaa pin server-side when absent.
        if (row.dataset.indexerId) payload.indexer_id = Number(row.dataset.indexerId);
    }
    fetch('/api/grab', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(payload),
    })
    .then(resp => resp.ok ? resp.json().then(body => ({ok: true, body})) : resp.text().then(text => ({ok: false, body: text})))
    .then(result => {
        // Outcome → button copy + toast. The `link_status` tag comes
        // from the `LibraryLinkOutcome::tag()` impl in
        // `services::library_link.rs`; new tags there MUST be added
        // here too or the toast falls through to the generic "Sent"
        // copy. Series title (when applicable) is rendered into the
        // toast body so the user sees what got linked or added.
        if (!result.ok) {
            btn.textContent = 'Error';
            btn.classList.add('btn-error');
            if (window.ryokanToast) {
                window.ryokanToast({
                    kind: 'error',
                    title: 'Grab failed',
                    body: typeof result.body === 'string' ? result.body : 'Download client rejected the request.',
                    category: 'grab',
                });
            }
            return;
        }
        const {link_status, series_title, detail, hash} = result.body || {};
        // Convert the Grab button into a Cancel button so users can
        // unwind a manual-search grab without leaving the page. The
        // canonical hash from the response is preferred over the
        // row's pre-add `data-info-hash` because BT hashes happen to
        // be the same shape but SAB grabs return an `nzo_id` that
        // wouldn't match the row's hash.
        const cancelHash = hash || (row && row.dataset.infoHash) || '';
        if (cancelHash) {
            flipGrabButtonToCancel(btn, cancelHash, url);
        } else {
            // Empty canonical id (rare — magnet add that returned
            // nothing). Fall back to the legacy "Sent" lock so the
            // button isn't clickable into a useless cancel call.
            btn.textContent = 'Sent';
            btn.classList.add('btn-success');
        }
        if (!window.ryokanToast) return;
        if (link_status === 'linked' && series_title) {
            window.ryokanToast({
                kind: 'success',
                title: 'Grabbed',
                body: 'Linked to ' + series_title,
                category: 'grab',
            });
        } else if (link_status === 'added' && series_title) {
            window.ryokanToast({
                kind: 'success',
                title: 'Grabbed and added to library',
                body: series_title,
                category: 'grab',
            });
        } else if (link_status === 'auto_add_disabled') {
            window.ryokanToast({
                kind: 'warn',
                title: 'Grabbed (no library link)',
                body: detail || 'Auto-add toggle is off in Settings.',
                category: 'grab',
            });
        } else if (link_status === 'ambiguous') {
            window.ryokanToast({
                kind: 'warn',
                title: 'Grabbed (no library link)',
                body: detail || 'AniList match was ambiguous.',
                category: 'grab',
            });
        } else if (link_status === 'detail_fetch_failed') {
            window.ryokanToast({
                kind: 'warn',
                title: 'Grabbed (link pending)',
                body: detail || 'AniList match found but detail fetch failed; will retry on next sync.',
                category: 'grab',
            });
        } else if (link_status === 'no_match') {
            window.ryokanToast({
                kind: 'warn',
                title: 'Grabbed (no library link)',
                body: detail || 'No library or AniList match.',
                category: 'grab',
            });
        } else {
            // 'not_attempted' (no title/hash on the form) — bare grab.
            window.ryokanToast({
                kind: 'success',
                title: 'Grabbed',
                category: 'grab',
            });
        }
    })
    .catch(() => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        if (window.ryokanToast) {
            window.ryokanToast({
                kind: 'error',
                title: 'Grab failed',
                body: 'Network error.',
                category: 'grab',
            });
        }
    });
}

// Convert a Grab button into the post-grab Cancel state. Used by
// both the direct `/api/grab` path and the picker `onConfirm`
// callback so a search-row Grab button gets the same Cancel UX
// regardless of which grab pipeline served the request. Captures
// the original URL on the button so the post-cancel reset can
// rewire a fresh "Grab" click — assigning `btn.onclick` clobbers
// the inline `onclick="grabRelease(...)"` from the template.
//
// **Re-render assumption:** this works because the search results
// table appends new rows on `loadMore()` paginated load but never
// re-renders existing ones; the swapped-in onclick survives for the
// life of the row. If a future change ever re-renders existing
// rows in place (e.g. live-update of seed counts via SSE), the
// post-grab Cancel state would silently revert to a fresh Grab
// button mid-operation. Re-binding the onclick from a row-mutation
// observer would fix that — but until such a change lands, the
// simpler shape is fine. `cancelGrabbedRelease` correctly re-wires
// `onclick` back to `grabRelease` after a successful cancel, so
// the round-trip is intact.
function flipGrabButtonToCancel(btn, cancelHash, grabUrl) {
    btn.disabled = false;
    btn.classList.remove('btn-success');
    btn.classList.add('btn-cancel');
    btn.textContent = 'Cancel';
    btn.dataset.cancelHash = cancelHash;
    btn.dataset.grabUrl = grabUrl || '';
    btn.onclick = function () { cancelGrabbedRelease(cancelHash, btn); };
}

// Cancel a release the user just grabbed via the search-page Grab
// button. Hits the same endpoint the Downloads-page delete button
// uses (`/api/downloads/delete`), routed by hash. The handler's
// `resolve_client_for_hash` will dispatch to whichever client the
// grab landed in (BT default, SAB by `SABnzbd_nzo_` prefix, or the
// per-grab `download_client_id` stamp written when the grab linked
// to a library series). The confirm modal includes a checkbox for
// removing files; for an in-flight torrent / queued NZB the file
// barely exists yet so the default (off) is the safe pick — a
// re-grab of the same release re-uses the same path.
function cancelGrabbedRelease(hash, btn) {
    if (!hash) return;
    const doDelete = function (deleteFiles) {
        btn.disabled = true;
        btn.textContent = 'Cancelling...';
        fetch('/api/downloads/delete', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({hash: hash, delete_files: !!deleteFiles}),
        })
        .then(function (resp) {
            if (!resp.ok) throw new Error('cancel failed');
            // Reset the button so the user can re-grab if they want.
            // Re-wiring `onclick` is necessary because the conversion
            // to Cancel replaced the inline `onclick="grabRelease(..)"`;
            // setting it to null would leave the button inert.
            const grabUrl = btn.dataset.grabUrl || '';
            btn.disabled = false;
            btn.textContent = 'Grab';
            btn.classList.remove('btn-cancel', 'btn-success', 'btn-error');
            btn.onclick = function () { grabRelease(grabUrl, btn); };
            delete btn.dataset.cancelHash;
            if (window.ryokanToast) {
                window.ryokanToast({
                    kind: 'success',
                    title: 'Cancelled',
                    body: 'Removed from download client',
                    category: 'grab',
                });
            }
        })
        .catch(function () {
            btn.disabled = false;
            btn.textContent = 'Cancel';
            if (window.ryokanToast) {
                window.ryokanToast({
                    kind: 'error',
                    title: 'Cancel failed',
                    body: 'Could not remove from download client. Try the Downloads page.',
                    category: 'grab',
                });
            }
        });
    };
    if (window.ryokanConfirm) {
        window.ryokanConfirm({
            title: 'Cancel grab',
            body: 'Remove this release from the download client?',
            yesLabel: 'Cancel grab',
            noLabel: 'Keep',
            extras: [{id: 'deleteFiles', label: 'Also delete downloaded files', default: false}],
        }).then(function (res) {
            if (!res.ok) return;
            doDelete(res.extras && res.extras.deleteFiles);
        });
    } else {
        // Fallback for the (impossible-in-practice) case where base.js
        // hasn't loaded — behave like a plain confirm dialog so the
        // button still works.
        if (window.confirm('Remove this release from the download client?')) {
            doDelete(false);
        }
    }
}

function escHtml(s) {
    const d = document.createElement('div');
    d.textContent = s == null ? '' : s;
    return d.innerHTML;
}

function escAttr(s) {
    return String(s == null ? '' : s).replace(/'/g, "\\'").replace(/"/g, '&quot;');
}

// Build the <div class="score-components"> panel content for a result,
// matching the server-rendered shape in templates/search.html so a row
// appended via loadMore() behaves identically to a page-1 row. Keeping
// the two paths in lockstep is load-bearing for the sort+expand UX.
function renderScoreBreakdown(r) {
    const parts = r.score_breakdown || [];
    let inner;
    if (parts.length === 0) {
        inner = `<div class="form-hint">No components fired (score stayed at 0).</div>`;
    } else {
        const lis = parts.map(function (c) {
            const deltaClass = c.delta > 0 ? 'sc-delta-pos' : 'sc-delta-neg';
            const sign = c.delta > 0 ? '+' : '';
            const detail = c.detail
                ? `<span class="sc-detail">${escHtml(c.detail)}</span>`
                : '';
            return `<li>
                <span class="sc-delta ${deltaClass}">${sign}${c.delta}</span>
                <span class="sc-label">${escHtml(c.label)}</span>
                ${detail}
            </li>`;
        }).join('');
        inner = `<ul>${lis}</ul>
            <div class="form-hint">CF contributions shown here are evaluated against the release's classification alone. SeaDex-based CFs need a tracked AniList series to resolve, so they never fire on the manual search page; open the series page's interactive search for the full breakdown.</div>`;
    }
    return `<div class="score-components">
        <div class="score-components-title">Base score breakdown</div>
        ${inner}
    </div>`;
}

// v1.3.0 — close any open <details class="score-details"> when the user
// clicks outside it or presses Escape. Without these, the only way to
// dismiss the expander is to click the score badge itself, which is a
// footgun on the mobile card layout where the score sits in a small
// target at the card's top-left corner.
//
// Scroll-only edge handling: when the panel opens near the viewport
// edge we apply `position: fixed` with a viewport-aware `max-height` +
// internal `overflow-y: auto` so long breakdowns scroll inside the
// panel instead of falling off-screen. Width is capped to the viewport
// so mobile layouts don't overflow horizontally either. No flip-above
// logic — one direction is easier to reason about and predictable for
// both keyboard and touch users.
(function () {
    if (window.__ryokanSearchScoreBreakdownInit) return;
    window.__ryokanSearchScoreBreakdownInit = true;
    function closeAllOpenBreakdowns(except) {
        document.querySelectorAll('details.score-details[open]').forEach(function (d) {
            if (d !== except) d.removeAttribute('open');
            const panel = d.querySelector('.score-components');
            if (panel && d !== except) resetPanelPosition(panel);
        });
    }
    function resetPanelPosition(panel) {
        panel.style.position = '';
        panel.style.top = '';
        panel.style.left = '';
        panel.style.width = '';
        panel.style.minWidth = '';
        panel.style.maxWidth = '';
        panel.style.maxHeight = '';
        panel.style.overflowY = '';
    }
    function positionPanel(details) {
        const panel = details.querySelector('.score-components');
        if (!panel) return;
        // Only lift the panel to fixed-positioning when it lives
        // inside an overflow-clipping ancestor (e.g. the interactive-
        // search modal). On the plain /search page the results table
        // is a direct descendant of the viewport, so CSS
        // `position: absolute; top: calc(100% + 6px); left: 0` anchored
        // to the `.score-details` summary is the correct placement —
        // it rides scroll with the row and keeps the CSS max-width
        // cap intact. An earlier blanket fixed-positioning here
        // stretched the panel to the full viewport width and left it
        // floating in place after a scroll.
        let clipped = false;
        let node = details.parentElement;
        while (node && node !== document.body) {
            const cs = window.getComputedStyle(node);
            if (cs.overflow !== 'visible' || cs.overflowX !== 'visible' || cs.overflowY !== 'visible') {
                clipped = true;
                break;
            }
            node = node.parentElement;
        }
        if (!clipped) {
            resetPanelPosition(panel);
            return;
        }

        const GAP = 6;
        const MARGIN = 8;
        const rect = details.getBoundingClientRect();
        const vw = window.innerWidth;
        const vh = window.innerHeight;

        const top = rect.bottom + GAP;
        const maxHeight = Math.max(120, vh - top - MARGIN);
        const maxWidth = Math.max(240, vw - 2 * MARGIN);
        const desiredWidth = Math.min(360, maxWidth);
        let left = rect.left;
        if (left + desiredWidth + MARGIN > vw) {
            left = Math.max(MARGIN, vw - desiredWidth - MARGIN);
        }
        if (left < MARGIN) left = MARGIN;

        panel.style.position = 'fixed';
        panel.style.top = top + 'px';
        panel.style.left = left + 'px';
        panel.style.minWidth = '240px';
        panel.style.maxWidth = maxWidth + 'px';
        panel.style.maxHeight = maxHeight + 'px';
        panel.style.overflowY = 'auto';
    }
    document.addEventListener('click', function (evt) {
        const inside = evt.target.closest('details.score-details');
        closeAllOpenBreakdowns(inside);
    });
    document.addEventListener('keydown', function (evt) {
        if (evt.key === 'Escape') {
            closeAllOpenBreakdowns(null);
        }
    });
    // `toggle` doesn't bubble, so we capture it.
    document.addEventListener('toggle', function (evt) {
        const d = evt.target;
        if (!(d instanceof HTMLDetailsElement)) return;
        if (!d.classList.contains('score-details')) return;
        if (d.open) positionPanel(d);
        else {
            const panel = d.querySelector('.score-components');
            if (panel) resetPanelPosition(panel);
        }
    }, true);
})();

// #6a — click-to-sort columns on the results table. Each row carries
// data-* attributes populated server-side (data-score, data-name,
// data-size, data-date, data-seeders, data-leechers, data-downloads).
// Clicking a sortable header toggles between asc/desc and re-orders
// the tbody rows in place. State is purely client-side — no URL
// params, no server round-trip.
(function () {
    function parseValue(raw, key) {
        if (raw == null) return null;
        // Numeric columns.
        if (key === 'score' || key === 'size' || key === 'seeders' || key === 'leechers' || key === 'downloads') {
            const n = parseFloat(raw);
            return isNaN(n) ? 0 : n;
        }
        // Date is sortable as a string in "YYYY-MM-DD HH:MM" shape;
        // empty → sort last.
        if (key === 'date') {
            return raw || '';
        }
        // Name — case-insensitive string compare.
        return String(raw).toLowerCase();
    }

    function sortRows(tbody, key, dir) {
        const rows = Array.from(tbody.children);
        const sign = dir === 'asc' ? 1 : -1;
        rows.sort(function (a, b) {
            const av = parseValue(a.dataset[key], key);
            const bv = parseValue(b.dataset[key], key);
            // Empty-date rows sort to the end regardless of direction.
            if (key === 'date') {
                if (!av && !bv) return 0;
                if (!av) return 1;
                if (!bv) return -1;
            }
            if (av < bv) return -1 * sign;
            if (av > bv) return 1 * sign;
            return 0;
        });
        const frag = document.createDocumentFragment();
        rows.forEach(function (r) { frag.appendChild(r); });
        tbody.appendChild(frag);
    }

    function bindSortHandlers() {
        const table = document.getElementById('results-table');
        if (!table) return;
        const tbody = document.getElementById('results-body');
        if (!tbody) return;
        table.querySelectorAll('th.sortable').forEach(function (th) {
            // Per-th `dataset.bound` guard: under hx-boost the IIFE
            // re-runs and we'd otherwise add another click listener
            // every visit.
            if (th.dataset.sortBound === '1') return;
            th.dataset.sortBound = '1';
            th.addEventListener('click', function () {
                const key = th.dataset.sortKey;
                const wasAsc = th.classList.contains('sort-asc');
                const wasDesc = th.classList.contains('sort-desc');
                // Clear other headers.
                table.querySelectorAll('th.sortable').forEach(function (other) {
                    other.classList.remove('sort-asc', 'sort-desc');
                });
                // Flip direction: no current sort → desc for numeric
                // columns (more is usually better), asc for name/date.
                let dir;
                if (wasAsc) dir = 'desc';
                else if (wasDesc) dir = 'asc';
                else dir = (key === 'name' || key === 'date') ? 'asc' : 'desc';
                th.classList.add(dir === 'asc' ? 'sort-asc' : 'sort-desc');
                sortRows(tbody, key, dir);
            });
        });

        // Template ships `sort-desc` on the Score column as the default
        // visual state, but the server hands us rows in Nyaa's natural
        // order (upload date descending) — not sorted by score. Without
        // this initial sort the arrow lies about the column state,
        // which read as "sort-by-score gives weird results" on page
        // load. Run the same sort the click handler would so the
        // rendered rows match whatever initial class the server set.
        const initial = table.querySelector('th.sortable.sort-asc, th.sortable.sort-desc');
        if (initial) {
            const key = initial.dataset.sortKey;
            const dir = initial.classList.contains('sort-asc') ? 'asc' : 'desc';
            sortRows(tbody, key, dir);
        }
    }
    // Use the page-lifecycle helper so the bind fires AFTER htmx
    // settles each body swap. Direct script-execution-time binding
    // was racy under boost: the script tag runs as part of htmx's
    // swap evaluation and the table elements aren't always queryable
    // yet by the time the script reaches the binding code. The
    // lifecycle helper wires through `htmx.onLoad`, which fires
    // *after* the swap completes and the DOM is settled.
    //
    // `dataset.sortBound` per-th guard inside `bindSortHandlers`
    // makes the mount idempotent, so re-firing on every htmx.onLoad
    // (including on this same page if the user does an in-place
    // refresh of just the results table) doesn't accumulate listeners.
    if (typeof window.ryokanRegisterPageInit === 'function') {
        window.ryokanRegisterPageInit('search-sort', {
            check: function () { return !!document.getElementById('results-table'); },
            mount: bindSortHandlers,
        });
    } else if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', bindSortHandlers);
    } else {
        bindSortHandlers();
    }
})();

// ── Date column rendering for `[data-utc]` cells ───────────────────
//
// Nyaa publishes upload timestamps as "YYYY-MM-DD HH:MM" UTC. The
// search-page date column shows just the date portion (YYYY-MM-DD)
// in the cell, with the full UTC datetime on hover via the `title`
// attribute. No timezone conversion — keeps the column visually
// uniform across rows (no "5d ago" / absolute date mixing) and
// keeps the data clearly UTC for users who want to see the exact
// upload time. Idempotent: a `data-utc-rendered` marker prevents
// re-rendering an already-rendered cell on boost-nav / htmx swap /
// paginated append.
function renderUploadDates() {
    document.querySelectorAll('[data-utc]').forEach(function (el) {
        if (el.dataset.utcRendered === '1') return;
        const utc = el.getAttribute('data-utc');
        if (!utc) return;
        // Strip the time portion: "YYYY-MM-DD HH:MM" → "YYYY-MM-DD".
        // The first 10 chars are always the date when Nyaa's format
        // is well-formed. Fall back to the full string if the shape
        // doesn't match (defensive — a malformed cell still shows
        // *something* readable).
        const m = utc.match(/^(\d{4}-\d{2}-\d{2})/);
        el.textContent = m ? m[1] : utc;
        el.title = utc + ' UTC';
        el.dataset.utcRendered = '1';
    });
}
window.ryokanRenderUploadDates = renderUploadDates;
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', renderUploadDates);
} else {
    renderUploadDates();
}
// Re-render on htmx swaps so a boost-nav back to /search picks up
// any new rows. Idempotent via the `data-utc-rendered` marker.
if (window.htmx && typeof window.htmx.onLoad === 'function') {
    window.htmx.onLoad(renderUploadDates);
}
