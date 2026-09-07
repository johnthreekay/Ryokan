// Episode-row sync + download-progress polling — the always-running
// page-lifecycle stuff for the series page. Other per-feature code
// lives in sibling files: series_helpers.js (SD Proxy + escape /
// format helpers), series_episode_actions.js (monitor + auto-search
// buttons), series_episode_modal.js (detail modal + history),
// series_interactive_search.js (per-ep + batch interactive search),
// series_config.js (monitor mode / manual override / upgrades / search
// overrides), series_lifecycle.js (add / remove series). All seven
// files are loaded via separate `<script>` tags in series.html; cross-
// file references resolve at invocation time against globals (function
// declarations hoist), so the split is behavior-preserving.

function updateEpisodeRow(epNum, state, group) {
    const rows = document.querySelectorAll('.episode-table tbody tr');
    for (const row of rows) {
        const numCell = row.querySelector('.ep-col-num');
        if (!numCell || parseInt(numCell.textContent.trim()) !== epNum) continue;

        if (state === 'grabbed') {
            row.classList.remove('ep-row-missing', 'ep-row-unaired');
            row.classList.add('ep-row-queued');
            const qualityCell = row.querySelector('.ep-col-quality');
            if (qualityCell) {
                // Show a 0% progress bar immediately; the download poller will update it
                if (!qualityCell.dataset.originalHtml) {
                    qualityCell.dataset.originalHtml = qualityCell.innerHTML;
                }
                qualityCell.innerHTML = '<div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:0%"></div></div><span class="dl-progress-text">0.0%</span></div>';
            }
        } else if (state === 'deleted') {
            // Strip both have/queued so a cancelled pending row doesn't
            // stay visually styled as queued (grey progress-bar row)
            // after the text content switches to "Missing". The prior
            // version only removed `ep-row-have`, which left a cancelled
            // pending row wearing both `ep-row-queued` and
            // `ep-row-missing` simultaneously until the next force-
            // refresh arrived — hence the "cancel one visually cancels
            // all" / "stuck queued" marker.
            row.classList.remove('ep-row-have', 'ep-row-queued');
            // A cancelled grab on a not-yet-aired episode goes back to
            // the neutral Unaired state, not red Missing. The row's
            // data-unaired attribute mirrors Episode.unaired (stamped
            // server-side and kept fresh by syncEpisodeDataset).
            var wasUnaired = row.dataset.unaired === 'true';
            row.classList.add(wasUnaired ? 'ep-row-unaired' : 'ep-row-missing');
            const statusCell = row.querySelector('.ep-col-status');
            if (statusCell) {
                statusCell.innerHTML = wasUnaired ? STATUS_ICON_UNAIRED : STATUS_ICON_MISSING;
            }
            const qualityCell = row.querySelector('.ep-col-quality');
            if (qualityCell) {
                qualityCell.innerHTML = wasUnaired
                    ? '<span class="ep-unaired-label">Unaired</span>'
                    : '<span class="ep-missing-label">Missing</span>';
                // Drop the cached `originalHtml` stash so future
                // showings of a progress bar on this row don't restore
                // the stale queued HTML.
                delete qualityCell.dataset.originalHtml;
            }
        }
        break;
    }
    // Keep the modal-footer cancel + delete buttons in sync with
    // the row's queued/have state. No-ops when the modal is closed
    // or showing a different episode. Cover both because
    // updateEpisodeRow is called for both 'grabbed' (cancel button
    // matters) and 'deleted' (delete button matters) state flips.
    syncCancelPendingButton(epNum);
    syncDeleteFileButton(epNum);
}

