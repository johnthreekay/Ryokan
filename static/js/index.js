// Server state handoff: window.initialTitleLanguage is set inline in
// index.html before this file loads. Fall back to empty string so
// `getTitleByLang` below still has something to compare against.
var initialTitleLanguage = window.initialTitleLanguage || '';

// Live client-side library search. The full library is in the DOM
// after page load, so substring-matching titles + toggling display
// is faster than a server round-trip and avoids the page reflow. We
// only stamp `?search=foo` into the URL via replaceState so a
// hard reload still lands the user on the same filtered view via
// the server-side handler (no-JS fallback). The dropdowns and sort
// still submit-on-change because list-membership and score-sort
// genuinely need DB work.
//
// 250ms debounce on `input` so a fast typist doesn't trigger a
// full grid re-walk + replaceState per keystroke. The DOM filter is
// cheap on a few hundred series but a power user with thousands
// would feel the per-keystroke layout thrash from the
// `style.display = 'none'` writes.
var _liveSearchTimer = null;
function liveLibrarySearch(input) {
    if (_liveSearchTimer) clearTimeout(_liveSearchTimer);
    _liveSearchTimer = setTimeout(() => {
        _liveSearchTimer = null;
        _liveLibrarySearchImmediate(input);
    }, 250);
}

// ── Two-part sort control (key select + direction toggle) ─────────
//
// The header renders sort as a key ('recent' / 'title' / 'score')
// plus a direction arrow; the server and URL keep the canonical
// six-value ?sort= vocabulary. This recomposes and navigates,
// preserving ?list= (scope) and the live search text.
function librarySortNavigate(sortValue) {
    var url = new URL(window.location.href);
    if (sortValue === 'recent') url.searchParams.delete('sort');
    else url.searchParams.set('sort', sortValue);
    // The search input's live value may be newer than the URL param
    // (liveLibrarySearch debounces its replaceState by 250ms).
    var search = document.getElementById('library-search');
    var q = search ? search.value.trim() : '';
    if (q) url.searchParams.set('search', q);
    else url.searchParams.delete('search');
    window.location.assign(url);
}

// Picking a key applies that key's default direction — newest first,
// A to Z, highest score first — matching the select's option values.
function librarySortKeyChanged(sel) {
    var defaults = { recent: 'recent', title: 'title_asc', score: 'score' };
    librarySortNavigate(defaults[sel.value] || 'recent');
}

// The arrow flips direction within the current key.
function librarySortDirToggled(btn) {
    var keySel = document.getElementById('library-sort-key');
    var key = keySel ? keySel.value : 'recent';
    var wasDesc = btn.dataset.desc === 'true';
    var composed = {
        recent: wasDesc ? 'oldest' : 'recent',
        title: wasDesc ? 'title_asc' : 'title_desc',
        score: wasDesc ? 'score_asc' : 'score',
    };
    librarySortNavigate(composed[key] || 'recent');
}

function _liveLibrarySearchImmediate(input) {
    const q = (input.value || '').trim().toLowerCase();
    const grid = document.getElementById('library-grid');
    if (!grid) return;

    let visibleCount = 0;
    grid.querySelectorAll('.series-card').forEach((card) => {
        // Match against all three title fields. They're in the DOM
        // as .title-option spans (title-switcher controls which one
        // is visible via CSS, but all three are searchable).
        const titles = card.querySelectorAll('.title-option');
        let matches = q === '';
        if (!matches) {
            for (let i = 0; i < titles.length; i++) {
                if (titles[i].textContent.toLowerCase().includes(q)) {
                    matches = true;
                    break;
                }
            }
        }
        card.style.display = matches ? '' : 'none';
        if (matches) visibleCount++;
    });

    // URL update without navigation. Other params (list, sort) are
    // preserved so a manual reload picks up everything.
    const url = new URL(window.location.href);
    if (q) url.searchParams.set('search', q);
    else url.searchParams.delete('search');
    window.history.replaceState(null, '', url);

    // No-matches inline empty state. Built once and toggled — avoids
    // navigation while still telling the user their query found
    // nothing.
    let empty = document.getElementById('library-search-empty');
    if (visibleCount === 0 && q) {
        if (!empty) {
            empty = document.createElement('div');
            empty.id = 'library-search-empty';
            empty.className = 'empty-state';
            const p = document.createElement('p');
            p.textContent = 'No series match your search.';
            empty.appendChild(p);
            grid.parentNode.insertBefore(empty, grid.nextSibling);
        }
        empty.style.display = '';
        grid.style.display = 'none';
    } else if (empty) {
        empty.style.display = 'none';
        grid.style.display = '';
    }
}

