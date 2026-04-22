// Mirrors OVERRIDE_SOURCE_MAP in series.js. Kept duplicated here rather
// than hoisted into base.js because base.js is shared across every page
// and most pages have no need for the override vocabulary — the
// duplication is short and grep-friendly.
const REVIEW_OVERRIDE_SOURCE_MAP = {
    bluray_bdmv: { source: 'BluRay', is_remux: false, is_bdmv: true,  web_kind: '' },
    bluray_remux:{ source: 'BluRay', is_remux: true,  is_bdmv: false, web_kind: '' },
    bluray:      { source: 'BluRay', is_remux: false, is_bdmv: false, web_kind: '' },
    web:         { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: '' },
    webrip:      { source: 'Web',    is_remux: false, is_bdmv: false, web_kind: 'WEBRip' },
    dvd:         { source: 'DVD',    is_remux: false, is_bdmv: false, web_kind: '' },
    hdtv:        { source: 'HDTV',   is_remux: false, is_bdmv: false, web_kind: '' },
    tv:          { source: 'TV',     is_remux: false, is_bdmv: false, web_kind: '' },
    // See series.js: no `unknown` entry; fallbacks use `bluray`.
};

// Resolve the right dropdown key for the current verdict, honoring the
// Sonarr-parity BD variant flags and the Web sub-tier so a row that was
// originally classified as BD-Remux / BD-RAW / WEBRip pre-fills the
// specific variant instead of collapsing to plain `bluray` / `web`. The
// quintet (source + is_remux + is_bdmv + web_kind) is the same space
// OVERRIDE_SOURCE_MAP in series.js canonicalizes.
function reviewKeyFromClassification(source, isRemux, isBdmv, webKind) {
    const src = (source || '').toLowerCase();
    if (src === 'bluray' || src === 'blu-ray') {
        if (isBdmv) return 'bluray_bdmv';
        if (isRemux) return 'bluray_remux';
        return 'bluray';
    }
    if (src === 'web') {
        return ((webKind || '').toLowerCase() === 'webrip') ? 'webrip' : 'web';
    }
    if (src === 'dvd') return 'dvd';
    if (src === 'hdtv') return 'hdtv';
    if (src === 'tv') return 'tv';
    return 'bluray'; // sensible default for an uncertain row
}

// `data-*` attributes come out as strings; a missing flag renders as
// the literal string "false" (askama's Display on bool), so parse with
// a strict true-only check to avoid treating "false" as truthy.
function boolAttr(value) {
    return String(value || '').toLowerCase() === 'true';
}

// Pre-fill the dropdowns with the current (uncertain) verdict so the
// user only has to flip the field that's wrong instead of building the
// classification from scratch.
document.addEventListener('DOMContentLoaded', function() {
    document.querySelectorAll('tr[data-series-id]').forEach(function(row) {
        const src = row.dataset.currentSource || '';
        const res = row.dataset.currentResolution || '';
        const isRemux = boolAttr(row.dataset.currentIsRemux);
        const isBdmv = boolAttr(row.dataset.currentIsBdmv);
        const webKind = row.dataset.currentWebKind || '';
        const srcSel = row.querySelector('.review-source');
        const resSel = row.querySelector('.review-resolution');
        if (srcSel) {
            const key = reviewKeyFromClassification(src, isRemux, isBdmv, webKind);
            srcSel.value = REVIEW_OVERRIDE_SOURCE_MAP[key] ? key : 'bluray';
        }
        if (resSel && res) {
            const match = Array.from(resSel.options).find(o => o.value.toLowerCase() === res.toLowerCase());
            if (match) resSel.value = match.value;
        }
    });
});

