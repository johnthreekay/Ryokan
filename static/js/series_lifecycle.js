// Add / remove series — the flows hit when an untracked series page
// renders (Add Series button) or when an already-tracked series's
// Remove Series button is clicked. The remove path opens a custom
// confirmation modal (rather than the generic ryokanConfirm) because
// it needs to surface concrete stakes — folder path, file count,
// total bytes — that the generic confirm primitive can't represent.

function addSeries() {
    const btn = document.getElementById('btn-track');
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/add', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            anilist_id: parseInt(SD.providerId),
            mal_id: SD.malId ? parseInt(SD.malId) : null,
            title: SD.titleEnglish || SD.titleRomaji || SD.titleNative,
            title_romaji: SD.titleRomaji,
            title_english: SD.titleEnglish,
            title_native: SD.titleNative,
            cover_url: SD.coverUrl,
            format: SD.format,
            status: SD.status,
            episodes: SD.episodes ? parseInt(SD.episodes) : null,
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Add failed');
        return data;
    })
    .then(data => {
        btn.textContent = 'Added';
        btn.className = 'btn btn-success';
        if (data.hydrating) {
            window.ryokanToast({
                kind: 'info',
                category: 'library',
                title: 'Added to library',
                body: 'Fetching metadata…',
            });
            setTimeout(() => location.reload(), 4000);
        } else {
            setTimeout(() => location.reload(), 400);
        }
    })
    .catch(err => {
        btn.textContent = 'Error';
        btn.className = 'btn btn-danger';
        window.ryokanToast({
            kind: 'error',
            category: 'library',
            title: 'Failed to add series',
            body: err && err.message ? err.message : 'Unknown error',
        });
    });
}

function removeSeries(dbId) {
    // Open the dedicated destructive modal. This is a one-off rather than
    // a ryokanConfirm call because library-remove needs to render concrete
    // stakes (path, file count, size) that the generic confirm primitive
    // can't represent. See the #remove-series-modal block above.
    const modal = document.getElementById('remove-series-modal');
    const cancelBtn = document.getElementById('remove-series-cancel');
    const closeBtn = document.getElementById('remove-series-close');
    const confirmBtn = document.getElementById('remove-series-confirm');
    // A fresh open starts with the sync-exclusion box unticked; a tick
    // from a dialog that was cancelled must not carry over.
    const excludeBox = document.getElementById('remove-series-exclude');
    if (excludeBox) excludeBox.checked = false;

    function close() {
        modal.style.display = 'none';
        cancelBtn.removeEventListener('click', close);
        closeBtn.removeEventListener('click', close);
        confirmBtn.removeEventListener('click', doRemove);
        modal.removeEventListener('click', onBackdrop);
        document.removeEventListener('keydown', onKey);
    }
    function onBackdrop(ev) { if (ev.target === modal) close(); }
    function onKey(ev) { if (ev.key === 'Escape') close(); }
    function doRemove() {
        close();
        performRemoveSeries(dbId);
    }

    cancelBtn.addEventListener('click', close);
    closeBtn.addEventListener('click', close);
    confirmBtn.addEventListener('click', doRemove);
    modal.addEventListener('click', onBackdrop);
    document.addEventListener('keydown', onKey);
    modal.style.display = 'flex';
    // Focus Cancel by default so a stray Enter keypress can't
    // surprise-trigger the destructive action.
    setTimeout(function() { cancelBtn.focus(); }, 10);
}

function performRemoveSeries(dbId) {
    const btn = document.getElementById('btn-track');
    const originalText = btn.textContent;
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/remove', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            id: dbId,
            delete_files: true,
            add_exclusion: !!(document.getElementById('remove-series-exclude') || {}).checked,
        })
    })
    .then(async r => {
        // Backend always replies with JSON now — success body is the
        // cleanup summary, error body is {ok:false, stage, message}.
        // Best-effort parse so a malformed body still shows *something*
        // useful instead of silently collapsing to a generic 'Error'.
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) {
            const stage = data.stage ? ' [' + data.stage + ']' : '';
            throw new Error((data.message || 'Remove failed') + stage);
        }
        return data;
    })
    .then(() => {
        // `replace` rather than `href = '/'` because:
        //   * The series we just removed has its detail URL
        //     (`/series/<id>`) at the top of history; leaving it
        //     there means a stray Back press lands on a 404.
        //   * `replace` does not interact with bfcache the way an
        //     assignment can — the destination is a fresh load,
        //     never a snapshot of the pre-delete library.
        // The reported 2026-05-02 symptom of "removed series didn't
        // disappear from /" matched a forward-cache restore: the
        // browser served a snapshot of `/` from before the delete
        // because we'd visited `/` earlier in the session and the
        // navigation looked snapshot-able to the bfcache heuristic.
        window.location.replace('/');
    })
    .catch(err => {
        // Surface the real reason instead of the old generic 'Error'
        // button state — this was the bug where rss_seen's missing
        // ON DELETE CASCADE silently blocked every remove with
        // "FOREIGN KEY constraint failed" and the user had no way to
        // see it without devtools.
        window.ryokanAlert({
            title: 'Remove failed',
            body: err && err.message ? err.message : 'unknown error',
        });
        btn.disabled = false;
        btn.textContent = originalText;
    });
}