function openAddModal() {
    document.getElementById('add-modal').style.display = 'flex';
    document.getElementById('anilist-query').focus();
}

function closeAddModal(e) {
    if (e && e.target !== document.getElementById('add-modal')) return;
    document.getElementById('add-modal').style.display = 'none';
    document.getElementById('anilist-results').innerHTML = '';
    document.getElementById('anilist-query').value = '';
}

function getTitleByLang(entry, lang) {
    if (lang === 'native') return entry.title_native || entry.title_romaji || entry.title_english || '';
    if (lang === 'romaji') return entry.title_romaji || entry.title_english || entry.title_native || '';
    return entry.title_english || entry.title_romaji || entry.title_native || '';
}

// Title-language switching for `.title-switcher` elements is handled by
// CSS via the `html[data-title-language]` attribute that base.html sets
// pre-paint. `getTitleByLang` above is still used by `addSeries` for the
// monitor-mode modal title (rendered via textContent, not a switcher span).

// Per-search provider override. Defaults to AniList; the user can flip
// to MAL via the toggle in the Add Series modal. The selection is
// remembered across modal opens within the page session, but resets on
// a hard reload (no localStorage — explicit-each-session by design).
// Issue #166 — AL/MAL search migrated to `hx-get` on the
// `#anilist-query` input. The handler at `/api/anilist/search` returns
// the rendered partial under HX-Request, JSON otherwise (preserving the
// existing programmatic-caller contract). `currentSearchSource` lives on
// `window` so the input's `hx-vals='js:...'` expression can read it
// without grabbing module-scope identifiers across boost re-execute.
window.currentSearchSource = window.currentSearchSource || 'al';

function setSearchSource(btn) {
    window.currentSearchSource = btn.dataset.source;
    document.querySelectorAll('.search-source-toggle .btn-pill').forEach(b => {
        b.classList.toggle('active', b === btn);
    });
    // Re-fire the HTMX search with the new source. `htmx.trigger` on a
    // synthetic event name listed in the input's `hx-trigger` causes
    // HTMX to re-issue the `hx-get` with the updated `hx-vals`.
    var input = document.getElementById('anilist-query');
    if (input && (input.value || '').trim() && window.htmx) {
        window.htmx.trigger(input, 'ryokan:search');
    }
}

var _pendingSeriesId = null;
var _selectedMonitorMode = 'all';

function addSeries(anilistId, btn) {
    const entry = JSON.parse(btn.dataset.entry);
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/add', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            anilist_id: entry.id,
            mal_id: entry.id_mal || null,
            title: entry.title_english || entry.title_romaji || entry.title_native,
            title_romaji: entry.title_romaji || '',
            title_english: entry.title_english || '',
            title_native: entry.title_native || '',
            cover_url: entry.cover_url,
            format: entry.format,
            status: entry.status,
            episodes: entry.episodes,
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Add failed');
        return data;
    })
    .then(data => {
        btn.textContent = 'Added';
        btn.classList.add('btn-success');
        _pendingSeriesId = data.id;
        _selectedMonitorMode = 'all';
        const lang = localStorage.getItem('titleLanguage') || initialTitleLanguage || 'english';
        const title = getTitleByLang(entry, lang) || entry.title_romaji || entry.title_english || 'this series';
        document.getElementById('monitor-series-title').textContent = title;
        // Reset mode buttons
        document.querySelectorAll('.monitor-option-btn').forEach(b => b.classList.remove('active'));
        const allBtn = document.querySelector('.monitor-option-btn[data-mode="all"]');
        if (allBtn) allBtn.classList.add('active');
        // Close add modal and show monitoring modal
        document.getElementById('add-modal').style.display = 'none';
        document.getElementById('monitor-modal').style.display = 'flex';
    })
    .catch(() => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        btn.disabled = false;
    });
}

