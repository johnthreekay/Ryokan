// Interactive search: per-episode + per-series-batch flows. Both
// share the same `#isearch-modal` element so the user only sees one
// modal style; the difference is which endpoint feeds the table and
// where the Grab button posts. Also owns the score-breakdown
// expander positioning logic (the `.score-details` panel needs
// `position: fixed` lifting when its parent has `overflow:hidden`).
//
// Issue #166 — the table render and the score-breakdown expander
// moved server-side to `templates/partials/series/interactive_search_table.html`.
// JS now opens the modal + kicks off the swap via `htmx.ajax`; the
// per-row Grab buttons carry the full SearchResult JSON in
// `data-result` so the click handlers can read it without keeping a
// `_isearchResults` / `_ibatchResults` JS array in lockstep with the
// rendered DOM. `parseQualityFromTitle` (~30 lines), `renderPeers`,
// and `renderScoreDetails` (~30 lines) are gone — Askama auto-escape
// replaces the manual `escHtml` calls those helpers depended on.

// The score-breakdown expander handling (close on outside click /
// Escape, fixed positioning inside an overflow-clipping modal) lives
// in base.js: the same <details class="score-details"> renders on the
// search page and in the Wanted page's interactive-search modal.

function searchBatchReleases(btn) {
    setBusyButton(btn, true, 'Searching…');
    const pid = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId: pid,
        kind: 'info',
        category: 'auto_search',
        title: 'Searching for batch releases',
        body: SD.titleEnglish || SD.titleRomaji || '',
    });
    fetch(`/api/series/${SD.id}/search-batch?progress_id=${encodeURIComponent(pid)}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'}
    })
    .then(async resp => {
        let data = {};
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) throw new Error(data.message || (resp.status === 404 ? 'No batch release found' : 'Batch search failed'));
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
            title: 'Batch search failed',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function openInteractiveSearch(epNum, btn) {
    const modal = document.getElementById('isearch-modal');
    const titleEl = document.getElementById('isearch-title');
    const body = document.getElementById('isearch-body');
    // ASCII separator: house style bans em dashes in user-facing text.
    titleEl.textContent = `Interactive Search: Episode ${epNum}`;
    body.innerHTML = '<div class="isearch-loading"><span class="isearch-loading-spinner" aria-hidden="true"></span><span>Searching indexers for episode ' + epNum + '</span></div>';
    modal.style.display = 'flex';

    // HTMX-driven swap (issue #166). `htmx.ajax` sends `HX-Request: true`,
    // so the handler returns the rendered partial; HTMX swaps it into
    // `#isearch-body`. Per-row Grab buttons carry their full result JSON
    // in `data-result` for `grabInteractiveResult` to read on click.
    window.htmx.ajax('GET', `/api/series/${SD.id}/interactive-search/${epNum}`, {
        target: '#isearch-body',
        swap: 'innerHTML',
    }).catch(function () {
        body.innerHTML = '<div style="text-align:center;color:var(--red);padding:32px">Search failed</div>';
    });
}

