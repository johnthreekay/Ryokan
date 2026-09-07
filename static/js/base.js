(function () {
    // In-app replacement for browser confirm(). Returns a promise that
    // resolves to {ok: boolean, extras: {id: value}}. Falls back to
    // ok=false if the user dismisses via the close button or backdrop.
    let current = null;
    const modal = document.getElementById('ryokan-confirm-modal');
    const titleEl = document.getElementById('ryokan-confirm-title');
    const bodyEl = document.getElementById('ryokan-confirm-body');
    const extrasEl = document.getElementById('ryokan-confirm-extras');
    const yesBtn = document.getElementById('ryokan-confirm-yes');
    const noBtn = document.getElementById('ryokan-confirm-no');
    const closeBtn = document.getElementById('ryokan-confirm-close');
    // base.html always ships the modal; a page that doesn't (test
    // fixtures, an error page) must not take the rest of this file
    // down with it.
    if (!modal || !titleEl || !bodyEl || !extrasEl || !yesBtn || !noBtn || !closeBtn) return;

    function collectExtras() {
        const out = {};
        if (!current || !current.extras) return out;
        for (const e of current.extras) {
            const cb = document.getElementById('ryokan-confirm-extra-' + e.id);
            if (cb) out[e.id] = cb.checked;
        }
        return out;
    }

    function close(result) {
        if (!current) return;
        const resolve = current.resolve;
        current = null;
        modal.style.display = 'none';
        resolve(result);
    }

    yesBtn.addEventListener('click', function () {
        close({ok: true, extras: collectExtras()});
    });
    noBtn.addEventListener('click', function () { close({ok: false, extras: {}}); });
    closeBtn.addEventListener('click', function () { close({ok: false, extras: {}}); });
    modal.addEventListener('click', function (ev) {
        if (ev.target === modal) close({ok: false, extras: {}});
    });
    document.addEventListener('keydown', function (ev) {
        if (!current) return;
        if (ev.key === 'Escape') close({ok: false, extras: {}});
        if (ev.key === 'Enter') close({ok: true, extras: collectExtras()});
    });

    window.ryokanConfirm = function (opts) {
        opts = opts || {};
        return new Promise(function (resolve) {
            current = {resolve: resolve, extras: opts.extras || []};
            titleEl.textContent = opts.title || 'Confirm';
            bodyEl.textContent = opts.body || 'Are you sure?';
            yesBtn.textContent = opts.yesLabel || 'Yes';
            noBtn.textContent = opts.noLabel || 'No';
            // Destructive-action treatment: red Yes button when
            // `danger` is set, default accent otherwise. Class is
            // toggled (not replaced) so any other classes the
            // button picks up later won't be clobbered. Reset on
            // close() so the next confirm starts neutral.
            yesBtn.classList.toggle('btn-danger', !!opts.danger);
            yesBtn.classList.toggle('btn-primary', !opts.danger);
            // Build extras checkboxes.
            extrasEl.innerHTML = '';
            for (const e of current.extras) {
                const label = document.createElement('label');
                label.className = 'checkbox-label';
                const cb = document.createElement('input');
                cb.type = 'checkbox';
                cb.id = 'ryokan-confirm-extra-' + e.id;
                cb.checked = !!e.default;
                label.appendChild(cb);
                label.appendChild(document.createTextNode(' ' + (e.label || e.id)));
                extrasEl.appendChild(label);
            }
            modal.style.display = 'flex';
            // Focus the No button by default so Enter doesn't surprise-confirm.
            setTimeout(function () { noBtn.focus(); }, 10);
        });
    };
})();

(function () {
    // In-app replacement for browser alert(). Returns a promise that
    // resolves when the user dismisses the modal.
    let current = null;
    const modal = document.getElementById('ryokan-alert-modal');
    const titleEl = document.getElementById('ryokan-alert-title');
    const bodyEl = document.getElementById('ryokan-alert-body');
    const okBtn = document.getElementById('ryokan-alert-ok');
    const closeBtn = document.getElementById('ryokan-alert-close');
    if (!modal || !titleEl || !bodyEl || !okBtn || !closeBtn) return;

    function close() {
        if (!current) return;
        const resolve = current.resolve;
        current = null;
        modal.style.display = 'none';
        resolve();
    }

    okBtn.addEventListener('click', close);
    closeBtn.addEventListener('click', close);
    modal.addEventListener('click', function (ev) {
        if (ev.target === modal) close();
    });
    document.addEventListener('keydown', function (ev) {
        if (!current) return;
        if (ev.key === 'Escape' || ev.key === 'Enter') close();
    });

    window.ryokanAlert = function (opts) {
        opts = opts || {};
        return new Promise(function (resolve) {
            current = {resolve: resolve};
            titleEl.textContent = opts.title || 'Notice';
            bodyEl.textContent = opts.body || '';
            okBtn.textContent = opts.okLabel || 'OK';
            modal.style.display = 'flex';
            setTimeout(function () { okBtn.focus(); }, 10);
        });
    };
})();

