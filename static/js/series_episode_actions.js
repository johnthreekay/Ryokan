// Per-episode action buttons (monitor toggle, auto-search) + the
// shared button-state helpers they all use. Split out from series.js
// 2026-05-08 to address the "find one of 51 functions" complaint
// (PR #164 frontend review): each per-feature file is small enough
// to grep through. Cross-file references resolve at invocation time
// against globals (function declarations hoist), so the split is
// behavior-preserving.

// The "(N monitored)" count beside the Monitoring dropdown. Only the
// count span changes; the parentheses and the "pinned" note around it
// are the server's.
function setMonitoredCount(text) {
    const count = document.getElementById('monitor-count') || document.getElementById('monitor-summary');
    if (count) count.textContent = text;
}

// One episode's Yes / No pill: text, color, tooltip, and the payload
// its next click sends (the partial bakes the *opposite* state into
// `hx-vals`, so an in-place update that left it alone made the next
// click a no-op).
function setEpisodePill(btn, monitored) {
    btn.textContent = monitored ? 'Yes' : 'No';
    btn.className = 'ep-mon-btn ' + (monitored ? 'ep-mon-yes' : 'ep-mon-no');
    btn.title = monitored ? 'Unmonitor' : 'Monitor';
    const row = btn.closest('tr');
    const numCell = row && row.querySelector('.ep-col-num');
    const epNum = numCell ? parseInt(numCell.textContent.trim(), 10) : NaN;
    if (typeof SD !== 'undefined' && SD.dbId && !isNaN(epNum)) {
        btn.setAttribute('hx-vals', JSON.stringify({ series_id: parseInt(SD.dbId, 10), episode_number: epNum, monitored: !monitored }));
    }
}

// Apply the server's view of a whole series after a mode change: every
// pill from the monitored list, the count, the bookmark button, the
// "pinned" note (only meaningful when the series can follow a linked
// list, i.e. the dropdown offers "Sync from"), and the dropdown itself.
function applyMonitoringState(detail) {
    if (Array.isArray(detail.monitored_episodes)) {
        const monitored = new Set(detail.monitored_episodes.map(Number));
        for (const row of document.querySelectorAll('.episode-table tbody tr')) {
            const numCell = row.querySelector('.ep-col-num');
            const btn = row.querySelector('.ep-mon-btn');
            if (!numCell || !btn) continue;
            setEpisodePill(btn, monitored.has(parseInt(numCell.textContent.trim(), 10)));
        }
    }
    if (typeof detail.monitored_count === 'number') setMonitoredCount(`${detail.monitored_count} monitored`);
    if (typeof detail.all_monitored === 'boolean' && typeof SD !== 'undefined') setMonitorAllButton(SD.dbId, detail.all_monitored);
    const select = document.getElementById('monitor-mode');
    if (typeof detail.monitor_mode_manual_override === 'boolean') {
        const pinned = document.getElementById('monitor-pinned');
        const canSync = !!(select && select.querySelector('option[value="sync"]'));
        if (pinned) pinned.hidden = !(detail.monitor_mode_manual_override && canSync);
        // A cleared override means "follow the linked list": the dropdown
        // shows Sync, not the mode the list last derived.
        if (select) select.value = detail.monitor_mode_manual_override ? (detail.monitor_mode || select.value) : 'sync';
    }
}

// The bookmark button in the season header: filled when every episode
// is monitored, outlined otherwise, and its click flips the whole set.
function setMonitorAllButton(dbId, allMonitored) {
    const btn = document.getElementById('btn-monitor-all');
    if (!btn) return;
    btn.disabled = false;
    btn.classList.toggle('is-active', allMonitored);
    btn.onclick = function() { toggleMonitorAll(dbId, allMonitored); };
    btn.title = allMonitored ? 'Unmonitor all' : 'Monitor all';
    btn.innerHTML = allMonitored
        ? '<svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" stroke="none"><path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z"/></svg>'
        : '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z"/></svg>';
}

