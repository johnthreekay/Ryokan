// grab_picker.js — interactive file-picker modal (issue #83).
//
// Exposes `window.openGrabPicker(url, ctx)` for callers that want to
// open a pending-grab preview and let the user pick files before the
// torrent starts downloading. The modal lives in search.html; the JS
// polls the preview/poll/confirm endpoints (POST /api/grab/preview, GET
// /api/grab/preview/{id}, POST /api/grab/heartbeat/{id}, POST
// /api/grab/confirm) to drive the lifecycle.
//
// Scope notes:
//   * Heuristic pre-uncheck runs client-side from filename patterns —
//     cheap, no server round-trip, and matches the plan's
//     `pick_unwanted_file_indices` inversion (decision #5). The
//     heuristic stays conservative (NCOP/NCED/sample/readme/nfo) so
//     it never pre-unchecks something the user clearly wanted.
//   * No "Wait another 30s" / "Grab with defaults" dialog on metadata
//     timeout — deferred (needs a retry endpoint + a
//     commit-with-empty-file-list path). Error state surfaces the
//     message and a Close button; the TTL sweep handles cleanup if
//     metadata never arrives.
//   * No Cancel button, no beforeunload/sendBeacon (plan decision #4).
//     X-in-corner stops the heartbeat; the sweep auto-commits with
//     all files wanted within ~2 minutes of walkaway.
//   * Same-hash dedup's "show current priorities" path (decision #6)
//     is deferred — the dedup only covers the Tab 1 / Tab 2
//     concurrency case via the server-side pre-flight check.