function selectMonitorMode(btn) {
    document.querySelectorAll('.monitor-option-btn').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    _selectedMonitorMode = btn.dataset.mode;
}

// Issue #166 — confirmMonitoring moved to declarative hx-post on the
// monitor-confirm-btn in templates/index.html. The button reads its
// payload from `confirmMonitoringVals()` via `hx-vals='js:'`, and the
// server returns `HX-Refresh: true` so htmx reloads the page on
// success. The pre-migration fallback `if (!_pendingSeriesId)
// location.reload()` is preserved by `hx-on::response:error="window.
// location.reload()"` plus the 400 the handler issues on missing id.
function confirmMonitoringVals() {
    return {
        series_id: _pendingSeriesId,
        monitor_mode: _selectedMonitorMode,
        auto_grab: true,
    };
}

// ── Bulk select (issue #125) ────────────────────────────────────────
//
// Sonarr-style "edit mode" pattern. The "Select" toggle in the filter
// row enters bulk-select mode, which makes every card a checkbox
// target instead of a navigation link. Default browsing is unaffected;
// the toolbar + checkboxes only appear when the user explicitly opts
// in. Selection state lives client-side; mode exit clears it.
//
// Keyed by integer series id; the card's `.selected` class is the
// visual projection of membership.

var bulkSelectMode = false;
var bulkSelectedIds = new Set();
var bulkPendingMode = null;

// Enter / exit bulk-select mode. The mode flag drives:
//   - .library-grid.selecting class (CSS shows the chip on every card,
//     suppresses the card-hover transform).
//   - The "Select" toggle's appearance (becomes mauve-active when on).
//   - The bottom toolbar's visibility.
//   - The delegated click handler's preventDefault behavior on cards.
function toggleBulkSelectMode() {
    if (bulkSelectMode) {
        exitBulkSelectMode();
    } else {
        enterBulkSelectMode();
    }
}

function enterBulkSelectMode() {
    bulkSelectMode = true;
    var grid = document.getElementById('library-grid');
    if (grid) grid.classList.add('selecting');
    // Body class lets the mobile-tabbar hide itself via CSS while
    // the bulk action bar takes its place at the bottom of the
    // viewport. Desktop CSS doesn't touch the regular mobile-tabbar
    // (which is already display:none above --bp-phone) so this is a
    // no-op for desktop; mobile uses it.
    document.body.classList.add('bulk-selecting');
    var toggle = document.getElementById('bulk-select-toggle');
    if (toggle) {
        toggle.classList.add('active');
        var label = toggle.querySelector('.bulk-select-toggle-label');
        if (label) label.textContent = 'Cancel';
    }
    renderBulkToolbar();
}

function exitBulkSelectMode() {
    bulkSelectMode = false;
    var grid = document.getElementById('library-grid');
    if (grid) grid.classList.remove('selecting');
    document.body.classList.remove('bulk-selecting');
    var toggle = document.getElementById('bulk-select-toggle');
    if (toggle) {
        toggle.classList.remove('active');
        var label = toggle.querySelector('.bulk-select-toggle-label');
        if (label) label.textContent = 'Select';
    }
    clearBulkSelection();
    renderBulkToolbar();
}

