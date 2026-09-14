// /wanted page: "Search selected" / "Search all" / per-row Auto search.
// Each starts a sticky progress toast and POSTs the series ids (and
// the tab, so a cutoff search reaches on-disk episodes) to
// /api/wanted/search, which runs the series' own auto-searches one
// after another in the background and reports on the toast.
//
// Per-row Interactive opens #wanted-isearch-modal, the series page's
// interactive-search shape: the same endpoints with ?from=wanted, so
// the table's Grab buttons come back carrying data-wanted-grab /
// data-wanted-grab-batch for the delegated handlers here instead of
// the series page's inline calls (those read series-page globals).
// A grab posts to the series grab endpoints, a batch goes through the
// file picker (grab_picker.js), and the list re-fetches afterwards.
//
// Document-scope delegated listeners, attached once per process
// (window-flag guard): hx-boost re-executes this file on every visit
// and replaces #wanted-page, so listeners on the page node would be
// lost or doubled. Each handler re-finds the live nodes when it fires.
(function () {
    if (window.__ryokanWantedDocListeners) return;
    window.__ryokanWantedDocListeners = true;

    function idsFrom(selector) {
        return Array.from(document.querySelectorAll(selector))
            .map(function (cb) { return parseInt(cb.value, 10); })
            .filter(function (n) { return n > 0; });
    }

    function activeTab() {
        var list = document.querySelector('.wanted-rows[data-tab]');
        if (list) return list.getAttribute('data-tab');
        var tab = document.querySelector('.wanted-tab.active');
        return tab ? tab.getAttribute('data-tab') : 'missing';
    }

    function startSearch(ids, title, body, emptyMessage) {
        if (!ids.length) {
            window.ryokanToast({
                kind: 'info',
                category: 'auto_search',
                title: 'Nothing to search',
                body: emptyMessage,
            });
            return;
        }
        var pid = window.ryokanNewProgressId();
        var toast = window.ryokanProgressToast({
            progressId: pid,
            kind: 'info',
            category: 'auto_search',
            title: title,
            body: body,
        });
        fetch('/api/wanted/search?progress_id=' + encodeURIComponent(pid), {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ series_ids: ids, tab: activeTab() }),
        })
        .then(async function (resp) {
            var data = {};
            try { data = await resp.json(); } catch (_) {}
            if (!resp.ok) {
                throw new Error(data.message || 'Search failed');
            }
        })
        .catch(function (err) {
            toast.finalize({
                kind: 'error',
                title: 'Search failed',
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
    }

    // ── Interactive search ─────────────────────────────────────
    // What the open modal is about. Re-set on every open; the modal
    // element itself is replaced by boosted navigation, so nothing is
    // cached on it.
    var isearch = { anilistId: 0, seriesId: null, title: '' };

    function isearchModal() { return document.getElementById('wanted-isearch-modal'); }

    function openWantedInteractive(btn) {
        var modal = isearchModal();
        var select = document.getElementById('wanted-isearch-episode');
        var titleEl = document.getElementById('wanted-isearch-title');
        if (!modal || !select || !titleEl) return;
        isearch.anilistId = parseInt(btn.getAttribute('data-wanted-isearch'), 10);
        isearch.seriesId = parseInt(btn.getAttribute('data-series-id'), 10) || null;
        isearch.title = btn.getAttribute('data-wanted-title') || '';
        var eps = (btn.getAttribute('data-wanted-episodes') || '')
            .split(',')
            .map(function (v) { return parseInt(v, 10); })
            .filter(function (n) { return n > 0; });
        titleEl.textContent = 'Interactive search: ' + isearch.title;
        select.innerHTML = '';
        eps.forEach(function (n) {
            var opt = document.createElement('option');
            opt.value = String(n);
            opt.textContent = 'Episode ' + n;
            select.appendChild(opt);
        });
        var batch = document.createElement('option');
        batch.value = 'batch';
        batch.textContent = 'Batch releases';
        select.appendChild(batch);
        select.value = eps.length ? String(eps[0]) : 'batch';
        modal.style.display = 'flex';
        loadWantedInteractive();
    }

    function loadWantedInteractive() {
        var select = document.getElementById('wanted-isearch-episode');
        var body = document.getElementById('wanted-isearch-body');
        if (!select || !body) return;
        var choice = select.value;
        var isBatch = choice === 'batch';
        body.innerHTML = '<div class="isearch-loading"><span class="isearch-loading-spinner" aria-hidden="true"></span><span>'
            + (isBatch ? 'Searching indexers for batch releases' : 'Searching indexers for episode ' + choice)
            + '</span></div>';
        var url = isBatch
            ? '/api/series/' + isearch.anilistId + '/interactive-search-batch?from=wanted'
            : '/api/series/' + isearch.anilistId + '/interactive-search/' + choice + '?from=wanted';
        // htmx.ajax sends HX-Request, so the handler answers with the
        // rendered table partial and htmx swaps it in.
        window.htmx.ajax('GET', url, { target: '#wanted-isearch-body', swap: 'innerHTML' })
            .catch(function () {
                body.innerHTML = '<div class="isearch-empty"><p>Search failed.</p><p class="isearch-empty-hint">Check System &rarr; Logs for the indexer error.</p></div>';
            });
    }

    function closeWantedInteractive() {
        var modal = isearchModal();
        if (modal) modal.style.display = 'none';
    }

    // Re-fetch the current tab's rows: a grabbed episode is
    // downloading now and leaves the list.
    function refreshWantedList() {
        var tab = activeTab();
        window.htmx.ajax('GET', '/wanted?tab=' + encodeURIComponent(tab), {
            target: '#wanted-list',
            swap: 'innerHTML',
        }).catch(function () {});
    }

    // epNum null = the batch flow. Mirrors grabInteractiveResult /
    // grabInteractiveBatchResult on the series page, minus the
    // episode-row updates that page does.
    function grabWantedResult(btn, epNum) {
        var result;
        try { result = JSON.parse(btn.dataset.result || '{}'); } catch (_) { result = null; }
        if (!result) return;
        var url = result.magnet || result.torrent || '';
        var isBatch = epNum === null || !!result.is_batch;
        var previewMode = window.GRAB_PREVIEW_MODE || 'batches_only';
        if (isBatch
            && previewMode !== 'never'
            && typeof window.openGrabPicker === 'function'
            && result.info_hash) {
            window.openGrabPicker(url, {
                title: result.title || '',
                size: result.size || '',
                seeders: Number(result.seeders) || 0,
                group: result.group || '',
                infoHash: result.info_hash || '',
                seriesId: isearch.seriesId,
                isBatch: true,
                onConfirm: function () {
                    closeWantedInteractive();
                    refreshWantedList();
                },
            });
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Grabbing…';
        var endpoint = epNum === null
            ? '/api/series/' + isearch.anilistId + '/grab-batch'
            : '/api/series/' + isearch.anilistId + '/grab/' + epNum;
        fetch(endpoint, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                url: url,
                title: result.title,
                group: result.group,
                resolution: result.resolution,
                info_hash: result.info_hash,
                size_bytes: result.size_bytes || 0,
                indexer_id: result.indexer_id == null ? null : result.indexer_id,
                match_provenance: result.match_provenance == null ? null : result.match_provenance,
            }),
        })
        .then(async function (r) {
            // axum's (StatusCode, String) errors are plain text, so read
            // text first and keep the server's message.
            var text = await r.text();
            if (!r.ok) throw new Error(text && text.trim().length > 0 ? text : 'Grab failed');
            btn.textContent = 'Sent';
            btn.classList.add('btn-success');
            window.ryokanToast({
                kind: 'success',
                category: 'grab',
                title: epNum === null ? 'Batch queued' : 'Episode ' + epNum + ' queued',
                body: result.title + (result.group ? ' · ' + result.group : ''),
            });
            setTimeout(function () {
                closeWantedInteractive();
                refreshWantedList();
            }, 600);
        })
        .catch(function (err) {
            btn.textContent = 'Error';
            btn.classList.add('btn-error');
            btn.disabled = false;
            window.ryokanToast({
                kind: 'error',
                category: 'grab',
                title: epNum === null ? 'Batch grab failed' : 'Grab failed for episode ' + epNum,
                body: err && err.message ? err.message : 'Unknown error',
            });
        });
    }

    document.addEventListener('click', function (ev) {
        if (!document.getElementById('wanted-page')) return;
        var isearchBtn = ev.target.closest('[data-wanted-isearch]');
        if (isearchBtn) { openWantedInteractive(isearchBtn); return; }
        if (ev.target.closest('[data-wanted-isearch-close]')) { closeWantedInteractive(); return; }
        var grab = ev.target.closest('[data-wanted-grab]');
        if (grab) { grabWantedResult(grab, parseInt(grab.getAttribute('data-wanted-grab'), 10)); return; }
        var grabBatch = ev.target.closest('[data-wanted-grab-batch]');
        if (grabBatch) { grabWantedResult(grabBatch, null); return; }
        var bulk = ev.target.closest('[data-wanted-search]');
        if (bulk) {
            var mode = bulk.getAttribute('data-wanted-search');
            var ids = mode === 'all' ? idsFrom('.wanted-select') : idsFrom('.wanted-select:checked');
            startSearch(
                ids,
                mode === 'all' ? 'Searching all wanted series' : 'Searching selected series',
                ids.length + ' series',
                mode === 'all' ? 'Nothing to search on this tab.' : 'Tick the series to search, or use Search all.'
            );
            return;
        }
        var one = ev.target.closest('[data-wanted-search-one]');
        if (one) {
            var id = parseInt(one.getAttribute('data-wanted-search-one'), 10);
            startSearch([id], 'Searching wanted episodes', one.getAttribute('data-wanted-title') || '', '');
        }
    });

    document.addEventListener('change', function (ev) {
        if (!document.getElementById('wanted-page')) return;
        var target = ev.target;
        if (target && target.id === 'wanted-select-all') {
            var on = target.checked;
            document.querySelectorAll('.wanted-select').forEach(function (cb) { cb.checked = on; });
        } else if (target && target.classList && target.classList.contains('wanted-select') && !target.checked) {
            var master = document.getElementById('wanted-select-all');
            if (master) master.checked = false;
        } else if (target && target.id === 'wanted-isearch-episode') {
            loadWantedInteractive();
        }
    });

    // Escape closes the interactive search modal, unless the file
    // picker is open on top of it (grab_picker.js closes that one).
    document.addEventListener('keydown', function (ev) {
        if (ev.key !== 'Escape') return;
        var modal = isearchModal();
        if (!modal || modal.style.display === 'none') return;
        var picker = document.getElementById('grab-picker-modal');
        if (picker && picker.style.display !== 'none') return;
        closeWantedInteractive();
    });

    // A tab click swaps #wanted-list only; htmx does not move
    // `.active`, so follow the clicked tab (or the URL) here and reset
    // the select-all box the fresh rows no longer match.
    document.body.addEventListener('htmx:after:swap', function (ev) {
        if (window.ryokanSwapTargetId(ev) !== 'wanted-list') return;
        try {
            var src = ev.detail && ev.detail.ctx && ev.detail.ctx.sourceElement;
            var tab = src && src.dataset && src.dataset.tab;
            if (!tab) {
                var url = new URL(window.location.href);
                tab = url.searchParams.get('tab') || 'missing';
            }
            document.querySelectorAll('.wanted-tab').forEach(function (a) {
                var on = a.dataset.tab === tab;
                a.classList.toggle('active', on);
                a.setAttribute('aria-selected', on ? 'true' : 'false');
            });
            var master = document.getElementById('wanted-select-all');
            if (master) master.checked = false;
        } catch (_) {}
    });
})();
