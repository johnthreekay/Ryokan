// Episode-detail modal: history, mark-failed, cancel-pending, and
// the cross-file-shared `_currentEpNum` cursor used by the modal-
// footer button-sync helpers in series.js. Per-row Delete File button
// is wired declaratively via `hx-post` (URL set per-modal-open in
// `showEpisodeDetail`); the `ryokan-episode-deleted` listener at the
// bottom handles the row update + toast.

// Shared cursor for the currently-open modal — also read by the
// modal-footer sync helpers in series.js (`syncCancelPendingButton`,
// `syncDeleteFileButton`) so a poll-tick that flips the row state
// can update the buttons live without the modal re-opening.
var _currentEpNum = null;

function showEpisodeDetail(epNum, btn) {
    const filename = btn.dataset.filename || '';
    const size = btn.dataset.size || '';
    const onDisk = btn.dataset.onDisk === 'true';
    const modal = document.getElementById('ep-detail-modal');
    const titleEl = document.getElementById('ep-detail-title');
    const body = document.getElementById('ep-detail-body');
    const deleteBtn = document.getElementById('btn-delete-file');
    const cancelBtn = document.getElementById('btn-cancel-pending');
    _currentEpNum = epNum;
    titleEl.textContent = 'Episode ' + epNum;

    // Reset both footer buttons before toggling visibility so a button
    // that was mid-request when its modal was closed shows up clean in
    // the next episode's modal.
    resetFooterButton(deleteBtn);
    resetFooterButton(cancelBtn);

    // Delete-file and Cancel-Pending button visibility are both synced
    // after the modal is shown (below). The actual logic lives in
    // `syncDeleteFileButton` / `syncCancelPendingButton` (series.js)
    // so updateEpisodeRow / patchEpisodeRows can re-run them after
    // they mutate the row class — without that, a download finishing
    // mid-modal-open would leave the Delete File button hidden (it
    // was set from the stale modal-open dataset.onDisk), and a
    // Mark-Failed → re-grab cycle would leave Cancel Pending hidden
    // for the same reason.

    // Two stable slots the grab-history loader patches in place once
    // data arrives: the library-side file path (media_root-relative,
    // rendered here when on_disk) and the download-client-side
    // content_path (rendered by renderGrabHistory if the current grab
    // has a client_content_path). Both can coexist when post-processing
    // uses hardlinks — the user wants to see both; the Sonarr dual-path
    // split (#14 follow-up) makes this possible.
    const mediaRoot = document.getElementById('series-data').dataset.mediaRoot || '';
    const folderName = document.getElementById('series-data').dataset.folderName || '';
    const libraryPath = (onDisk && filename)
        ? [mediaRoot, folderName, filename].filter(Boolean).join('/')
        : '';
    body.innerHTML =
        '<div class="ep-detail-two-col">' +
        (libraryPath
            ? '<div class="ep-detail-row ep-detail-full"><span class="ep-detail-label">Library path</span><span class="ep-detail-value ep-detail-path">' + escHtml(libraryPath) + '</span></div>'
            : '<div class="ep-detail-row ep-detail-full" id="ep-detail-library-placeholder"><span class="ep-detail-label">Library path</span><span class="ep-detail-value" style="color:var(--text-dim)">Not in library root</span></div>') +
        // Client path row is always rendered as a placeholder;
        // renderGrabHistory fills it in when the current grab has a
        // client_content_path. Hidden until then so empty state doesn't
        // render for rows whose torrent hasn't finished downloading yet.
        '<div class="ep-detail-row ep-detail-full" id="ep-detail-client-path" style="display:none"><span class="ep-detail-label">Output path</span><span class="ep-detail-value ep-detail-path" id="ep-detail-client-path-value"></span></div>' +
        // The Size row starts with the on-disk file size and is
        // patched in place by `renderGrabHistory` once the grab
        // history loads: if the latest grab was a batch, the row is
        // rewritten to show the whole-torrent total with a
        // "(batch total)" hint. Always rendered (even when size is
        // empty) so the batch-patch has a stable target to find.
        '<div class="ep-detail-row"><span class="ep-detail-label">Size</span><span class="ep-detail-value ep-detail-size-value">' + escHtml(size || '—') + '</span></div>' +
        '</div>' +
        '<div class="ep-detail-row" id="grab-history-section" style="margin-top:16px"><span class="ep-detail-label">Grab History</span><div id="grab-history-body" style="margin-top:6px;color:var(--text-dim);font-size:12px">Loading…</div></div>';
    modal.style.display = 'flex';
    // Now that the modal is open, sync the footer Delete File and
    // Cancel-Pending button visibilities. Both helpers gate on
    // `modal.style.display === 'flex'`, so calling them before the
    // display flip would be a no-op. They're shared with the
    // refresh path so a download finishing mid-modal-open updates
    // both buttons live.
    syncDeleteFileButton(epNum);
    syncCancelPendingButton(epNum);
    // Ask the download client once now, so the grab history's State
    // column shows "seeding" / "paused" on open even when the poller
    // is idle; the poller keeps ticking while the modal stays open.
    _currentEntries = null;
    if (SD.dbId && typeof pollDownloadProgress === 'function') pollDownloadProgress();

    // Load grab history
    if (SD.dbId) {
        fetch(`/api/series/${SD.id}/grab-history/${epNum}`)
            .then(r => r.json())
            .then(entries => renderGrabHistory(entries, epNum))
            .catch(() => {
                const el = document.getElementById('grab-history-body');
                if (el) el.textContent = 'No history.';
            });
    }
}