(function () {
    // In-app replacement for browser prompt(). Returns a promise that
    // resolves to the submitted string, or null if cancelled. Optional
    // validator: (value) => errorString | null.
    let current = null;
    const modal = document.getElementById('ryokan-prompt-modal');
    const titleEl = document.getElementById('ryokan-prompt-title');
    const bodyEl = document.getElementById('ryokan-prompt-body');
    const labelEl = document.getElementById('ryokan-prompt-label');
    const inputEl = document.getElementById('ryokan-prompt-input');
    const errorEl = document.getElementById('ryokan-prompt-error');
    const okBtn = document.getElementById('ryokan-prompt-ok');
    const cancelBtn = document.getElementById('ryokan-prompt-cancel');
    const closeBtn = document.getElementById('ryokan-prompt-close');
    if (!modal || !titleEl || !bodyEl || !labelEl || !inputEl || !errorEl || !okBtn || !cancelBtn || !closeBtn) return;

    function close(result) {
        if (!current) return;
        const resolve = current.resolve;
        current = null;
        modal.style.display = 'none';
        errorEl.style.display = 'none';
        errorEl.textContent = '';
        resolve(result);
    }

    function submit() {
        if (!current) return;
        const value = inputEl.value;
        if (current.validator) {
            const err = current.validator(value);
            if (err) {
                errorEl.textContent = err;
                errorEl.style.display = 'block';
                return;
            }
        }
        close(value);
    }

    okBtn.addEventListener('click', submit);
    cancelBtn.addEventListener('click', function () { close(null); });
    closeBtn.addEventListener('click', function () { close(null); });
    modal.addEventListener('click', function (ev) {
        if (ev.target === modal) close(null);
    });
    document.addEventListener('keydown', function (ev) {
        if (!current) return;
        if (ev.key === 'Escape') close(null);
        if (ev.key === 'Enter' && ev.target === inputEl) {
            ev.preventDefault();
            submit();
        }
    });

    window.ryokanPrompt = function (opts) {
        opts = opts || {};
        return new Promise(function (resolve) {
            current = {resolve: resolve, validator: opts.validator || null};
            titleEl.textContent = opts.title || 'Enter value';
            if (opts.body) {
                bodyEl.textContent = opts.body;
                bodyEl.style.display = 'block';
            } else {
                bodyEl.textContent = '';
                bodyEl.style.display = 'none';
            }
            if (opts.label) {
                labelEl.textContent = opts.label;
                labelEl.style.display = 'block';
            } else {
                labelEl.textContent = '';
                labelEl.style.display = 'none';
            }
            inputEl.value = opts.defaultValue != null ? String(opts.defaultValue) : '';
            inputEl.placeholder = opts.placeholder || '';
            okBtn.textContent = opts.okLabel || 'OK';
            cancelBtn.textContent = opts.cancelLabel || 'Cancel';
            errorEl.style.display = 'none';
            errorEl.textContent = '';
            modal.style.display = 'flex';
            setTimeout(function () { inputEl.focus(); inputEl.select(); }, 10);
        });
    };
})();

