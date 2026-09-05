// ── Logs tab ────────────────────────────────────────────────────────────

// Per-page JS files are re-executed by hx-boost on every nav-back to a
// previously-visited page (htmx evaluates inserted `<script src>`
// tags). Without a one-shot guard around `addEventListener` calls,
// every visit attaches another copy of the listener — by the Nth visit,
// every event fires N callbacks. Surfaced as the "Episode 10 deleted ×7"
// toast spam after my Phase 2 delete migration. Same pattern applied
// to all module-scope listeners across system.js, settings.js, series.js.
if (!window.__ryokanSystemListeners) {
    window.__ryokanSystemListeners = true;

    // Click-outside-to-close for the log-download dropdown. The .open
    // toggle on the trigger button is enough to show the menu; this
    // listener handles dismissal — clicking anywhere outside the menu
    // (or on a menu item, which navigates) closes it.
    document.addEventListener('click', function (ev) {
        const menu = document.getElementById('log-download-options');
        if (!menu || !menu.classList.contains('open')) return;
        // The trigger button is inside .log-download-menu — let its
        // own click open + immediately re-toggle (don't fight it). For
        // option clicks, the navigation closes the menu naturally; for
        // any other click, dismiss.
        if (ev.target.closest('.log-download-menu') && !ev.target.closest('.log-download-option')) {
            return;
        }
        menu.classList.remove('open');
    });
}

// `var` (not `let`) at module scope is deliberate across every per-page
// JS file: htmx body-swap re-executes the inserted `<script>` tag when
// the user navigates back to a previously-visited page, but the prior
// declarations still occupy the global scope. A `let` / `const`
// redeclaration is a parser-stage SyntaxError — the whole file is
// rejected and the lifecycle registration never runs. `var` redeclares
// silently. See `feedback_no_module_scope_dom_under_boost` memory.
var pollTimer = null;
// Initial "latest seen" log id is read from the first rendered row's
// data-id attribute (server-side Askama writes one on every <tr>) so the
// JS stays free of Askama templating. 0 when the logs tab isn't rendered.
var latestId = (function () {
    const firstRow = document.querySelector('#log-tbody tr[data-id]');
    return firstRow ? parseInt(firstRow.dataset.id, 10) || 0 : 0;
})();

function applyFilters() {
    const level = document.getElementById('filter-level').value;
    const category = document.getElementById('filter-category').value;
    const search = document.getElementById('filter-search').value;
    const params = new URLSearchParams({tab: 'logs', level});
    if (category) params.set('category', category);
    if (search) params.set('search', search);
    window.location.href = '/system?' + params.toString();
}

async function clearLogs() {
    const result = await window.ryokanConfirm({
        title: 'Clear logs',
        body: 'Clear all log entries?',
        yesLabel: 'Clear',
    });
    if (!result.ok) return;
    try {
        const r = await fetch('/api/logs/clear', {method: 'POST', headers: {'Content-Type': 'application/json'}});
        await r.json();
        location.reload();
    } catch (err) {
        console.error('Failed to clear logs:', err);
        window.ryokanToast({kind: 'error', title: 'Clear logs failed', body: err && err.message ? err.message : 'Unknown error'});
    }
}