// Toggle one series's selection state. Called from the delegated
// click handler on the library grid (which preventDefaults the
// card's <a> navigation while in selecting mode).
function toggleSeriesSelectById(seriesId) {
    var card = document.getElementById('series-' + seriesId);
    if (!card) return;
    var checkbox = card.querySelector('.series-card-select');
    if (bulkSelectedIds.has(seriesId)) {
        bulkSelectedIds.delete(seriesId);
        card.classList.remove('selected');
        if (checkbox) checkbox.checked = false;
    } else {
        bulkSelectedIds.add(seriesId);
        card.classList.add('selected');
        if (checkbox) checkbox.checked = true;
    }
    renderBulkToolbar();
}

function selectAllVisibleSeries() {
    if (!bulkSelectMode) return;
    document.querySelectorAll('.series-card').forEach(function (card) {
        // Skip cards filtered out by liveLibrarySearch (display: none).
        if (card.style.display === 'none') return;
        var id = parseInt(card.id.slice(7), 10);
        if (!id || bulkSelectedIds.has(id)) return;
        bulkSelectedIds.add(id);
        card.classList.add('selected');
        var cb = card.querySelector('.series-card-select');
        if (cb) cb.checked = true;
    });
    renderBulkToolbar();
}

// True when every visible card is in the selected set (and there's at
// least one visible card). Drives the Select-all toggle's label.
function areAllVisibleSelected() {
    var anyVisible = false;
    var allSelected = true;
    var cards = document.querySelectorAll('.series-card');
    for (var i = 0; i < cards.length; i++) {
        var card = cards[i];
        if (card.style.display === 'none') continue;
        anyVisible = true;
        var id = parseInt(card.id.slice(7), 10);
        if (!id || !bulkSelectedIds.has(id)) {
            allSelected = false;
            break;
        }
    }
    return anyVisible && allSelected;
}

// Toggle the Select-all action: if every visible card is already
// selected, deselect all visible. Otherwise select all visible.
// Bound to the toolbar's #bulk-action-select-all-btn.
function toggleSelectAllVisible() {
    if (!bulkSelectMode) return;
    if (areAllVisibleSelected()) {
        document.querySelectorAll('.series-card').forEach(function (card) {
            if (card.style.display === 'none') return;
            var id = parseInt(card.id.slice(7), 10);
            if (!id || !bulkSelectedIds.has(id)) return;
            bulkSelectedIds.delete(id);
            card.classList.remove('selected');
            var cb = card.querySelector('.series-card-select');
            if (cb) cb.checked = false;
        });
    } else {
        selectAllVisibleSeries();
    }
    renderBulkToolbar();
}


function renderBulkToolbar() {
    var bar = document.getElementById('bulk-action-toolbar');
    var num = document.getElementById('bulk-action-count-num');
    var monitorBtn = document.getElementById('bulk-action-monitor-btn');
    var deleteBtn = document.getElementById('bulk-action-delete-btn');
    var selectAllBtn = document.getElementById('bulk-action-select-all-btn');
    if (!bar || !num) return;
    num.textContent = String(bulkSelectedIds.size);
    // Toolbar visibility tracks selecting MODE, not selection count —
    // the user wants context that they're in select mode even with
    // nothing selected yet. Action buttons disable when N == 0.
    if (bulkSelectMode) {
        bar.hidden = false;
        bar.classList.add('visible');
    } else {
        bar.hidden = true;
        bar.classList.remove('visible');
    }
    var hasSelection = bulkSelectedIds.size > 0;
    if (monitorBtn) monitorBtn.disabled = !hasSelection;
    if (deleteBtn) deleteBtn.disabled = !hasSelection;
    if (selectAllBtn) {
        // The button now contains an inline SVG icon plus a `.bulk-action-label`
        // span; don't `textContent =` the whole button (that would wipe the
        // icon). Update just the label span.
        var selectAllLabel = selectAllBtn.querySelector('.bulk-action-label');
        var label = areAllVisibleSelected() ? 'Deselect all' : 'Select all';
        if (selectAllLabel) {
            selectAllLabel.textContent = label;
        } else {
            selectAllBtn.textContent = label;
        }
    }
}