(function () {
    // Transient toast notifications. window.ryokanToast({kind, title,
    // body, category, duration, sticky, busy, log, actions}) pushes a
    // new toast into the top-right stack. kind ∈ {info, success, warn,
    // error}. Auto-dismiss after duration ms (default 4000, 0 disables
    // auto-dismiss). Pause on hover. `busy: true` shows a spinner next
    // to the title until `update({busy: false})` or `finalize()`. Every
    // toast is also mirrored to POST /api/logs/client so it persists in
    // the System → Logs tab after the transient UI disappears. Pass
    // `category` to classify the log row; falls back to `system` on the
    // server. Pass `log: false` to opt out of persistence (e.g. purely
    // decorative toasts).
    //
    // Toasts follow the user across pages. base.js re-executes on every
    // boosted swap and the stack element is part of the swapped body,
    // so the live toasts are kept on `window.__ryokanToastRuntime`
    // (which survives re-execution) and re-appended to the new stack
    // with their timers, action buttons, and progress followers intact.
    // A full page load (reload, `location.href`) starts from the
    // sessionStorage record the runtime keeps of its live toasts:
    // transient ones come back with their remaining time, progress
    // toasts re-attach to their job (see `ryokanProgressToast`). Action
    // buttons do not survive a full load; their closures are gone.
    const STORAGE_KEY = 'ryokanLiveToasts';
    const KINDS = ['info', 'success', 'warn', 'error'];
    const runtime = window.__ryokanToastRuntime || (window.__ryokanToastRuntime = {
        live: [],
        restored: false,
    });

    // Looked up on every use: a module-scope snapshot goes stale after
    // a body swap (templates/AGENTS.md).
    function getStack() {
        return document.getElementById('ryokan-toast-stack');
    }

    function persistToast(kind, category, title, body) {
        // Fire-and-forget. A failing log write must never surface
        // another toast — that would be an infinite loop on an
        // outage. Silently console-log any failure.
        try {
            fetch('/api/logs/client', {
                method: 'POST',
                headers: {'Content-Type': 'application/json'},
                body: JSON.stringify({
                    kind: kind,
                    category: category || null,
                    title: title || '',
                    body: body || '',
                }),
                credentials: 'same-origin',
                keepalive: true,
            }).catch(function (err) {
                console.warn('[ryokanToast] log persist failed:', err);
            });
        } catch (err) {
            console.warn('[ryokanToast] log persist threw:', err);
        }
    }

    // Write the live set to sessionStorage (same-tab, cleared when the
    // tab closes: the lifetime of "show me what I kicked off"). Called
    // on every change so a reload at any moment restores the truth.
    function save() {
        try {
            const records = [];
            runtime.live.forEach(function (e) {
                if (e.dismissed) return;
                records.push({
                    id: e.id,
                    kind: e.kind,
                    title: e.titleEl.textContent,
                    body: e.bodyEl.textContent,
                    category: e.category || null,
                    sticky: e.sticky,
                    busy: e.busy,
                    // Armed: absolute deadline. Paused (hovered): what
                    // is left. Sticky: neither.
                    expiresAt: !e.sticky && e.timer ? e.timerStart + e.remaining : null,
                    remaining: !e.sticky && !e.timer ? e.remaining : null,
                    progressId: e.progressId || null,
                });
            });
            if (records.length) {
                sessionStorage.setItem(STORAGE_KEY, JSON.stringify(records));
            } else {
                sessionStorage.removeItem(STORAGE_KEY);
            }
        } catch (_) {
            // Storage unavailable (private mode, quota): toasts still
            // work for this page, they just do not survive a reload.
        }
    }

    function forget(entry) {
        const i = runtime.live.indexOf(entry);
        if (i >= 0) runtime.live.splice(i, 1);
        save();
    }

    function dismiss(entry) {
        if (!entry || entry.dismissed) return;
        entry.dismissed = true;
        if (entry.timer) { clearTimeout(entry.timer); entry.timer = null; }
        forget(entry);
        entry.el.classList.add('ryokan-toast-leaving');
        setTimeout(function () {
            if (entry.el.parentNode) entry.el.parentNode.removeChild(entry.el);
        }, 200);
    }

    function setKind(entry, kind) {
        if (KINDS.indexOf(kind) < 0) return;
        KINDS.forEach(function (k) { entry.el.classList.remove('ryokan-toast-' + k); });
        entry.el.classList.add('ryokan-toast-' + kind);
        entry.el.setAttribute('role', kind === 'error' || kind === 'warn' ? 'alert' : 'status');
        entry.kind = kind;
    }

    function setBusy(entry, busy) {
        entry.busy = !!busy;
        entry.spinner.style.display = entry.busy ? '' : 'none';
    }

    function applyPatch(entry, patch) {
        patch = patch || {};
        if (patch.kind) setKind(entry, patch.kind);
        if (patch.title != null) {
            entry.titleEl.textContent = patch.title;
            entry.titleRow.style.display = patch.title ? '' : 'none';
        }
        if (patch.body != null) {
            entry.bodyEl.textContent = patch.body;
            entry.bodyEl.style.display = patch.body ? '' : 'none';
        }
        if (patch.busy != null) setBusy(entry, patch.busy);
        save();
    }

    function armTimer(entry) {
        if (entry.sticky || entry.dismissed || entry.timer) return;
        entry.timerStart = Date.now();
        entry.timer = setTimeout(function () { dismiss(entry); }, Math.max(entry.remaining, 0));
        save();
    }

    function pauseTimer(entry) {
        if (!entry.timer) return;
        clearTimeout(entry.timer);
        entry.timer = null;
        // Never let a hovered toast run out while paused: it would
        // stay forever, since a timer never re-arms at zero.
        entry.remaining = Math.max(entry.remaining - (Date.now() - entry.timerStart), 250);
        save();
    }

    function newId() {
        return 't_' + Date.now().toString(36) + '_' + Math.random().toString(36).slice(2, 8);
    }

    window.ryokanToast = function (opts) {
        opts = opts || {};
        const kind = KINDS.indexOf(opts.kind) >= 0 ? opts.kind : 'info';
        // `sticky: true` disables auto-dismiss — use for long-running
        // jobs where the toast represents live state and should only
        // close on explicit user action (or when `handle.finalize()`
        // upgrades it to a normal auto-dismissing toast).
        const sticky = !!opts.sticky;
        const duration = sticky ? 0 : (opts.duration != null ? Number(opts.duration) : 4000);

        if (opts.log !== false) {
            persistToast(kind, opts.category, opts.title, opts.body);
        }

        const toast = document.createElement('div');
        toast.className = 'ryokan-toast ryokan-toast-' + kind;
        toast.setAttribute('role', kind === 'error' || kind === 'warn' ? 'alert' : 'status');

        const accent = document.createElement('span');
        accent.className = 'ryokan-toast-accent';
        toast.appendChild(accent);

        const content = document.createElement('div');
        content.className = 'ryokan-toast-content';
        const titleRow = document.createElement('div');
        titleRow.className = 'ryokan-toast-title-row';
        const spinner = document.createElement('span');
        spinner.className = 'ryokan-toast-spinner';
        spinner.setAttribute('aria-hidden', 'true');
        titleRow.appendChild(spinner);
        const titleEl = document.createElement('div');
        titleEl.className = 'ryokan-toast-title';
        titleEl.textContent = opts.title || '';
        titleRow.appendChild(titleEl);
        if (!opts.title) titleRow.style.display = 'none';
        content.appendChild(titleRow);
        const bodyEl = document.createElement('div');
        bodyEl.className = 'ryokan-toast-body';
        bodyEl.textContent = opts.body || '';
        if (!opts.body) bodyEl.style.display = 'none';
        content.appendChild(bodyEl);

        const entry = {
            id: opts.id || newId(),
            el: toast,
            titleRow: titleRow,
            titleEl: titleEl,
            bodyEl: bodyEl,
            spinner: spinner,
            kind: kind,
            category: opts.category || null,
            sticky: sticky,
            busy: false,
            remaining: duration,
            timerStart: 0,
            timer: null,
            dismissed: false,
            progressId: opts.progressId || null,
        };
        setBusy(entry, opts.busy);

        // Optional action buttons (`opts.action` or `opts.actions: [...]`),
        // e.g. "Undo" after a recycle-bin delete. The click handler gets a
        // small handle so it can repaint or dismiss the toast; the button
        // removes itself on first click so it can't fire twice.
        const actionList = Array.isArray(opts.actions) ? opts.actions : (opts.action ? [opts.action] : []);
        if (actionList.length) {
            const actionsEl = document.createElement('div');
            actionsEl.className = 'ryokan-toast-actions';
            actionList.forEach(function (a) {
                if (!a || !a.label) return;
                const b = document.createElement('button');
                b.type = 'button';
                b.className = 'btn btn-sm ' + (a.primary ? 'btn-primary' : 'btn-secondary');
                b.textContent = a.label;
                b.addEventListener('click', function () {
                    b.remove();
                    if (!actionsEl.children.length) actionsEl.remove();
                    if (typeof a.onClick !== 'function') return;
                    a.onClick({
                        dismiss: function () { dismiss(entry); },
                        update: function (patch) { applyPatch(entry, patch); },
                    });
                });
                actionsEl.appendChild(b);
            });
            content.appendChild(actionsEl);
        }
        toast.appendChild(content);

        const close = document.createElement('button');
        close.type = 'button';
        close.className = 'ryokan-toast-close';
        close.setAttribute('aria-label', 'Dismiss');
        close.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6L6 18M6 6l12 12"/></svg>';
        close.addEventListener('click', function () { dismiss(entry); });
        toast.appendChild(close);

        runtime.live.push(entry);
        const stack = getStack();
        if (stack) stack.appendChild(toast);

        toast.addEventListener('mouseenter', function () { pauseTimer(entry); });
        toast.addEventListener('mouseleave', function () { armTimer(entry); });
        armTimer(entry);
        save();

        return {
            dismiss: function () { dismiss(entry); },
            // Mutate the live toast in place. Used by ryokanProgressToast
            // to repaint title/body/kind as stage events arrive. Does not
            // re-arm the auto-dismiss timer — a sticky toast that's been
            // updating should stay sticky until `finalize()` ends it.
            update: function (patch) { applyPatch(entry, patch); },
            // Convert a sticky toast into a terminal one that auto-dismisses
            // after `duration` ms (default 4000 for success/info, 0 for
            // warn/error so the user has time to read). Also persists the
            // final state to /api/logs/client once, matching the log
            // persistence behavior of a one-shot ryokanToast call. The
            // spinner goes with the sticky state.
            finalize: function (final) {
                final = final || {};
                applyPatch(entry, {kind: final.kind, title: final.title, body: final.body, busy: false});
                entry.progressId = null;
                if (final.log !== false) {
                    persistToast(entry.kind, entry.category, entry.titleEl.textContent, entry.bodyEl.textContent);
                }
                const finalDuration = final.duration != null
                    ? Number(final.duration)
                    : (entry.kind === 'error' || entry.kind === 'warn' ? 0 : 4000);
                if (entry.timer) { clearTimeout(entry.timer); entry.timer = null; }
                entry.sticky = finalDuration <= 0;
                entry.remaining = finalDuration;
                armTimer(entry);
                save();
            },
        };
    };

    // Boosted swap: the previous body took the old stack with it. Put
    // every live toast back, timers and followers untouched. Runs now
    // (base.js re-executes with the swapped body) and on every
    // `htmx.onLoad`, so the carry-over does not depend on where the
    // script tags sit.
    function carryOver() {
        const stack = getStack();
        if (!stack) return;
        runtime.live.forEach(function (e) {
            if (!e.dismissed && e.el.parentNode !== stack) stack.appendChild(e.el);
        });
    }
    carryOver();
    if (!runtime.onLoadBound && window.htmx && typeof window.htmx.onLoad === 'function') {
        runtime.onLoadBound = true;
        window.htmx.onLoad(function () { carryOver(); });
    }

    // Full page load: rebuild the live set from the stored record.
    // Runs once per document, after `ryokanProgressToast` is defined
    // (progress toasts re-attach through it), so the caller below in
    // this file invokes it.
    window.__ryokanRestoreToasts = function () {
        if (runtime.restored) return;
        runtime.restored = true;
        let records = [];
        try {
            records = JSON.parse(sessionStorage.getItem(STORAGE_KEY) || '[]');
        } catch (_) {
            records = [];
        }
        try { sessionStorage.removeItem(STORAGE_KEY); } catch (_) {}
        if (!Array.isArray(records)) return;
        const now = Date.now();
        records.forEach(function (r) {
            if (!r || typeof r !== 'object' || !r.id) return;
            if (runtime.live.some(function (e) { return e.id === r.id; })) return;
            let duration = 0;
            if (!r.sticky) {
                duration = r.remaining != null ? Number(r.remaining) : Number(r.expiresAt) - now;
                if (!(duration > 0)) return;
            }
            const base = {
                id: r.id,
                kind: r.kind,
                title: r.title || '',
                body: r.body || '',
                category: r.category || undefined,
                log: false,
            };
            if (r.sticky && r.progressId && window.ryokanProgressToast) {
                base.progressId = r.progressId;
                window.ryokanProgressToast(base);
            } else {
                base.sticky = !!r.sticky;
                base.duration = duration;
                base.busy = !!r.busy;
                window.ryokanToast(base);
            }
        });
    };
})();