// Keep the modal-footer Cancel-Pending button visibility in sync with
// the episode row's `ep-row-queued` class. Called from showEpisodeDetail
// (initial open), updateEpisodeRow, and refreshEpisodeRows after they
// mutate the row. Bails when the modal is closed or showing a different
// episode so callers don't have to gate. The modal starts with
// `style="display:none"`, opens to `flex`, closes back to `none`.
function syncCancelPendingButton(epNum) {
    const modal = document.getElementById('ep-detail-modal');
    if (!modal || modal.style.display !== 'flex') return;
    if (_currentEpNum !== epNum) return;
    const cancelBtn = document.getElementById('btn-cancel-pending');
    if (!cancelBtn) return;
    let isPending = false;
    const rows = document.querySelectorAll('.episode-table tbody tr');
    for (const row of rows) {
        const numCell = row.querySelector('.ep-col-num');
        if (!numCell || parseInt(numCell.textContent.trim()) !== epNum) continue;
        isPending = row.classList.contains('ep-row-queued');
        break;
    }
    cancelBtn.style.display = isPending ? '' : 'none';
}

// Keep the modal-footer Delete File button visibility (and its
// per-episode hx-post URL + confirm-bridge body copy) in sync with
// the episode row's `ep-row-have` class. Called from showEpisodeDetail
// (initial open) and refreshEpisodeRows after a /api/series/<id>/episodes
// patch lands. Bails when the modal is closed or showing a different
// episode.
//
// Without this re-running on patch, an episode that finished
// downloading while the modal was open kept the Delete File button
// hidden — visibility was set once at modal-open from the
// (then-stale) `dataset.onDisk`, and the file landing later didn't
// retrigger the show. User had to close and reopen the modal to
// see it.
//
// Always re-applies the hx-post URL and confirm body when on_disk
// is true, even on subsequent re-syncs where the URL hasn't changed.
// `htmx.process()` is idempotent on already-bound elements so the
// re-bind is cheap and ensures the confirm-bridge attrs pick up any
// per-episode customization (the body copy quotes the episode number).
function syncDeleteFileButton(epNum) {
    const modal = document.getElementById('ep-detail-modal');
    if (!modal || modal.style.display !== 'flex') return;
    if (_currentEpNum !== epNum) return;
    const deleteBtn = document.getElementById('btn-delete-file');
    if (!deleteBtn) return;
    let onDisk = false;
    const rows = document.querySelectorAll('.episode-table tbody tr');
    for (const row of rows) {
        const numCell = row.querySelector('.ep-col-num');
        if (!numCell || parseInt(numCell.textContent.trim()) !== epNum) continue;
        onDisk = row.classList.contains('ep-row-have');
        break;
    }
    deleteBtn.style.display = onDisk ? '' : 'none';
    if (onDisk) {
        deleteBtn.setAttribute(
            'hx-post',
            `/api/series/${SD.id}/delete-file/${epNum}`,
        );
        const recycleOn = SD.recycleEnabled === 'true';
        deleteBtn.setAttribute(
            'data-ryokan-confirm-body',
            recycleOn
                ? `Move the file for Episode ${epNum} to the recycle bin? You can restore it from the Recycle Bin page until it is purged.`
                : `Delete the file for Episode ${epNum} from disk? This cannot be undone.`,
        );
        deleteBtn.setAttribute(
            'data-ryokan-confirm-yes',
            recycleOn ? 'Move to recycle bin' : 'Delete',
        );
        if (window.htmx && typeof window.htmx.process === 'function') {
            window.htmx.process(deleteBtn);
        }
    }
}

