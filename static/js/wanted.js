// /wanted page: "Search selected" / "Search all" / per-row Search.
// Each starts a sticky progress toast and POSTs the series ids (and
// the tab, so a cutoff search reaches on-disk episodes) to
// /api/wanted/search, which runs the series' own auto-searches one
// after another in the background and reports on the toast.
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

    document.addEventListener('click', function (ev) {
        if (!document.getElementById('wanted-page')) return;
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