(function () {
    // ryokanQueueToast({...}) — persist a toast across a navigation
    // or reload. Use this instead of ryokanToast right before
    // `location.reload()` / `location.href = …`, otherwise the
    // navigation tears down the DOM ~200ms later and the toast
    // stack disappears mid-display. The block below reads the
    // queued entry on the next page load and fires it through the
    // normal ryokanToast path so persistence to /api/logs/client
    // and auto-dismiss timing are unchanged from a fresh toast.
    //
    // sessionStorage (not localStorage) is the right scope: same-tab
    // only, cleared when the tab closes — matches the lifetime of
    // a "show me the result of the action I just kicked off" toast.
    const KEY = 'ryokanPendingToast';

    window.ryokanQueueToast = function (opts) {
        if (!opts || typeof opts !== 'object') return;
        try {
            sessionStorage.setItem(KEY, JSON.stringify(opts));
        } catch (_) {
            // sessionStorage is unavailable (private mode, quota
            // exhausted) — fall back to firing the toast inline so
            // the user at least sees it briefly before the reload.
            if (window.ryokanToast) window.ryokanToast(opts);
        }
    };

    // On every page load, drain the queued toast (if any).
    try {
        const raw = sessionStorage.getItem(KEY);
        if (raw) {
            sessionStorage.removeItem(KEY);
            const opts = JSON.parse(raw);
            if (opts && typeof opts === 'object' && window.ryokanToast) {
                window.ryokanToast(opts);
            }
        }
    } catch (_) {
        // Malformed JSON or storage unavailable — drop the entry
        // silently so we don't loop on the same broken value.
        try { sessionStorage.removeItem(KEY); } catch (_) {}
    }
})();