// Grab-history entries of the open modal, kept so a poll tick can
// re-render the State column with the client's live state without
// refetching the history.
var _currentEntries = null;

// The live client item for each history row, keyed by row id. A row
// matches by torrent hash when both sides carry one, else the pending
// item goes to the `grabbed` row and the imported item to the
// `completed` row. Each client item is handed to one row only, the
// newest (entries arrive newest first): the same torrent can have an
// older row that is a record of the grab, not the torrent's present.
function assignLiveItems(entries, epNum) {
    const byHash = dlPoll.lastByHash || {};
    const forEpisode = (dlPoll.lastItems || {})[epNum] || null;
    const used = new Set();
    const out = new Map();
    for (const entry of entries) {
        const hash = (entry.grab_hash || '').toLowerCase();
        let item = null;
        if (hash && byHash[hash]) {
            item = byHash[hash];
        } else if (forEpisode && !hash) {
            if (entry.state === 'grabbed' && !forEpisode.imported) item = forEpisode;
            if (entry.state === 'completed' && forEpisode.imported) item = forEpisode;
        }
        if (!item || used.has(item.hash)) continue;
        used.add(item.hash);
        out.set(entry.id, item);
    }
    return out;
}

// The State cell: the client's word when it still holds the torrent
// ("seeding", "paused"), else the grab's own state; a dim second line
// carries progress, speed, or why it is paused.
function grabStateCell(entry, item) {
    const dbClass = entry.state === 'failed' ? 'grab-state-failed'
        : entry.state === 'removed' ? 'grab-state-removed'
        : entry.state === 'replaced' ? 'grab-state-replaced'
        : entry.state === 'completed' ? 'grab-state-completed'
        : 'grab-state-grabbed';
    if (!item) return { cls: dbClass, text: entry.state, title: '', detail: '' };
    const label = clientStateLabel(item);
    const pct = (item.progress * 100).toFixed(1) + '%';
    if (label.key === 'seeding') {
        return {
            cls: 'grab-state-seeding', text: 'seeding',
            title: item.imported ? 'Imported; the torrent is still seeding in the download client' : 'Download complete; seeding while it waits for import',
            detail: item.imported ? '' : (window.POST_PROCESSING_ENABLED ? 'waiting for import' : 'complete'),
        };
    }
    if (label.key === 'paused') {
        return {
            cls: 'grab-state-paused', text: 'paused',
            title: item.seeding_done ? 'Stopped by the download client at its seeding limit' : 'Paused in the download client',
            detail: item.seeding_done ? 'seeding limit reached' : (item.imported || item.progress >= 1 ? '' : 'at ' + pct),
        };
    }
    if (label.key === 'error') {
        return { cls: 'grab-state-failed', text: 'error', title: 'The download client reports an error for this torrent', detail: '' };
    }
    // Still downloading: the grab's own state, with the client's numbers under it.
    let detail = label.key === 'downloading' ? pct : label.text.toLowerCase() + ' at ' + pct;
    if (label.key === 'downloading' && item.speed > 0) detail += ' · ' + formatBytes(item.speed) + '/s';
    return { cls: dbClass, text: entry.state, title: '', detail: detail };
}