// --- Relations carousel scroll buttons ---
// The relations list is a horizontal-scroll flexbox. The native scrollbar is
// styled thin and is easy to miss, so we flank the cards with chevron buttons
// when the content actually overflows. At the scroll edges the buttons are
// kept in layout (via visibility:hidden in CSS) so the cards don't jitter as
// the user crosses the first or last position. For series whose relations
// already fit in view, the `.no-scroll` class removes the buttons entirely so
// short lists aren't padded with dead space.
//
// Mount via `ryokanRegisterPageInit` so the bind fires on htmx.onLoad
// (after the body swap commits) rather than at script-load time. On a
// boost-nav, dynamically-injected `<script src=...>` tags ignore `defer`
// and execute as soon as the file finishes loading — which can race
// ahead of htmx finishing the swap. An IIFE here would measure
// `track.scrollWidth` before the cards are in DOM (or with zero
// dimensions) and `.no-scroll` would stick → no arrows. F5 reload
// fixed it because the script ran post-DOM. Lifecycle helper defers
// the mount to the right moment.
// `var` (not `let` / `const`) per CLAUDE.md: top-level `let` throws
// "redeclaration" SyntaxError when the script re-executes after a
// hx-boost body swap. Re-execution is fine; redeclaration isn't.
var initRelationsCarousel = function (section) {
    const EDGE_SLOP = 2; // pixels of tolerance for "reached the edge"
    const row = section.querySelector('.relations-row');
    const track = section.querySelector('.relation-cards');
    const btnLeft = section.querySelector('.relation-scroll-btn-left');
    const btnRight = section.querySelector('.relation-scroll-btn-right');
    if (!row || !track || !btnLeft || !btnRight) return;
    // Idempotent guard: page_lifecycle.js dedupes registrations by
    // name, but the immediate-mount path can fire a second time if
    // htmx.onLoad has already run before this script even loaded.
    // Re-binding click listeners on the same buttons would stack
    // duplicate handlers → one click scrolls twice. Skip the bind
    // when we've already wired this section up.
    if (section.dataset.ryokanRelationsBound === '1') return;
    section.dataset.ryokanRelationsBound = '1';

    const updateButtons = () => {
        const scrollable = track.scrollWidth > track.clientWidth + EDGE_SLOP;
        if (!scrollable) {
            row.classList.add('no-scroll');
            btnLeft.hidden = true;
            btnRight.hidden = true;
            return;
        }
        row.classList.remove('no-scroll');
        btnLeft.hidden = track.scrollLeft <= EDGE_SLOP;
        btnRight.hidden = track.scrollLeft >= track.scrollWidth - track.clientWidth - EDGE_SLOP;
    };

    const scrollByViewport = direction => {
        // Scroll by roughly one viewport minus a card, so the card at the
        // current edge remains visible as an anchor.
        const delta = Math.max(track.clientWidth - 152, 200) * direction;
        track.scrollBy({ left: delta, behavior: 'smooth' });
    };

    btnLeft.addEventListener('click', () => scrollByViewport(-1));
    btnRight.addEventListener('click', () => scrollByViewport(1));
    track.addEventListener('scroll', updateButtons, { passive: true });
    // Per-section resize listener used to attach to `window` here.
    // Under body-wide hx-boost, the script re-runs on every nav-back
    // and a fresh `window.resize` listener attached for each visit
    // (window persists across body swaps even when the section DOM
    // doesn't). After N visits, N closures captured stale section
    // refs and fired on every resize. Use ResizeObserver on `track`
    // instead — the observer is GC'd with the detached section,
    // so no cross-visit accumulation.
    if (typeof ResizeObserver !== 'undefined') {
        new ResizeObserver(updateButtons).observe(track);
    }

    updateButtons();
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('series-relations-carousel', {
        check: () => !!document.querySelector('.relations-section'),
        mount: () => {
            document.querySelectorAll('.relations-section').forEach(section => {
                initRelationsCarousel(section);
            });
        },
        unmount: () => {
            // No interval to clear; ResizeObservers are GC'd with the
            // section DOM nodes when boost detaches them. The dataset
            // bind-guard naturally resets because the new boost-nav
            // brings in fresh `.relations-section` elements without
            // the dataset attribute set.
        },
    });
}