function grabInteractiveResult(epNum, btn) {
    var result;
    try { result = JSON.parse(btn.dataset.result || '{}'); } catch (_) { result = null; }
    if (!result) return;
    const url = result.magnet || result.torrent || '';

    // Issue #83 — batch releases open the file-picker modal so the
    // user can narrow to the episodes they actually want. Single-file
    // releases always take the direct /api/series/.../grab path
    // (nothing to pick). `grab_preview_mode = 'never'` opts out
    // globally and keeps 1.3.0-style one-click behavior.
    const previewMode = window.GRAB_PREVIEW_MODE || 'batches_only';
    if (result.is_batch
        && previewMode !== 'never'
        && typeof window.openGrabPicker === 'function'
        && result.info_hash) {
        window.openGrabPicker(url, {
            title: result.title || '',
            size: result.size || '',
            seeders: Number(result.seeders) || 0,
            group: result.group || '',
            infoHash: result.info_hash || '',
            seriesId: SD.dbId || null,
            isBatch: true,
            onConfirm: function () {
                updateEpisodeRow(epNum, 'grabbed', result.group);
                ensureDlPollRunning();
                refreshEpisodeRows({ force: true });
                const ismodal = document.getElementById('isearch-modal');
                if (ismodal) ismodal.style.display = 'none';
            },
        });
        return;
    }

    btn.disabled = true;
    btn.textContent = 'Grabbing…';
    fetch(`/api/series/${SD.id}/grab/${epNum}`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            url,
            title: result.title,
            group: result.group,
            resolution: result.resolution,
            info_hash: result.info_hash,
            size_bytes: result.size_bytes || 0,
            indexer_id: result.indexer_id ?? null,
            match_provenance: result.match_provenance ?? null
        })
    })
    .then(async r => {
        // The server returns errors as `(StatusCode, String)`, which axum
        // serializes as plain-text bodies — NOT JSON. Reading r.json()
        // first returns `{}`, dropping the actual server error and leaving
        // the user with an unhelpful "Grab failed" toast (the JS-side
        // fallback). Read text first, parse JSON only on success
        // responses where the handler returns a JSON envelope.
        const text = await r.text();
        if (!r.ok) {
            throw new Error(text && text.trim().length > 0 ? text : 'Grab failed');
        }
        let data = {};
        try { data = JSON.parse(text); } catch (_) {}
        btn.textContent = 'Sent';
        btn.classList.add('btn-success');
        // Update the episode row to show grabbed state
        updateEpisodeRow(epNum, 'grabbed', result.group);
        ensureDlPollRunning();
        refreshEpisodeRows({ force: true });
        window.ryokanToast({
            kind: 'success',
            category: 'grab',
            title: `Episode ${epNum} queued`,
            body: result.title + (result.group ? ' · ' + result.group : ''),
        });
        // Close the modal after a short delay
        setTimeout(() => {
            document.getElementById('isearch-modal').style.display = 'none';
        }, 600);
    })
    .catch(err => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        btn.disabled = false;
        window.ryokanToast({
            kind: 'error',
            category: 'grab',
            title: `Grab failed for episode ${epNum}`,
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function closeInteractiveSearch(e) {
    const modal = document.getElementById('isearch-modal');
    if (e && e.target !== modal) return;
    modal.style.display = 'none';
}

// ── Interactive batch search ───────────────────────────────────────
// Parallel flow to openInteractiveSearch but for batch releases.
// Shares the isearch-modal element so the UI only has one modal to
// style. The results render is nearly identical but routes its Grab
// action to /grab-batch instead of the per-episode /grab endpoint.

function openInteractiveBatchSearch(btn) {
    const modal = document.getElementById('isearch-modal');
    const titleEl = document.getElementById('isearch-title');
    const body = document.getElementById('isearch-body');
    titleEl.textContent = 'Interactive Batch Search';
    body.innerHTML = '<div class="isearch-loading"><span class="isearch-loading-spinner" aria-hidden="true"></span><span>Searching indexers for batch releases</span></div>';
    modal.style.display = 'flex';

    window.htmx.ajax('GET', `/api/series/${SD.id}/interactive-search-batch`, {
        target: '#isearch-body',
        swap: 'innerHTML',
    }).catch(function () {
        body.innerHTML = '<div style="text-align:center;color:var(--red);padding:32px">Search failed</div>';
    });
}

function grabInteractiveBatchResult(btn) {
    var result;
    try { result = JSON.parse(btn.dataset.result || '{}'); } catch (_) { result = null; }
    if (!result) return;
    const url = result.magnet || result.torrent || '';

    // Issue #83 — every result in the interactive batch search is a
    // batch by definition, so the file-picker modal opens unless the
    // user has opted out globally via `grab_preview_mode = 'never'`.
    const previewMode = window.GRAB_PREVIEW_MODE || 'batches_only';
    if (previewMode !== 'never'
        && typeof window.openGrabPicker === 'function'
        && result.info_hash) {
        window.openGrabPicker(url, {
            title: result.title || '',
            size: result.size || '',
            seeders: Number(result.seeders) || 0,
            group: result.group || '',
            infoHash: result.info_hash || '',
            seriesId: SD.dbId || null,
            isBatch: true,
            onConfirm: function () {
                ensureDlPollRunning();
                refreshEpisodeRows({ force: true });
                const ismodal = document.getElementById('isearch-modal');
                if (ismodal) ismodal.style.display = 'none';
            },
        });
        return;
    }

    btn.disabled = true;
    btn.textContent = 'Grabbing…';
    fetch(`/api/series/${SD.id}/grab-batch`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            url,
            title: result.title,
            group: result.group,
            resolution: result.resolution,
            info_hash: result.info_hash,
            size_bytes: result.size_bytes || 0,
            indexer_id: result.indexer_id ?? null,
            match_provenance: result.match_provenance ?? null
        })
    })
    .then(async r => {
        // Same error-surfacing pattern as the single-episode grab —
        // axum's `(StatusCode, String)` errors come back as plain text,
        // not JSON, so reading r.json() first drops the actual server
        // message.
        const text = await r.text();
        if (!r.ok) {
            throw new Error(text && text.trim().length > 0 ? text : 'Grab failed');
        }
        btn.textContent = 'Sent';
        btn.classList.add('btn-success');
        ensureDlPollRunning();
        refreshEpisodeRows({ force: true });
        window.ryokanToast({
            kind: 'success',
            category: 'grab',
            title: 'Batch queued',
            body: result.title + (result.group ? ' · ' + result.group : ''),
        });
        setTimeout(() => {
            document.getElementById('isearch-modal').style.display = 'none';
        }, 600);
    })
    .catch(err => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        btn.disabled = false;
        window.ryokanToast({
            kind: 'error',
            category: 'grab',
            title: 'Batch grab failed',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}