function clearBulkSelection() {
    bulkSelectedIds.forEach(function (id) {
        var card = document.getElementById('series-' + id);
        if (card) {
            card.classList.remove('selected');
            var cb = card.querySelector('.series-card-select');
            if (cb) cb.checked = false;
        }
    });
    bulkSelectedIds.clear();
    renderBulkToolbar();
}

function openBulkMonitorModal() {
    if (bulkSelectedIds.size === 0) return;
    var modal = document.getElementById('bulk-monitor-modal');
    var count = document.getElementById('bulk-monitor-count');
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (count) count.textContent = String(bulkSelectedIds.size);
    bulkPendingMode = null;
    // Reset both `disabled` AND `textContent` because a previous Apply
    // that succeeded left textContent at "Applying…" and the modal
    // close didn't reset it. Without this the second open shows the
    // stale text.
    if (confirmBtn) {
        confirmBtn.disabled = true;
        confirmBtn.textContent = 'Apply';
    }
    document.querySelectorAll('#bulk-monitor-options .monitor-option-btn').forEach(function (btn) {
        btn.classList.remove('active');
        btn.onclick = function () { selectBulkMonitorMode(btn); };
    });
    if (modal) modal.style.display = 'flex';
}

function closeBulkMonitorModal(event) {
    // Allow the close button + backdrop click + explicit programmatic
    // close to all hit this. The backdrop click sets event.target ===
    // event.currentTarget; the close button uses .btn-icon. Anything
    // else (e.g. clicks bubbling up from inside the panel) is ignored.
    if (event && event.target && event.currentTarget && event.target !== event.currentTarget) {
        if (!event.target.closest('.btn-icon') && !event.target.classList.contains('btn-secondary')) {
            return;
        }
    }
    var modal = document.getElementById('bulk-monitor-modal');
    if (modal) modal.style.display = 'none';
}

function selectBulkMonitorMode(btn) {
    document.querySelectorAll('#bulk-monitor-options .monitor-option-btn').forEach(function (b) {
        b.classList.remove('active');
    });
    btn.classList.add('active');
    bulkPendingMode = btn.dataset.mode;
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (confirmBtn) confirmBtn.disabled = false;
}

function confirmBulkMonitor() {
    if (!bulkPendingMode || bulkSelectedIds.size === 0) return;
    var ids = Array.from(bulkSelectedIds);
    var mode = bulkPendingMode;
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (confirmBtn) {
        confirmBtn.disabled = true;
        confirmBtn.textContent = 'Applying…';
    }
    fetch('/api/library/bulk/monitor', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ series_ids: ids, mode: mode })
    })
    .then(function (r) { return r.json(); })
    .then(function (outcome) {
        renderBulkOutcome(outcome, 'Monitor mode updated');
        // No reload necessary — the library page doesn't render
        // per-episode monitor state. Series-detail pages will reflect
        // the new monitor mode on next visit. Future bulk actions
        // that affect visible badges (delete, upgrades) will refresh
        // their per-card state in place from the response.
    })
    .catch(function (e) {
        if (confirmBtn) {
            confirmBtn.disabled = false;
            confirmBtn.textContent = 'Apply';
        }
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Bulk update failed', body: (e && e.message) || 'Network error', log: true });
        }
    });
}