// ── Bulk actions ────────────────────────────────────────────────────
// Row checkbox + header "select all" + a sticky action bar that applies
// a chosen source/resolution to every selected row in one request via
// /api/library/bulk-manual-override. Rows fade and self-remove on
// success, matching the single-row flow.
(function () {
    const bar = document.getElementById('review-bulk-bar');
    const countEl = document.getElementById('review-bulk-count-n');
    const selectAll = document.getElementById('review-select-all');
    const applyBtn = document.getElementById('review-bulk-apply');
    const clearBtn = document.getElementById('review-bulk-clear');
    const bulkSource = document.getElementById('review-bulk-source');
    const bulkResolution = document.getElementById('review-bulk-resolution');
    if (!bar || !selectAll || !applyBtn) return;

    function rowChecks() {
        return Array.from(document.querySelectorAll('.review-row-check'));
    }
    function selectedRows() {
        return rowChecks().filter(cb => cb.checked).map(cb => cb.closest('tr'));
    }
    function refresh() {
        const n = selectedRows().length;
        countEl.textContent = n;
        bar.hidden = n === 0;
        const total = rowChecks().length;
        selectAll.checked = total > 0 && n === total;
        selectAll.indeterminate = n > 0 && n < total;
    }

    document.addEventListener('change', function (ev) {
        if (ev.target && ev.target.classList && ev.target.classList.contains('review-row-check')) {
            refresh();
        }
    });
    selectAll.addEventListener('change', function () {
        const on = selectAll.checked;
        rowChecks().forEach(cb => { cb.checked = on; });
        refresh();
    });
    clearBtn.addEventListener('click', function () {
        rowChecks().forEach(cb => { cb.checked = false; });
        refresh();
    });
    document.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape' && !bar.hidden) {
            rowChecks().forEach(cb => { cb.checked = false; });
            refresh();
        }
    });

    applyBtn.addEventListener('click', function () {
        const rows = selectedRows();
        if (rows.length === 0) return;
        const key = bulkSource.value;
        const mapped = REVIEW_OVERRIDE_SOURCE_MAP[key] || REVIEW_OVERRIDE_SOURCE_MAP.bluray;
        const resolution = bulkResolution.value;
        const items = rows.map(function (row) {
            return {
                series_id: parseInt(row.dataset.seriesId, 10),
                episode_number: parseInt(row.dataset.episode, 10),
                source: mapped.source,
                resolution: resolution,
                is_remux: mapped.is_remux,
                is_bdmv: mapped.is_bdmv,
                web_kind: mapped.web_kind,
            };
        });
        applyBtn.disabled = true;
        fetch('/api/library/bulk-manual-override', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({items: items}),
        })
        .then(async function (r) {
            let data = {};
            try { data = await r.json(); } catch (_) {}
            if (!r.ok) throw new Error(data.message || 'Bulk apply failed');
            return data;
        })
        .then(function (data) {
            const appliedIds = new Set();
            (data.failed || []).forEach(function (f) {
                appliedIds.add(f.series_id + ':' + f.episode_number);
            });
            // Fade out the rows that succeeded. Failed rows stay visible.
            rows.forEach(function (row) {
                const key = row.dataset.seriesId + ':' + row.dataset.episode;
                if (appliedIds.has(key)) return;
                row.style.transition = 'opacity 0.2s';
                row.style.opacity = '0';
                setTimeout(function () {
                    if (row.parentNode) row.parentNode.removeChild(row);
                    refresh();
                    const tbody = document.querySelector('.review-table tbody');
                    if (tbody && tbody.children.length === 0) location.reload();
                }, 200);
            });
            const kind = (data.failed && data.failed.length > 0) ? 'warn' : 'success';
            const title = data.applied + ' of ' + data.requested + ' applied';
            window.ryokanToast({
                kind: kind,
                title: title,
                body: data.failed && data.failed.length > 0 ? (data.failed.length + ' failed') : '',
                category: 'library',
            });
        })
        .catch(function (err) {
            window.ryokanToast({
                kind: 'error',
                title: 'Bulk apply failed',
                body: err.message || String(err),
                category: 'library',
            });
        })
        .finally(function () {
            applyBtn.disabled = false;
        });
    });

    refresh();
})();

function applyReviewOverride(btn) {
    const row = btn.closest('tr');
    if (!row) return;
    const seriesId = parseInt(row.dataset.seriesId, 10);
    const episodeNumber = parseInt(row.dataset.episode, 10);
    const key = row.querySelector('.review-source').value;
    // Defensive fallback to `bluray` instead of `unknown` — the dropdown
    // no longer offers Unknown (the handler 400s on Source::Unknown), so
    // falling back to the now-gone `unknown` map entry would have
    // produced `undefined.source` at runtime. Mirror series.js.
    const mapped = REVIEW_OVERRIDE_SOURCE_MAP[key] || REVIEW_OVERRIDE_SOURCE_MAP.bluray;
    const resolution = row.querySelector('.review-resolution').value;
    const status = row.querySelector('.review-status');
    if (status) {
        status.textContent = 'Saving…';
        status.style.display = 'block';
    }
    btn.disabled = true;
    fetch('/api/library/manual-override', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            series_id: seriesId,
            episode_number: episodeNumber,
            source: mapped.source,
            resolution: resolution,
            is_remux: mapped.is_remux,
            is_bdmv: mapped.is_bdmv,
            web_kind: mapped.web_kind,
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Failed to apply override');
        return data;
    })
    .then(_ => {
        // Manual override clears needs_review, so the row no longer
        // belongs in this list. Fade it out and remove instead of doing
        // a full page reload — keeps any other in-flight overrides
        // running uninterrupted.
        row.style.transition = 'opacity 0.2s';
        row.style.opacity = '0';
        setTimeout(function() {
            const tbody = row.parentNode;
            if (tbody) tbody.removeChild(row);
            if (tbody && tbody.children.length === 0) location.reload();
        }, 200);
    })
    .catch(err => {
        if (status) status.textContent = err.message || 'Failed';
        btn.disabled = false;
    });
}