// Re-render the open modal's State cells from the poller's latest
// response. Returns true when the modal is open on an episode the
// client still holds, so the poller keeps its 5s cadence for it.
function syncGrabHistoryState(epNum) {
    const modal = document.getElementById('ep-detail-modal');
    if (!modal || modal.style.display !== 'flex' || epNum == null || epNum !== _currentEpNum) return false;
    if (!_currentEntries) return !!(dlPoll.lastItems || {})[epNum];
    const live = assignLiveItems(_currentEntries, epNum);
    for (const cell of document.querySelectorAll('#grab-history-body td[data-history-id]')) {
        const entry = _currentEntries.find(function(e) { return String(e.id) === cell.dataset.historyId; });
        if (!entry) continue;
        const st = grabStateCell(entry, live.get(entry.id) || null);
        cell.className = st.cls;
        cell.title = st.title;
        cell.innerHTML = escHtml(st.text) + (st.detail ? '<div class="grab-state-detail">' + escHtml(st.detail) + '</div>' : '');
    }
    return live.size > 0;
}

function renderGrabHistory(entries, epNum) {
    const el = document.getElementById('grab-history-body');
    if (!el) return;
    _currentEntries = entries || [];
    if (!entries || !entries.length) {
        el.textContent = 'No grab history.';
        return;
    }
    // Table lives inside a scroll container so history past 10 entries
    // scrolls within the modal rather than blowing out modal height.
    const live = assignLiveItems(entries, epNum);
    let html = '<div class="grab-history-scroll"><table class="grab-history-table"><thead><tr><th>Quality</th><th>Release</th><th>File Name</th><th>Group</th><th>Size</th><th>Date</th><th>State</th><th></th></tr></thead><tbody>';
    for (const e of entries) {
        // The State cell says what the download client is doing with
        // this torrent while it still has it (seeding, paused, the
        // download's progress); the grab's own state otherwise.
        const st = grabStateCell(e, live.get(e.id) || null);
        // Only active 'grabbed' rows expose the Mark Failed action.
        // The per-row Cancel button used to live alongside it as a
        // workaround for the modal-footer Cancel Pending button
        // hiding when the row's `ep-row-queued` class lagged behind
        // reality; that desync is now fixed at the source (template
        // renders `ep-row-queued` for grabbed episodes and
        // `syncCancelPendingButton` re-runs on every refresh patch),
        // so the per-row Cancel was redundant — removed 2026-05-03.
        // Mark Failed stays here because it has no equivalent in
        // the modal footer: it flags the grab as failed in the DB
        // without touching the download client, for the rare drift
        // case where the torrent is fine but the grab is silently
        // dead.
        const canFail = e.state === 'grabbed';
        // File name column: shows the post-processed on-disk basename
        // once post-processing lands the file. Before that it's still
        // seeded with the release title, so hide the duplicate — the
        // Release column already carries that.
        const fileName = e.file_name && e.file_name.length ? e.file_name : e.release_title;
        const sameAsRelease = fileName === e.release_title;
        const fileCell = sameAsRelease
            ? '<span style="color:var(--text-dim)">—</span>'
            : escHtml(fileName);
        // Size column: for batch grabs it's the whole-torrent total
        // (suffixed with a dim " (batch)" hint) so the user can tell
        // it's a pack size rather than a per-episode size.
        const sizeText = formatBytes(e.size_bytes);
        const sizeCell = sizeText
            ? (e.is_batch
                ? escHtml(sizeText) + ' <span style="color:var(--text-dim);font-size:10px">(batch)</span>'
                : escHtml(sizeText))
            : '';
        html += `<tr>
            <td>${escHtml(e.quality_tag)}</td>
            <td class="grab-history-ellipsis" title="${escHtml(e.release_title)}">${escHtml(e.release_title)}${e.grab_match_summary ? `<div style="color:var(--text-dim);font-size:10px;white-space:normal">${escHtml(e.grab_match_summary)}</div>` : ''}</td>
            <td class="grab-history-ellipsis" title="${escHtml(fileName)}">${fileCell}</td>
            <td>${escHtml(e.release_group)}</td>
            <td style="white-space:nowrap;color:var(--text-dim)">${sizeCell}</td>
            <td style="white-space:nowrap;color:var(--text-dim)">${escHtml(e.grabbed_at)}</td>
            <td class="${st.cls}" data-history-id="${e.id}"${st.title ? ` title="${escHtml(st.title)}"` : ''}>${escHtml(st.text)}${st.detail ? `<div class="grab-state-detail">${escHtml(st.detail)}</div>` : ''}</td>
            <td>${canFail ? `
                <button class="btn-mark-failed" onclick="markEpisodeFailed(${e.id}, ${epNum}, this)">Mark Failed</button>
            ` : ''}</td>
        </tr>`;
    }
    html += '</tbody></table></div>';
    el.innerHTML = html;

    // Task 24: the episode detail "Size" row above the grab history
    // should reflect the batch total when the latest grab for this
    // episode was a batch, not the per-file on-disk size. We only
    // know this once history loads, so patch the row in-place after
    // the table renders. Find the newest non-failed entry as the
    // current source of truth — a 'completed' row wins over an older
    // 'grabbed' row sitting behind a failed upgrade attempt.
    const current = entries.find(function(e) { return e.state === 'completed' || e.state === 'grabbed'; });
    if (current && current.is_batch && current.size_bytes > 0) {
        const sizeValueEl = document.querySelector('#ep-detail-body .ep-detail-size-value');
        if (sizeValueEl) {
            sizeValueEl.innerHTML = escHtml(formatBytes(current.size_bytes))
                + ' <span style="color:var(--text-dim);font-size:11px">(batch total)</span>';
        }
    }

    // Dual-path display: if the current grab has a client content path
    // (populated by post-processing when the torrent reports complete),
    // reveal the client path row in the detail header. Shown whenever
    // present — with post-proc on + hardlink mode both paths point at
    // the same bytes but are still worth surfacing so the operator can
    // find the torrent in the download client without guessing.
    if (current && current.client_content_path) {
        const clientRow = document.getElementById('ep-detail-client-path');
        const clientValue = document.getElementById('ep-detail-client-path-value');
        if (clientRow && clientValue) {
            clientValue.textContent = current.client_content_path;
            clientRow.style.display = '';
        }
    }
}