// Render a BulkOutcome ({ succeeded: [], failed: [{series_id, reason}] })
// as a toast. All-success → success toast. All-failure → error toast,
// selection preserved so the user can retry without re-selecting.
// Partial → warn toast; succeeded IDs cleared from selection,
// failed IDs remain selected so the user sees which ones to address.
//
// Reused across all bulk actions; the `verb` argument is the user-
// facing summary noun ("Monitor mode updated" / "Series deleted" / etc.).
function renderBulkOutcome(outcome, verb, action) {
    var ok = (outcome.succeeded || []).length;
    var bad = (outcome.failed || []).length;
    // Close any open bulk modal FIRST — the early-return below on
    // missing toast helper would otherwise leave the modal open with
    // a stuck "Applying…" button. Modal cleanup is independent of
    // toast availability.
    ['bulk-monitor-modal', 'bulk-delete-modal'].forEach(function (id) {
        var m = document.getElementById(id);
        if (m) m.style.display = 'none';
    });
    // Defensive: reset the bulk-monitor confirm button so a subsequent
    // openBulkMonitorModal opens with "Apply", not stale "Applying…".
    var confirmBtn = document.getElementById('bulk-monitor-confirm-btn');
    if (confirmBtn) {
        confirmBtn.disabled = true;
        confirmBtn.textContent = 'Apply';
    }
    if (!window.ryokanToast) return;

    if (bad === 0) {
        // All succeeded: toast + exit mode (which clears selection).
        window.ryokanToast({ kind: 'success', title: verb, body: ok + ' succeeded', log: false, duration: action ? 6000 : 3000, action: action });
        exitBulkSelectMode();
    } else if (ok === 0) {
        // All failed: keep mode open + selection intact so the user
        // can retry without re-selecting.
        var first = outcome.failed[0];
        var summary = bad + ' failed';
        if (first) summary += '; ' + (first.reason || 'unknown error');
        window.ryokanToast({ kind: 'error', title: verb + ' failed', body: summary, log: true });
    } else {
        // Partial: succeeded IDs cleared from selection, failed IDs
        // remain so the user sees what still needs attention. Mode
        // stays on.
        var failedSet = new Set(outcome.failed.map(function (f) { return f.series_id; }));
        var stillSelected = new Set();
        bulkSelectedIds.forEach(function (id) {
            if (failedSet.has(id)) {
                stillSelected.add(id);
            } else {
                var c = document.getElementById('series-' + id);
                if (c) {
                    c.classList.remove('selected');
                    var cb = c.querySelector('.series-card-select');
                    if (cb) cb.checked = false;
                }
            }
        });
        bulkSelectedIds = stillSelected;
        renderBulkToolbar();
        window.ryokanToast({
            kind: 'warn',
            title: verb,
            body: ok + ' succeeded, ' + bad + ' failed (selection still shows the failures)',
            log: true,
            duration: 6000
        });
    }
}

// One-shot global listeners. The `__ryokan*` boot guard pattern keeps
// hx-boost re-execs of this file from accumulating duplicate handlers
// on every nav-back into /. Delegated click + keydown listeners are
// attached to `document` rather than the library grid because the
// grid element gets replaced on hx-boost navigation; document-level
// listeners survive the swap.
if (!window.__ryokanBulkSelectInit) {
    window.__ryokanBulkSelectInit = true;

    // Document-level capture-phase click delegation. In selecting
    // mode, intercepts every click that lands inside a .series-card
    // (chip OR card body), preventDefaults the parent <a>'s
    // navigation, and toggles selection.
    //
    // Capture phase (third arg `true`) is load-bearing: HTMX's body-
    // wide hx-boost listener handles clicks in the bubble phase, so
    // a bubble-phase listener here would race with HTMX. Capture
    // fires outermost-in (document → body → ... → target), so our
    // capture listener runs BEFORE any element-level listener,
    // including HTMX's body-level one. preventDefault + stopPropagation
    // here means the click never reaches HTMX or the <a>'s default
    // navigation — the chip click works regardless of what happened
    // before us.
    document.addEventListener('click', function (ev) {
        if (!bulkSelectMode) return;
        var card = ev.target.closest && ev.target.closest('.series-card');
        if (!card) return;
        ev.preventDefault();
        ev.stopPropagation();
        var id = parseInt(card.id.slice(7), 10);
        if (!id) return;
        toggleSeriesSelectById(id);
    }, true);

    // "/" focuses the library search from anywhere on the page —
    // the same muscle memory as Nyaa, GitHub, and the *arr stack.
    // Skipped while typing in any field so a literal slash still
    // works there, and no-ops on non-library pages (the input isn't
    // in the DOM). Document-level for the same hx-boost-survival
    // reason as the listeners above.
    document.addEventListener('keydown', function (ev) {
        if (ev.key !== '/' || ev.ctrlKey || ev.metaKey || ev.altKey) return;
        var el = document.activeElement;
        if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' ||
                   el.tagName === 'SELECT' || el.isContentEditable)) return;
        var search = document.getElementById('library-search');
        if (!search) return;
        ev.preventDefault();
        search.focus();
        search.select();
    });

    // Esc: exits the bulk delete modal first if open, then bulk
    // monitor modal, then selecting mode (clears selection on the
    // way out).
    document.addEventListener('keydown', function (ev) {
        if (ev.key !== 'Escape') return;
        var deleteModal = document.getElementById('bulk-delete-modal');
        if (deleteModal && deleteModal.style.display !== 'none' && deleteModal.style.display !== '') {
            deleteModal.style.display = 'none';
            return;
        }
        var monitorModal = document.getElementById('bulk-monitor-modal');
        if (monitorModal && monitorModal.style.display !== 'none' && monitorModal.style.display !== '') {
            monitorModal.style.display = 'none';
            return;
        }
        if (bulkSelectMode) {
            exitBulkSelectMode();
        }
    });
}

