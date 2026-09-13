// /wanted page: "Search selected" / "Search all" / per-row Search.
// Each starts a sticky progress toast and POSTs the series ids to
// /api/wanted/search, which runs the series' own auto-searches one
// after another in the background and reports on the toast.
//
// Listeners hang off #wanted-page, which every boosted navigation
// replaces, so mount() on re-entry never doubles them; the tab swap
// only replaces #wanted-list inside it.
(function () {
    function idsFrom(selector) {
        return Array.from(document.querySelectorAll(selector))
            .map(function (cb) { return parseInt(cb.value, 10); })
            .filter(function (n) { return n > 0; });
    }

    function startSearch(ids, title, body) {
        if (!ids.length) {
            window.ryokanToast({
                kind: 'info',
                category: 'auto_search',
                title: 'Nothing selected',
                body: 'Tick the series to search, or use Search all.',
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
            body: JSON.stringify({ series_ids: ids }),
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

    function onClick(ev) {
        var bulk = ev.target.closest('[data-wanted-search]');
        if (bulk) {
            var mode = bulk.getAttribute('data-wanted-search');
            var ids = mode === 'all' ? idsFrom('.wanted-select') : idsFrom('.wanted-select:checked');
            startSearch(
                ids,
                mode === 'all' ? 'Searching all wanted series' : 'Searching selected series',
                ids.length + (ids.length === 1 ? ' series' : ' series')
            );
            return;
        }
        var one = ev.target.closest('[data-wanted-search-one]');
        if (one) {
            var id = parseInt(one.getAttribute('data-wanted-search-one'), 10);
            startSearch([id], 'Searching wanted episodes', one.getAttribute('data-wanted-title') || '');
        }
    }

    function onChange(ev) {
        if (ev.target && ev.target.id === 'wanted-select-all') {
            var on = ev.target.checked;
            document.querySelectorAll('.wanted-select').forEach(function (cb) { cb.checked = on; });
        }
    }

    window.ryokanRegisterPageInit('wanted', {
        check: function () { return !!document.getElementById('wanted-page'); },
        mount: function () {
            var root = document.getElementById('wanted-page');
            if (!root) return;
            root.addEventListener('click', onClick);
            root.addEventListener('change', onChange);
        },
        unmount: function () {},
    });
})();