// Sticky progress toast backed by /api/progress/{id}. The caller is
// expected to mint a `progressId` string, pass it as `?progress_id=`
// on the trigger endpoint, and hand it here — this helper opens the
// sticky toast, polls for events, and rolls up to a terminal state.
//
// Usage:
//     const id = ryokanNewProgressId();
//     const toast = ryokanProgressToast({
//         progressId: id,
//         title: 'Searching…',
//         category: 'auto_search',
//     });
//     fetch('/api/series/123/auto-search?progress_id=' + id, {method: 'POST'})
//         .then(r => r.json())
//         .catch(e => toast.finalize({kind: 'error', title: 'Failed', body: String(e)}));
//     // Toast handles the rest — updates on stage events, closes on terminal.
window.ryokanNewProgressId = function () {
    return 'p_' + Date.now().toString(36) + '_' + Math.random().toString(36).slice(2, 10);
};

window.ryokanProgressToast = function (opts) {
    opts = opts || {};
    if (!opts.progressId) throw new Error('ryokanProgressToast requires opts.progressId');
    const toast = window.ryokanToast({
        id: opts.id,
        kind: opts.kind || 'info',
        title: opts.title || 'Working…',
        body: opts.body || null,
        category: opts.category || 'system',
        sticky: true,
        // The spinner says "still running" until the terminal event;
        // `finalize()` clears it with the sticky state.
        busy: true,
        // Recorded so a full page load can re-attach to the job: the
        // stream replays the buffer from the start (`poll` is
        // non-destructive), so a resumed toast repaints every event.
        progressId: opts.progressId,
        // The terminal event will persist to logs via `finalize()`.
        // Skipping the initial log write avoids a "Working…" row in
        // System → Logs that gets immediately superseded.
        log: false,
    });
    let stopped = false;
    let lastTerminal = null;
    // Apply a single ProgressEvent to the toast; track terminal so
    // close() can fire onTerminal + finalize cleanly. Shared between
    // the SSE and the polling-fallback code paths.
    function consumeEvent(ev) {
        toast.update({kind: ev.kind, title: ev.title, body: ev.body || ''});
        if (ev.terminal) lastTerminal = ev;
    }
    function close() {
        if (stopped) return;
        stopped = true;
        // Always finalize so a sticky toast can't get orphaned. Two
        // cases land here:
        //   1. Normal completion: `lastTerminal` carries the success/
        //      error event the server emitted; pass it verbatim.
        //   2. Stream ended without a terminal (e.g. job swept after
        //      we lost connection, server closed cleanly because
        //      poll() returned None). Pass an empty descriptor so the
        //      toast un-stickies and dismisses on its normal timeout.
        // Skipping finalize in case 2 was an earlier bug that left
        // the toast stuck across the entire user session.
        if (lastTerminal) {
            toast.finalize({kind: lastTerminal.kind, title: lastTerminal.title, body: lastTerminal.body || ''});
        } else {
            toast.finalize({});
        }
        if (typeof opts.onTerminal === 'function') {
            try { opts.onTerminal(lastTerminal || {}); }
            catch (e) { console.warn('[ryokanProgressToast] onTerminal threw:', e); }
        }
    }

    // SSE-first path. EventSource is universal in modern browsers and
    // gives us push-driven updates without burning a request per
    // 500 ms tick. The server endpoint at
    // `/api/progress/{id}/stream` (`stream_progress` in
    // `src/handlers/progress.rs`) drains the same in-memory buffer
    // the polling endpoint uses, so the two sides are interchangeable
    // and any failure in the SSE path falls back to polling on the
    // legacy endpoint without state loss.
    let es = null;
    let fellBackToPolling = false;
    function startPolling() {
        if (fellBackToPolling || stopped) return;
        fellBackToPolling = true;
        let cursor = 0;
        function schedule(ms) {
            if (stopped) return;
            setTimeout(tick, ms);
        }
        function tick() {
            if (stopped) return;
            fetch('/api/progress/' + encodeURIComponent(opts.progressId) + '?since=' + cursor, {
                credentials: 'same-origin',
            }).then(function (r) {
                if (r.status === 404) { close(); return null; }
                if (!r.ok) throw new Error('progress poll HTTP ' + r.status);
                return r.json();
            }).then(function (payload) {
                if (!payload) return;
                cursor = payload.next_cursor;
                for (let i = 0; i < payload.events.length; i++) {
                    consumeEvent(payload.events[i]);
                }
                if (payload.terminal || lastTerminal) { close(); return; }
                schedule(500);
            }).catch(function (err) {
                console.warn('[ryokanProgressToast] poll failed:', err);
                schedule(2000);
            });
        }
        schedule(500);
    }

    if (typeof EventSource === 'function') {
        try {
            es = new EventSource('/api/progress/' + encodeURIComponent(opts.progressId) + '/stream');
            // Server emits each event as `event: progress` with a JSON
            // payload. EventSource fires one MessageEvent per SSE event
            // when listening on the named event type.
            es.addEventListener('progress', function (msg) {
                if (stopped) return;
                let ev;
                try { ev = JSON.parse(msg.data); }
                catch (e) { console.warn('[ryokanProgressToast] bad SSE payload:', e); return; }
                consumeEvent(ev);
                if (ev.terminal) {
                    if (es) { es.close(); es = null; }
                    close();
                }
            });
            // EventSource auto-reconnects on transient errors. We get
            // notified via `onerror` for *any* error, including the
            // terminal close where the server cleanly ends the stream
            // — Firefox + Chrome both fire `onerror` then move
            // readyState to CLOSED. If we already saw the terminal
            // event, that's expected; otherwise fall back to polling
            // on the legacy endpoint so a 404 / proxy interruption /
            // 500 doesn't strand the toast.
            es.addEventListener('error', function () {
                if (stopped) return;
                // readyState 2 = CLOSED. Browsers set this for both
                // server-side EOF and unrecoverable errors.
                if (es && es.readyState === 2) {
                    es.close();
                    es = null;
                    if (lastTerminal) {
                        close();
                    } else {
                        // Stream closed without a terminal event —
                        // fall through to polling so we don't lose
                        // the job's final state.
                        startPolling();
                    }
                }
            });
        } catch (e) {
            console.warn('[ryokanProgressToast] EventSource construct failed:', e);
            startPolling();
        }
    } else {
        // Browser without EventSource (none in our supported set
        // today, but cheap defensive fallback).
        startPolling();
    }
    return {
        dismiss: function () {
            stopped = true;
            if (es) { es.close(); es = null; }
            toast.dismiss();
        },
        update: function (p) { toast.update(p); },
        finalize: function (p) {
            stopped = true;
            if (es) { es.close(); es = null; }
            toast.finalize(p);
        },
    };
};