function toggleMonitorAll(dbId, currentlyAllMonitored) {
    if (!dbId) return;
    const btn = document.getElementById('btn-monitor-all');
    const newMode = currentlyAllMonitored ? 'none' : 'all';
    if (btn) btn.disabled = true;
    setMonitoredCount('Updating…');
    // Issue #166 — `/api/library/monitoring` switched from a Json<>
    // extractor to Form<> when the dropdown + add-modal callers
    // migrated to declarative HTMX. This call site keeps imperative
    // DOM updates (the bookmark toggle changes many .ep-mon-btn
    // elements at once, which doesn't fit HTMX's per-element swap),
    // so we send URL-encoded form data and read JSON back from the
    // handler's non-HTMX path. No `HX-Request` header → server picks
    // the JSON branch unchanged.
    fetch('/api/library/monitoring', {
        method: 'POST',
        headers: {'Content-Type': 'application/x-www-form-urlencoded'},
        body: new URLSearchParams({ series_id: dbId, monitor_mode: newMode }),
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed');
        const newState = newMode === 'all';
        document.querySelectorAll('.ep-mon-btn').forEach(monBtn => setEpisodePill(monBtn, newState));
        setMonitorAllButton(dbId, newState);
        setMonitoredCount(`${data.monitored_count || 0} monitored`);
        const select = document.getElementById('monitor-mode');
        if (select) select.value = newMode;
        const pinned = document.getElementById('monitor-pinned');
        if (pinned && select) pinned.hidden = !select.querySelector('option[value="sync"]');
    })
    .catch(err => {
        setMonitoredCount(err.message || 'Failed to update monitoring');
        if (btn) btn.disabled = false;
    });
}

// Both monitoring writes answer with `HX-Trigger: ryokan-monitoring-
// changed`. A per-episode pill swaps only itself and sends the new
// count and all-monitored flag; the mode dropdown swaps nothing and
// sends the whole monitored list as well, so every pill, the count,
// the bookmark button, and the pinned note follow without a reload.
// One-shot guard, like the other module-scope listeners: hx-boost
// re-runs this script on every visit to a series page.
if (!window.__ryokanMonitoringListener) {
    window.__ryokanMonitoringListener = true;
    document.body.addEventListener('ryokan-monitoring-changed', function (ev) {
        applyMonitoringState(ev.detail || {});
    });
}

// HTMX migration (issue #129) — toggleEpisodeMonitor() removed; the
// per-episode monitor button now uses `hx-post` directly. The handler
// at `/api/library/episode-monitoring` returns the swapped button HTML
// for HX-Request, JSON otherwise (preserving the API contract).

// Restore a footer button (Delete File / Cancel Pending) to its
// ready-to-click shape on every modal open. The fetch success path
// intentionally leaves the button with `disabled = true` + loading
// text so a double-click can't fire a second request in flight; but
// the modal closes immediately and the button lives on in the DOM
// (it's a singleton footer element, not re-rendered per episode),
// so without this reset the next modal opens with a stuck
// "Deleting…" / "Cancelling…" label and disabled state.
//
// The initial HTML snapshot is captured on first use via
// `dataset.defaultHtml` — avoids having to re-declare the SVG +
// label inline in the JS.
function resetFooterButton(btn) {
    if (!btn) return;
    if (!btn.dataset.defaultHtml) {
        btn.dataset.defaultHtml = btn.innerHTML;
    }
    btn.innerHTML = btn.dataset.defaultHtml;
    btn.disabled = false;
}

function setBusyButton(btn, busy, busyLabel) {
    if (!btn) return;
    btn.disabled = busy;
    btn.classList.toggle('is-loading', busy);
    const label = btn.querySelector('.btn-label');
    if (label) {
        if (!btn.dataset.originalLabel) btn.dataset.originalLabel = label.textContent;
        label.textContent = busy ? busyLabel : btn.dataset.originalLabel;
    }
}

var SEARCH_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><circle cx="11" cy="11" r="8"/><path d="M21 21l-4.35-4.35"/></svg>';
var SUCCESS_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="20 6 9 17 4 12"/></svg>';
var ERROR_ICON_SVG = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>';

// How long to leave the success/error icon up before reverting to
// the default search icon. Picked to roughly match the toast
// auto-dismiss feel — long enough for the user to register the
// outcome, short enough that the button doesn't look "stuck" on a
// long-lived series page where they grabbed something an hour ago.
// Errors get a longer window so the user can read the tooltip.
var EPISODE_BTN_REVERT_SUCCESS_MS = 2500;
var EPISODE_BTN_REVERT_ERROR_MS = 4000;