// --- Episode download progress polling ---
//
// Module-scope state for the per-series download-progress poller is
// stashed on `window` (not bare `var`) because hx-boost re-executes
// this script on every nav-back to a series page. A bare
// `var dlPollTimer = null;` reassigns to null on every re-execution,
// which would wipe the prior visit's `setInterval` handle the moment
// the new visit's script runs — the singleton-clear at the bottom of
// this file would then find `null`, skip its `clearInterval`, and
// start a second poller on top of the first. After N visits, N
// pollers each hit `/api/series/<id>/download-progress` every 5s;
// when one self-clears on idle, it cancels the latest ID and leaves
// the older ones firing forever. Window-scoped state survives the
// re-execution, so the singleton-clear has the correct prior handle
// to cancel.
var dlPoll = (window.__ryokanSeriesDlPoll = window.__ryokanSeriesDlPoll || {
    timer: null,
    active: false,
    refreshing: false,
    // Queued-force flag: a force refresh that arrives while a
    // non-force one is in flight would otherwise be silently dropped
    // by the early-return guard. Remembering the force intent here
    // lets the in-flight refresh chain a force one when it settles,
    // so mutations like grab-success can't lose their force-path
    // patch (rows not already showing a progress bar updating; season
    // summary recompute).
    queuedForce: false,
    // Episode → item from the last /download-progress response, for
    // the episode modal's grab history: its State column shows what
    // the client is doing with the current grab (seeding, paused).
    lastItems: {},
    lastByHash: {},
});

// What the download client says about an item, in the words the
// Downloads page uses. `paused` covers a torrent the client stopped at
// its own seeding limit (`seeding_done`) as well as one paused by hand:
// from the user's chair both are "the client is not moving this".
function clientStateLabel(item) {
    const kind = item.state_kind || '';
    if (kind === 'errored') return { key: 'error', text: 'Error' };
    if (kind === 'paused' || kind === 'paused-complete' || item.seeding_done) return { key: 'paused', text: 'Paused' };
    if (kind.startsWith('seeding') || kind === 'checking-seed') return { key: 'seeding', text: 'Seeding' };
    if (kind === 'downloading-stalled') return { key: 'stalled', text: 'Stalled' };
    if (kind === 'downloading-queued') return { key: 'queued', text: 'Queued' };
    if (kind === 'checking-download') return { key: 'checking', text: 'Checking' };
    return { key: 'downloading', text: 'Downloading' };
}

// Sync the episode table with the server's authoritative state.
//
// By default this only touches rows that are currently showing a progress
// bar — the poll-path use case: when a download disappears from the
// progress response (post-processing finished moving the file, or the grab
// failed), we need to replace the bar with the real row state.
//
// Pass `{ force: true }` from mutation handlers (grab, delete, batch
// search) to patch every row, including rows that weren't previously
// showing a progress bar.
//
// The season summary (badge "5 / 12" + season-size span) is always
// recomputed: the row patch can flip an episode from queued to on-disk
// when post-processing imports a file mid-poll, and both surfaces
// need to follow even on the non-force path. Skipping the recompute
// left the count and the total-bytes display stuck at pre-import
// values until the user reloaded the page.
function refreshEpisodeRows(options) {
    const opts = options || {};
    const force = !!opts.force;
    if (!SD.dbId) return;
    if (dlPoll.refreshing) {
        // A non-force refresh is in flight. Don't pile on a duplicate
        // fetch, but if the caller wanted force, remember to chain a
        // force refresh once the in-flight one settles. Without this,
        // a grab landing while the poll-path was mid-fetch would lose
        // its force-path patch (rows not already showing a progress
        // bar wouldn't update).
        if (force) dlPoll.queuedForce = true;
        return;
    }
    dlPoll.refreshing = true;
    fetch(`/api/series/${SD.id}/episodes`)
        .then(r => r.ok ? r.json() : null)
        .then(episodes => {
            if (!episodes) return;
            patchEpisodeRows(episodes, force);
            updateSeasonSummary(episodes);
            // After patching the table, the open modal's footer
            // buttons may need to flip too — e.g. a poll detected
            // the torrent finished, the row went from queued to
            // have, Cancel Pending should hide and Delete File
            // should show. No-op when the modal is closed.
            syncDeleteFileButton(_currentEpNum);
            syncCancelPendingButton(_currentEpNum);
        })
        .catch(() => {})
        .finally(() => {
            dlPoll.refreshing = false;
            if (dlPoll.queuedForce) {
                dlPoll.queuedForce = false;
                refreshEpisodeRows({ force: true });
            }
        });
}