(function () {
    'use strict';

    const POLL_INTERVAL_MS = 750;
    const HEARTBEAT_INTERVAL_MS = 30_000;

    // Active session state — one modal at a time. Nested calls during
    // an open session are a no-op so a double-click on Grab doesn't
    // spawn two preview rows.
    let session = null;

    function $(id) { return document.getElementById(id); }

    function escHtml(s) {
        const d = document.createElement('div');
        d.textContent = s == null ? '' : s;
        return d.innerHTML;
    }

    function formatBytes(bytes) {
        const n = Number(bytes) || 0;
        if (n <= 0) return '0 B';
        const units = ['B', 'KB', 'MB', 'GB', 'TB'];
        let i = 0;
        let v = n;
        while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
        const decimals = v < 10 && i > 0 ? 2 : v < 100 && i > 0 ? 1 : 0;
        return v.toFixed(decimals) + ' ' + units[i];
    }

    // Extract the 40-char v1 infohash from a magnet URI. Mirrors the
    // server's `services::nyaa::extract_hash` but client-side so we
    // can pass it verbatim to /api/grab/preview (required field). For
    // `.torrent` HTTP URLs we return empty — the server re-derives on
    // its end, and the browser side doesn't need to hash .torrent files in-browser.
    function extractInfoHash(url) {
        if (!url) return '';
        const m = /xt=urn:btih:([a-fA-F0-9]{40})/.exec(url);
        return m ? m[1].toLowerCase() : '';
    }

    // Filename-heuristic pre-uncheck patterns. Case-insensitive match
    // against the last path segment. Kept conservative — only fires
    // on unambiguously non-episode files (creditless OP/ED, samples,
    // trailers, text notes, NFOs). See plan decision #5 for rationale.
    const UNWANTED_PATTERNS = [
        /\bNC(OP|ED)\b/i,                  // NCOP / NCED — creditless OP/ED
        /\b(OP|ED)\d*v?\d*\.(?:mkv|mp4|avi|flac|m4a|wav)$/i, // "OP01.mkv" / "ED.mkv"
        /\bsample\b/i,                      // "sample.mkv", ".sample."
        /\btrailer\b/i,                     // "Trailer.mkv"
        /\.(txt|nfo|md|url|jpg|jpeg|png)$/i, // Readmes / scene NFOs / artwork junk
    ];

    function looksUnwanted(name) {
        const base = name.split('/').pop() || name;
        return UNWANTED_PATTERNS.some(re => re.test(base));
    }

    function computeDefaultUnwanted(files) {
        const out = new Set();
        for (let i = 0; i < files.length; i++) {
            if (looksUnwanted(files[i].name)) out.add(i);
        }
        return out;
    }

    // `E02` / `E05-E06` for a file's parsed episodes.
    function episodeLabel(eps) {
        const pad = n => 'E' + String(n).padStart(2, '0');
        if (!eps || !eps.length) return '';
        if (eps.length === 1) return pad(eps[0]);
        return pad(eps[0]) + '-' + pad(eps[eps.length - 1]);
    }

    // Wanted-page batch grabs: keep only the files whose parsed
    // episodes (`PreviewFile.episodes`, parsed server-side) include one
    // the caller wants, so a twelve-episode pack for two missing
    // episodes downloads two files, and the commit records those two.
    // When no file matches (a pack whose names carry no numbers), the
    // default selection stands and the note says so; the user can still
    // pick by hand.
    function applyWantedEpisodes() {
        if (!session || !session.wantedEpisodes.length) return;
        const want = new Set(session.wantedEpisodes);
        const matching = [];
        session.files.forEach((f, idx) => {
            const eps = Array.isArray(f.episodes) ? f.episodes : [];
            if (eps.some(e => want.has(e))) matching.push(idx);
        });
        const label = session.wantedEpisodes.map(e => episodeLabel([e])).join(', ');
        if (matching.length === 0) {
            session.preselectNote = 'No file in this release matched ' + label
                + '. Every file is selected; untick what you do not want.';
            return;
        }
        session.wanted = new Set(matching.filter(i => !looksUnwanted(session.files[i].name)));
        session.preselectNote = 'Selected ' + session.wanted.size + ' of ' + session.files.length
            + ' files for ' + label + '. Tick more to take the rest of the release.';
    }

    // ─── Session lifecycle ─────────────────────────────────────────

    function resetSession() {
        if (!session) return;
        if (session.pollTimer) clearTimeout(session.pollTimer);
        if (session.heartbeatTimer) clearInterval(session.heartbeatTimer);
        session = null;
    }

    function closeModal() {
        const modal = $('grab-picker-modal');
        if (modal) modal.style.display = 'none';
        // Heartbeat-loop teardown is what makes walkaway equivalent to
        // "abandoned"; the TTL sweep then auto-commits per decision #3.
        resetSession();
    }

    function openModal() {
        const modal = $('grab-picker-modal');
        if (!modal) {
            console.error('[grab-picker] modal element missing — wire templates/search.html');
            return false;
        }
        modal.style.display = 'flex';
        // Reset the confirm button shape explicitly on open. A prior
        // session's `confirmGrab` sets the button to "Sending…" +
        // disabled, and closeModal doesn't undo it; without this,
        // the next modal open inherits the stale "Sending…" state
        // and the user can't click through.
        const confirmBtn = $('grab-picker-confirm');
        if (confirmBtn) {
            confirmBtn.disabled = false;
            confirmBtn.textContent = 'Confirm';
        }
        return true;
    }

    // ─── UI rendering ──────────────────────────────────────────────

    function renderHeader(ctx) {
        const titleEl = $('grab-picker-title');
        const subtitleEl = $('grab-picker-subtitle');
        if (titleEl) titleEl.textContent = ctx.title || 'Release';
        if (subtitleEl) {
            const parts = [];
            if (ctx.size) parts.push(ctx.size);
            if (ctx.seeders != null) parts.push(`${ctx.seeders} seeders`);
            if (ctx.group) parts.push(ctx.group);
            subtitleEl.textContent = parts.join(' · ');
        }
    }

    function renderStatus(text, opts) {
        opts = opts || {};
        const body = $('grab-picker-body');
        if (!body) return;
        const errCls = opts.error ? ' grab-picker-status-error' : '';
        const spinner = opts.spinner ? '<div class="grab-picker-spinner" aria-hidden="true"></div>' : '';
        body.innerHTML = `
            <div class="grab-picker-status${errCls}">
                ${spinner}
                <div>${escHtml(text)}</div>
                ${opts.hint ? `<div class="form-hint" style="max-width:420px">${escHtml(opts.hint)}</div>` : ''}
            </div>
        `;
        const footer = $('grab-picker-footer');
        if (footer) footer.style.display = opts.showFooter ? 'flex' : 'none';
        const toolbar = $('grab-picker-toolbar');
        if (toolbar) toolbar.style.display = opts.showToolbar ? 'flex' : 'none';
    }

    function renderFileList() {
        const body = $('grab-picker-body');
        if (!body || !session) return;
        const toolbar = $('grab-picker-toolbar');
        const footer = $('grab-picker-footer');
        if (toolbar) toolbar.style.display = 'flex';
        if (footer) footer.style.display = 'flex';

        const banner = (session.blocklisted && !session.unblockAcked)
            ? renderBlocklistBanner()
            : '';
        const note = session.preselectNote
            ? `<div class="grab-picker-preselect form-hint">${escHtml(session.preselectNote)}</div>`
            : '';
        const list = (session.view === 'tree') ? renderTreeView() : renderFlatView();
        body.innerHTML = banner + note + list;
        attachRowHandlers();
        attachBlocklistHandlers();
        updateSelectionTotal();
        updateViewToggle();
    }

    function renderBlocklistBanner() {
        return `
            <div class="grab-picker-blocklist-banner" role="alert">
                <div class="grab-picker-blocklist-text">
                    <strong>Previously blocklisted.</strong>
                    This release is in the blocked list from an earlier grab.
                    Clicking <em>Unblock and continue</em> will clear the old
                    blocklist entry and start a fresh grab with your
                    selections. Closing this modal leaves the blocklist alone.
                </div>
                <button type="button" class="btn btn-primary" data-grab-picker-action="unblock">
                    Unblock and continue
                </button>
            </div>
        `;
    }

    function attachBlocklistHandlers() {
        const btn = document.querySelector('[data-grab-picker-action="unblock"]');
        if (btn) {
            btn.addEventListener('click', () => {
                if (!session) return;
                session.unblockAcked = true;
                renderFileList();
            });
        }
    }

    function renderFlatView() {
        const rows = session.files.map((f, idx) => {
            const checked = session.wanted.has(idx) ? 'checked' : '';
            const rowCls = session.wanted.has(idx) ? '' : ' grab-picker-row-unwanted';
            const base = f.name.split('/').pop() || f.name;
            const dir = f.name.substring(0, f.name.length - base.length).replace(/\/$/, '');
            const pathLine = dir ? `<div class="grab-picker-file-path">${escHtml(dir)}</div>` : '';
            const ep = episodeLabel(f.episodes);
            const epTag = ep ? `<span class="tag grab-picker-file-ep">${escHtml(ep)}</span>` : '';
            return `
                <tr data-idx="${idx}" class="${rowCls}">
                    <td class="col-check"><input type="checkbox" data-role="file-check" data-idx="${idx}" ${checked}></td>
                    <td>
                        <div class="grab-picker-file-name">${escHtml(base)}${epTag}</div>
                        ${pathLine}
                    </td>
                    <td class="col-size">${escHtml(formatBytes(f.size))}</td>
                </tr>
            `;
        }).join('');
        return `
            <div class="grab-picker-file-list">
                <table>
                    <tbody>${rows}</tbody>
                </table>
            </div>
        `;
    }

    function renderTreeView() {
        // Build a tree keyed by directory segments. Each node has
        // { subdirs: Map, files: [{idx, name}] }. The recursive render
        // emits <details> per directory so the native toggle chevron
        // handles expand/collapse — no extra JS needed.
        const root = { subdirs: new Map(), files: [] };
        session.files.forEach((f, idx) => {
            const segs = f.name.split('/');
            const leaf = segs.pop();
            let node = root;
            for (const s of segs) {
                if (!node.subdirs.has(s)) node.subdirs.set(s, { subdirs: new Map(), files: [] });
                node = node.subdirs.get(s);
            }
            node.files.push({ idx, name: leaf });
        });

        function sumNode(node) {
            let total = 0;
            for (const f of node.files) total += session.files[f.idx].size || 0;
            for (const [, sub] of node.subdirs) total += sumNode(sub);
            return total;
        }

        function renderNode(node) {
            let html = '';
            const dirNames = Array.from(node.subdirs.keys()).sort();
            for (const name of dirNames) {
                const sub = node.subdirs.get(name);
                const size = sumNode(sub);
                html += `<details open>
                    <summary>
                        <span>${escHtml(name)}/</span>
                        <span class="grab-picker-tree-meta">${escHtml(formatBytes(size))}</span>
                    </summary>
                    ${renderNode(sub)}
                </details>`;
            }
            for (const f of node.files) {
                const idx = f.idx;
                const checked = session.wanted.has(idx) ? 'checked' : '';
                const rowCls = session.wanted.has(idx) ? '' : ' grab-picker-row-unwanted';
                const size = session.files[idx].size;
                const ep = episodeLabel(session.files[idx].episodes);
                const epTag = ep ? `<span class="tag grab-picker-file-ep">${escHtml(ep)}</span>` : '';
                html += `<div class="grab-picker-tree-file${rowCls}" data-idx="${idx}">
                    <input type="checkbox" data-role="file-check" data-idx="${idx}" ${checked}>
                    <span class="grab-picker-file-name">${escHtml(f.name)}${epTag}</span>
                    <span class="col-size">${escHtml(formatBytes(size))}</span>
                </div>`;
            }
            return html;
        }

        return `<div class="grab-picker-file-list"><div class="grab-picker-tree">${renderNode(root)}</div></div>`;
    }

    function attachRowHandlers() {
        const body = $('grab-picker-body');
        if (!body) return;
        body.querySelectorAll('input[data-role="file-check"]').forEach(cb => {
            cb.addEventListener('change', () => {
                const idx = Number(cb.dataset.idx);
                if (cb.checked) session.wanted.add(idx);
                else session.wanted.delete(idx);
                // Toggle the row strikethrough class without a full
                // re-render so the user's scroll position stays put.
                const row = cb.closest('tr, .grab-picker-tree-file');
                if (row) row.classList.toggle('grab-picker-row-unwanted', !cb.checked);
                updateSelectionTotal();
            });
        });
    }

    function updateSelectionTotal() {
        const totalEl = $('grab-picker-total');
        if (!totalEl || !session) return;
        let selected = 0;
        let total = 0;
        for (let i = 0; i < session.files.length; i++) {
            total += session.files[i].size || 0;
            if (session.wanted.has(i)) selected += session.files[i].size || 0;
        }
        totalEl.innerHTML = `Selected <strong>${escHtml(formatBytes(selected))}</strong>
            of ${escHtml(formatBytes(total))}
            (${session.wanted.size} of ${session.files.length} files)`;
    }

    function updateViewToggle() {
        const btnFlat = $('grab-picker-view-flat');
        const btnTree = $('grab-picker-view-tree');
        if (!btnFlat || !btnTree || !session) return;
        const flatActive = session.view === 'flat';
        btnFlat.classList.toggle('active', flatActive);
        btnTree.classList.toggle('active', !flatActive);
        btnFlat.setAttribute('aria-selected', flatActive ? 'true' : 'false');
        btnTree.setAttribute('aria-selected', flatActive ? 'false' : 'true');
    }

    // ─── Toolbar actions (Level A convenience buttons, decision #11) ─

    function applyFilter(action) {
        if (session) session.preselectNote = '';
        if (!session) return;
        switch (action) {
            case 'check-all':
                session.wanted = new Set(session.files.map((_, i) => i));
                break;
            case 'uncheck-all':
                session.wanted = new Set();
                break;
            case 'uncheck-ncops':
                for (let i = 0; i < session.files.length; i++) {
                    const base = session.files[i].name.split('/').pop();
                    if (/\bNC(OP|ED)\b/i.test(base) || /\b(OP|ED)\d*v?\d*\.(?:mkv|mp4|avi)$/i.test(base)) {
                        session.wanted.delete(i);
                    }
                }
                break;
            case 'uncheck-samples':
                for (let i = 0; i < session.files.length; i++) {
                    if (/\bsample\b/i.test(session.files[i].name.split('/').pop())) {
                        session.wanted.delete(i);
                    }
                }
                break;
            case 'uncheck-extras': {
                // "Extras" folder match. Case-insensitive on any path
                // segment. "Specials" is intentionally NOT matched —
                // anime specials are episodes and users want them.
                for (let i = 0; i < session.files.length; i++) {
                    if (/(^|\/)extras\//i.test(session.files[i].name)) {
                        session.wanted.delete(i);
                    }
                }
                break;
            }
        }
        renderFileList();
    }

    // ─── Server communication ──────────────────────────────────────

    function pollPreview() {
        if (!session) return;
        fetch(`/api/grab/preview/${encodeURIComponent(session.previewId)}`)
            .then(r => {
                if (r.status === 404) {
                    // The sweep committed or a cancel fired — bail
                    // gracefully. Matches the plan's "modal should stop
                    // polling and show 'already committed'" contract.
                    renderStatus('This grab was already committed.', {
                        hint: 'The release is downloading with its default file selection. Check the Downloads page to adjust priorities if needed.',
                    });
                    if (session.heartbeatTimer) clearInterval(session.heartbeatTimer);
                    return null;
                }
                if (!r.ok) throw new Error(`preview fetch failed (${r.status})`);
                return r.json();
            })
            .then(data => {
                if (!data || !session) return;
                if (data.status === 'error') {
                    // Terminal for now — the two-button retry dialog
                    // needs backend endpoints that aren't implemented yet.
                    // User's action is Close; sweep handles cleanup.
                    renderStatus(data.error || 'Metadata fetch failed', {
                        error: true,
                        hint: 'The tracker may be slow or the release may have no seeders yet. Close this modal to let the background sweep handle it, or open the Downloads page to manage the paused torrent directly.',
                    });
                    if (session.heartbeatTimer) clearInterval(session.heartbeatTimer);
                    return;
                }
                if (data.status === 'ready') {
                    session.files = data.file_list || [];
                    session.blocklisted = !!data.blocklisted;
                    session.unblockAcked = session.unblockAcked || false;
                    session.wanted = new Set();
                    for (let i = 0; i < session.files.length; i++) session.wanted.add(i);
                    const unwanted = computeDefaultUnwanted(session.files);
                    unwanted.forEach(i => session.wanted.delete(i));
                    applyWantedEpisodes();
                    renderFileList();
                    return;
                }
                // Still fetching_metadata — keep polling.
                session.pollTimer = setTimeout(pollPreview, POLL_INTERVAL_MS);
            })
            .catch(err => {
                console.error('[grab-picker] poll failed:', err);
                if (!session) return;
                // Network blip — don't give up. Retry at the normal cadence.
                session.pollTimer = setTimeout(pollPreview, POLL_INTERVAL_MS * 2);
            });
    }

    function sendHeartbeat() {
        if (!session) return;
        fetch(`/api/grab/heartbeat/${encodeURIComponent(session.previewId)}`, { method: 'POST' })
            .then(r => {
                if (r.status === 404 && session) {
                    // Session was swept while we were staring. The next
                    // poll will also 404 and trigger the "already
                    // committed" message; stop the heartbeat timer so
                    // we don't keep banging a 404 endpoint.
                    clearInterval(session.heartbeatTimer);
                    session.heartbeatTimer = null;
                }
            })
            .catch(() => {/* transient network errors; next tick retries */});
    }

    function confirmGrab() {
        if (!session) return;
        const confirmBtn = $('grab-picker-confirm');
        if (confirmBtn) { confirmBtn.disabled = true; confirmBtn.textContent = 'Sending…'; }
        const wantedIndices = Array.from(session.wanted).sort((a, b) => a - b);
        fetch('/api/grab/confirm', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({
                preview_id: session.previewId,
                wanted_indices: wantedIndices,
                // Carry the user's acknowledgement of the inline
                // blocklist warning through to the backend so it can
                // flip the old failed rows to `replaced` alongside
                // the fresh grab write.
                unblock: !!session.unblockAcked,
            }),
        })
        .then(async r => {
            const data = await r.json().catch(() => ({}));
            if (!r.ok) {
                const msg = (data && data.message) || (typeof data === 'string' ? data : '') || `confirm failed (${r.status})`;
                throw new Error(msg);
            }
            return data;
        })
        .then(data => {
            const errs = (data && data.file_priority_errors) || [];
            const resumeErr = data && data.resume_error;
            if (errs.length || resumeErr) {
                window.ryokanToast && window.ryokanToast({
                    kind: 'warn',
                    title: 'Grab sent with partial errors',
                    body: [resumeErr, ...errs].filter(Boolean).join('\n'),
                });
            } else {
                window.ryokanToast && window.ryokanToast({
                    kind: 'success',
                    title: 'Grab sent',
                    body: `${session.wanted.size} of ${session.files.length} files queued`,
                });
            }
            const onConfirm = session && session.onConfirm;
            closeModal();
            if (typeof onConfirm === 'function') {
                try { onConfirm(data); } catch (e) { console.error('[grab-picker] onConfirm threw:', e); }
            }
        })
        .catch(err => {
            if (confirmBtn) { confirmBtn.disabled = false; confirmBtn.textContent = 'Confirm'; }
            window.ryokanToast && window.ryokanToast({
                kind: 'error',
                title: 'Grab confirm failed',
                body: (err && err.message) || 'Unknown error',
            });
        });
    }

    // ─── Public entry point ────────────────────────────────────────

    // Open the picker for a given release. `ctx` is optional metadata
    // that feeds the header — `title`, `size` (human string),
    // `seeders`, `group`, `infoHash`. When `infoHash` is omitted we
    // derive it from the magnet URL; a `.torrent` HTTP URL without a
    // hash in the ctx is rejected — the caller must pre-compute it
    // (search results carry `info_hash` in their row data).
    window.openGrabPicker = function openGrabPicker(url, ctx) {
        ctx = ctx || {};
        if (session) return; // one modal at a time

        const infoHash = (ctx.infoHash || extractInfoHash(url) || '').toLowerCase();
        if (!infoHash) {
            window.ryokanToast && window.ryokanToast({
                kind: 'error',
                title: 'Grab failed',
                body: 'Could not determine info hash for this release. Falling back to direct grab is not implemented yet.',
            });
            return;
        }

        if (!openModal()) return;
        renderHeader(ctx);
        renderStatus('Adding torrent and waiting for metadata…', { spinner: true });

        const body = {
            url,
            info_hash: infoHash,
            release_metadata: {
                title: ctx.title || '',
                size: ctx.size || '',
                seeders: ctx.seeders != null ? Number(ctx.seeders) : null,
                group: ctx.group || '',
                // Forward the search-hit's batch flag so the backend's
                // grab-row write picks it up verbatim instead of
                // inferring from file count.
                is_batch: !!ctx.isBatch,
            },
        };
        if (ctx.seriesId) body.series_id = Number(ctx.seriesId);

        fetch('/api/grab/preview', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify(body),
        })
        .then(async r => {
            const data = await r.json().catch(() => ({}));
            if (!r.ok) {
                const msg = (data && data.message) || (typeof data === 'string' ? data : '') || `preview failed (${r.status})`;
                throw new Error(msg);
            }
            return data;
        })
        .then(data => {
            session = {
                previewId: data.preview_id,
                files: [],
                wanted: new Set(),
                view: 'flat',
                pollTimer: null,
                heartbeatTimer: null,
                onConfirm: typeof ctx.onConfirm === 'function' ? ctx.onConfirm : null,
                // Episodes the caller is after (the Wanted page's batch
                // grab): once the file list arrives, only their files
                // start checked. See applyWantedEpisodes.
                wantedEpisodes: Array.isArray(ctx.wantedEpisodes)
                    ? ctx.wantedEpisodes.map(Number).filter(n => n > 0)
                    : [],
                preselectNote: '',
            };
            // Heartbeat immediately so a slow-metadata case doesn't
            // trip the TTL sweep before the first 30s interval fires.
            sendHeartbeat();
            session.heartbeatTimer = setInterval(sendHeartbeat, HEARTBEAT_INTERVAL_MS);
            pollPreview();
        })
        .catch(err => {
            renderStatus((err && err.message) || 'Failed to open grab preview', {
                error: true,
                hint: 'Check that the download client is configured and reachable in Settings.',
            });
        });
    };

    // ─── Global handlers ───────────────────────────────────────────
    //
    // hx-boost re-runs this script on every nav-back, but
    // DOMContentLoaded only fires on the very first full page load —
    // after that, a listener attached to it never executes. Without
    // the readyState check the grab-picker modal's close / confirm /
    // view / Escape / filter handlers would fail to bind on every
    // revisit and the modal would be unresponsive.
    //
    // Per-element `dataset.bound` guards prevent the per-button
    // listeners from accumulating; `__ryokanGrabPickerKeyHandlerBound`
    // gates the document-level keydown listener since `document`
    // persists across body swaps.
    function bindGrabPickerHandlers() {
        const close = $('grab-picker-close');
        const backdrop = $('grab-picker-modal');
        const confirm = $('grab-picker-confirm');
        const flatBtn = $('grab-picker-view-flat');
        const treeBtn = $('grab-picker-view-tree');

        function bindOnce(el, ev, handler) {
            if (!el || el.dataset.gpBound === '1') return;
            el.dataset.gpBound = '1';
            el.addEventListener(ev, handler);
        }
        bindOnce(close, 'click', closeModal);
        if (backdrop && backdrop.dataset.gpBound !== '1') {
            backdrop.dataset.gpBound = '1';
            backdrop.addEventListener('click', ev => {
                if (ev.target === backdrop) closeModal();
            });
        }
        bindOnce(confirm, 'click', confirmGrab);
        bindOnce(flatBtn, 'click', () => {
            if (session) { session.view = 'flat'; renderFileList(); }
        });
        bindOnce(treeBtn, 'click', () => {
            if (session) { session.view = 'tree'; renderFileList(); }
        });

        if (!window.__ryokanGrabPickerKeyHandlerBound) {
            window.__ryokanGrabPickerKeyHandlerBound = true;
            document.addEventListener('keydown', ev => {
                if (!session) return;
                if (ev.key === 'Escape') closeModal();
            });
        }

        // Toolbar filter buttons. The buttons themselves are part of
        // the grab-picker modal markup so they're fresh elements on
        // each page render; per-element guards here prevent N×M
        // accumulation if the modal is re-rendered without the page
        // navigating.
        document.querySelectorAll('[data-grab-picker-action]').forEach(btn => {
            if (btn.dataset.gpBound === '1') return;
            btn.dataset.gpBound = '1';
            btn.addEventListener('click', () => applyFilter(btn.dataset.grabPickerAction));
        });
    }
    // Wire through the page-lifecycle helper so the bind fires
    // AFTER htmx settles each body swap. Direct script-execution-time
    // binding was racy under boost — the modal element wasn't always
    // queryable yet when the script reached the binding code, so on
    // a boost-nav from another page the close / confirm / view /
    // Escape / filter handlers never attached and the picker was
    // unresponsive. Lifecycle helper wires through `htmx.onLoad`,
    // which fires after the swap completes.
    //
    // The per-element `dataset.gpBound` guards in
    // `bindGrabPickerHandlers` make the mount idempotent, so
    // re-firing on every htmx.onLoad (including in-place refreshes
    // of just the modal markup) doesn't accumulate listeners.
    if (typeof window.ryokanRegisterPageInit === 'function') {
        window.ryokanRegisterPageInit('grab-picker', {
            check: function () { return !!document.getElementById('grab-picker-modal'); },
            mount: bindGrabPickerHandlers,
        });
    } else if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', bindGrabPickerHandlers);
    } else {
        bindGrabPickerHandlers();
    }
})();