// A full page load rebuilds the toasts the previous page still had
// (see the toast runtime above). This has to run after
// `ryokanProgressToast` exists; a boosted swap is a no-op here.
if (typeof window.__ryokanRestoreToasts === 'function') {
    window.__ryokanRestoreToasts();
}

// Auto-promote any `[data-ryokan-toast]` element on the page into a
// ryokanToast on DOM ready. Lets server-side flash banners (Settings,
// System Debug) double as toast notifications without duplicating the
// message content. The banner stays in place as durable context; the
// toast is the transient eye-catch.
//
// Pass `log: false` so we don't re-log these: the backend already
// wrote the log row that produced the flash banner in the first
// place, so re-ingesting via /api/logs/client would double the entry.
window.addEventListener('DOMContentLoaded', function () {
    document.querySelectorAll('[data-ryokan-toast]').forEach(function (el) {
        const kind = el.getAttribute('data-ryokan-toast') || 'info';
        const title = el.getAttribute('data-ryokan-toast-title') || '';
        const body = (el.textContent || '').trim();
        if (!body) return;
        window.ryokanToast({kind: kind, title: title, body: body, log: false});
    });
});

// Declarative confirm-on-submit. Any <form data-ryokan-confirm-title="...">
// has its submit intercepted; the in-app ryokanConfirm modal is shown
// with the configured copy, and the form only submits if the user
// confirms. Replaces the browser-native `onclick="return confirm(...)"`
// pattern so destructive actions get the same dark-themed dialog as
// the rest of the app instead of a system-styled popup that doesn't
// match.
//
// Supported attributes:
//   data-ryokan-confirm-title    (required — picking up the attr is
//                                 the opt-in signal)
//   data-ryokan-confirm-body     ("Are you sure?" if absent)
//   data-ryokan-confirm-yes      ("Yes")
//   data-ryokan-confirm-no       ("Cancel")
//   data-ryokan-confirm-danger   (any truthy value tints the Yes
//                                 button red — use for destructive
//                                 actions like delete / regenerate)
// Read the data-ryokan-confirm-* attributes off `elt` and call
// `window.ryokanConfirm`. Returns the resolved promise. Shared between
// the native-form submit listener (below) and the htmx:confirm bridge
// (further below) so the modal copy is configured the same way for
// both paths.
function ryokanConfirmFromAttrs(elt) {
    return window.ryokanConfirm({
        title: elt.getAttribute('data-ryokan-confirm-title') || 'Confirm',
        body: elt.getAttribute('data-ryokan-confirm-body') || 'Are you sure?',
        yesLabel: elt.getAttribute('data-ryokan-confirm-yes') || 'Yes',
        noLabel: elt.getAttribute('data-ryokan-confirm-no') || 'Cancel',
        danger: !!elt.getAttribute('data-ryokan-confirm-danger'),
    });
}

// Forms split into two paths:
//
//   1. Forms htmx drives — any hx-* verb, or boosted by the body-wide
//      `hx-boost:inherited` (every plain form except the
//      `hx-boost="false"` opt-outs) — are gated through the
//      `htmx:confirm` bridge below. Not via the submit listener:
//      htmx's own submit handler runs first, so the request would
//      already be in flight by the time we prevented anything.
//   2. Forms htmx leaves alone (`hx-boost="false"`) — the submit
//      listener intercepts, shows the modal, and calls
//      `form.submit()` on confirm.
//
// Which path applies is decided at submit time: htmx marks the
// elements it boosted on `elt._htmx.boosted`, which is not set yet
// when this DOMContentLoaded handler runs.
function ryokanFormIsHtmxDriven(form) {
    if (form._htmx && form._htmx.boosted) return true;
    return form.matches('[hx-get], [hx-post], [hx-put], [hx-patch], [hx-delete]');
}

window.addEventListener('DOMContentLoaded', function () {
    document.querySelectorAll('form[data-ryokan-confirm-title]').forEach(function (form) {
        form.addEventListener('submit', function (ev) {
            if (ryokanFormIsHtmxDriven(form)) return; // htmx:confirm bridge owns it
            ev.preventDefault();
            ryokanConfirmFromAttrs(form).then(function (result) {
                if (!result || !result.ok) return;
                // HTMLFormElement.submit() bypasses event handlers
                // by spec — this listener won't re-fire, so no
                // re-prompt loop possible.
                form.submit();
            });
        });
    });
});