function formatTimestamp(iso) {
    try {
        const d = new Date(iso + 'Z');
        const pad = n => String(n).padStart(2, '0');
        return `${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    } catch (_) {
        return iso;
    }
}

function escapeHtml(s) {
    const d = document.createElement('div');
    d.textContent = s;
    return d.innerHTML;
}

function pollLogs() {
    const toggle = document.getElementById('poll-toggle');
    if (!toggle || !toggle.checked) return;

    const level = document.getElementById('filter-level').value;
    const category = document.getElementById('filter-category').value;
    const params = new URLSearchParams({after: latestId});
    if (level) params.set('level', level);
    if (category) params.set('category', category);

    fetch('/api/logs/poll?' + params.toString())
        .then(r => r.json())
        .then(entries => {
            if (!entries || !entries.length) return;
            const tbody = document.getElementById('log-tbody');
            const empty = document.querySelector('.logs-empty');
            if (empty) empty.remove();

            // Entries come newest-first; insert at top in order.
            for (let i = entries.length - 1; i >= 0; i--) {
                const e = entries[i];
                if (e.id > latestId) latestId = e.id;
                const tr = document.createElement('tr');
                tr.className = `log-row log-level-${e.level} log-row-new`;
                tr.dataset.id = e.id;
                tr.innerHTML = `
                    <td class="log-col-time" title="${escapeHtml(e.timestamp)}">${escapeHtml(e.timestamp)}</td>
                    <td class="log-col-level"><span class="log-badge log-badge-${e.level}">${escapeHtml(e.level)}</span></td>
                    <td class="log-col-cat">${escapeHtml(e.category_label || e.category)}</td>
                    <td class="log-col-msg">
                        <span class="log-message">${escapeHtml(e.message)}</span>
                        ${e.detail ? `<span class="log-detail" title="${escapeHtml(e.detail)}">${escapeHtml(e.detail)}</span>` : ''}
                    </td>`;
                tbody.insertBefore(tr, tbody.firstChild);
                // Remove new-row highlight after animation.
                setTimeout(() => tr.classList.remove('log-row-new'), 2000);
            }
        })
        .catch(() => {}); // Silently fail polling.
}

function startPolling() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = setInterval(pollLogs, 3000);
}

function stopPolling() {
    if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

// Page-lifecycle-aware logs poller. Phase B of the hx-boost rollout:
// boost-swaps don't re-fire DOMContentLoaded, and a module-scope IIFE
// only runs on initial document load. Without lifecycle wiring the
// poll-toggle binding leaks (toggle clicked on the second visit
// changes nothing because the listener is bound to the prior page's
// element) and `startPolling()` accumulates duplicate intervals on
// repeat visits.
//
// `mount` re-resolves the toggle element each entry, binds the change
// listener, starts polling. `unmount` stops polling. The toggle's
// own change handler still calls start/stop directly so the user-
// driven toggle works regardless of nav state.
window.ryokanRegisterPageInit('system-logs-poll', {
    check: function () {
        return !!document.getElementById('poll-toggle');
    },
    mount: function () {
        const pollToggle = document.getElementById('poll-toggle');
        if (!pollToggle) return; // defensive; check() should preclude this
        // `data-bound` flag mirrors the pattern in settings.js: a
        // re-mount on the SAME page (rare — shouldn't happen via
        // boost since check() returning the same truthy doesn't
        // re-fire mount, but a future htmx.process call elsewhere
        // could trigger it) is idempotent w.r.t. event listeners.
        if (!pollToggle.dataset.ryokanBound) {
            pollToggle.addEventListener('change', function () {
                if (this.checked) startPolling(); else stopPolling();
            });
            pollToggle.dataset.ryokanBound = '1';
        }
        startPolling();
    },
    unmount: function () {
        stopPolling();
    },
});

// ── RSS tab ─────────────────────────────────────────────────────────────

function filterRssRows() {
    const search = (document.getElementById('rss-filter-search')?.value || '').toLowerCase().trim();
    const decision = document.getElementById('rss-filter-decision')?.value || 'all';
    const rows = document.querySelectorAll('#rss-decision-table tbody tr');
    rows.forEach(row => {
        const text = (row.dataset.rssText || '').toLowerCase();
        const rowDecision = row.dataset.rssDecision || '';
        const matchesDecision = decision === 'all' || rowDecision === decision;
        const matchesSearch = !search || text.includes(search);
        row.style.display = (matchesDecision && matchesSearch) ? '' : 'none';
    });
}

function runRssSync(btn) {
    const result = document.getElementById('rss-sync-result');
    btn.disabled = true;
    result.textContent = 'Syncing...';
    window.ryokanToast({kind: 'info', title: 'RSS sync running', body: 'Checking the feed for new episodes.'});
    // AbortController + pagehide/beforeunload listeners so a user who
    // tabs out mid-sync doesn't see a misleading "RSS sync failed:
    // NetworkError" toast (and have it persisted to /api/logs/client).
    // The server-side work continues regardless thanks to the
    // detached_task spawn-detach in api_rss_sync.
    const controller = new AbortController();
    const onLeaving = () => controller.abort();
    window.addEventListener('beforeunload', onLeaving, { once: true });
    window.addEventListener('pagehide', onLeaving, { once: true });
    fetch('/api/rss/sync', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        signal: controller.signal,
    })
        .then(async r => {
            const data = await r.json();
            if (!r.ok) throw new Error(data.message || 'RSS sync failed');
            result.textContent = data.message || 'RSS sync finished.';
            // Queue across the reload so the toast survives the
            // navigation that re-renders the RSS decisions table.
            window.ryokanQueueToast({
                kind: 'success',
                title: 'RSS sync complete',
                body: data.message || 'Feed checked.',
            });
            setTimeout(() => window.location.reload(), 600);
        })
        .catch(err => {
            if (controller.signal.aborted) return;
            result.textContent = err.message;
            window.ryokanToast({
                kind: 'error',
                title: 'RSS sync failed',
                body: err && err.message ? err.message : 'Unknown error',
            });
        })
        .finally(() => {
            // Listeners self-remove on first fire via {once: true}; if
            // the fetch settled normally without navigation, they
            // stay registered until the next pagehide / beforeunload
            // fires (one-shot, no leak). The btn re-enable is the
            // only thing this needs to do.
            btn.disabled = false;
        });
}

// ── Scheduled tasks tab ─────────────────────────────────────────────────

function forceRunTask(btn, taskKey) {
    const endpoints = {
        rss_sync: '/api/rss/sync',
        metadata_refresh: '/api/tasks/metadata-refresh',
        airing_refresh: '/api/tasks/airing-refresh',
        // The rebuild handler `api_rebuild_cached_metadata` writes a
        // `metadata_rebuild` scheduled-tasks row, so once the user has
        // ever clicked Rebuild on the Debug tab the task shows up in
        // the Scheduled Tasks list with a Run-now button. Map it
        // through to the same endpoint so re-runs work from there too.
        metadata_rebuild: '/api/system/rebuild-anilist-cache',
        cleanup: '/api/tasks/cleanup',
        post_processing: '/api/tasks/post-processing',
        library_classify: '/api/tasks/library-classify',
        upgrade_search: '/api/tasks/upgrade-search',
        anibridge_refresh: '/api/system/reload-anibridge',
        external_sync: '/api/tasks/external-sync',
        backup: '/api/tasks/backup',
    };
    const url = endpoints[taskKey];
    if (!url) {
        window.ryokanAlert({
            title: 'Unknown task',
            body: 'No run endpoint for task: ' + taskKey,
        });
        return;
    }
    btn.disabled = true;
    btn.textContent = 'Running...';
    // AbortController + pagehide/beforeunload listeners so tab-out
    // doesn't surface a misleading "Task error: NetworkError" toast.
    // Server-side work continues thanks to the `detached_task`
    // spawn-detach in each handler — the in-flight fetch being
    // cancelled is just the browser unwinding its connection.
    const controller = new AbortController();
    const onLeaving = () => controller.abort();
    window.addEventListener('beforeunload', onLeaving, { once: true });
    window.addEventListener('pagehide', onLeaving, { once: true });
    fetch(url, { method: 'POST', signal: controller.signal })
        .then(r => r.json().then(data => ({ ok: r.ok, data })).catch(() => ({ ok: r.ok, data: null })))
        .then(({ ok, data }) => {
            // Queue across the reload — `location.reload()` below
            // tears down the DOM and a non-queued toast disappears
            // before the user can read it.
            if (data && data.message) {
                window.ryokanQueueToast({
                    kind: ok ? 'success' : 'error',
                    title: ok ? 'Task complete' : 'Task failed',
                    body: data.message,
                });
            } else if (!ok) {
                window.ryokanQueueToast({
                    kind: 'error',
                    title: 'Task failed',
                    body: 'The task did not report a reason.',
                });
            } else {
                window.ryokanQueueToast({
                    kind: 'success',
                    title: 'Task complete',
                    body: taskKey + ' finished.',
                });
            }
            location.reload();
        })
        .catch(err => {
            if (controller.signal.aborted) return;
            window.ryokanToast({
                kind: 'error',
                title: 'Task error',
                body: err && err.message ? err.message : String(err),
            });
        })
        .finally(() => {
            // Listeners self-remove on first fire via {once: true};
            // see runRssSync for the rationale.
            btn.disabled = false;
            btn.textContent = 'Run now';
        });
}

// ── Debug tab ───────────────────────────────────────────────────────────

// Debug-tab fetch helper: all four buttons share the same toast shape —
// info toast on start, success/error toast on completion — with the
// disabled button as the only in-flight indicator. No inline result span.
//
// The long-running actions (metadata rebuild, library classify, …) run
// detached on the server via `tokio::spawn`, so their server-side work
// continues even if the client navigates away. The browser, however,
// still aborts its own in-flight fetch on navigation, which used to
// fire a misleading "Rebuild failed / NetworkError" toast that then
// got persisted back to the server logs via `/api/logs/client`.
//
// Wire up an AbortController to the fetch and trip it on
// `beforeunload` / `pagehide`: when the catch fires, `signal.aborted`
// tells us the abort was ours (navigation) vs. a real network/server
// failure, and we skip the toast on the navigation case.
function runDebugAction(btn, opts) {
    btn.disabled = true;
    window.ryokanToast({
        kind: 'info',
        title: opts.startTitle,
        body: opts.startBody || '',
    });
    const controller = new AbortController();
    const onLeaving = () => controller.abort();
    // `pagehide` is the reliable signal across browsers — `beforeunload`
    // is deliberately skipped by some (iOS Safari) and blocked by BFCache.
    // Register both; whichever fires first wins.
    window.addEventListener('beforeunload', onLeaving, { once: true });
    window.addEventListener('pagehide', onLeaving, { once: true });
    fetch(opts.url, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        signal: controller.signal,
    })
    .then(async r => {
        const data = await r.json();
        if (!r.ok) throw new Error(data.message || opts.failureTitle);
        window.ryokanToast({
            kind: 'success',
            title: opts.successTitle,
            body: data.message || opts.successBody || '',
        });
    })
    .catch(err => {
        if (controller.signal.aborted) {
            // Browser cancelled our own fetch because the user is
            // navigating away. The server-side work continues
            // detached; don't show a misleading failure toast.
            return;
        }
        window.ryokanToast({
            kind: 'error',
            title: opts.failureTitle,
            body: err && err.message ? err.message : 'Unknown error',
        });
    })
    .finally(() => {
        window.removeEventListener('beforeunload', onLeaving);
        window.removeEventListener('pagehide', onLeaving);
        btn.disabled = false;
    });
}

function reconcileFallbacks(btn) {
    runDebugAction(btn, {
        url: '/api/library/reconcile-fallbacks',
        startTitle: 'Reconciling fallback entries',
        successTitle: 'Reconciliation complete',
        successBody: 'Fallback reconciliation complete.',
        failureTitle: 'Reconciliation failed',
    });
}

async function rebuildAniListCache(btn) {
    const confirmed = await window.ryokanConfirm({
        title: 'Rebuild metadata cache',
        body: 'Rebuild cached metadata, relations, episode data, and artwork for tracked series using the best currently available provider data? This can use MAL/Tenrai fallback when AniList is unavailable.',
        yesLabel: 'Rebuild',
    });
    if (!confirmed.ok) return;
    runDebugAction(btn, {
        url: '/api/system/rebuild-anilist-cache',
        startTitle: 'Rebuilding metadata cache',
        startBody: 'This can take a while for large libraries.',
        successTitle: 'Metadata cache rebuilt',
        successBody: 'Metadata cache rebuild complete.',
        failureTitle: 'Rebuild failed',
    });
}

async function classifyLibrary(btn) {
    const confirmed = await window.ryokanConfirm({
        title: 'Classify library',
        body: 'Run the source/resolution classifier on every tracked series folder? Files that already have a structured classification row are skipped. This can take a while for large libraries because it runs ffprobe on each unclassified file.',
        yesLabel: 'Classify',
    });
    if (!confirmed.ok) return;
    runDebugAction(btn, {
        url: '/api/tasks/library-classify',
        startTitle: 'Classifying imported files',
        startBody: 'Running ffprobe on unclassified files.',
        successTitle: 'Library classify complete',
        successBody: 'Library classify complete.',
        failureTitle: 'Library classify failed',
    });
}

async function clearRssHistory(btn) {
    const confirmed = await window.ryokanConfirm({
        title: 'Clear RSS history',
        body: 'Clear all RSS grab history? Previously grabbed episodes will be re-evaluated on the next RSS sync.',
        yesLabel: 'Clear',
    });
    if (!confirmed.ok) return;
    runDebugAction(btn, {
        url: '/api/rss/clear-history',
        startTitle: 'Clearing grab history',
        successTitle: 'Grab history cleared',
        successBody: 'Grab history cleared.',
        failureTitle: 'Clear failed',
    });
}

// ── System → Notifications card+modal (issue gh-121) ────────────────
//
// Same shape as the DC modal helpers in static/js/settings.js. Lives
// in system.js (not settings.js) because the Notifications tab is
// mounted on the System page, and base.html only loads the per-page
// JS for the active page. Boot-order discipline for hx-boost re-execs:
// `var` (not `let` / `const`) at module scope, one-shot guard via
// `__ryokanSystemNotifModule` for the listeners.
function openNotificationModal(title) {
    var modal = document.getElementById('notif-modal');
    if (!modal) return;
    if (typeof title === 'string' && title.length > 0) {
        var titleEl = document.getElementById('notif-modal-title');
        if (titleEl) titleEl.textContent = title;
    }
    modal.style.display = 'flex';
    var firstInput = modal.querySelector('input[type="text"], input[type="url"]');
    if (firstInput) firstInput.focus();
}
function closeNotificationModal() {
    var modal = document.getElementById('notif-modal');
    if (modal) modal.style.display = 'none';
}
function fetchAndOpenNotifModal(url, title) {
    var body = document.getElementById('notif-modal-body');
    if (body) body.innerHTML = '';
    openNotificationModal(title);
    if (window.htmx) {
        window.htmx.ajax('GET', url, {
            target: '#notif-modal-body',
            swap: 'innerHTML',
        });
    }
}
function openNotificationEditModal(id, name) {
    fetchAndOpenNotifModal(
        '/system/notifications/' + encodeURIComponent(id) + '/edit-form',
        'Editing ' + (name || 'notification provider')
    );
}
function openNotificationAddModal() {
    fetchAndOpenNotifModal(
        '/system/notifications/add-form',
        'Add notification provider'
    );
}
// Kind-flip + clear-secret + in-modal Send-test wiring. Called on
// every htmx:after:settle into `#notif-modal-body` so the freshly-
// rendered form picks up behavior without per-template inline JS.
function bindNotifModalForm(form) {
    if (!form || form.dataset.ryokanNotifFormBound === '1') return;
    form.dataset.ryokanNotifFormBound = '1';

    var kindSelect = form.querySelector('[data-notif-kind-select]');
    if (kindSelect) {
        kindSelect.addEventListener('change', function () {
            var k = kindSelect.value;
            form.querySelectorAll('[data-notif-field-group]').forEach(function (g) {
                g.style.display = (g.dataset.notifFieldGroup === k) ? '' : 'none';
            });
        });
    }

    form.querySelectorAll('[data-notif-clear-secret]').forEach(function (btn) {
        btn.addEventListener('click', function () {
            var id = btn.getAttribute('data-notif-clear-secret');
            var input = document.getElementById(id);
            if (!input) return;
            input.value = '__CLEAR__';
            input.placeholder = '[will be cleared on save]';
            input.type = 'text';   // unmask so the user sees the sentinel
        });
    });

    form.querySelectorAll('[data-test-provider]').forEach(function (btn) {
        btn.addEventListener('click', notifTestClickHandler);
    });
}
// Modal Send-test button click handler. The card no longer carries
// a per-row test action; only the modal footer's Send-test button
// fires this. Uses `function() { ... this ... }` (not arrow) so the
// click target stays accessible via `this`.
async function notifTestClickHandler() {
    var btn = this;
    var id = btn.getAttribute('data-test-provider');
    var out = document.getElementById('notif-modal-test-result-' + id);
    if (out) {
        out.textContent = 'Sending...';
        out.className = 'notif-modal-test-result';
    }
    try {
        var r = await fetch('/api/notifications/' + id + '/test', { method: 'POST' });
        var data = await r.json();
        if (out) {
            if (r.ok) {
                out.textContent = 'OK (' + (data.status || 200) + ')';
                out.className += ' ok';
            } else {
                out.textContent = 'Error: ' + (data.error || data.body || r.statusText);
                out.className += ' err';
            }
        }
    } catch (e) {
        if (out) {
            out.textContent = 'Network error: ' + e.message;
            out.className += ' err';
        }
    }
}
// Boot-time + lifecycle wiring. One-shot guard so hx-boost re-execs
// of system.js don't accumulate listener copies on every nav-back.
if (!window.__ryokanSystemNotifModule) {
    window.__ryokanSystemNotifModule = true;
    document.body.addEventListener('htmx:after:settle', function(ev) {
        if (ev.target && ev.target.id === 'notif-modal-body') {
            var form = ev.target.querySelector('form');
            if (form) bindNotifModalForm(form);
            var firstInput = ev.target.querySelector('input[type="text"], input[type="url"]');
            if (firstInput) firstInput.focus();
        }
    });
    // Re-bind backdrop-click after every section-partial swap. The
    // modal element is replaced when #notif-section re-renders, so
    // a one-shot listener attached at boot would lose its target.
    document.body.addEventListener('htmx:after:swap', function(ev) {
        if (window.ryokanSwapTargetId(ev) === 'notif-section') {
            bindNotificationModalDismiss();
        }
    });
    // Escape key — global listener; `display !== 'none'` gate keeps
    // it cheap when the modal isn't open.
    document.addEventListener('keydown', function(ev) {
        var modal = document.getElementById('notif-modal');
        if (!modal) return;
        if (ev.key === 'Escape' && modal.style.display !== 'none') {
            closeNotificationModal();
        }
    });
}
// Click on the backdrop (any space outside the inner `.modal` panel)
// closes the modal. `ev.target === modal` is the contract — clicks
// inside `.modal` (the panel) bubble up with `ev.target` set to
// whatever child got clicked, so they don't match. Same shape as the
// DC backdrop dismiss.
function bindNotificationModalDismiss() {
    var modal = document.getElementById('notif-modal');
    if (!modal) return;
    if (modal.dataset.ryokanNotifDismissBound === '1') return;
    modal.dataset.ryokanNotifDismissBound = '1';
    modal.addEventListener('click', function (ev) {
        if (ev.target === modal) closeNotificationModal();
    });
}
// Initial pass for first-paint and boost-nav re-entries — the
// listener above attaches once via the guard, but the pre-rendered
// form body and the backdrop-click dismiss need wiring on every page
// load. (The modal Send-test button is bound by `bindNotifModalForm`
// itself when the form is wired.)
function applyNotifInitialBindings() {
    document.querySelectorAll('#notif-modal-body form').forEach(bindNotifModalForm);
    bindNotificationModalDismiss();
}
window.addEventListener('DOMContentLoaded', applyNotifInitialBindings);
applyNotifInitialBindings();

// ── Backup tab (issue #126) ─────────────────────────────────────────
//
// Two enhancements over the plain forms and links on the tab: the
// Download link picks up the two option checkboxes as query params,
// and the restore upload streams the chosen file as a raw gzip body
// (no multipart, no buffering server-side) then paints the server's
// verdict inline. Everything else on the tab is a form the confirm
// bridge and hx-boost already handle.
var bindBackupTab = function () {
    const tab = document.getElementById('backup-tab');
    if (!tab || tab.dataset.ryokanBound === '1') return;
    tab.dataset.ryokanBound = '1';

    const link = document.getElementById('backup-download');
    const options = Array.from(tab.querySelectorAll('[data-backup-option]'));
    const syncLink = () => {
        if (!link) return;
        const params = [];
        options.forEach((o) => { if (o.checked) params.push(o.getAttribute('data-backup-option') + '=1'); });
        link.href = '/api/backup/download' + (params.length ? '?' + params.join('&') : '');
    };
    options.forEach((o) => o.addEventListener('change', syncLink));
    syncLink();

    const fileInput = document.getElementById('restore-file');
    const button = document.getElementById('restore-upload');
    const out = document.getElementById('restore-result');
    if (!fileInput || !button || !out) return;
    // Server strings (error bodies, manifest fields) are rendered as
    // text nodes, never markup, so nothing from an uploaded archive
    // can reach innerHTML.
    const show = (parts) => {
        out.textContent = '';
        parts.forEach((p) => {
            out.appendChild(typeof p === 'string' ? document.createTextNode(p) : p);
        });
    };
    button.addEventListener('click', async () => {
        const file = fileInput.files && fileInput.files[0];
        if (!file) {
            out.hidden = false;
            out.textContent = 'Choose a backup file first.';
            return;
        }
        button.disabled = true;
        out.hidden = false;
        out.textContent = 'Uploading ' + file.name + ' (' + Math.round(file.size / 1048576) + ' MB)...';
        try {
            const r = await fetch('/api/restore/upload', {
                method: 'POST',
                headers: {'Content-Type': 'application/gzip'},
                credentials: 'same-origin',
                body: file,
            });
            const data = await r.json().catch(() => ({}));
            if (!r.ok || !data.ok) {
                show(['Not staged: ' + String(data.error || ('HTTP ' + r.status))]);
                button.disabled = false;
                return;
            }
            const strong = document.createElement('strong');
            strong.textContent = 'Restart Ryokan to apply it.';
            const parts = [
                'Restore staged from a backup made ' + String(data.backup_time) + ' (Ryokan ' + String(data.version) + '). '
                + 'A backup of the current state was saved as ' + String(data.pre_restore_backup) + '. ',
                strong,
            ];
            (data.warnings || []).forEach((w) => {
                parts.push(document.createElement('br'));
                parts.push(String(w));
            });
            show(parts);
            // Re-render the tab so the staged banner and Cancel appear.
            setTimeout(() => { window.location.href = '/system?tab=backup'; }, 1500);
        } catch (e) {
            out.textContent = 'Upload failed: ' + (e && e.message ? e.message : e);
            button.disabled = false;
        }
    });
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('system-backup', {
        check: function () { return !!document.getElementById('backup-tab'); },
        mount: bindBackupTab,
        unmount: function () {},
    });
} else {
    bindBackupTab();
}