// Sync the row's title-button dataset (size/filename/on-disk) with
// the latest /api/series/<id>/episodes payload. Important because
// `showEpisodeDetail` reads these from `btn.dataset.*` at modal-open
// time; without this sync, every modal open after page load would
// use the stale values captured by the server template at first
// render. Symptom: episode finishes downloading mid-session, user
// opens the modal, sees `—` for size because `dataset.size` was
// empty at template time. Cheap and unconditional — happens on
// every patch call regardless of force/showingProgress gating
// because the dataset is read on demand, not on every poll tick.
function syncEpisodeDataset(row, ep) {
    // Keep the row's unaired marker fresh so optimistic patches
    // (updateEpisodeRow's 'deleted' flip) restore the right state.
    if (typeof ep.unaired === 'boolean') {
        row.dataset.unaired = ep.unaired ? 'true' : 'false';
    }
    const titleBtn = row.querySelector('.ep-title-btn');
    if (!titleBtn) return;
    if (typeof ep.size_display === 'string') {
        titleBtn.dataset.size = ep.size_display;
    }
    if (typeof ep.filename === 'string') {
        titleBtn.dataset.filename = ep.filename;
    }
    titleBtn.dataset.onDisk = ep.on_disk ? 'true' : 'false';
}

// If the episode-detail modal is currently open and showing this
// episode, patch its size cell live so a long-open modal sees the
// import landing without the user having to close-and-reopen.
// Skips when `renderGrabHistory` has already patched the cell to
// show a batch total — that view is more useful for batch grabs
// than the per-file size and re-rendering on the next /episodes
// patch would clobber it.
function maybeUpdateOpenModalSize(epNum, ep) {
    const modal = document.getElementById('ep-detail-modal');
    if (!modal || modal.style.display !== 'flex') return;
    if (_currentEpNum !== epNum) return;
    const sizeValueEl = document.querySelector('#ep-detail-body .ep-detail-size-value');
    if (!sizeValueEl) return;
    if (sizeValueEl.innerHTML.includes('(batch')) return;
    sizeValueEl.textContent = ep.size_display && ep.size_display.length > 0
        ? ep.size_display
        : '—';
}