// Bridge `data-ryokan-confirm-*` into htmx's request-confirmation hook.
//
// htmx 4 fires `htmx:confirm` only for a request whose context carries
// a confirm (`ctx.confirm`, normally from `hx-confirm`). Rather than
// sprinkle `hx-confirm` over every opt-in element, `htmx:config:request`
// (which fires before the confirm check) stamps a marker onto the
// context for any source element that opted in. The marker is never
// shown: the `htmx:confirm` listener always `preventDefault()`s for
// those elements, which tells htmx to wait for `issueRequest()` /
// `dropRequest()` instead of falling through to `window.confirm`.
// Elements without the opt-in never get a confirm and htmx never
// fires the event for them.
//
// Both listeners sit on <body> rather than per element because htmx
// processes swapped-in content automatically; per-element registration
// would miss anything added to the DOM after initial load.
document.body.addEventListener('htmx:config:request', function (ev) {
    var ctx = ev.detail && ev.detail.ctx;
    var elt = ctx && ctx.sourceElement;
    if (elt && elt.hasAttribute && elt.hasAttribute('data-ryokan-confirm-title')) {
        ctx.confirm = 'ryokan';
    }
});
document.body.addEventListener('htmx:confirm', function (ev) {
    var detail = ev.detail || {};
    var elt = detail.ctx && detail.ctx.sourceElement;
    if (!elt || !elt.hasAttribute || !elt.hasAttribute('data-ryokan-confirm-title')) return;
    ev.preventDefault();
    ryokanConfirmFromAttrs(elt).then(function (result) {
        if (result && result.ok) {
            detail.issueRequest();
        } else {
            detail.dropRequest();
        }
    });
});

// The element an `htmx:after:swap` event swapped. htmx 4 dispatches
// that event on the request's *source* element (re-pointed at the
// target when the source was detached by an outerHTML swap of its own
// section, and at `document` only if both are gone), so `ev.target`
// no longer identifies the swapped region. Section re-bind listeners
// compare this instead.
window.ryokanSwapTargetId = function (ev) {
    var ctx = ev && ev.detail && ev.detail.ctx;
    var target = ctx && ctx.target;
    return (target && target.id) || '';
};

// HTML-escape a string for safe concatenation into an `innerHTML`
// sink. Use this wherever a user-controlled value (release title, CF
// name, fetched error message, etc.) flows into a template literal
// that's assigned to `.innerHTML`. Prefer DOM APIs (textContent,
// createElement) for new code where feasible; this helper exists for
// the "I'm building an HTML string with 5 interpolations and only
// two of them are user-controlled" case.
window.ryokanEscapeHtml = function (value) {
    if (value == null) return '';
    return String(value)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
};

// Generic copy-to-clipboard helper. `text` is the string to copy;
// `btn` is the optional button to flash a confirmation on. Falls back
// to a toast when the clipboard API is unavailable (HTTP contexts
// without a secure origin don't expose navigator.clipboard).
window.ryokanCopy = function (text, btn) {
    if (text == null || text === '') return Promise.resolve();
    // No in-button flash — the toast carries the success/failure
    // feedback. The previous flash mutated `btn.innerHTML` to swap
    // the SVG for "Copied!" text and back; on icon-only buttons
    // (downloads queue actions, settings API-key copy) the
    // round-trip occasionally clobbered padding because any layout
    // change racing with the 1500ms restore left the button
    // visibly squashed. `btn` is now an unused parameter, kept on
    // the signature so existing call sites (`ryokanCopy(hash, this)`)
    // don't need to change.
    void btn;
    const success = function () {
        window.ryokanToast({kind: 'success', title: 'Copied to clipboard', body: '', log: false, duration: 1500});
    };
    const failure = function (err) {
        window.ryokanToast({kind: 'error', title: 'Copy failed', body: (err && err.message) || 'Browser denied clipboard access', log: false});
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
        return navigator.clipboard.writeText(String(text)).then(success).catch(failure);
    }
    // Fallback for non-secure contexts: execCommand path.
    try {
        const ta = document.createElement('textarea');
        ta.value = String(text);
        ta.setAttribute('readonly', '');
        ta.style.position = 'fixed';
        ta.style.left = '-9999px';
        document.body.appendChild(ta);
        ta.select();
        const ok = document.execCommand('copy');
        document.body.removeChild(ta);
        if (ok) { success(); return Promise.resolve(); }
        failure(new Error('execCommand rejected'));
        return Promise.reject();
    } catch (e) {
        failure(e);
        return Promise.reject(e);
    }
};

// Toggle a `type=password` input between masked and revealed. The
// click button's text is flipped between "Show" / "Hide" to mirror
// the masked state; rebinds friendly to per-page lifecycle since
// the function lives on `window` and reads the input by id.
window.ryokanTogglePassword = function (inputId, btn) {
    var input = document.getElementById(inputId);
    if (!input) return;
    if (input.type === 'password') {
        input.type = 'text';
        if (btn) btn.textContent = 'Hide';
    } else {
        input.type = 'password';
        if (btn) btn.textContent = 'Show';
    }
};

// Copy an input element's current value to the clipboard, falling
// back to the same select-and-prompt path `ryokanCopy` uses on
// non-secure contexts. `btn` is unused (toast handles feedback) but
// kept for signature parity with `ryokanCopy(text, btn)`.
window.ryokanCopyInput = function (inputId, btn) {
    var input = document.getElementById(inputId);
    if (!input || !input.value) return Promise.resolve();
    return window.ryokanCopy(input.value, btn);
};