function markEpisodeFailed(historyId, epNum, btn) {
    window.ryokanConfirm({
        title: `Mark Episode ${epNum} as Failed`,
        body: 'Mark this grab as failed and re-search for the episode?',
        yesLabel: 'Mark Failed',
        noLabel: 'Cancel',
        extras: [{id: 'blocklist', label: 'Also add this release to the blocklist', default: false}],
    }).then(function(res) {
        if (!res.ok) return;
        const addToBlocklist = !!res.extras.blocklist;
        btn.disabled = true;
        btn.textContent = 'Searching…';
        fetch(`/api/series/${SD.id}/mark-failed/${epNum}`, {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({ history_id: historyId, blocklist: addToBlocklist })
        })
        .then(async r => {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Failed');
            const grabbed = Array.isArray(data.grabbed) ? data.grabbed.length : 0;
            btn.textContent = grabbed > 0 ? 'Re-grabbed' : 'No result';
            if (grabbed > 0) {
                const first = data.grabbed[0];
                updateEpisodeRow(epNum, 'grabbed', first.release_group);
                ensureDlPollRunning();
                refreshEpisodeRows({ force: true });
                window.ryokanToast({
                    kind: 'success',
                    category: 'auto_search',
                    title: `Episode ${epNum} re-grabbed`,
                    body: first.release_title + (first.release_group ? ' · ' + first.release_group : ''),
                });
            } else {
                window.ryokanToast({
                    kind: 'warn',
                    category: 'auto_search',
                    title: `No replacement for episode ${epNum}`,
                    body: 'Nothing on Nyaa matched after marking the current grab as failed.',
                });
            }
            // Refresh the grab history in the modal
            if (SD.dbId) {
                fetch(`/api/series/${SD.id}/grab-history/${epNum}`)
                    .then(r => r.json())
                    .then(entries => renderGrabHistory(entries, epNum))
                    .catch(() => {});
            }
        })
        .catch(err => {
            btn.disabled = false;
            btn.textContent = 'Mark Failed';
            window.ryokanToast({
                kind: 'error',
                category: 'auto_search',
                title: `Mark-failed error for episode ${epNum}`,
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
    });
}

// Per-episode delete is wired declaratively: the `#btn-delete-file`
// in the modal footer carries `hx-post` (URL set per-modal-open in
// `showEpisodeDetail`), `data-ryokan-confirm-*` (routed through the
// htmx:confirm bridge in `base.js`), `hx-on::after:request` (closes
// the modal on success), and `hx-swap="none"` (the empty 200 response
// from the handler has no body to swap — the row update happens via
// the `ryokan-episode-deleted` listener below). The handler emits an
// `HX-Trigger: ryokan-episode-deleted` header on both success and
// failure so a single listener handles toast + row state.
//
// One-shot guard: hx-boost re-evaluates this script on every nav-back
// to a series page. Without the guard, each visit adds another copy of
// the listener and a single delete fires N toasts on the Nth visit
// ("Episode 10 deleted × 7" was the symptom). Same pattern repeated
// at the other module-scope listeners across system.js / settings.js.
if (!window.__ryokanSeriesListeners) {
    window.__ryokanSeriesListeners = true;
    document.body.addEventListener('ryokan-episode-deleted', function (ev) {
        const detail = ev.detail || {};
        const epNum = parseInt(detail.episode_number, 10);
        if (!detail.ok) {
            window.ryokanToast({
                kind: 'error',
                category: 'library',
                title: epNum ? `Delete failed for episode ${epNum}` : 'Delete failed',
                body: detail.message || 'Unknown error',
            });
            return;
        }
        if (epNum) {
            updateEpisodeRow(epNum, 'deleted');
            refreshEpisodeRows({ force: true });
        }
        // Recycle bin (#123): when the file went to the bin the payload
        // carries the entry id, and the toast gets an Undo that restores
        // it in place. Longer duration so there's time to change your mind.
        const entryId = detail.recycle_entry_id;
        window.ryokanToast({
            kind: 'success',
            category: 'library',
            title: epNum
                ? (entryId ? `Episode ${epNum} moved to the recycle bin` : `Episode ${epNum} deleted`)
                : 'Episode deleted',
            body: detail.message || 'File removed from disk.',
            duration: entryId ? 10000 : 4000,
            action: entryId ? {
                label: 'Undo',
                onClick: function (handle) {
                    fetch('/api/library/recycle/' + encodeURIComponent(entryId) + '/restore', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                    })
                        .then(function (r) {
                            return r.json().catch(function () { return { ok: false, message: 'HTTP ' + r.status }; });
                        })
                        .then(function (res) {
                            if (res && res.ok) {
                                handle.update({
                                    kind: 'success',
                                    title: epNum ? `Episode ${epNum} restored` : 'Restored',
                                    body: res.message || 'The file is back where it was.',
                                });
                                if (epNum) refreshEpisodeRows({ force: true });
                            } else {
                                handle.update({ kind: 'error', title: 'Restore failed', body: (res && res.message) || 'Unknown error' });
                            }
                        })
                        .catch(function (e) {
                            handle.update({ kind: 'error', title: 'Restore failed', body: (e && e.message) || 'Network error' });
                        });
                },
            } : undefined,
        });
    });
}

// Cancel an in-flight grab: removes the torrent from qBit (with its
// partial/complete data), marks the grab 'removed' in the DB, clears
// the episode's quality tag. Does NOT trigger a re-search — the user
// wanted to drop this one, not find a replacement. The pending-grab
// equivalent of the (now declarative) per-episode delete flow above.
async function cancelPendingEpisode() {
    const epNum = _currentEpNum;
    if (!epNum) return;
    const confirmed = await window.ryokanConfirm({
        title: 'Cancel pending grab',
        body: `Remove the in-flight torrent for Episode ${epNum} from the download client and mark it cancelled? This will delete any downloaded data and will not trigger a re-search.`,
        yesLabel: 'Cancel grab',
        noLabel: 'Keep',
    });
    if (!confirmed.ok) return;
    const btn = document.getElementById('btn-cancel-pending');
    if (btn) { btn.disabled = true; btn.textContent = 'Cancelling…'; }
    fetch(`/api/series/${SD.id}/cancel-pending/${epNum}`, { method: 'POST', headers: {'Content-Type': 'application/json'} })
        .then(async r => {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Cancel failed');
            document.getElementById('ep-detail-modal').style.display = 'none';
            updateEpisodeRow(epNum, 'deleted');
            refreshEpisodeRows({ force: true });
            window.ryokanToast({
                kind: 'success',
                category: 'library',
                title: `Episode ${epNum} cancelled`,
                body: `${data.cancelled || 0} pending grab(s) removed.`,
            });
        })
        .catch(err => {
            if (btn) { btn.disabled = false; btn.textContent = 'Cancel Pending'; }
            window.ryokanToast({
                kind: 'error',
                category: 'library',
                title: `Cancel failed for episode ${epNum}`,
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
}

function closeEpisodeDetail(e) {
    const modal = document.getElementById('ep-detail-modal');
    if (e && e.target !== modal) return;
    modal.style.display = 'none';
}