function patchEpisodeRows(episodes, force) {
    const byNum = {};
    for (const ep of episodes) byNum[ep.number] = ep;
    const rows = document.querySelectorAll('.episode-table tbody tr');
    for (const row of rows) {
        const numCell = row.querySelector('.ep-col-num');
        if (!numCell) continue;
        const n = parseInt(numCell.textContent.trim());
        const ep = byNum[n];
        if (!ep) continue;

        // Dataset + open-modal size sync run on every row regardless
        // of the force/showingProgress gating below — the visual
        // class/innerHTML changes are gated to avoid blowing away
        // in-flight progress bars on the poll path, but the modal's
        // dataset can always benefit from a fresh value.
        syncEpisodeDataset(row, ep);
        maybeUpdateOpenModalSize(n, ep);

        const qualityCell = row.querySelector('.ep-col-quality');
        if (!qualityCell) continue;

        const showingProgress = qualityCell.querySelector('.dl-progress-wrap') !== null;
        // Poll-path: only touch rows currently showing a progress bar.
        // Force-path: touch everything.
        if (!force && !showingProgress) continue;

        const statusCell = row.querySelector('.ep-col-status');

        if (ep.on_disk) {
            row.classList.remove('ep-row-missing', 'ep-row-unaired', 'ep-row-queued');
            row.classList.add('ep-row-have');
            if (statusCell) statusCell.innerHTML = STATUS_ICON_HAVE;
            const quality = ep.quality || 'UNKNOWN';
            qualityCell.innerHTML = `<span class="tag tag-quality">${escHtml(quality)}</span>`;
            delete qualityCell.dataset.originalHtml;
        } else if (ep.quality_state === 'grabbed') {
            // Episode was just grabbed (or is still queued). Show a 0%
            // progress bar — the poller will update it once the
            // download client reports real progress.
            row.classList.remove('ep-row-have', 'ep-row-missing', 'ep-row-unaired');
            row.classList.add('ep-row-queued');
            if (!showingProgress) {
                if (!qualityCell.dataset.originalHtml) {
                    qualityCell.dataset.originalHtml = qualityCell.innerHTML;
                }
                qualityCell.innerHTML = DL_PROGRESS_HTML_ZERO;
            }
        } else if (ep.quality_state === 'completed') {
            // Post-processing-off: torrent finished and the lightweight
            // sweep flipped the tag to 'completed', but the file isn't
            // in media_root (and won't be — post-proc is off). Mirror
            // the server template's `ep.downloaded` branch (line 357 of
            // series.html) which renders ep-row-have + the quality tag
            // even though on_disk is false. Without this branch, a
            // completed row falls through to the missing fallback below
            // and flashes back to "Missing" mid-poll.
            row.classList.remove('ep-row-missing', 'ep-row-unaired', 'ep-row-queued');
            row.classList.add('ep-row-have');
            if (statusCell) statusCell.innerHTML = STATUS_ICON_HAVE;
            qualityCell.innerHTML = `<span class="tag tag-quality">${escHtml(ep.quality || 'UNKNOWN')}</span>`;
            delete qualityCell.dataset.originalHtml;
        } else if (ep.quality_state === 'failed') {
            // Failed stays red even on an unaired episode — a grab
            // existed for it, so there's something to act on.
            row.classList.remove('ep-row-queued', 'ep-row-have', 'ep-row-unaired');
            row.classList.add('ep-row-missing');
            if (statusCell) statusCell.innerHTML = STATUS_ICON_MISSING;
            qualityCell.innerHTML = `<span class="tag tag-quality-failed">${escHtml(ep.quality || '')} ✗</span>`;
            delete qualityCell.dataset.originalHtml;
        } else {
            // Neither on disk, grabbed, completed, nor failed — missing.
            // The tag was cleared server-side (cancel-pending, the 30s
            // stale-grab reconcile in episode_download_progress when a
            // torrent vanished from the download client externally, or
            // a downloads-page delete on a queued grab). Clear the
            // progress bar HTML unconditionally — the prior `if (force)`
            // gate left an orphaned 0% bar layered under the
            // ep-row-missing class until the next page refresh, which
            // is the bug the user reports as "stays at 0% after
            // deleting in qBit / on the downloads page."
            row.classList.remove('ep-row-have', 'ep-row-queued');
            // Unaired-vs-missing split mirrors the server template:
            // neutral state for episodes that haven't aired yet, red
            // Missing for aired-but-absent.
            if (ep.unaired) {
                row.classList.remove('ep-row-missing');
                row.classList.add('ep-row-unaired');
                if (statusCell) statusCell.innerHTML = STATUS_ICON_UNAIRED;
                qualityCell.innerHTML = '<span class="ep-unaired-label">Unaired</span>';
            } else {
                row.classList.remove('ep-row-unaired');
                row.classList.add('ep-row-missing');
                if (statusCell) statusCell.innerHTML = STATUS_ICON_MISSING;
                qualityCell.innerHTML = '<span class="ep-missing-label">Missing</span>';
            }
            delete qualityCell.dataset.originalHtml;
        }
    }
}

// Format a byte count to match the server's `services::media::format_size`:
//   X.X GiB    when bytes ≥ 1 GiB
//   N MiB      otherwise (rounded to int)
// Empty string for zero so the season-size span renders invisibly when
// the season has nothing on disk. Diverges from `formatBytes` in
// series_helpers.js (different rounding) — this one is the live mirror
// of the server output so a JS-driven update looks identical to a
// fresh page render. Don't unify them.
function formatSeasonSize(bytes) {
    if (!bytes || bytes <= 0) return '';
    const gb = bytes / (1024 * 1024 * 1024);
    if (gb >= 1) return gb.toFixed(1) + ' GiB';
    const mb = bytes / (1024 * 1024);
    return Math.round(mb) + ' MiB';
}

