// System > Misgrabs. The Restore and Dismiss forms are plain hx-post
// rows: a successful response is an empty 200 that swaps the row away,
// and the outcome rides in an HX-Trigger event so one listener can
// toast it and reveal the empty state once the table drains. A failed
// action comes back with HX-Reswap: none, so the row stays and only the
// toast fires.
(function () {
    if (window.__ryokanMisgrabDocListeners) return;
    window.__ryokanMisgrabDocListeners = true;

    document.addEventListener('ryokan-misgrab-action', function (ev) {
        var d = (ev && ev.detail) || {};
        if (typeof window.ryokanToast === 'function') {
            window.ryokanToast({
                kind: d.ok ? 'success' : 'error',
                title: d.ok
                    ? (d.action === 'restore' ? 'Misgrab restored' : 'Misgrab dismissed')
                    : (d.action === 'restore' ? 'Restore failed' : 'Dismiss failed'),
                body: d.message || '',
                category: 'grab',
            });
        }
        if (!d.ok) return;
        // The row swap happens after this event; check on the next tick.
        setTimeout(function () {
            var table = document.getElementById('misgrab-table');
            var empty = document.getElementById('misgrab-empty-state');
            var tbody = table ? table.querySelector('tbody') : null;
            if (tbody && tbody.querySelectorAll('tr').length === 0) {
                table.hidden = true;
                if (empty) empty.hidden = false;
            }
        }, 50);
    });
})();
