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
// The "Search for" dropdown is server-rendered (/wanted/search-menu):
// batch releases, the whole wanted set in one search that leaves
// batches out, or one episode. A grab posts to the series grab
// endpoints; a batch goes through the file picker (grab_picker.js)
// with the wanted episodes, so only their files start checked; the
// list re-fetches afterwards.
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
    // What the open modal is about: the series, and every episode the
    // row wants (read off the menu once it arrives), which the batch
    // grab hands to the file picker.
    var isearch = { anilistId: 0, seriesId: null, title: '', episodes: [] };

    function isearchModal() { return document.getElementById('wanted-isearch-modal'); }

    function loadingHtml(message) {
        return '<div class="isearch-loading"><span class="isearch-loading-spinner" aria-hidden="true"></span><span>'
            + message + '</span></div>';
    }

    function emptyHtml(message, hint) {
        return '<div class="isearch-empty"><p>' + message + '</p>'
            + (hint ? '<p class="isearch-empty-hint">' + hint + '</p>' : '') + '</div>';
    }

    function openWantedInteractive(btn) {
        var modal = isearchModal();
        var pick = document.getElementById('wanted-isearch-pick');
        var titleEl = document.getElementById('wanted-isearch-title');
        var body = document.getElementById('wanted-isearch-body');
        if (!modal || !pick || !titleEl || !body) return;
        isearch.anilistId = parseInt(btn.getAttribute('data-wanted-isearch'), 10);
        isearch.seriesId = parseInt(btn.getAttribute('data-series-id'), 10) || null;
        isearch.title = btn.getAttribute('data-wanted-title') || '';
        isearch.episodes = [];
        titleEl.textContent = 'Interactive search: ' + isearch.title;
        closeDropdowns();
        pick.innerHTML = '';
        body.innerHTML = loadingHtml('Loading');
        modal.style.display = 'flex';
        var url = '/wanted/search-menu?series_id=' + encodeURIComponent(isearch.seriesId || 0)
            + '&tab=' + encodeURIComponent(activeTab());
        // The menu is server-rendered; its default item names the first
        // search to run (the whole wanted set, or the one episode).
        window.htmx.ajax('GET', url, { target: '#wanted-isearch-pick', swap: 'innerHTML' })
            .then(function () { startFromMenu(pick, body); })
            .catch(function () { startFromMenu(pick, body); });
    }

    function startFromMenu(pick, body) {
        var items = pick.querySelectorAll('[data-isearch-choice="episode"]');
        isearch.episodes = Array.from(items)
            .map(function (b) { return parseInt(b.getAttribute('data-episode'), 10); })
            .filter(function (n) { return n > 0; });
        var def = pick.querySelector('[data-default]') || pick.querySelector('.dropdown-item');
        if (def) {
            pickWantedChoice(def);
        } else {
            body.innerHTML = emptyHtml('Nothing is wanted for this series on this tab.', 'Reload the page to refresh the list.');
        }
    }

    // A menu item was picked: mark it, show its label on the trigger,
    // and run the search it names.
    function pickWantedChoice(item) {
        var menu = item.closest('.dropdown-menu');
        if (menu) menu.querySelectorAll('.dropdown-item').forEach(function (b) { b.classList.toggle('active', b === item); });
        var pick = document.getElementById('wanted-isearch-pick');
        var label = pick && pick.querySelector('[data-dropdown-label]');
        if (label) label.textContent = item.getAttribute('data-label') || item.textContent.trim();
        closeDropdowns();
        var kind = item.getAttribute('data-isearch-choice');
        var base = '/api/series/' + isearch.anilistId;
        if (kind === 'batch') {
            loadWantedSearch(base + '/interactive-search-batch?from=wanted', 'Searching indexers for batch releases');
        } else if (kind === 'episodes') {
            var eps = item.getAttribute('data-episodes') || '';
            loadWantedSearch(base + '/interactive-search-episodes?from=wanted&episodes=' + encodeURIComponent(eps),
                'Searching indexers for ' + (item.getAttribute('data-label') || 'the wanted episodes').toLowerCase());
        } else {
            var ep = item.getAttribute('data-episode');
            loadWantedSearch(base + '/interactive-search/' + ep + '?from=wanted', 'Searching indexers for episode ' + ep);
        }
    }

    function loadWantedSearch(url, message) {
        var body = document.getElementById('wanted-isearch-body');
        if (!body) return;
        body.innerHTML = loadingHtml(message);
        // htmx.ajax sends HX-Request, so the handler answers with the
        // rendered table partial and htmx swaps it in.
        window.htmx.ajax('GET', url, { target: '#wanted-isearch-body', swap: 'innerHTML' })
            .catch(function () {
                body.innerHTML = emptyHtml('Search failed.', 'Check System &rarr; Logs for the indexer error.');
            });
    }

    // ── Dropdown (the server-rendered menu recipe) ────────────────
    // The menu lives inside the modal, whose overflow is hidden, so an
    // open menu is moved under <body> and placed with fixed
    // coordinates below its trigger: nothing clips it, its height is
    // what the viewport leaves, and it scrolls on its own. Closing
    // puts the node back where the partial rendered it.
    var MENU_GAP = 4;
    var MENU_MARGIN = 8;

    function closeDropdowns() {
        document.querySelectorAll('.dropdown-menu:not([hidden])').forEach(function (menu) {
            menu.hidden = true;
            ['position', 'top', 'left', 'maxHeight', 'minWidth', 'zIndex'].forEach(function (k) { menu.style[k] = ''; });
            var home = menu.__dropdownHome;
            if (home && menu.parentNode !== home) home.appendChild(menu);
            var trigger = home && home.querySelector('[data-dropdown-trigger]');
            if (trigger) trigger.setAttribute('aria-expanded', 'false');
        });
    }

    function openDropdown(dd) {
        var menu = dd.querySelector('.dropdown-menu');
        var trigger = dd.querySelector('[data-dropdown-trigger]');
        if (!menu || !trigger) return;
        var rect = trigger.getBoundingClientRect();
        menu.__dropdownHome = dd;
        document.body.appendChild(menu);
        var top = rect.bottom + MENU_GAP;
        menu.style.position = 'fixed';
        menu.style.top = top + 'px';
        menu.style.left = rect.left + 'px';
        menu.style.minWidth = rect.width + 'px';
        menu.style.maxHeight = Math.max(160, Math.min(420, window.innerHeight - top - MENU_MARGIN)) + 'px';
        menu.style.zIndex = '1100';
        menu.hidden = false;
        // Keep the right edge on screen now that the width is known.
        var width = menu.offsetWidth;
        var left = rect.left;
        if (left + width + MENU_MARGIN > window.innerWidth) left = Math.max(MENU_MARGIN, window.innerWidth - width - MENU_MARGIN);
        menu.style.left = left + 'px';
        trigger.setAttribute('aria-expanded', 'true');
        var active = menu.querySelector('.dropdown-item.active');
        if (active && typeof active.scrollIntoView === 'function') active.scrollIntoView({ block: 'nearest' });
    }

    function toggleDropdown(dd) {
        var menu = dd && dd.querySelector('.dropdown-menu');
        var wasOpen = !!(menu && !menu.hidden);
        closeDropdowns();
        if (dd && !wasOpen) openDropdown(dd);
    }

    // A fixed menu does not follow its trigger: close it when the
    // window changes or anything but the menu itself scrolls.
    window.addEventListener('resize', closeDropdowns);
    document.addEventListener('scroll', function (ev) {
        var t = ev.target;
        if (t && t.closest && t.closest('.dropdown-menu')) return;
        closeDropdowns();
    }, true);

    function closeWantedInteractive() {
        closeDropdowns();
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
        // The picker is where "only the wanted episodes" happens, so a
        // batch from this page opens it even when the grab preview is
        // set to never; the wanted files start checked, so confirming
        // is one click.
        var smartBatch = isBatch && isearch.episodes.length > 0;
        if (isBatch
            && (previewMode !== 'never' || smartBatch)
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
                // Only the wanted episodes' files start checked.
                wantedEpisodes: isearch.episodes,
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
        var trigger = ev.target.closest('[data-dropdown-trigger]');
        if (trigger) { toggleDropdown(trigger.closest('[data-dropdown]')); return; }
        var item = ev.target.closest('.dropdown-item[data-isearch-choice]');
        if (item) { pickWantedChoice(item); return; }
        if (!ev.target.closest('[data-dropdown], .dropdown-menu')) closeDropdowns();
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
        }
    });

    // Escape closes the interactive search modal, unless the file
    // picker is open on top of it (grab_picker.js closes that one).
    document.addEventListener('keydown', function (ev) {
        if (ev.key !== 'Escape') return;
        if (document.querySelector('.dropdown-menu:not([hidden])')) { closeDropdowns(); return; }
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