// Recompute the "N / total" season badge AND the total-size span at
// the top of the episodes table from the server's episode list.
// Without the size sync, a download landing mid-session updated the
// row and the badge but left the total-bytes display stuck at the
// page-load value until refresh.
function updateSeasonSummary(episodes) {
    const onDisk = episodes.filter(ep => ep.on_disk).length;
    const total = episodes.length;
    const badge = document.querySelector('.season-header-left .season-badge');
    if (badge) {
        if (total > 0) {
            badge.textContent = `${onDisk} / ${total}`;
        } else {
            badge.textContent = `${onDisk} files`;
        }
        badge.classList.remove('season-badge-complete', 'season-badge-partial', 'season-badge-missing');
        if (total > 0 && onDisk >= total) {
            badge.classList.add('season-badge-complete');
        } else if (onDisk > 0) {
            badge.classList.add('season-badge-partial');
        } else {
            badge.classList.add('season-badge-missing');
        }
    }

    // Sum size_bytes across episodes that are on disk. Episodes with
    // size_bytes = 0 (not on disk yet, or pre-1.6 server response) are
    // included but contribute zero — same as on the server side.
    const sizeSpan = document.querySelector('.season-header-left .season-size');
    if (sizeSpan) {
        let totalBytes = 0;
        for (const ep of episodes) {
            if (typeof ep.size_bytes === 'number' && ep.size_bytes > 0) {
                totalBytes += ep.size_bytes;
            }
        }
        sizeSpan.textContent = formatSeasonSize(totalBytes);
    }
}

// Start the download-progress poller if it isn't already running. Called
// after any manual grab so newly-queued progress bars start ticking without
// waiting for a page reload.
function ensureDlPollRunning() {
    if (!SD.dbId || dlPoll.active) return;
    dlPoll.active = true;
    pollDownloadProgress();
}

// The poller schedules itself: 5s while anything is downloading, a row
// still needs patching, or the episode modal is open on an episode the
// client still holds (its grab history shows the live state); it stops
// when nothing is left. Imported episodes whose torrent is only seeding
// don't keep it running on their own: nothing in the table shows them.
function scheduleNextPoll(delay) {
    if (dlPoll.timer) clearTimeout(dlPoll.timer);
    dlPoll.timer = setTimeout(pollDownloadProgress, delay);
}

function stopDlPoll() {
    dlPoll.active = false;
    if (dlPoll.timer) {
        clearTimeout(dlPoll.timer);
        dlPoll.timer = null;
    }
}

