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
    // body, category, duration}) pushes a new toast into the top-right
    // stack. kind ∈ {info, success, warn, error}. Auto-dismiss after
    // duration ms (default 4000, 0 disables auto-dismiss). Pause on
    // hover. Every toast is also mirrored to POST /api/logs/client
    // so it persists in the System → Logs tab after the transient
    // UI disappears. Pass `category` to classify the log row; falls
    // back to `system` on the server. Pass `log: false` to opt out
    // of persistence (e.g. purely decorative toasts).
    const stack = document.getElementById('ryokan-toast-stack');

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

    function dismiss(toast) {
        if (!toast || toast.dataset.dismissed === '1') return;
        toast.dataset.dismissed = '1';
        toast.classList.add('ryokan-toast-leaving');
        setTimeout(function () {
            if (toast.parentNode) toast.parentNode.removeChild(toast);
        }, 200);
    }

    window.ryokanToast = function (opts) {
        opts = opts || {};
        const kind = opts.kind && ['info', 'success', 'warn', 'error'].indexOf(opts.kind) >= 0
            ? opts.kind : 'info';
        // `sticky: true` disables auto-dismiss — use for long-running
        // jobs where the toast represents live state and should only
        // close on explicit user action (or when `handle.finalize()`
        // upgrades it to a normal auto-dismissing toast).
        const duration = opts.sticky ? 0 : (opts.duration != null ? Number(opts.duration) : 4000);

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
        const titleEl = document.createElement('div');
        titleEl.className = 'ryokan-toast-title';
        titleEl.textContent = opts.title || '';
        if (!opts.title) titleEl.style.display = 'none';
        content.appendChild(titleEl);
        const bodyEl = document.createElement('div');
        bodyEl.className = 'ryokan-toast-body';
        bodyEl.textContent = opts.body || '';
        if (!opts.body) bodyEl.style.display = 'none';
        content.appendChild(bodyEl);
        toast.appendChild(content);

        const close = document.createElement('button');
        close.type = 'button';
        close.className = 'ryokan-toast-close';
        close.setAttribute('aria-label', 'Dismiss');
        close.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6L6 18M6 6l12 12"/></svg>';
        close.addEventListener('click', function () { dismiss(toast); });
        toast.appendChild(close);

        stack.appendChild(toast);

        let remaining = duration;
        let timerStart = Date.now();
        let timer = null;
        function armTimer() {
            if (remaining <= 0) return;
            timerStart = Date.now();
            timer = setTimeout(function () { dismiss(toast); }, remaining);
        }
        function pauseTimer() {
            if (timer) {
                clearTimeout(timer);
                timer = null;
                remaining -= (Date.now() - timerStart);
            }
        }
        toast.addEventListener('mouseenter', pauseTimer);
        toast.addEventListener('mouseleave', armTimer);
        armTimer();

        return {
            dismiss: function () { dismiss(toast); },
            // Mutate the live toast in place. Used by ryokanProgressToast
            // to repaint title/body/kind as stage events arrive. Does not
            // re-arm the auto-dismiss timer — a sticky toast that's been
            // updating should stay sticky until `finalize()` ends it.
            update: function (patch) {
                patch = patch || {};
                if (patch.kind && ['info', 'success', 'warn', 'error'].indexOf(patch.kind) >= 0) {
                    toast.classList.remove('ryokan-toast-info', 'ryokan-toast-success', 'ryokan-toast-warn', 'ryokan-toast-error');
                    toast.classList.add('ryokan-toast-' + patch.kind);
                    toast.setAttribute('role', patch.kind === 'error' || patch.kind === 'warn' ? 'alert' : 'status');
                }
                if (patch.title != null) {
                    titleEl.textContent = patch.title;
                    titleEl.style.display = patch.title ? '' : 'none';
                }
                if (patch.body != null) {
                    bodyEl.textContent = patch.body;
                    bodyEl.style.display = patch.body ? '' : 'none';
                }
            },
            // Convert a sticky toast into a terminal one that auto-dismisses
            // after `duration` ms (default 4000 for success/info, 0 for
            // warn/error so the user has time to read). Also persists the
            // final state to /api/logs/client once, matching the log
            // persistence behavior of a one-shot ryokanToast call.
            finalize: function (final) {
                final = final || {};
                if (final.kind || final.title != null || final.body != null) {
                    this.update(final);
                }
                const finalKind = final.kind || kind;
                if (final.log !== false) {
                    persistToast(finalKind, opts.category, titleEl.textContent, bodyEl.textContent);
                }
                const finalDuration = final.duration != null
                    ? Number(final.duration)
                    : (finalKind === 'error' || finalKind === 'warn' ? 0 : 4000);
                if (timer) { clearTimeout(timer); timer = null; }
                remaining = finalDuration;
                armTimer();
            },
        };
    };
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
        kind: opts.kind || 'info',
        title: opts.title || 'Working…',
        body: opts.body || null,
        category: opts.category || 'system',
        sticky: true,
        // The terminal event will persist to logs via `finalize()`.
        // Skipping the initial log write avoids a "Working…" row in
        // System → Logs that gets immediately superseded.
        log: false,
    });
    let cursor = 0;
    let stopped = false;
    // Poll interval: 500ms is fast enough that users see updates
    // within ~half the visual debounce window of typical UI spinners,
    // but slow enough that an unattended tab doesn't burn a request
    // per frame. Backoff to 2s after the first successful terminal
    // read so a frontend that failed to catch the terminal event
    // still doesn't keep hammering after the job has been swept.
    function schedule(ms) {
        if (stopped) return;
        setTimeout(tick, ms);
    }
    function tick() {
        if (stopped) return;
        fetch('/api/progress/' + encodeURIComponent(opts.progressId) + '?since=' + cursor, {
            credentials: 'same-origin',
        }).then(function (r) {
            if (r.status === 404) {
                // Job swept or never existed. Treat as a silent stop —
                // the trigger response flow will still give us a final
                // state via its own then/catch.
                stopped = true;
                return null;
            }
            if (!r.ok) throw new Error('progress poll HTTP ' + r.status);
            return r.json();
        }).then(function (payload) {
            if (!payload) return;
            cursor = payload.next_cursor;
            let last = null;
            for (let i = 0; i < payload.events.length; i++) {
                const ev = payload.events[i];
                toast.update({kind: ev.kind, title: ev.title, body: ev.body || ''});
                if (ev.terminal) last = ev;
            }
            if (payload.terminal || last) {
                stopped = true;
                if (last) {
                    toast.finalize({kind: last.kind, title: last.title, body: last.body || ''});
                } else {
                    // Terminal flag set but no terminal event in this
                    // batch — shouldn't happen, but don't lock up
                    // if it does.
                    toast.finalize({});
                }
                return;
            }
            schedule(500);
        }).catch(function (err) {
            console.warn('[ryokanProgressToast] poll failed:', err);
            // Network hiccup: back off to avoid flooding. Don't stop —
            // the job might still complete and the next tick will
            // catch up.
            schedule(2000);
        });
    }
    schedule(500);
    return {
        dismiss: function () {
            stopped = true;
            toast.dismiss();
        },
        update: function (p) { toast.update(p); },
        finalize: function (p) {
            stopped = true;
            toast.finalize(p);
        },
    };
};

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
        });
    }

    // Expose for callers who inject new [data-ts] nodes after DOM ready.
    window.ryokanRefreshTimestamps = refresh;

    window.addEventListener('DOMContentLoaded', refresh);
    setInterval(refresh, 30000);
})();