// ── Bulk delete modal ──────────────────────────────────────────────

function openBulkDeleteModal() {
    if (bulkSelectedIds.size === 0) return;
    var modal = document.getElementById('bulk-delete-modal');
    var count = document.getElementById('bulk-delete-count');
    var checkbox = document.getElementById('bulk-delete-files-toggle');
    var confirmBtn = document.getElementById('bulk-delete-confirm-btn');
    if (count) count.textContent = String(bulkSelectedIds.size);
    if (checkbox) checkbox.checked = false;
    if (confirmBtn) {
        confirmBtn.disabled = false;
        confirmBtn.textContent = 'Remove from library';
    }
    if (modal) modal.style.display = 'flex';
}

function closeBulkDeleteModal(event) {
    if (event && event.target && event.currentTarget && event.target !== event.currentTarget) {
        if (!event.target.closest('.btn-icon') && !event.target.classList.contains('btn-secondary')) {
            return;
        }
    }
    var modal = document.getElementById('bulk-delete-modal');
    if (modal) modal.style.display = 'none';
}

function confirmBulkDelete() {
    if (bulkSelectedIds.size === 0) return;
    var ids = Array.from(bulkSelectedIds);
    var checkbox = document.getElementById('bulk-delete-files-toggle');
    var deleteFiles = !!(checkbox && checkbox.checked);
    var confirmBtn = document.getElementById('bulk-delete-confirm-btn');
    if (confirmBtn) {
        confirmBtn.disabled = true;
        confirmBtn.textContent = 'Removing…';
    }
    fetch('/api/library/bulk/delete', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ series_ids: ids, delete_files: deleteFiles })
    })
    .then(function (r) { return r.json(); })
    .then(function (outcome) {
        // Remove succeeded cards from the DOM directly — they're
        // gone server-side, so leaving them in the grid would be
        // misleading. Failed cards stay (per renderBulkOutcome's
        // partial-failure logic).
        (outcome.succeeded || []).forEach(function (id) {
            var card = document.getElementById('series-' + id);
            if (card) card.remove();
        });
        // Recycle bin (#123): when files went to the bin, give the toast
        // a way there.
        var modal = document.getElementById('bulk-delete-modal');
        var recycleOn = !!(modal && modal.dataset.recycleEnabled === 'true');
        var action = (deleteFiles && recycleOn)
            ? { label: 'View recycle bin', onClick: function () { window.location.href = '/library/recycle'; } }
            : undefined;
        renderBulkOutcome(outcome, 'Removed from library', action);
    })
    .catch(function (e) {
        if (confirmBtn) {
            confirmBtn.disabled = false;
            confirmBtn.textContent = 'Remove from library';
        }
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Bulk delete failed', body: (e && e.message) || 'Network error', log: true });
        }
    });
}