function setEpisodeButtonState(btn, state, title) {
    if (!btn) return;
    // Cancel any pending auto-revert from a prior terminal state —
    // the new state takes over the button, and a leftover revert
    // would otherwise stomp it mid-display (e.g. user grabbed,
    // success → revert-pending; user grabs again 1s later → loading
    // briefly, then the original revert fires and resets back to
    // default while the new request is still in flight).
    if (btn._ryokanRevertTimer) {
        clearTimeout(btn._ryokanRevertTimer);
        btn._ryokanRevertTimer = null;
    }
    btn.disabled = state === 'loading';
    btn.classList.remove('is-loading', 'is-success', 'is-error');
    const inner = btn.querySelector('.icon-btn-inner');
    if (state === 'loading') {
        btn.classList.add('is-loading');
        if (inner) inner.innerHTML = '<span class="ep-search-spinner"></span>';
        btn.title = title || 'Searching...';
    } else if (state === 'success') {
        btn.classList.add('is-success');
        if (inner) inner.innerHTML = SUCCESS_ICON_SVG;
        btn.title = title || 'Queued';
        // Auto-revert: without this, the success checkmark sat on
        // the button forever (until F5). The persistence looked
        // intentional in a brief test session but felt stuck on a
        // page kept open across multiple grab cycles — user report
        // 2026-05-02. Stash the timer id on the element so the
        // top-of-function clear can cancel it on the next state
        // change.
        btn._ryokanRevertTimer = setTimeout(() => {
            btn._ryokanRevertTimer = null;
            setEpisodeButtonState(btn, 'default');
        }, EPISODE_BTN_REVERT_SUCCESS_MS);
    } else if (state === 'error') {
        btn.classList.add('is-error');
        if (inner) inner.innerHTML = ERROR_ICON_SVG;
        btn.title = title || 'Search failed';
        setBusyButton(btn, false);
        // Same auto-revert as success but with a longer window so
        // the user has time to read the tooltip explaining what
        // went wrong before the icon flips back to "search".
        btn._ryokanRevertTimer = setTimeout(() => {
            btn._ryokanRevertTimer = null;
            setEpisodeButtonState(btn, 'default');
        }, EPISODE_BTN_REVERT_ERROR_MS);
    } else {
        if (inner) inner.innerHTML = SEARCH_ICON_SVG;
        btn.title = title || btn.title;
        setBusyButton(btn, false);
    }
}

function autoSearchEpisode(episodeNumber, btn) {
    const seriesTitle = SD.titleEnglish || SD.titleRomaji || SD.titleNative || '';
    setEpisodeButtonState(btn, 'loading', `Searching episode ${episodeNumber}...`);
    const pid = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId: pid,
        kind: 'info',
        category: 'auto_search',
        title: `Searching episode ${episodeNumber}`,
        body: seriesTitle,
    });

    fetch(`/api/series/${SD.id}/auto-search/${episodeNumber}?progress_id=${encodeURIComponent(pid)}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'}
    })
    .then(async resp => {
        let data = {};
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) {
            throw new Error(data.message || 'Episode search failed');
        }
        const first = Array.isArray(data.grabbed) ? data.grabbed[0] : null;
        if (first) {
            setEpisodeButtonState(btn, 'success', `Queued: ${first.release_title}`);
            updateEpisodeRow(episodeNumber, 'grabbed', first.release_group);
            ensureDlPollRunning();
            refreshEpisodeRows({ force: true });
        } else {
            setEpisodeButtonState(btn, 'error', 'No matching release found');
        }
    })
    .catch(err => {
        setEpisodeButtonState(btn, 'error', err.message || 'Episode search failed');
        toast.finalize({
            kind: 'error',
            title: `Episode ${episodeNumber} search failed`,
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function autoSearchSeries(btn) {
    const seriesTitle = SD.titleEnglish || SD.titleRomaji || SD.titleNative || '';
    setBusyButton(btn, true, 'Searching…');
    const pid = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId: pid,
        kind: 'info',
        category: 'auto_search',
        title: 'Searching monitored episodes',
        body: seriesTitle,
    });

    fetch(`/api/series/${SD.id}/auto-search?progress_id=${encodeURIComponent(pid)}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'}
    })
    .then(async resp => {
        let data = {};
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) {
            throw new Error(data.message || 'Auto search failed');
        }
        const grabbed = Array.isArray(data.grabbed) ? data.grabbed.length : 0;
        setBusyButton(btn, false);
        if (grabbed > 0) {
            ensureDlPollRunning();
            refreshEpisodeRows({ force: true });
        }
    })
    .catch(err => {
        setBusyButton(btn, false);
        toast.finalize({
            kind: 'error',
            title: 'Auto search failed',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}