// Relative timestamp rendering. Any element with a `data-ts` attribute
// gets its textContent replaced by a humanized delta ("3m ago",
// "2h ago", "in 58s") and its `title` set to an absolute UTC string.
// Ticks every 30s so stale values don't linger.
//
// Accepted formats on `data-ts`:
//   - SQLite CURRENT_TIMESTAMP ("YYYY-MM-DD HH:MM:SS", treated as UTC)
//   - ISO 8601 ("YYYY-MM-DDTHH:MM:SSZ" or with offset)
//   - Unix epoch seconds (10 digits) or ms (13 digits)
//
// Opt in by adding `data-ts="{{ grab.grabbed_at }}"` and leaving the
// element's textContent empty (or a placeholder that will be replaced).
(function () {
    function parseTimestamp(raw) {
        if (!raw) return null;
        const s = raw.trim();
        if (!s) return null;
        if (/^\d{10}$/.test(s)) return new Date(parseInt(s, 10) * 1000);
        if (/^\d{13}$/.test(s)) return new Date(parseInt(s, 10));
        const m = s.match(/^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(Z|[+-]\d{2}:?\d{2})?$/);
        if (m) {
            const [, y, mo, d, h, mi, se, tz] = m;
            if (!tz) {
                return new Date(Date.UTC(+y, +mo - 1, +d, +h, +mi, +se));
            }
            return new Date(s.replace(' ', 'T'));
        }
        const d = new Date(s);
        return isNaN(d.getTime()) ? null : d;
    }

    function pad2(n) { return n < 10 ? '0' + n : '' + n; }

    function formatAbsolute(d) {
        return d.getUTCFullYear() + '-' + pad2(d.getUTCMonth() + 1) + '-' + pad2(d.getUTCDate())
            + ' ' + pad2(d.getUTCHours()) + ':' + pad2(d.getUTCMinutes()) + ' UTC';
    }

    function humanize(deltaSec) {
        const abs = Math.abs(deltaSec);
        const future = deltaSec < 0;
        if (abs < 5) return future ? 'just now' : 'just now';
        let value, unit;
        if (abs < 60) { value = abs; unit = 's'; }
        else if (abs < 3600) { value = Math.round(abs / 60); unit = 'm'; }
        else if (abs < 86400) { value = Math.round(abs / 3600); unit = 'h'; }
        else if (abs < 30 * 86400) { value = Math.round(abs / 86400); unit = 'd'; }
        else { return null; }
        return future ? 'in ' + value + unit : value + unit + ' ago';
    }

    function refresh() {
        const now = Date.now();
        document.querySelectorAll('[data-ts]').forEach(function (el) {
            const d = parseTimestamp(el.getAttribute('data-ts'));
            if (!d) return;
            const deltaSec = Math.round((now - d.getTime()) / 1000);
            const rel = humanize(deltaSec);
            const abs = formatAbsolute(d);
            // For very old timestamps, show a short date instead of "... ago"
            if (rel === null) {
                el.textContent = abs.slice(0, 10);
            } else {
                el.textContent = rel;
            }
            el.setAttribute('title', abs);
            // Stamp the "rendered" marker so the CSS rule
            // `[data-ts]:not([data-ts-rendered]) { visibility: hidden }`
            // in base.css flips the element visible. Without this
            // marker (and the matching CSS rule), the raw `data-ts`
            // textContent flashes briefly between body-swap and the
            // first refresh tick — visible on every boost-nav to
            // /search, /downloads, /series. The visibility-hidden
            // approach preserves layout (column widths don't reflow)
            // so the flip is just a content reveal, no jolt.
            el.setAttribute('data-ts-rendered', '1');
        });
    }

    // Expose for callers who inject new [data-ts] nodes after DOM ready.
    window.ryokanRefreshTimestamps = refresh;

    window.addEventListener('DOMContentLoaded', refresh);
    // Phase B of the hx-boost rollout — also refresh on every
    // boosted swap. Without this, navigating to a page with new
    // [data-ts] nodes via boost would leave them showing the raw
    // timestamp until the 30s `setInterval` tick caught up.
    if (window.htmx && typeof window.htmx.onLoad === 'function') {
        window.htmx.onLoad(refresh);
    }
    setInterval(refresh, 30000);
})();

// ── Click outside a modal dismisses it ────────────────────────────
// Every modal is a `.modal-backdrop` with the dialog box inside it, so
// a click whose target is the backdrop itself landed outside the box.
// Dismiss through the modal's own close control when it has one (the
// header × or a `data-modal-close` button), so any teardown that
// control runs (cancelling a grab preview, resetting a form) still
// happens; hide the backdrop directly only when there is no control.
// Backdrops that carry their own `onclick` already handle this.
// The mousedown check keeps a text selection that starts inside the
// box and ends on the backdrop from counting as a dismiss.
(function () {
    if (window.__ryokanBackdropDismiss) return;
    window.__ryokanBackdropDismiss = true;
    var pressedOn = null;
    document.addEventListener('mousedown', function (ev) { pressedOn = ev.target; }, true);
    document.addEventListener('click', function (ev) {
        var el = ev.target;
        if (!el || !el.classList || !el.classList.contains('modal-backdrop')) return;
        if (pressedOn !== el) return;
        if (el.hasAttribute('onclick')) return;
        if (el.style.display === 'none' || el.hidden) return;
        var close = el.querySelector('[data-modal-close], .modal-header .btn-icon[aria-label="Close"], .modal-header .btn-icon');
        if (close) {
            close.click();
        } else {
            el.style.display = 'none';
        }
    });
})();