function pollDownloadProgress() {
    if (!SD.dbId) return;
    fetch(`/api/series/${SD.id}/download-progress`)
        .then(r => r.ok ? r.json() : [])
        .then(items => {
            items = items || [];
            const epMap = {};
            for (const item of items) {
                // A pending grab (an upgrade in flight) outranks the
                // imported torrent still seeding for the same episode.
                const cur = epMap[item.episode];
                if (!cur || (cur.imported && !item.imported)) epMap[item.episode] = item;
            }
            dlPoll.lastItems = epMap;
            // Every item by hash: the modal's grab history matches its
            // rows to the client this way, so an imported torrent still
            // seeding shows as such even while an upgrade downloads.
            const byHash = {};
            for (const item of items) if (item.hash) byHash[item.hash.toLowerCase()] = item;
            dlPoll.lastByHash = byHash;

            const rows = document.querySelectorAll('.episode-table tbody tr');
            let needsRefresh = false;

            for (const row of rows) {
                const numCell = row.querySelector('.ep-col-num');
                if (!numCell) continue;
                const ep = parseInt(numCell.textContent.trim());
                const qualityCell = row.querySelector('.ep-col-quality');
                if (!qualityCell) continue;

                const item = epMap[ep];
                const showingProgress = qualityCell.querySelector('.dl-progress-wrap') !== null;

                if (item && item.imported) {
                    // The file is in the library; the torrent is only
                    // still in the client. The row shows its quality tag
                    // as usual; if it is still showing the bar from the
                    // download, the import landed since the last tick.
                    if (showingProgress) needsRefresh = true;
                } else if (item) {
                    if (!qualityCell.dataset.originalHtml) {
                        qualityCell.dataset.originalHtml = qualityCell.innerHTML;
                    }
                    const kind = item.state_kind || '';
                    const isComplete = kind.startsWith('seeding') || kind === 'paused-complete'
                        || kind === 'checking-seed' || item.progress >= 1.0;
                    if (isComplete) {
                        if (window.POST_PROCESSING_ENABLED) {
                            // Torrent finished but post-processing hasn't imported
                            // the release into the library yet. Show a full bar with
                            // an "Importing..." label so the user isn't staring at a
                            // stale 0.0% until the next post-processing tick.
                            qualityCell.innerHTML = '<div class="dl-progress-wrap"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:100%"></div></div><span class="dl-progress-text">Importing…</span></div>';
                        } else {
                            // Post-processing is off — there is no import step to
                            // wait on. The lightweight post-proc sweep
                            // (advance_state_without_import) flips the tag state
                            // to 'completed' on the next tick; trigger a refresh
                            // so the checkmark appears without flashing a
                            // spurious "Importing…" that'll never advance.
                            needsRefresh = true;
                        }
                    } else {
                        const pct = (item.progress * 100).toFixed(1);
                        // A paused download keeps its bar but says so; the
                        // bare percentage read as "still going" for as long
                        // as the torrent sat paused in the client.
                        const paused = kind === 'paused';
                        const text = paused ? 'Paused · ' + pct + '%' : pct + '%';
                        qualityCell.innerHTML = `<div class="dl-progress-wrap${paused ? ' dl-progress-paused' : ''}"><div class="dl-progress-bar"><div class="dl-progress-fill" style="width:${pct}%"></div></div><span class="dl-progress-text">${text}</span></div>`;
                    }
                } else if (showingProgress) {
                    // Row was showing a progress bar but the download is no
                    // longer in the response — fetch fresh episode state.
                    needsRefresh = true;
                }
            }

            // The open episode modal's grab history shows the same item.
            const modalWatching = typeof syncGrabHistoryState === 'function' && syncGrabHistoryState(_currentEpNum);

            if (needsRefresh) {
                refreshEpisodeRows();
            }

            // Next tick: fast while anything is in flight or a row still
            // needs patching, slow when only seeding torrents remain,
            // and stop once everything is idle.
            const stillShowingProgress = document.querySelector('.episode-table tbody .ep-col-quality .dl-progress-wrap') !== null;
            const anyPending = items.some(i => !i.imported);
            if (anyPending || needsRefresh || stillShowingProgress || modalWatching) {
                scheduleNextPoll(5000);
            } else {
                stopDlPoll();
            }
        })
        .catch(() => {});
}

// Start polling if the series is tracked. The 5s interval polls the
// per-series download-progress endpoint to update the in-row queue
// progress bars; `pollDownloadProgress` clears its own interval when
// nothing is downloading. Wrapped in a singleton-clear pattern: hx-boost
// body-swap re-runs this script on every nav-back to a series page,
// and a fresh `setInterval` here without clearing the previous one
// would stack — N visits = N parallel pollers all hitting the same
// API every 5s. The `dlPoll` accumulator lives on `window` (see the
// declaration above), so the prior visit's `dlPoll.timer` survives
// the script's re-execution and the singleton-clear can cancel it
// before starting the new poller.
if (SD.dbId) {
    if (dlPoll.timer) {
        clearTimeout(dlPoll.timer);
        dlPoll.timer = null;
    }
    dlPoll.active = true;
    pollDownloadProgress();
}
