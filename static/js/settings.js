// HTMX migration (issue #129) — the confirm-modal wiring for forms
// with `data-ryokan-confirm-title` lives in `base.js` now (one
// DOMContentLoaded listener for native form-POST forms + an
// `htmx:confirm` body listener for HTMX forms, both routed through
// `ryokanConfirmFromAttrs`). The IIFE that previously lived here
// was a stale duplicate that read the renamed `data-ryokan-confirm-label`
// attribute (gone since PR 131); `base.js`'s shim is the single
// source of truth for both paths.

// #11.1 — Client-side filter for the CF card grid. Matches against
// `data-cf-*` attributes on each card (name is pre-lowercased
// server-side, score/trash_id/origin match case-insensitively). Empty
// query = show all. Updates the `#cf-visible-count` pill and toggles
// the "no matches" placeholder so the grid doesn't render as a blank
// void when every card is filtered out.
function filterCfList(query) {
    const q = (query || '').trim().toLowerCase();
    const grid = document.getElementById('cf-list-tbody');
    if (!grid) return;
    const cards = grid.querySelectorAll('.cf-card:not(.cf-card-add)');
    let visible = 0;
    cards.forEach(function(card) {
        if (!q) {
            card.style.display = '';
            visible++;
            return;
        }
        const name = card.dataset.cfName || '';
        const score = (card.dataset.cfScore || '').toLowerCase();
        const trashId = card.dataset.cfTrashId || '';
        const origin = (card.dataset.cfOrigin || '').toLowerCase();
        const hit = name.includes(q)
            || score.includes(q)
            || trashId.includes(q)
            || origin.includes(q);
        card.style.display = hit ? '' : 'none';
        if (hit) visible++;
    });
    const countEl = document.getElementById('cf-visible-count');
    if (countEl) countEl.textContent = visible;
    const emptyEl = document.getElementById('cf-list-empty-filter');
    if (emptyEl) emptyEl.style.display = (visible === 0 && cards.length > 0) ? '' : 'none';
    // Hide the trailing "+ Add" tile while the user is filtering —
    // mixing it into search results reads as noise.
    const addCard = grid.querySelector('.cf-card-add');
    if (addCard) addCard.style.display = q ? 'none' : '';
}

// #11.1 stage 2 — modal editor open/close. The modal element itself is
// server-rendered with the pre-filled form when ?edit_id=N is set, so
// auto-opening is just flipping display. "+ Add Custom Format" opens a
// fresh modal — but since the form markup is tied to server-side edit
// state, clicking + on a page that's rendering in edit mode would
// open the edit form, not a blank one. Fix by clearing edit_id via a
// GET navigation first whenever the user hits + from an edit-mode page.
function openCfEditorModal() {
    const modal = document.getElementById('cf-editor-modal');
    if (!modal) return;
    // Reset the form to "Add Custom Format" shape in-place. The form
    // was server-rendered with the previous edit's values if the page
    // load had ?edit_id=N, so just flipping display:flex would show
    // those stale fields. Navigate-to-reset (the previous fix) worked
    // but required two clicks to see the empty modal. Clearing in-place
    // keeps it a single click.
    const form = modal.querySelector('form');
    if (form) {
        const hiddenId = form.querySelector('input[type="hidden"][name="id"]');
        if (hiddenId) hiddenId.remove();
        const trashDesc = form.querySelector('.cf-trash-description');
        if (trashDesc) trashDesc.remove();
        const name = form.querySelector('#cf_name');
        if (name) name.value = '';
        const score = form.querySelector('#cf_score');
        if (score) score.value = '0';
        const trashId = form.querySelector('#cf_trash_id');
        if (trashId) trashId.value = '';
        const json = form.querySelector('#cf_json');
        if (json) json.value = '';
    }
    const title = document.getElementById('cf-editor-modal-title');
    if (title) title.textContent = 'Add Custom Format';
    const submit = document.getElementById('cf-upsert-submit');
    if (submit) submit.textContent = 'Create Custom Format';
    // Hide the Delete form — nothing to delete when we haven't saved
    // yet. Leaving it visible in Add mode would let the user click
    // Delete with the previously-edited CF's hidden id still in the
    // form (server-rendered). Hiding also collapses the footer to the
    // single-button layout on the right.
    const deleteForm = document.getElementById('cf-delete-form');
    if (deleteForm) deleteForm.style.display = 'none';

    modal.style.display = 'flex';
    const nameEl = document.getElementById('cf_name');
    if (nameEl) nameEl.focus();
}
function closeCfEditorModal() {
    const modal = document.getElementById('cf-editor-modal');
    if (modal) modal.style.display = 'none';
    // Drop ?edit_id=N from the URL without reloading so Cancel doesn't
    // leave the user on a URL that'd re-open the modal on refresh.
    const params = new URLSearchParams(window.location.search);
    if (params.has('edit_id')) {
        params.delete('edit_id');
        const newUrl = window.location.pathname + (params.toString() ? '?' + params.toString() : '');
        history.replaceState(null, '', newUrl);
    }
    // Drop the selected-card highlight. The class was server-rendered
    // based on ?edit_id=N, so closing without a full reload leaves the
    // highlight stuck on the originally-edited card until next refresh.
    document.querySelectorAll('.cf-card-selected').forEach(function(el) {
        el.classList.remove('cf-card-selected');
    });
}
// CF editor modal open/close lifecycle is now mostly server-driven:
// the partial template renders `display:flex` when the handler set
// `custom_format_edit = Some(...)` (i.e. the URL had `?edit_id=N`).
// The JS-side dance of reading `window.location.search` from an
// inline IIFE got into trouble under hx-boost — htmx pushes the URL
// *after* inserted body scripts evaluate, so on a boost-nav the IIFE
// would see the previous page's URL and leave the modal hidden until
// the user did a hard refresh. Letting the server own the initial
// display state sidesteps the timing problem entirely.
//
// What's left for JS: backdrop click + Escape dismissal. Backdrop
// listener attaches per visit (modal element is fresh each render);
// Escape listener is gated so it doesn't accumulate on `document`.
(function() {
    const modal = document.getElementById('cf-editor-modal');
    if (modal) {
        modal.addEventListener('click', function(ev) {
            if (ev.target === modal) closeCfEditorModal();
        });
    }
    if (window.__ryokanCfEditorEscBound) return;
    window.__ryokanCfEditorEscBound = true;
    document.addEventListener('keydown', function(ev) {
        const m = document.getElementById('cf-editor-modal');
        if (!m) return;
        if (ev.key === 'Escape' && m.style.display !== 'none') closeCfEditorModal();
    });
})();

// ── Settings → Download Clients shared add/edit modal ──────────────
// One modal serves both flows. Per-card click → openDcEditModal(id,
// name); "+ Add" tile → openDcAddModal(). Each clears the modal body
// (so the previous form's content doesn't briefly show through),
// opens the modal immediately for instant feedback, and fires
// htmx.ajax() to fetch the right form body into `#dc-modal-body`.
//
// The form is rendered server-side with the kind-aware shape
// (visibility, label names, input types) baked in for the row's
// kind, so when the swap lands it's already correct — no async JS
// relabel pass on first paint, no structural flash. The JS path in
// `applyDcKindCopy` still owns the live kind-flip case (user toggles
// the dropdown after the modal is open). Keep `DC_KIND_COPY` in JS
// in lockstep with `copy_for_kind` in
// `src/handlers/settings/download_clients.rs`.
//
// After a successful save the form's hx-target="#dc-section" causes
// the server's section-partial response to replace the entire
// section, including the modal, at display:none with the Add form
// back in body — closing + resetting in one shot. No manual
// `hx-on::after:request="closeModal()"` needed (and removed because
// `<button>` containing block content was getting auto-closed by
// the parser, breaking the inline JS hooks anyway — the click
// handlers live on `<div role="button">` now).
function openDownloadClientModal(title) {
    const modal = document.getElementById('dc-modal');
    if (!modal) return;
    if (typeof title === 'string' && title.length > 0) {
        const titleEl = document.getElementById('dc-modal-title');
        if (titleEl) titleEl.textContent = title;
    }
    modal.style.display = 'flex';
    // Focus the first text/url input in the body for keyboard
    // ergonomics. The body may be empty here (we clear it on click
    // so the previous form doesn't flash through while the fetch
    // is in flight); the htmx:after:settle listener below picks up
    // the focus once the form lands. querySelector matches in DOM
    // order so the Name field wins on both Add and Edit forms.
    const firstInput = modal.querySelector('input[type="text"], input[type="url"]');
    if (firstInput) firstInput.focus();
}
function focusDcModalFirstInput() {
    const modal = document.getElementById('dc-modal');
    if (!modal || modal.style.display === 'none') return;
    const firstInput = modal.querySelector('input[type="text"], input[type="url"]');
    if (firstInput) firstInput.focus();
}
function closeDownloadClientModal() {
    const modal = document.getElementById('dc-modal');
    if (modal) modal.style.display = 'none';
}
function fetchAndOpenDcModal(url, title) {
    // Clear the modal body so the previous form's content doesn't
    // briefly show through while the fetch is in flight. The form
    // rebuilds in place when the swap lands — kind-aware rendering
    // is server-side now, so the swap is correct on arrival and no
    // JS relabel pass is needed on first paint.
    const body = document.getElementById('dc-modal-body');
    if (body) body.innerHTML = '';
    openDownloadClientModal(title);
    if (window.htmx) {
        window.htmx.ajax(
            'GET',
            url,
            { target: '#dc-modal-body', swap: 'innerHTML' }
        );
    }
}
function openDcEditModal(id, name) {
    fetchAndOpenDcModal(
        '/settings/download-clients/' + encodeURIComponent(id) + '/edit-form',
        'Editing ' + (name || 'download client')
    );
}
function openDcAddModal() {
    fetchAndOpenDcModal(
        '/api/download-clients/add-form',
        'Add download client'
    );
}
// One-shot guard wraps every `addEventListener` at module scope in this
// file. hx-boost re-runs the script on each nav-back, so an unguarded
// `addEventListener` accumulates a copy per visit (Nth visit fires N
// callbacks). Same pattern applied to the 2 `htmx:after:settle` and 1
// `DOMContentLoaded` listeners further down the file.
if (!window.__ryokanSettingsTriggerListeners) {
    window.__ryokanSettingsTriggerListeners = true;
    // Test-connection result — server fires `ryokan-dc-test-result` via
    // HX-Trigger header (empty response body, so the modal footer's
    // button row doesn't grow to fit the message). Convert to a toast
    // that surfaces at the top of the viewport regardless of message
    // length.
    document.body.addEventListener('ryokan-dc-test-result', function (ev) {
        const detail = ev.detail || {};
        window.ryokanToast({
            kind: detail.ok ? 'success' : 'error',
            title: detail.ok ? 'Connection OK' : 'Connection failed',
            body: detail.message || '',
        });
    });
    // Indexer Test result — same shape as the DC variant. Server fires
    // `ryokan-indexer-test-result` via HX-Trigger from /api/indexers/test.
    // Used by both the modal-footer Test button (Add and Edit) and the
    // per-card Test button on the configured-indexer cards.
    document.body.addEventListener('ryokan-indexer-test-result', function (ev) {
        const detail = ev.detail || {};
        window.ryokanToast({
            kind: detail.ok ? 'success' : 'error',
            category: 'indexer',
            title: detail.ok ? 'Indexer reachable' : 'Indexer test failed',
            body: detail.message || '',
        });
    });
}
// Companion to the modal-footer Test button. htmx's `hx-disable`
// re-enables the button on htmx:after:request automatically, but we
// also want the button text to flash "Testing…" while the request is
// in flight so the user has visual feedback. No-op when called
// outside an htmx context.
window.ryokanWaitForIndexerTest = function (btn) {
    if (!btn) return;
    const original = btn.textContent;
    btn.textContent = 'Testing…';
    const restore = function () { btn.textContent = original; };
    // htmx:after:request fires once per request. Use a one-shot
    // listener scoped to this btn so concurrent Test clicks elsewhere
    // don't restore each other prematurely.
    const handler = function (ev) {
        if (ev.target === btn) {
            restore();
            btn.removeEventListener('htmx:after:request', handler);
        }
    };
    btn.addEventListener('htmx:after:request', handler);
    // Safety net: if htmx swallows the event for some reason (e.g. the
    // request errors out before sending), the disabled-elt timer
    // restores the button after 6s.
    setTimeout(function() {
        if (btn.textContent === 'Testing…') restore();
    }, 6000);
};
// Backdrop-click + Escape dismissal. Re-bound on every section swap
// because the modal element is replaced when #dc-section re-renders;
// htmx 4 fires `htmx:after:swap` on the request's source element (or
// `document` once that element was swapped out), so we listen on body
// once at module scope, match the swapped region via
// `ryokanSwapTargetId`, and re-attach the per-modal listeners.
(function() {
    function bindDownloadClientModal() {
        const modal = document.getElementById('dc-modal');
        if (!modal) return;
        if (modal.dataset.bound === '1') return;
        modal.dataset.bound = '1';
        modal.addEventListener('click', function(ev) {
            if (ev.target === modal) closeDownloadClientModal();
        });
    }
    bindDownloadClientModal();
    if (window.__ryokanDcModalGlobalListeners) return;
    window.__ryokanDcModalGlobalListeners = true;
    document.body.addEventListener('htmx:after:swap', function(ev) {
        if (window.ryokanSwapTargetId(ev) === 'dc-section') {
            bindDownloadClientModal();
        }
    });
    document.addEventListener('keydown', function(ev) {
        const modal = document.getElementById('dc-modal');
        if (!modal) return;
        if (ev.key === 'Escape' && modal.style.display !== 'none') {
            closeDownloadClientModal();
        }
    });
})();

// ── DC modal: kind-aware credential / hint relabel ────────────────
//
// The download_clients table has one schema (name, kind, url, username,
// password, label, download_path) but five client kinds map onto it
// differently — most notably SAB, where the `password` column carries
// the API key and `username` is unused. The form templates carry the
// raw column names + generic labels by default, then this helper
// rewrites labels / hints / input types when the kind dropdown
// changes so the UI accurately describes what each field means for
// the currently-selected kind.
//
// Wired off `data-dc-*` markers so it works for both add_form_body
// and edit_form_body without per-form duplication. Re-runs on:
//   1. modal open (htmx:after:swap into #dc-modal-body) — sets the
//      initial state for an Edit form whose kind is already
//      pre-selected.
//   2. kind dropdown change — handles the user flipping kinds while
//      composing the form.
//
// The DC_KIND_COPY map is the source of truth for per-kind copy.
// Keep new kinds here in sync with `models::download_clients`'s
// `protocol_for_kind` so the protocol-mismatch guard at save time
// doesn't reject inputs the form description encouraged the user
// to enter.
// `var` (not `const`) at module scope is deliberate across every per-
// page JS file: htmx body-swap re-executes the inserted `<script>` tag
// on every navigation back to a previously-visited page, and a
// `let`/`const` redeclaration is a parser-stage SyntaxError that
// rejects the whole file. See `feedback_no_module_scope_dom_under_boost`
// memory and the matching note at the top of `system.js`.
var DC_KIND_COPY = {
    qbittorrent: {
        url_placeholder: 'http://localhost:8080',
        url_hint: "Point at qBittorrent's Web UI base. Ryokan handles the API path internally.",
        username_visible: true,
        username_hint: "qBit's Web UI username (default is admin).",
        password_label: 'Password',
        password_type: 'password',
        password_hint: "qBit's Web UI password. qBittorrent 4.6.1+ generates a random temporary password on first start. Pre-4.6.1's default password is 'adminadmin'.",
        label_label: 'Category',
        label_hint: "qBit category Ryokan tags every torrent with. Determines scoping (Ryokan only sees torrents in this category) AND the post-processing target directory if qBit's category-rule has one set.",
    },
    deluge: {
        url_placeholder: 'http://localhost:8112',
        url_hint: "Point at Deluge's Web UI base.",
        username_visible: false,
        username_hint: '',
        password_label: 'Password',
        password_type: 'password',
        password_hint: "Deluge Web UI password. Deluge has no per-user auth at the API layer; the password is the only credential.",
        label_label: 'Label',
        label_hint: "Deluge's Label plugin tag. The plugin must be enabled; Ryokan auto-enables it on first connect when Label shows up in available_plugins but not enabled_plugins.",
    },
    transmission: {
        url_placeholder: 'http://localhost:9091',
        url_hint: "Point at Transmission's RPC endpoint base.",
        username_visible: true,
        username_hint: "Transmission HTTP Basic auth user (matches rpc-username in settings.json).",
        password_label: 'Password',
        password_type: 'password',
        password_hint: "Transmission HTTP Basic auth password (matches rpc-password in settings.json).",
        label_label: 'Label',
        label_hint: "Transmission native label (4.x+). On 3.x and earlier Ryokan falls back to a save-path prefix for scoping.",
    },
    rtorrent: {
        url_placeholder: 'http://localhost/RPC2',
        url_hint: "Point at rtorrent's XML-RPC endpoint (typically /RPC2 under the SCGI / nginx proxy).",
        username_visible: true,
        username_hint: "HTTP Basic auth user if the RPC endpoint is fronted by nginx with auth_basic. Leave blank for unauthenticated RPC.",
        password_label: 'Password',
        password_type: 'password',
        password_hint: "HTTP Basic auth password matching the username above.",
        label_label: 'Label',
        label_hint: "Sets the custom1 field on every added torrent (the ruTorrent label convention). Ryokan filters list_scoped by this tag.",
    },
    sabnzbd: {
        url_placeholder: 'http://localhost:8080',
        url_hint: "Point at SABnzbd's Web UI base. Ryokan appends /api. If your SAB has URL_BASE set (e.g. /sabnzbd), include it: http://host:8080/sabnzbd.",
        username_visible: false,
        username_hint: '',
        password_label: 'API Key',
        // text (not password) so the user can see what they pasted.
        // API keys aren't really secrets in the way a user-chosen
        // password is, and verifying the value visually is more
        // useful here than masking it.
        password_type: 'text',
        password_hint: "SABnzbd's API key. Find it in SABnzbd → Config → General → Security → API Key.",
        label_label: 'Category',
        label_hint: "SAB category. Determines the post-processing target directory. Ryokan filters list_scoped by category so it only sees jobs it added.",
    },
};
// Map a download-client kind to its protocol family. Mirrors
// `models::download_clients::protocol_for_kind` on the server side;
// keep them in sync if a new kind lands.
function dcProtocolForKind(kind) {
    if (kind === 'sabnzbd') return 'usenet';
    return 'torrent';
}
function applyDcKindCopy(form, kind) {
    if (!form) return;
    const copy = DC_KIND_COPY[kind] || DC_KIND_COPY.qbittorrent;

    // Per-protocol "first client" auto-check on the Default checkbox.
    // The handler stamps `data-first-torrent` / `data-first-usenet`
    // on the checkbox at render time so a kind flip can re-evaluate
    // without a server round-trip. Only auto-check on KIND CHANGE
    // (not on every relabel pass) so the user can uncheck the box
    // and have the uncheck stick across other field edits — without
    // this guard, every change to URL / username etc. would re-run
    // the relabel and force-check the box back. The
    // `dc-prev-kind` data attr tracks "what kind was previously
    // selected" so we only flip the checkbox the moment the user
    // changes the dropdown.
    const defaultBox = form.querySelector('[data-dc-default-checkbox]');
    if (defaultBox) {
        const prevKind = form.dataset.dcPrevKind;
        if (prevKind !== kind) {
            const protocol = dcProtocolForKind(kind);
            const flag = protocol === 'usenet' ? defaultBox.dataset.firstUsenet : defaultBox.dataset.firstTorrent;
            defaultBox.checked = flag === '1';
        }
        form.dataset.dcPrevKind = kind;
    }

    const urlInput = form.querySelector('[data-dc-url-input]');
    if (urlInput) urlInput.placeholder = copy.url_placeholder;
    const urlHint = form.querySelector('[data-dc-url-hint]');
    if (urlHint) urlHint.textContent = copy.url_hint;

    const usernameGroup = form.querySelector('[data-dc-username-group]');
    if (usernameGroup) {
        usernameGroup.style.display = copy.username_visible ? '' : 'none';
    }
    const usernameHint = form.querySelector('[data-dc-username-hint]');
    if (usernameHint) usernameHint.textContent = copy.username_hint;

    const passwordLabel = form.querySelector('[data-dc-password-label]');
    if (passwordLabel) passwordLabel.textContent = copy.password_label;
    const passwordInput = form.querySelector('[data-dc-password-input]');
    if (passwordInput) passwordInput.type = copy.password_type;
    const passwordHint = form.querySelector('[data-dc-password-hint]');
    if (passwordHint) {
        if (copy.password_hint) {
            passwordHint.textContent = copy.password_hint;
            passwordHint.style.display = '';
        } else {
            passwordHint.textContent = '';
            passwordHint.style.display = 'none';
        }
    }

    const labelLabel = form.querySelector('[data-dc-label-label]');
    if (labelLabel) labelLabel.textContent = copy.label_label;
    const labelHint = form.querySelector('[data-dc-label-hint]');
    if (labelHint) labelHint.textContent = copy.label_hint;
}
function bindDcKindCopyToForm(form) {
    if (!form || form.dataset.dcKindBound === '1') return;
    form.dataset.dcKindBound = '1';
    const kindSelect = form.querySelector('[data-dc-kind-select]');
    if (!kindSelect) return;
    // Pre-seed `dc-prev-kind` to the CURRENT kind so the initial
    // `applyDcKindCopy` pass skips the Default-checkbox toggle. The
    // server already rendered the right initial state — for Add via
    // `first_torrent_client` (kind defaults to qBit/torrent), for
    // Edit via `row.is_default || first-of-protocol`. Without this
    // pre-seed, the initial pass would clobber Edit's checkbox state
    // (a user editing a non-default torrent client when another
    // torrent client IS the default would see the box auto-checked,
    // which is wrong).
    form.dataset.dcPrevKind = kindSelect.value;
    applyDcKindCopy(form, kindSelect.value);
    kindSelect.addEventListener('change', function() {
        applyDcKindCopy(form, kindSelect.value);
    });
}
// One-shot guard — see top of file for rationale.
if (!window.__ryokanSettingsDcModalListeners) {
    window.__ryokanSettingsDcModalListeners = true;
    // htmx swaps the modal-body when an Edit/Add modal opens. Run the
    // relabel pass on every fresh body so the initial state matches the
    // pre-selected kind (Edit) or the qBittorrent default (Add).
    document.body.addEventListener('htmx:after:settle', function(ev) {
        if (ev.target && ev.target.id === 'dc-modal-body') {
            const form = ev.target.querySelector('form');
            if (form) bindDcKindCopyToForm(form);
            // Pick up the focus the body-clear-on-open dance left
            // pending — `openDownloadClientModal` ran before the swap
            // landed so its querySelector found nothing, and the
            // user's first keystroke would otherwise go nowhere.
            focusDcModalFirstInput();
        }
    });
    // Initial load (the section partial pre-renders the Add form body
    // so the modal opens fast on first click — see
    // download_clients/list.html
    // `{%~ include "...add_form_body.html" %}`). Apply the relabel pass
    // to that pre-rendered form too so the user sees correct copy if
    // they happen to change kind before any modal swap fires.
    //
    // DOMContentLoaded already fired by the time hx-boost re-runs this
    // file on a nav-back, so the listener attaches but never fires
    // again. Run the apply directly to cover the boost-nav case.
    window.addEventListener('DOMContentLoaded', function() {
        document.querySelectorAll('#dc-modal-body form').forEach(bindDcKindCopyToForm);
    });
    document.querySelectorAll('#dc-modal-body form').forEach(bindDcKindCopyToForm);
}

// ── Settings → Indexers shared add/edit modal ─────────────────────
// Mirrors the DC modal flow: clear the body on click, open the
// modal immediately for instant feedback, fire htmx.ajax to fill
// the body. Indexer kind-aware copy (URL placeholder + API-key
// hint) is server-rendered for `row.kind` (see commit 723fac4),
// so the swap is correct on first paint and we don't need to
// pre-pass the relabel before opening — same one-shape-for-both
// the review pass landed on.
//
// Catalog seed cards → `openIndexerAddModal(slug, name)` fetches
// the Add form pre-filled with that seed's defaults; existing-
// indexer cards → `openIndexerEditModal(id, name)` fetches the
// row's Edit form. Both land in `#indexer-modal-body`. After a
// successful save the form's hx-target="#indexer-section" causes
// the server's section-partial response to replace the entire
// section, including the modal, at display:none — closing +
// resetting in one shot.
function openIndexerModal(title) {
    const modal = document.getElementById('indexer-modal');
    if (!modal) return;
    if (typeof title === 'string' && title.length > 0) {
        const titleEl = document.getElementById('indexer-modal-title');
        if (titleEl) titleEl.textContent = title;
    }
    modal.style.display = 'flex';
    // Focus the first text/url input in the body for keyboard
    // ergonomics. The body may be empty here (we clear it on click
    // so the previous form doesn't flash through while the fetch is
    // in flight); the htmx:after:settle listener below picks up the
    // focus once the form lands.
    const firstInput = modal.querySelector('input[type="text"], input[type="url"]');
    if (firstInput) firstInput.focus();
}
function focusIndexerModalFirstInput() {
    const modal = document.getElementById('indexer-modal');
    if (!modal || modal.style.display === 'none') return;
    const firstInput = modal.querySelector('input[type="text"], input[type="url"]');
    if (firstInput) firstInput.focus();
}
function closeIndexerModal() {
    const modal = document.getElementById('indexer-modal');
    if (modal) modal.style.display = 'none';
}
function fetchAndOpenIndexerModal(url, title) {
    // Same clear-then-open shape as `fetchAndOpenDcModal` — drops
    // the previous form's content so a rapid Edit↔Add toggle
    // doesn't flash the prior row's fields into view, opens the
    // modal immediately for instant click feedback, lets htmx fill
    // the body in.
    const body = document.getElementById('indexer-modal-body');
    if (body) body.innerHTML = '';
    openIndexerModal(title);
    if (window.htmx) {
        window.htmx.ajax(
            'GET',
            url,
            { target: '#indexer-modal-body', swap: 'innerHTML' }
        );
    }
}
function openIndexerEditModal(id, name) {
    fetchAndOpenIndexerModal(
        '/settings/indexers/' + encodeURIComponent(id) + '/edit-form',
        'Editing ' + (name || 'indexer')
    );
}
function openNyaaEditModal() {
    fetchAndOpenIndexerModal('/settings/indexers/nyaa/edit-form', 'Nyaa');
}
function openIndexerAddModal(slug, name) {
    const url = slug
        ? '/settings/indexers/add-form?template=' + encodeURIComponent(slug)
        : '/settings/indexers/add-form';
    fetchAndOpenIndexerModal(url, 'Add ' + (name || 'indexer'));
}
// Backdrop-click + Escape dismissal. Re-bound on every section swap
// because the modal element is replaced when #indexer-section
// re-renders.
(function() {
    function bindIndexerModal() {
        const modal = document.getElementById('indexer-modal');
        if (!modal) return;
        if (modal.dataset.bound === '1') return;
        modal.dataset.bound = '1';
        modal.addEventListener('click', function(ev) {
            if (ev.target === modal) closeIndexerModal();
        });
    }
    bindIndexerModal();
    if (window.__ryokanIndexerModalGlobalListeners) return;
    window.__ryokanIndexerModalGlobalListeners = true;
    document.body.addEventListener('htmx:after:swap', function(ev) {
        if (window.ryokanSwapTargetId(ev) === 'indexer-section') {
            bindIndexerModal();
        }
    });
    document.addEventListener('keydown', function(ev) {
        const modal = document.getElementById('indexer-modal');
        if (!modal) return;
        if (ev.key === 'Escape' && modal.style.display !== 'none') {
            closeIndexerModal();
        }
    });
})();

// ── Indexer modal: kind-aware hint relabel ────────────────────────
//
// Same shape as the DC modal's `applyDcKindCopy` but for the
// indexer Add/Edit form. The protocol-specific hint under the API
// Key field calls out the wire format ("torznab spec" vs "newznab
// spec") and the URL placeholder follows Prowlarr's per-protocol
// path conventions. Without this, both kinds shared the torznab-
// only hint copy, which read incorrectly when the user picked
// newznab.
var INDEXER_KIND_COPY = {
    torznab: {
        url_placeholder: 'https://prowlarr.local/{N}/api',
        api_key_hint: "Sent in the request URL per torznab spec; appears in Prowlarr / Jackett access logs and any reverse-proxy logs in front of them. Find this key in Prowlarr Settings → General (or Jackett's UI).",
    },
    newznab: {
        url_placeholder: 'https://nzb.indexer.example/api',
        api_key_hint: "Sent in the request URL per newznab spec; the same key Sonarr/Radarr/Prowlarr use against this indexer. For Prowlarr-fronted indexers, find it in Prowlarr Settings → General; for direct-to-indexer setups, find it on the indexer's site (e.g. NZBGeek → Profile → API Key).",
    },
};
function applyIndexerKindCopy(form, kind) {
    if (!form) return;
    const copy = INDEXER_KIND_COPY[kind] || INDEXER_KIND_COPY.torznab;
    const urlInput = form.querySelector('[data-indexer-url-input]');
    if (urlInput) urlInput.placeholder = copy.url_placeholder;
    const apiKeyHint = form.querySelector('[data-indexer-api-key-hint]');
    if (apiKeyHint) apiKeyHint.textContent = copy.api_key_hint;
}
function bindIndexerKindCopyToForm(form) {
    if (!form || form.dataset.indexerKindBound === '1') return;
    form.dataset.indexerKindBound = '1';
    const kindSelect = form.querySelector('[data-indexer-kind-select]');
    if (!kindSelect) return;
    applyIndexerKindCopy(form, kindSelect.value);
    kindSelect.addEventListener('change', function() {
        applyIndexerKindCopy(form, kindSelect.value);
    });
}
// ── Indexer modal: category chips ─────────────────────────────────
//
// The Edit form folds what the indexer's caps report under the
// Categories field as chips. A chip toggles its id in the comma list
// and reads as on while the list holds it, so the field stays a plain
// text input (paste a list, type an id the indexer does not report)
// and the chips are a shortcut, not the source of truth.
function indexerCategoryIds(input) {
    return input.value.split(',').map(function(s) { return s.trim(); }).filter(Boolean);
}
function paintIndexerCategoryChips(form) {
    const input = form.querySelector('[data-indexer-categories-input]');
    if (!input) return;
    const ids = indexerCategoryIds(input);
    form.querySelectorAll('.cat-chip[data-cat-id]').forEach(function(chip) {
        chip.classList.toggle('is-on', ids.indexOf(chip.dataset.catId) !== -1);
    });
}
function bindIndexerCategoryChips(form) {
    if (!form || form.dataset.indexerChipsBound === '1') return;
    form.dataset.indexerChipsBound = '1';
    const input = form.querySelector('[data-indexer-categories-input]');
    if (!input) return;
    form.addEventListener('click', function(ev) {
        const chip = ev.target.closest('.cat-chip[data-cat-id]');
        if (!chip) return;
        ev.preventDefault();
        const id = chip.dataset.catId;
        let ids = indexerCategoryIds(input);
        if (ids.indexOf(id) === -1) {
            ids.push(id);
        } else {
            ids = ids.filter(function(x) { return x !== id; });
        }
        input.value = ids.join(', ');
        paintIndexerCategoryChips(form);
    });
    input.addEventListener('input', function() { paintIndexerCategoryChips(form); });
    paintIndexerCategoryChips(form);
}
// One-shot guard — see top of file for rationale.
if (!window.__ryokanSettingsIndexerModalListener) {
    window.__ryokanSettingsIndexerModalListener = true;
    document.body.addEventListener('htmx:after:settle', function(ev) {
        if (ev.target && ev.target.id === 'indexer-modal-body') {
            const form = ev.target.querySelector('form');
            if (form) {
                bindIndexerKindCopyToForm(form);
                bindIndexerCategoryChips(form);
            }
            // Pick up focus the body-clear-on-open dance left
            // pending — `openIndexerModal` ran before the swap
            // landed, so its querySelector found nothing.
            focusIndexerModalFirstInput();
        }
    });
}

// #11.4 — CF export selector. Radios pick the mode, checkboxes pick the
// ids, then two actions: download the file (via the existing GET endpoint)
// or copy the pretty-printed JSON to the clipboard (same endpoint, fetch
// then navigator.clipboard.writeText).

// #11.3 — When a JSON file is picked, read it and drop into the paste
// textarea so the user doesn't need a two-step ceremony. The server-side
// flow stays the same (POST payload → preview → resolve).
function cfImportFilePicked(input) {
    const file = input.files && input.files[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = function(ev) {
        const target = document.getElementById('cf_import_payload');
        if (target && ev.target && typeof ev.target.result === 'string') {
            target.value = ev.target.result;
            target.focus();
        }
    };
    reader.onerror = function() {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Import', body: 'Could not read the file.' });
        }
    };
    reader.readAsText(file);
}

function cfExportSelectAll(state) {
    document.querySelectorAll('input[name="cf_export_ids"]').forEach(function(cb) {
        cb.checked = state;
    });
}

function cfExportBuildUrl() {
    const mode = document.querySelector('input[name="cf_export_mode"]:checked');
    const ids = Array.from(
        document.querySelectorAll('input[name="cf_export_ids"]:checked')
    ).map(function(cb) { return cb.value; });
    const params = new URLSearchParams();
    if (mode && mode.value) params.set('mode', mode.value);
    // Only attach `ids` when the user has deselected at least one; an
    // empty `ids` param would be interpreted by the server as "all" (by
    // design — keeps curl workflows unchanged), but sending the full list
    // as the default is also harmless.
    if (ids.length > 0) params.set('ids', ids.join(','));
    const qs = params.toString();
    return '/settings/custom-formats/export' + (qs ? '?' + qs : '');
}

function cfExportDownload() {
    // Simplest possible "download file" trigger: navigate to the URL —
    // the server sets Content-Disposition: attachment and the browser
    // handles the rest. Guards against "select none, click export" by
    // bailing with a toast.
    const ids = document.querySelectorAll('input[name="cf_export_ids"]:checked');
    if (ids.length === 0) {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'warn', title: 'Export', body: 'Select at least one Custom Format to export.' });
        }
        return;
    }
    window.location.href = cfExportBuildUrl();
}

async function cfExportClipboard(btn) {
    const ids = document.querySelectorAll('input[name="cf_export_ids"]:checked');
    if (ids.length === 0) {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'warn', title: 'Export', body: 'Select at least one Custom Format to copy.' });
        }
        return;
    }
    const originalText = btn.textContent;
    btn.disabled = true;
    btn.textContent = 'Copying…';
    try {
        const resp = await fetch(cfExportBuildUrl(), { credentials: 'same-origin' });
        if (!resp.ok) throw new Error('HTTP ' + resp.status);
        const text = await resp.text();
        await navigator.clipboard.writeText(text);
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'success', title: 'Copied', body: ids.length + ' Custom Format(s) copied to clipboard.' });
        }
    } catch (e) {
        if (window.ryokanToast) {
            window.ryokanToast({ kind: 'error', title: 'Copy failed', body: String(e && e.message ? e.message : e) });
        }
    } finally {
        btn.disabled = false;
        btn.textContent = originalText;
    }
}

// CF test box (#18). Posts the pasted release title to
// /api/custom-formats/test and renders matched/not-matched CFs with
// the summed score. Title-based specs only — Size and SeaDex specs
// always miss here, and the section copy on the page says so.
async function runCfTest() {
    const input = document.getElementById('cf-test-input');
    const out = document.getElementById('cf-test-results');
    if (!input || !out) return;
    const title = (input.value || '').trim();
    if (!title) {
        out.style.display = 'none';
        return;
    }
    // All user-controlled strings flowing into the rendered HTML below
    // (CF names, parsed fields derived from the title the user pasted,
    // error bodies from the server) must be HTML-escaped — CF names
    // persist across requests, so a malicious CF name would otherwise
    // self-execute for any admin who ran a test.
    const esc = window.ryokanEscapeHtml;
    out.style.display = 'block';
    out.innerHTML = '<p class="form-hint">Testing…</p>';
    try {
        const r = await fetch('/api/custom-formats/test', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({release_title: title}),
        });
        const data = await r.json();
        if (!r.ok || !data.ok) {
            out.innerHTML = '<p class="form-hint">Test failed: ' + esc(data.error || r.status) + '</p>';
            return;
        }
        const parsed = data.parsed || {};
        const rows = [];
        rows.push('<p class="form-hint" style="margin-bottom:8px">Parsed: source=<code>' + esc(parsed.source || 'Unknown') + '</code>, resolution=<code>' + esc(parsed.resolution || 'Unknown') + '</code>, group=<code>' + esc(parsed.group || '(none)') + '</code>' + (parsed.is_remux ? ', <code>remux</code>' : '') + (parsed.is_bdmv ? ', <code>BDMV</code>' : '') + '</p>');
        rows.push('<p><strong>Total score: ' + Number(data.total_score) + '</strong> · <span class="form-hint">' + Number(data.matched.length) + ' matched, ' + Number(data.not_matched.length) + ' not matched</span></p>');
        if (data.matched.length > 0) {
            rows.push('<div class="settings-subheading">Matched</div>');
            rows.push('<ul style="list-style:none;padding:0;margin:0 0 12px 0">');
            data.matched.forEach(cf => {
                const score = Number(cf.score);
                const sign = score > 0 ? '+' : '';
                const cls = score > 0 ? 'cf-score-positive' : (score < 0 ? 'cf-score-negative' : 'cf-score-zero');
                rows.push('<li style="padding:4px 0;display:flex;gap:10px;align-items:baseline"><span class="cf-score ' + cls + '" style="min-width:48px;text-align:right">' + sign + score + '</span><span>' + esc(cf.name) + '</span></li>');
            });
            rows.push('</ul>');
        }
        if (data.not_matched.length > 0) {
            rows.push('<details><summary class="form-hint">' + Number(data.not_matched.length) + ' CFs did not match</summary>');
            rows.push('<ul style="list-style:none;padding:0;margin:4px 0 0 0">');
            data.not_matched.forEach(cf => {
                rows.push('<li style="padding:2px 0;color:var(--text-dim);font-size:13px">' + esc(cf.name) + ' <span class="form-hint">(score ' + Number(cf.score) + ')</span></li>');
            });
            rows.push('</ul></details>');
        }
        out.innerHTML = rows.join('');
    } catch (e) {
        out.innerHTML = '<p class="form-hint">Test failed: ' + esc(e && e.message ? e.message : e) + '</p>';
    }
}

function clearCfTest() {
    const input = document.getElementById('cf-test-input');
    const out = document.getElementById('cf-test-results');
    if (input) input.value = '';
    if (out) { out.innerHTML = ''; out.style.display = 'none'; }
}

function generateApiKey() {
    const chars = 'abcdefghijklmnopqrstuvwxyz0123456789';
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    let key = '';
    for (let i = 0; i < 32; i++) key += chars[buf[i] % chars.length];
    document.getElementById('sonarr_api_key').value = key;
}

function generateRadarrApiKey() {
    const chars = 'abcdefghijklmnopqrstuvwxyz0123456789';
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    let key = '';
    for (let i = 0; i < 32; i++) key += chars[buf[i] % chars.length];
    document.getElementById('radarr_api_key').value = key;
}

// #63 — show/hide the credential fieldset for the selected
// download client. Both fieldsets stay in the DOM so a user
// mid-edit doesn't lose form state when they toggle back.
function toggleClientFieldset(value) {
    const qbit = document.getElementById('qbit-fieldset');
    const deluge = document.getElementById('deluge-fieldset');
    const transmission = document.getElementById('transmission-fieldset');
    const rtorrent = document.getElementById('rtorrent-fieldset');
    if (qbit) qbit.style.display = value === 'qbittorrent' ? '' : 'none';
    if (deluge) deluge.style.display = value === 'deluge' ? '' : 'none';
    if (transmission) transmission.style.display = value === 'transmission' ? '' : 'none';
    if (rtorrent) rtorrent.style.display = value === 'rtorrent' ? '' : 'none';
}

// API-key inputs render as type="password" so the secret isn't visible
// to anyone glancing at the admin's screen. These two helpers restore
// the workflow that masking otherwise broke: Show toggles visibility
// for verification, Copy puts the value on the clipboard so the user
// can paste straight into Seerr without ever needing to read it.
function toggleApiKeyVisibility(inputId, btn) {
    const input = document.getElementById(inputId);
    if (!input) return;
    if (input.type === 'password') {
        input.type = 'text';
        btn.textContent = 'Hide';
    } else {
        input.type = 'password';
        btn.textContent = 'Show';
    }
}

async function copyApiKey(inputId, btn) {
    const input = document.getElementById(inputId);
    if (!input || !input.value) return;
    const original = btn.textContent;
    const flash = (label, ms) => {
        btn.textContent = label;
        setTimeout(() => { btn.textContent = original; }, ms);
    };
    // navigator.clipboard.writeText needs a secure context (HTTPS or
    // localhost). Self-hosted Ryokan often runs over plain HTTP on a
    // LAN address, so fall back to surfacing the value in a text input
    // and selecting it — the user can then Ctrl+C themselves.
    try {
        await navigator.clipboard.writeText(input.value);
        flash('Copied!', 1500);
    } catch (_e) {
        input.type = 'text';
        input.focus();
        input.select();
        flash('Select & copy', 2500);
    }
}

function syncTitleLanguagePreview(lang) {
    localStorage.setItem('titleLanguage', lang);
}

// Gather the per-collision radio selections and rename text fields
// into the two newline-delimited hidden inputs that the import-resolve
// handler expects. See plan §6.2 — each line is "<index>:<action>".
function buildCfImportResolvePayload(form) {
    const rows = form.querySelectorAll('tr[data-cf-collision-idx]');
    const decisions = [];
    const renames = [];
    for (const row of rows) {
        const idx = row.getAttribute('data-cf-collision-idx');
        const chosen = row.querySelector('input[name="cf_action_' + idx + '"]:checked');
        const action = chosen ? chosen.value : 'skip';
        decisions.push(idx + ':' + action);
        if (action === 'rename') {
            const input = row.querySelector('input[name="cf_rename_' + idx + '"]');
            const newName = input ? input.value.trim() : '';
            if (!newName) {
                window.ryokanAlert({
                    title: 'Rename required',
                    body: 'Collision ' + idx + ': pick a new name or choose a different action.',
                });
                return false;
            }
            renames.push(idx + ':' + newName);
        }
    }
    form.querySelector('#cf_import_resolve_decisions').value = decisions.join('\n');
    form.querySelector('#cf_import_resolve_renames').value = renames.join('\n');
    return true;
}

// HTMX migration (issue #129, Phase 1.5 grab-bag) — testDownloadClient,
// testJellyfin, refreshJellyfin all removed. The buttons now use
// `hx-post` + `hx-include="closest form"` + `hx-target="next .dc-test-result"`
// (or `#jellyfin-test-result` for the singletons). The server returns
// the rendered partial at `templates/partials/settings/connection_test_result.html`,
// always 200 so HTMX swaps in both success and failure (the `noSwap`
// default error policy skips the swap on 4xx/5xx). Loading state via
// `hx-disable="this"` (htmx adds `disabled` for the duration of
// the request and removes it on response).

// Auto-check connection health on integrations tab load.
// The download-client status dispatches by `type` (sonarr_impl_name:
// "QBittorrent" | "Deluge" | "Transmission") so the badge lights up
// next to the correct fieldset legend regardless of which client is
// active. Only the active client's badge is populated — the others
// stay blank (no stale "Disconnected" on a client the user isn't
// even trying to use).
// Settings → Connections health-badge poller. Mounted via
// `ryokanRegisterPageInit` so the getElementById lookups happen
// AFTER htmx commits the body swap. Pre-fix this was a bare IIFE
// that ran at script-load time; under boost the script could
// finish loading before the swap settled, no badges in DOM, the
// `if (!anyClientBadge && !jellyfinHealth) return` early-exit
// fired, and the user saw blank health badges until F5.
var bindConnectionHealthBadges = function () {
    const badges = {
        QBittorrent: document.getElementById('qbit-health'),
        Deluge: document.getElementById('deluge-health'),
        Transmission: document.getElementById('transmission-health'),
        RTorrent: document.getElementById('rtorrent-health'),
    };
    const jellyfinHealth = document.getElementById('jellyfin-health');
    const anyClientBadge = Object.values(badges).some(b => b);
    if (!anyClientBadge && !jellyfinHealth) return;
    // Idempotency guard: re-mounting on the same DOM (no body
    // swap in between) would fire a duplicate /api/health request
    // and re-populate the same badges. Cheap, but a stacked fetch
    // could race itself if the user toggles tabs fast.
    const guardEl = jellyfinHealth || Object.values(badges).find(b => b);
    if (!guardEl || guardEl.dataset.ryokanHealthBound === '1') return;
    guardEl.dataset.ryokanHealthBound = '1';

    fetch('/api/health')
        .then(r => r.json())
        .then(data => {
            let activeType = null;
            if (data.download_client) {
                const dc = data.download_client;
                activeType = dc.type;
                const target = badges[dc.type];
                if (target) {
                    if (dc.ok) {
                        target.innerHTML = '<span class="log-badge log-badge-info">' + window.ryokanEscapeHtml(dc.message) + '</span>';
                    } else if (dc.message === 'Not configured') {
                        target.innerHTML = '<span class="log-badge log-badge-warn">Not configured</span>';
                    } else {
                        target.innerHTML = '<span class="log-badge log-badge-error">Disconnected</span>';
                    }
                }
            }
            // Fill non-active client badges with a neutral "Not active"
            // so the badge slot reads consistently across all four
            // fieldsets when a user toggles the dropdown to view
            // credentials for a client they haven't activated.
            Object.keys(badges).forEach(function (key) {
                const el = badges[key];
                if (!el) return;
                if (key === activeType) return;
                if (el.innerHTML.trim() !== '') return;
                el.innerHTML = '<span class="log-badge">Not active</span>';
            });
            if (jellyfinHealth && data.jellyfin) {
                if (data.jellyfin.ok) {
                    jellyfinHealth.innerHTML = '<span class="log-badge log-badge-info">' + window.ryokanEscapeHtml(data.jellyfin.message) + '</span>';
                } else if (data.jellyfin.message !== 'Not configured') {
                    jellyfinHealth.innerHTML = '<span class="log-badge log-badge-error">Disconnected</span>';
                }
            }
        })
        .catch(() => {});
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('settings-connection-health', {
        check: function () {
            return !!(document.getElementById('qbit-health')
                || document.getElementById('deluge-health')
                || document.getElementById('transmission-health')
                || document.getElementById('rtorrent-health')
                || document.getElementById('jellyfin-health'));
        },
        mount: bindConnectionHealthBadges,
    });
} else {
    bindConnectionHealthBadges();
}

// Dirty-state guard on the Settings form. Flips a flag on any input
// change, prompts the user on nav-away (topbar click, browser back, tab
// close). Clears the flag on submit so the save itself doesn't trigger
// the prompt.
//
// Mounted via `ryokanRegisterPageInit` so the form lookup happens
// AFTER htmx commits the body swap. Pre-fix the bare IIFE could
// run at script-load before the form was committed under boost
// (dynamically-injected scripts ignore `defer`, see relations
// carousel commit), the early-return at the null check fired,
// and a user editing settings via boost-nav would see no
// unsaved-changes prompt on accidental nav-away.
//
// `dirty` lives at module scope so the beforeunload window
// listener (registered once via __ryokanSettingsDirtyGuardInit)
// reads the latest value across re-mounts.
var __ryokanSettingsDirty = false;
var bindSettingsDirtyGuard = function () {
    const form = document.querySelector('form.settings-form[action="/settings"]');
    if (!form) return;
    if (form.dataset.ryokanDirtyBound === '1') return;
    form.dataset.ryokanDirtyBound = '1';
    const markDirty = () => { __ryokanSettingsDirty = true; };
    form.addEventListener('input', markDirty);
    form.addEventListener('change', markDirty);
    form.addEventListener('submit', () => { __ryokanSettingsDirty = false; });

    // Window-scoped beforeunload attaches once per process. Reads
    // module-scope `__ryokanSettingsDirty` rather than a closure
    // var so it stays current across boost-nav re-mounts.
    if (!window.__ryokanSettingsDirtyGuardInit) {
        window.__ryokanSettingsDirtyGuardInit = true;
        window.addEventListener('beforeunload', (ev) => {
            if (!__ryokanSettingsDirty) return;
            ev.preventDefault();
            ev.returnValue = '';
        });
    }
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('settings-dirty-guard', {
        check: function () { return !!document.querySelector('form.settings-form[action="/settings"]'); },
        mount: bindSettingsDirtyGuard,
        unmount: function () {
            // Clear the dirty flag on nav-away — a saved-and-now-stale
            // form shouldn't carry its dirty state into a future visit.
            __ryokanSettingsDirty = false;
        },
    });
} else {
    bindSettingsDirtyGuard();
}

// ── File naming live preview (issue #124) ────────────────────────
//
// Every keystroke in one of the three template inputs (debounced)
// posts all three to /api/settings/naming-preview and paints the
// server's verdict under each field plus the combined sample path.
// The server renders the same way it will at import time, so there
// is no JS resolver to drift. Reset puts the default template back
// and fires `input` so the dirty guard and the preview both notice.
var bindNamingPreview = function () {
    const box = document.getElementById('naming-settings');
    if (!box || box.dataset.ryokanNamingBound === '1') return;
    box.dataset.ryokanNamingBound = '1';
    const inputs = Array.from(box.querySelectorAll('input[data-naming-default]'));
    let timer = null;
    // Responses can arrive out of order under the debounce; only the
    // newest request is allowed to paint.
    let seq = 0;
    const payload = () => {
        const p = {};
        inputs.forEach((i) => { p[i.name] = i.value; });
        return p;
    };
    const paint = (data) => {
        inputs.forEach((input) => {
            const out = box.querySelector('[data-naming-preview="' + input.name + '"]');
            const f = data.fields && data.fields[input.name];
            if (!out || !f) return;
            out.textContent = f.ok ? f.preview : (f.error || '');
            out.classList.toggle('naming-preview-error', !f.ok);
        });
        const path = box.querySelector('[data-naming-path]');
        if (path) path.textContent = data.path || '';
        const warn = box.querySelector('[data-naming-warning]');
        if (warn) {
            warn.textContent = data.warning || '';
            warn.hidden = !data.warning;
        }
    };
    const refresh = async () => {
        const mine = ++seq;
        try {
            const r = await fetch('/api/settings/naming-preview', {
                method: 'POST',
                headers: {'Content-Type': 'application/json'},
                credentials: 'same-origin',
                body: JSON.stringify(payload()),
            });
            if (!r.ok) return;
            const data = await r.json();
            if (mine !== seq) return;
            paint(data);
        } catch (e) {
            // Leave the last server-rendered preview in place.
        }
    };
    const schedule = () => {
        clearTimeout(timer);
        timer = setTimeout(refresh, 250);
    };
    inputs.forEach((i) => i.addEventListener('input', schedule));
    box.querySelectorAll('[data-naming-reset]').forEach((btn) => {
        btn.addEventListener('click', () => {
            const input = box.querySelector('input[name="' + btn.getAttribute('data-naming-reset') + '"]');
            if (!input) return;
            input.value = input.dataset.namingDefault || '';
            input.dispatchEvent(new Event('input', { bubbles: true }));
        });
    });
};

if (typeof window.ryokanRegisterPageInit === 'function') {
    window.ryokanRegisterPageInit('settings-naming-preview', {
        check: function () { return !!document.getElementById('naming-settings'); },
        mount: bindNamingPreview,
        unmount: function () {},
    });
} else {
    bindNamingPreview();
}

// The General form saves through an outerHTML swap of
// #settings-general-region, which replaces the fieldset and every
// listener bound to it, and the page lifecycle deliberately no-ops a
// re-render of the same page. Rebind from the swap event instead; the
// fresh fieldset has no bound flag, so this is a first bind, not a
// double one. Registered once per process, like the other body-level
// afterSwap handlers in this file.
if (!window.__ryokanNamingPreviewSwapListener) {
    window.__ryokanNamingPreviewSwapListener = true;
    document.body.addEventListener('htmx:after:swap', function () {
        bindNamingPreview();
    });
}

// ── External Accounts (AL / MAL, issue #62) ──────────────────────
//
// Three interactions on the Settings → Integrations → External
// Accounts card:
//
//   1. `startExternalAccountLink(provider)` — opens the provider's
//      OAuth authorize page in a new tab via Ryokan's /start endpoint
//      (which redirects), then shows a paste-modal for the user to
//      return to once they have a token/code from the broker page.
//   2. `saveExternalAccountPrefs()` — fires on any checkbox change
//      in the linked-state panel; POSTs the whole preference set so
//      the sync task's next tick picks it up without a full form save.
//   3. `unlinkExternalAccount()` — confirmation + POST /settings/
//      oauth/unlink + reload.
//
// The paste-modal is built inline to avoid yet another templates/
// partials/ file for what's essentially a single-field prompt.

// Origin of the gh-pages-hosted broker page that AL/MAL redirect to
// after user approval. The postMessage receiver below validates
// `event.origin` against this value before reading any data.
var EXT_BROKER_ORIGIN = 'https://johnthreekay.github.io';

// Single in-flight link attempt at module scope. Holds the
// {handler, timer, provider} for the active OAuth flow so a second
// click on Link AL / Link MAL aborts the prior listener and the
// prior 10-minute cleanup timer. Without this, a user clicking
// Link AL then Link MAL before the AL flow completes would leave
// both listeners alive — and since both modals share fixed input
// IDs, the AL postMessage would auto-fill the MAL modal and
// trigger an AL submit while the user was looking at MAL.
var _extLinkAttempt = null;

function clearExtLinkAttempt() {
    if (!_extLinkAttempt) return;
    window.removeEventListener('message', _extLinkAttempt.handler);
    if (_extLinkAttempt.timer) clearTimeout(_extLinkAttempt.timer);
    _extLinkAttempt = null;
}

function startExternalAccountLink(provider) {
    // Abort any prior in-flight attempt — the user clicked Link
    // again, so the previous flow's broker postback should not be
    // accepted into the now-different modal.
    clearExtLinkAttempt();

    // Set up a one-shot postMessage listener BEFORE opening the
    // popup so a fast-completing flow (already-authenticated user,
    // already-approved app) can't deliver before we're listening.
    // Receiver validates origin + message shape; the broker page
    // parses token/state from the URL fragment/query and posts back
    // here as soon as it loads, skipping the copy-paste step.
    const expectedType = `ryokan-oauth-${provider}`;
    const handler = (event) => {
        if (event.origin !== EXT_BROKER_ORIGIN) return;
        const data = event.data || {};
        if (data.type !== expectedType) return;
        // Belt-and-suspenders: the attempt may already have been
        // cleared (timeout fired, second click came in) by the time
        // a duplicate emit lands. Only act on the still-active one.
        if (!_extLinkAttempt || _extLinkAttempt.handler !== handler) return;
        clearExtLinkAttempt();
        autoSubmitExternalAccount(provider, data);
    };
    // Auto-clean after the OAuth-state TTL (10 min) so a forgotten
    // flow doesn't leave a stale listener / timer attached for the
    // rest of the session.
    const timer = setTimeout(clearExtLinkAttempt, 10 * 60 * 1000);
    _extLinkAttempt = { handler, timer, provider };
    window.addEventListener('message', handler);

    // Open the OAuth authorize flow in a new tab so the Settings
    // page stays loaded behind it. NOT passing 'noopener' is
    // deliberate — the broker page needs `window.opener` to be set
    // so it can post values back to this tab via postMessage. The
    // popup navigates only to URLs we control (`/start` → AL/MAL
    // authorize → our gh-pages broker), so the standard tabnabbing
    // protections noopener provides aren't load-bearing here.
    window.open(`/settings/oauth/${provider}/start`, '_blank');
    openExternalAccountPasteModal(provider);
}

// Auto-fill the paste modal from a postMessage payload, then
// submit. Falls through to the manual paste UI if something looks
// off — value or state empty, network error on submit, etc.
function autoSubmitExternalAccount(provider, data) {
    const value = provider === 'anilist' ? data.access_token : data.code;
    const stateValue = data.state || '';
    if (!value || !stateValue) {
        console.warn('[ext-accounts] postMessage missing fields, falling back to manual paste');
        return;
    }
    const valueInput = document.getElementById('ext-accounts-paste-value');
    const stateInput = document.getElementById('ext-accounts-paste-state');
    if (valueInput) valueInput.value = value;
    if (stateInput) stateInput.value = stateValue;
    // Tiny delay so the user catches the auto-fill visually before
    // the modal closes on success — better feedback than an instant
    // disappear.
    setTimeout(() => submitExternalAccountPaste(provider), 200);
}

function openExternalAccountPasteModal(provider) {
    const isAnilist = provider === 'anilist';
    const providerLabel = isAnilist ? 'AniList' : 'MyAnimeList';
    const fieldLabel = isAnilist ? 'Access token' : 'Authorization code';
    const hint = isAnilist
        ? 'Approve in the AniList tab. The token + state will fill in here automatically once the broker page loads — no copy-paste needed in the common case. If your popup blocker prevented the tab from opening, copy the values from the broker page manually and paste them below.'
        : 'Approve in the MyAnimeList tab. The code + state will fill in here automatically once the broker page loads — no copy-paste needed in the common case. If your popup blocker prevented the tab from opening, copy the values from the broker page manually and paste them below.';

    let modal = document.getElementById('ext-accounts-paste-modal');
    if (modal) modal.remove();
    modal = document.createElement('div');
    modal.id = 'ext-accounts-paste-modal';
    modal.className = 'modal-backdrop';
    modal.style.display = 'flex';
    modal.innerHTML = `
        <div class="modal" role="dialog" aria-modal="true" style="max-width:480px">
            <div class="modal-header">
                <div style="font-weight:600;font-size:15px">Link ${providerLabel}</div>
                <button type="button" class="btn-icon" aria-label="Close" onclick="closeExternalAccountPasteModal()">
                    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6L6 18M6 6l12 12"/></svg>
                </button>
            </div>
            <div class="modal-body" style="padding:18px">
                <p class="form-hint" style="margin-top:0">${hint}</p>
                <div class="form-group">
                    <label for="ext-accounts-paste-value">${fieldLabel}</label>
                    <textarea id="ext-accounts-paste-value" rows="3" style="width:100%;font-family:monospace;font-size:12px"></textarea>
                </div>
                <div class="form-group">
                    <label for="ext-accounts-paste-state">State</label>
                    <input id="ext-accounts-paste-state" type="text" style="width:100%;font-family:monospace;font-size:12px">
                    <span class="form-hint">CSRF nonce; required. Both fields appear on the callback page.</span>
                </div>
                <div id="ext-accounts-paste-error" class="form-hint" style="color:var(--red);display:none"></div>
                <div style="display:flex;gap:8px;justify-content:flex-end;margin-top:12px">
                    <button type="button" class="btn btn-secondary" onclick="closeExternalAccountPasteModal()">Cancel</button>
                    <button type="button" class="btn btn-primary" id="ext-accounts-paste-submit"
                        onclick="submitExternalAccountPaste('${provider}')">Link</button>
                </div>
            </div>
        </div>`;
    document.body.appendChild(modal);
    setTimeout(() => {
        const input = document.getElementById('ext-accounts-paste-value');
        if (input) input.focus();
    }, 0);
}

function closeExternalAccountPasteModal() {
    const modal = document.getElementById('ext-accounts-paste-modal');
    if (modal) modal.remove();
}

function submitExternalAccountPaste(provider) {
    const input = document.getElementById('ext-accounts-paste-value');
    const stateInput = document.getElementById('ext-accounts-paste-state');
    const err = document.getElementById('ext-accounts-paste-error');
    const btn = document.getElementById('ext-accounts-paste-submit');
    const value = (input && input.value || '').trim();
    const stateValue = (stateInput && stateInput.value || '').trim();
    if (!value || !stateValue) {
        if (err) {
            err.textContent = 'Paste both the value and the state from the callback page.';
            err.style.display = '';
        }
        return;
    }
    if (err) err.style.display = 'none';
    if (btn) { btn.disabled = true; btn.textContent = 'Linking…'; }

    const body = provider === 'anilist'
        ? { access_token: value, state: stateValue }
        : { code: value, state: stateValue };
    fetch(`/settings/oauth/${provider}/submit`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    })
    .then(async (r) => {
        // The handler returns plain-text error bodies via axum's
        // `(StatusCode, String)` shape on every 400/409/502 path.
        // Reading `r.json()` first would silently drop those — the
        // user would see the unhelpful "Link failed (400)" fallback
        // even when the server told them exactly which token / state
        // / rate-limit issue caused it (e.g. AL 429'ing the Viewer
        // probe under a per-account quota surfaces here as
        // "AniList rejected the token: AniList rate-limited ..."
        // and the user needs that detail to know it's the AL
        // backend, not their paste). Read text first, parse JSON
        // only on success responses where the handler returns a
        // `LinkResponse` envelope.
        const text = await r.text();
        if (!r.ok) {
            const trimmed = text && text.trim();
            throw new Error(trimmed && trimmed.length > 0 ? trimmed : `Link failed (${r.status})`);
        }
        let data = {};
        try { data = JSON.parse(text); } catch (_) {}
        return data;
    })
    .then(() => {
        closeExternalAccountPasteModal();
        window.location.reload();
    })
    .catch((e) => {
        if (err) {
            err.textContent = e && e.message ? e.message : 'Link failed';
            err.style.display = '';
        }
        if (btn) { btn.disabled = false; btn.textContent = 'Link'; }
    });
}

function unlinkExternalAccount() {
    if (!window.ryokanConfirm) {
        // Fallback to native confirm if the shared helper isn't loaded.
        if (!confirm('Unlink this external account? Imported series stay in your library; user scores and custom-list memberships are cleared.')) {
            return;
        }
        return unlinkExternalAccountConfirmed();
    }
    window.ryokanConfirm({
        title: 'Unlink external account',
        body: 'Imported series stay in your library. User scores and custom-list memberships are cleared. Re-link to restore them.',
        yesLabel: 'Unlink',
        noLabel: 'Cancel',
    }).then((res) => {
        if (res && res.ok) unlinkExternalAccountConfirmed();
    });
}

function unlinkExternalAccountConfirmed() {
    fetch('/settings/oauth/unlink', { method: 'POST' })
        .then((r) => r.json().catch(() => ({})))
        .then(() => window.location.reload())
        .catch((e) => console.error('[ext-accounts] unlink failed:', e));
}

function syncWatchListNow() {
    if (typeof window.ryokanNewProgressId !== 'function' || typeof window.ryokanProgressToast !== 'function') {
        // Sticky-toast helpers come from base.js; if they're missing
        // it's a load-order bug, not a user-facing failure mode.
        console.error('[ext-accounts] progress toast helpers not loaded');
        return;
    }
    const progressId = window.ryokanNewProgressId();
    const toast = window.ryokanProgressToast({
        progressId,
        title: 'Watch-list sync starting…',
        category: 'external_sync',
        onTerminal: (ev) => {
            // On a successful manual sync, snap the last-sync hint to
            // "Just now" right away so the user gets immediate
            // feedback. The 30s `[data-relative-time]` updater takes
            // care of subsequent ticks. A failed/warned sync leaves
            // the previous timestamp untouched.
            if (ev && ev.kind === 'success') {
                const nowSec = Math.floor(Date.now() / 1000);
                document.querySelectorAll('[data-relative-time]').forEach((el) => {
                    el.setAttribute('data-relative-time', String(nowSec));
                    el.textContent = 'Just now';
                });
                // A successful sync also clears `last_sync_auth_failed`
                // server-side. The banner + legend badge are still
                // server-rendered to whatever the page-load state was,
                // so flip them in-place — without this, the user has
                // to reload Settings to see "Re-link required" go
                // away after a successful sync.
                document
                    .querySelectorAll('[data-ext-auth-banner]')
                    .forEach((el) => el.setAttribute('hidden', ''));
                document
                    .querySelectorAll('[data-ext-auth-badge="error"]')
                    .forEach((el) => el.setAttribute('hidden', ''));
                document
                    .querySelectorAll('[data-ext-auth-badge="ok"]')
                    .forEach((el) => el.removeAttribute('hidden'));
            }
        },
    });
    fetch('/settings/oauth/sync-now', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ progress_id: progressId }),
    })
        .then((r) =>
            // Defense in depth: finalize the toast if the response is
            // a non-2xx OR if the body fails to parse. The handler
            // always emits JSON now, but a future regression that
            // serves a plain-text body must not leave the toast
            // spinning indefinitely (worst possible failure mode —
            // looks like work is happening when nothing is).
            r
                .json()
                .catch(() => ({ ok: false, error: 'Server returned an unparseable response.' }))
                .then((data) => ({ ok: r.ok, data }))
        )
        .then(({ ok, data }) => {
            // The sync runs in the background; the toast finalizes off
            // the progress feed when the request succeeded. A bad-
            // state response (account unlinked between page load and
            // click, or a transport-level error) finalizes here.
            if (!ok || (data && data.ok === false)) {
                toast.finalize({
                    kind: 'error',
                    title: 'Sync could not start',
                    body: (data && data.error) || 'Try reloading the Settings page.',
                });
            }
        })
        .catch((err) => {
            toast.finalize({ kind: 'error', title: 'Sync request failed', body: String(err) });
        });
}

var _extPrefsSaveTimer = null;
function saveExternalAccountPrefs() {
    // Debounce so the user toggling three checkboxes in a row doesn't
    // fire three POSTs back-to-back.
    if (_extPrefsSaveTimer) clearTimeout(_extPrefsSaveTimer);
    _extPrefsSaveTimer = setTimeout(() => {
        const read = (key) => {
            const el = document.querySelector(`[data-ext-pref="${key}"]`);
            return el ? el.checked : false;
        };
        fetch('/settings/oauth/preferences', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                import_watching: read('import_watching'),
                import_planning: read('import_planning'),
                import_paused: read('import_paused'),
                import_dropped: read('import_dropped'),
                import_completed: read('import_completed'),
                skip_already_watched: read('skip_already_watched'),
            }),
        })
        .then((r) => { if (!r.ok) console.error('[ext-accounts] prefs save failed:', r.status); });
    }, 250);
}

// #62 — live updater for `[data-relative-time]` elements.
// Mirrors the Rust-side `humanize_relative_time` policy so the
// label that JS produces matches what a fresh page load from the
// server would produce. Re-runs every 30 seconds; the user
// browsing Settings sees "last sync 4 minutes ago" tick over to
// "5 minutes ago" without reloading. Idempotent: if the page has
// no marker elements it's a no-op + the timer is skipped.
//
// Singleton guard: hx-boost re-runs this script on every nav-back to
// Settings. Without the guard, each visit starts another `setInterval`
// + attaches another `DOMContentLoaded` listener (the latter is a
// harmless no-op on boost-navs — DOMContentLoaded already fired — but
// the timer accumulation is a real CPU leak). Clear any prior timer
// so the latest visit wins; the IIFE itself uses an inner early-return
// guard so its DOMContentLoaded attach + initial tick only fire once.
if (window.__ryokanSettingsRelativeTimeTimer) {
    clearInterval(window.__ryokanSettingsRelativeTimeTimer);
    window.__ryokanSettingsRelativeTimeTimer = null;
}
(function () {
    // First-run-only flag for the DOMContentLoaded listener attach +
    // initial tick. The setInterval is reset every visit (the outer
    // clearInterval ensures single-timer semantics).
    const firstRun = !window.__ryokanSettingsRelativeTimeInit;
    window.__ryokanSettingsRelativeTimeInit = true;

    function humanize(unixTs, nowSec) {
        const delta = Math.max(0, nowSec - unixTs);
        if (delta < 60) return 'Just now';
        if (delta < 3600) {
            const m = Math.floor(delta / 60);
            return m + ' minute' + (m === 1 ? '' : 's') + ' ago';
        }
        if (delta < 86400) {
            const h = Math.floor(delta / 3600);
            return h + ' hour' + (h === 1 ? '' : 's') + ' ago';
        }
        const d = Math.floor(delta / 86400);
        return d + ' day' + (d === 1 ? '' : 's') + ' ago';
    }

    function tick() {
        const now = Math.floor(Date.now() / 1000);
        document.querySelectorAll('[data-relative-time]').forEach(function (el) {
            const ts = parseInt(el.getAttribute('data-relative-time'), 10);
            if (!Number.isFinite(ts) || ts <= 0) return;
            el.textContent = humanize(ts, now);
        });
    }

    // First tick immediately so any clock drift since the
    // server-render gets corrected on page load. Then every 30s.
    if (firstRun) {
        if (document.readyState === 'loading') {
            document.addEventListener('DOMContentLoaded', tick);
        } else {
            tick();
        }
    } else {
        // Boost-nav re-entry: DOMContentLoaded already fired ages ago
        // and tick() may not have run since. Run it once so the
        // freshly-rendered timestamp markers update immediately.
        tick();
    }
    window.__ryokanSettingsRelativeTimeTimer = setInterval(tick, 30 * 1000);
})();

// Per-indexer Download Client dropdown: filter options by protocol so
// torznab indexers can't pin to SAB and newznab can't pin to BT
// clients. Server-side validation in the upsert handler is the
// authority — this is just UX so the user doesn't pick a doomed
// option and bounce off an error toast on save. Each option carries
// `data-protocol` ("torrent" or "usenet") set by the Askama template.
(function () {
    function applyProtocolFilter(form) {
        const kindSel = form.querySelector('select[name="kind"]');
        const dcSel = form.querySelector('select[name="download_client_id"]');
        if (!kindSel || !dcSel) return;
        const wantedProto = kindSel.value === 'newznab' ? 'usenet' : 'torrent';
        let activeStillValid = false;
        for (const opt of dcSel.options) {
            if (!opt.value) {
                // The "(use default)" sentinel — always allowed.
                continue;
            }
            const proto = opt.dataset.protocol;
            // Missing data-protocol means an unknown client kind that
            // `protocol_for_client_kind` rejected. Leave such options
            // visible — server-side validation will catch them.
            const ok = !proto || proto === wantedProto;
            opt.hidden = !ok;
            opt.disabled = !ok;
            if (ok && opt.selected) activeStillValid = true;
        }
        if (!activeStillValid && dcSel.value) {
            // Currently-selected option got hidden (user flipped kind
            // after picking a now-mismatched client). Fall back to
            // "(use default)" so the form's about-to-be-submitted
            // state matches what the user can see.
            dcSel.value = '';
        }
    }

    function wireForm(form) {
        const kindSel = form.querySelector('select[name="kind"]');
        if (!kindSel) return;
        applyProtocolFilter(form);
        kindSel.addEventListener('change', () => applyProtocolFilter(form));
    }

    function init() {
        // Both the Add Indexer form and the Edit Indexer form sit
        // under the indexers tab and share the kind+download_client
        // shape. `[action$="/settings/indexers/upsert"]` matches both.
        document
            .querySelectorAll('form[action$="/settings/indexers/upsert"]')
            .forEach(wireForm);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }
})();

// ─── Issue #114 — scoped API keys ────────────────────────────────
//
// CRUD against /api/api-keys/* with a one-time-plaintext modal flow.
// Wrapped in an IIFE so the var-at-module-scope rule (CLAUDE.md
// per-page JS quirks) applies; the function is reentered every
// time hx-boost re-runs the script after a body swap.
//
// State machine:
//   1. List view (server-rendered + JS re-rendered after CRUD).
//   2. Create button → modal opens with name + scope checkbox form.
//   3. Submit → POST /api/api-keys → response carries plaintext +
//      view. Modal swaps to "save your key" view with a copy button.
//   4. "I've saved it" → close modal, refresh list via GET /api/api-keys.
//
// Per-row toggle / delete go through their own JSON endpoints; the
// row markup is mutated in place rather than re-fetching the entire
// list, which would cost a round-trip per click and feel laggy.
(function () {
    function escHtml(s) {
        return String(s == null ? '' : s)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    function renderCard(view) {
        var scopeChips = view.scopes
            .map(function (s) { return '<span class="api-key-scope-chip">' + escHtml(s) + '</span>'; })
            .join('');
        var statusPillClass = view.enabled
            ? 'api-key-status-pill-on'
            : 'api-key-status-pill-off';
        var statusLabel = view.enabled ? 'Enabled' : 'Disabled';
        var cardDisabledClass = view.enabled ? '' : ' api-key-card-disabled';
        return ''
            + '<article class="api-key-card' + cardDisabledClass + '" data-api-key-id="' + view.id + '">'
            +   '<div class="api-key-card-header">'
            +     '<div class="api-key-card-title">'
            +       '<strong class="api-key-card-name">' + escHtml(view.name) + '</strong>'
            +     '</div>'
            +     '<span class="api-key-status-pill ' + statusPillClass + '">' + statusLabel + '</span>'
            +   '</div>'
            +   '<div class="api-key-card-scopes">' + scopeChips + '</div>'
            +   '<dl class="api-key-card-meta">'
            +     '<div class="api-key-card-meta-row">'
            +       '<dt>Last used</dt>'
            +       '<dd class="api-key-last-used">' + escHtml(view.last_used_display) + '</dd>'
            +     '</div>'
            +     '<div class="api-key-card-meta-row">'
            +       '<dt>Created</dt>'
            +       '<dd class="api-key-created">' + escHtml(view.created_display) + '</dd>'
            +     '</div>'
            +   '</dl>'
            +   '<div class="api-key-reveal-row" data-api-key-id="' + view.id + '">'
            +     '<input type="text" class="api-key-reveal-input" data-api-key-reveal-input value="••••••••••••••••••••••••••••••••" readonly aria-label="API key (hidden, click Show to reveal)">'
            +     '<button type="button" class="btn btn-ghost btn-sm api-key-show-btn" data-api-key-id="' + view.id + '" data-api-key-name="' + escHtml(view.name) + '" title="Show key">Show</button>'
            +     '<button type="button" class="btn btn-ghost btn-sm api-key-copy-btn" data-api-key-id="' + view.id + '" data-api-key-name="' + escHtml(view.name) + '" title="Copy key">Copy</button>'
            +   '</div>'
            +   '<div class="api-key-card-footer">'
            +     '<label class="api-key-switch">'
            +       '<input type="checkbox" class="api-key-enabled-toggle" data-api-key-id="' + view.id + '"' + (view.enabled ? ' checked' : '') + '>'
            +       '<span class="api-key-switch-track"><span class="api-key-switch-thumb"></span></span>'
            +       '<span class="api-key-switch-label">' + statusLabel + '</span>'
            +     '</label>'
            +   '</div>'
            +   '<div class="api-key-card-actions">'
            +     '<button type="button" class="btn btn-icon-danger api-key-delete-btn" data-api-key-id="' + view.id + '" data-api-key-name="' + escHtml(view.name) + '" title="Delete ' + escHtml(view.name) + '" aria-label="Delete ' + escHtml(view.name) + '">×</button>'
            +   '</div>'
            + '</article>';
    }

    function renderAddTile() {
        return ''
            + '<button type="button" class="api-key-card api-key-card-add" id="api-keys-create-btn">'
            +   '<div class="api-key-card-add-icon">+</div>'
            +   '<div class="api-key-card-add-label">Create API key</div>'
            + '</button>';
    }

    function renderList(views) {
        var listEl = document.getElementById('api-keys-list');
        if (!listEl) return;
        var cards = (views || []).map(renderCard).join('');
        listEl.innerHTML = '<div class="api-key-card-grid">' + cards + renderAddTile() + '</div>';
        wireListEvents();
        wireCreateButton();
    }

    async function refreshList() {
        try {
            var res = await fetch('/api/api-keys', { credentials: 'same-origin' });
            if (!res.ok) return;
            var views = await res.json();
            renderList(views);
        } catch (_) {
            // Best-effort refresh; a failure leaves the previous list
            // visible rather than blanking the tab.
        }
    }

    // Apply the admin-shadows-others rule. When admin is checked,
    // every other scope tile is grayed out + uncheckable + their
    // checkboxes are cleared (admin is universal-grant, so holding
    // it makes the others meaningless). When admin is unchecked,
    // the others re-enable and the user can click them again.
    function syncScopeShadowing() {
        var adminEl = document.querySelector(
            '#api-keys-create-form input[name="scope"][value="admin"]'
        );
        var adminOn = adminEl ? adminEl.checked : false;
        document
            .querySelectorAll('#api-keys-create-form .api-keys-scope-tile')
            .forEach(function (tile) {
                var input = tile.querySelector('input[name="scope"]');
                if (!input || input.value === 'admin') return;
                tile.classList.toggle('api-keys-scope-tile-shadowed', adminOn);
                if (adminOn && input.checked) {
                    input.checked = false;
                }
            });
    }

    // Helper for setting the error text without clobbering the icon
    // child element that lives alongside it inside the alert div.
    function setCreateError(msg) {
        var errEl = document.getElementById('api-keys-create-error');
        if (!errEl) return;
        if (!msg) {
            errEl.hidden = true;
            return;
        }
        var textEl = errEl.querySelector('.api-keys-form-error-text');
        if (textEl) {
            textEl.textContent = msg;
        }
        errEl.hidden = false;
    }

    function openCreateModal() {
        var modal = document.getElementById('api-keys-create-modal');
        if (!modal) return;
        // Reset the form to step 1 every open so a previous create's
        // plaintext doesn't persist.
        var formStep = document.getElementById('api-keys-create-form-step');
        var resultStep = document.getElementById('api-keys-create-result-step');
        var nameEl = document.getElementById('api-keys-create-name');
        if (formStep) formStep.hidden = false;
        if (resultStep) resultStep.hidden = true;
        setCreateError('');
        if (nameEl) nameEl.value = '';
        // Reset every scope checkbox so the user explicitly picks
        // what their new key needs (no defaulted-to-calendar
        // surprise that gets shipped as a feature inadvertently).
        document
            .querySelectorAll('#api-keys-create-form input[name="scope"]')
            .forEach(function (el) { el.checked = false; });
        // Clear any leftover admin-shadowed state from a previous
        // open where admin was selected. With every checkbox
        // unchecked above, admin is unchecked too, so this just
        // strips the shadowed class off the other tiles.
        syncScopeShadowing();
        // Modal uses `style.display` (matches the notif/dc modal-backdrop
        // pattern) rather than the `hidden` attribute, so a CSS rule on
        // .modal-backdrop with `display: flex` for the open state isn't
        // necessary — the inline style does the work.
        modal.style.display = 'flex';
        if (nameEl) nameEl.focus();
    }

    function closeCreateModal() {
        var modal = document.getElementById('api-keys-create-modal');
        if (modal) modal.style.display = 'none';
    }

    async function submitCreateForm(ev) {
        ev.preventDefault();
        var nameEl = document.getElementById('api-keys-create-name');
        var submitBtn = document.getElementById('api-keys-create-submit');
        if (!nameEl) return;

        var scopes = Array
            .from(document.querySelectorAll('#api-keys-create-form input[name="scope"]:checked'))
            .map(function (el) { return el.value; })
            .join(',');
        var fd = new URLSearchParams();
        fd.set('name', nameEl.value);
        fd.set('scopes', scopes);

        if (submitBtn) {
            submitBtn.disabled = true;
            submitBtn.textContent = 'Creating...';
        }
        setCreateError('');
        try {
            var res = await fetch('/api/api-keys', {
                method: 'POST',
                credentials: 'same-origin',
                headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
                body: fd.toString(),
            });
            if (!res.ok) {
                var msg = await res.text();
                setCreateError(msg || ('Create failed (HTTP ' + res.status + ')'));
                return;
            }
            var data = await res.json();
            // Step 2: show plaintext.
            var formStep = document.getElementById('api-keys-create-form-step');
            var resultStep = document.getElementById('api-keys-create-result-step');
            var plaintextEl = document.getElementById('api-keys-plaintext');
            if (formStep) formStep.hidden = true;
            if (resultStep) resultStep.hidden = false;
            if (plaintextEl) {
                plaintextEl.value = data.plaintext;
                plaintextEl.select();
            }
        } catch (e) {
            setCreateError('Network error: ' + e.message);
        } finally {
            if (submitBtn) {
                submitBtn.disabled = false;
                submitBtn.textContent = 'Create key';
            }
        }
    }

    async function copyPlaintext() {
        var plaintextEl = document.getElementById('api-keys-plaintext');
        var confirmEl = document.getElementById('api-keys-copy-confirm');
        if (!plaintextEl) return;
        try {
            await navigator.clipboard.writeText(plaintextEl.value);
            if (confirmEl) {
                confirmEl.hidden = false;
                setTimeout(function () { confirmEl.hidden = true; }, 2000);
            }
        } catch (_) {
            // Fallback: select the input so the user can ctrl-C.
            plaintextEl.select();
            plaintextEl.setSelectionRange(0, 99999);
        }
    }

    async function toggleKeyEnabled(id, enabled) {
        try {
            var fd = new URLSearchParams();
            fd.set('enabled', enabled ? 'true' : 'false');
            var res = await fetch('/api/api-keys/' + id + '/toggle', {
                method: 'POST',
                credentials: 'same-origin',
                headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
                body: fd.toString(),
            });
            if (!res.ok) return false;
            var view = await res.json();
            // Update the card's visual state in three places: the
            // header status pill (top-right), the switch label
            // (footer), and the .api-key-card-disabled class on the
            // article (whole-card opacity hint). The switch checkbox
            // itself already shows the right state — the user clicked
            // it, the browser flipped it.
            var card = document.querySelector('.api-key-card[data-api-key-id="' + id + '"]');
            if (card) {
                card.classList.toggle('api-key-card-disabled', !view.enabled);
                var label = view.enabled ? 'Enabled' : 'Disabled';
                var switchLabel = card.querySelector('.api-key-switch-label');
                if (switchLabel) switchLabel.textContent = label;
                var pill = card.querySelector('.api-key-status-pill');
                if (pill) {
                    pill.textContent = label;
                    pill.classList.toggle('api-key-status-pill-on', view.enabled);
                    pill.classList.toggle('api-key-status-pill-off', !view.enabled);
                }
            }
            return true;
        } catch (_) {
            return false;
        }
    }

    async function deleteKey(id, name) {
        // Use the in-app confirm modal (defined in base.js) instead of
        // the browser's native `window.confirm` so the prompt looks
        // consistent with the rest of Ryokan's destructive-action
        // confirmations (Delete File, Cancel Pending, etc.). The
        // `danger: true` flag turns the Yes button red.
        if (!window.ryokanConfirm) {
            // Fallback for the unlikely case base.js hasn't loaded.
            if (!window.confirm('Delete API key "' + name + '"?')) return;
        } else {
            var res = await window.ryokanConfirm({
                title: 'Delete API key',
                body: 'Delete the "' + name + '" API key? Any integration using it will start receiving 401 errors immediately. This cannot be undone.',
                yesLabel: 'Delete',
                noLabel: 'Cancel',
                danger: true,
            });
            if (!res || !res.ok) return;
        }
        try {
            var deleteRes = await fetch('/api/api-keys/' + id + '/delete', {
                method: 'POST',
                credentials: 'same-origin',
            });
            if (!deleteRes.ok) {
                if (window.ryokanToast) window.ryokanToast('Delete failed', 'error');
                return;
            }
            // Remove the card immediately. Selector targets the card
            // article (was tr in the legacy table layout). After
            // removal the "+ Create" tile still sits in the grid so
            // there's no need to render a separate empty state.
            var card = document.querySelector('.api-key-card[data-api-key-id="' + id + '"]');
            if (card) card.remove();
        } catch (_) {
            if (window.ryokanToast) window.ryokanToast('Network error', 'error');
        }
    }

    async function fetchPlaintext(id) {
        try {
            var res = await fetch('/api/api-keys/' + id + '/reveal', {
                credentials: 'same-origin',
            });
            if (!res.ok) {
                var msg = await res.text();
                if (window.ryokanToast) window.ryokanToast(msg || 'Reveal failed', 'error');
                return null;
            }
            var data = await res.json();
            return data.plaintext || null;
        } catch (_) {
            if (window.ryokanToast) window.ryokanToast('Network error', 'error');
            return null;
        }
    }

    async function showKey(id, btn) {
        var card = document.querySelector('.api-key-reveal-row[data-api-key-id="' + id + '"]');
        if (!card) return;
        var input = card.querySelector('[data-api-key-reveal-input]');
        if (!input) return;

        // Toggle: if already revealed, re-mask. Hide → fetch + show.
        if (input.dataset.revealed === '1') {
            input.value = '••••••••••••••••••••••••••••••••';
            input.classList.remove('api-key-reveal-input-revealed');
            input.dataset.revealed = '0';
            if (btn) btn.textContent = 'Show';
            return;
        }

        if (btn) {
            btn.disabled = true;
            btn.textContent = '…';
        }
        var plaintext = await fetchPlaintext(id);
        if (btn) btn.disabled = false;
        if (!plaintext) {
            if (btn) btn.textContent = 'Show';
            return;
        }
        input.value = plaintext;
        input.classList.add('api-key-reveal-input-revealed');
        input.dataset.revealed = '1';
        input.select();
        if (btn) btn.textContent = 'Hide';
    }

    async function copyKey(id, btn) {
        // Copy without requiring a Show first — fetch on demand and
        // write to clipboard. Saves a click in the common case where
        // the user just wants the value pasted into an integration.
        if (btn) {
            btn.disabled = true;
            btn.textContent = '…';
        }
        var plaintext = await fetchPlaintext(id);
        if (btn) btn.disabled = false;
        if (!plaintext) {
            if (btn) btn.textContent = 'Copy';
            return;
        }
        try {
            await navigator.clipboard.writeText(plaintext);
            if (btn) {
                btn.textContent = 'Copied';
                setTimeout(function () { btn.textContent = 'Copy'; }, 1500);
            }
        } catch (_) {
            // Clipboard API blocked (older browsers, insecure origin).
            // Fall back: surface plaintext in the input and select it
            // so the user can ctrl-C manually.
            var input = document
                .querySelector('.api-key-reveal-row[data-api-key-id="' + id + '"] [data-api-key-reveal-input]');
            if (input) {
                input.value = plaintext;
                input.classList.add('api-key-reveal-input-revealed');
                input.dataset.revealed = '1';
                input.select();
                input.setSelectionRange(0, 99999);
            }
            if (btn) btn.textContent = 'Copy';
        }
    }

    function wireListEvents() {
        document.querySelectorAll('.api-key-enabled-toggle').forEach(function (el) {
            if (el.dataset.apiKeysWired === '1') return;
            el.dataset.apiKeysWired = '1';
            el.addEventListener('change', function () {
                var id = parseInt(el.dataset.apiKeyId, 10);
                if (!id) return;
                toggleKeyEnabled(id, el.checked).then(function (ok) {
                    if (!ok) el.checked = !el.checked; // revert
                });
            });
        });
        document.querySelectorAll('.api-key-delete-btn').forEach(function (el) {
            if (el.dataset.apiKeysWired === '1') return;
            el.dataset.apiKeysWired = '1';
            el.addEventListener('click', function () {
                var id = parseInt(el.dataset.apiKeyId, 10);
                var name = el.dataset.apiKeyName || '';
                if (id) deleteKey(id, name);
            });
        });
        document.querySelectorAll('.api-key-show-btn').forEach(function (el) {
            if (el.dataset.apiKeysWired === '1') return;
            el.dataset.apiKeysWired = '1';
            el.addEventListener('click', function () {
                var id = parseInt(el.dataset.apiKeyId, 10);
                if (id) showKey(id, el);
            });
        });
        document.querySelectorAll('.api-key-copy-btn').forEach(function (el) {
            if (el.dataset.apiKeysWired === '1') return;
            el.dataset.apiKeysWired = '1';
            el.addEventListener('click', function () {
                var id = parseInt(el.dataset.apiKeyId, 10);
                if (id) copyKey(id, el);
            });
        });
    }

    // Extracted so renderList() can re-bind after replacing the DOM
    // (the "+ Create API key" tile lives inside the grid that gets
    // re-rendered, so its onclick listener has to follow). Idempotent
    // via the dataset.apiKeysCreateWired guard so init() and renderList
    // can both call it without stacking handlers.
    function wireCreateButton() {
        var createBtn = document.getElementById('api-keys-create-btn');
        if (!createBtn || createBtn.dataset.apiKeysCreateWired === '1') return;
        createBtn.dataset.apiKeysCreateWired = '1';
        createBtn.addEventListener('click', openCreateModal);
    }

    function init() {
        // Only wire when the api-keys tab is rendered. Selector doubles
        // as a tab-presence check; a no-op on every other tab.
        if (!document.getElementById('api-keys-tab')) return;
        wireListEvents();
        wireCreateButton();

        var form = document.getElementById('api-keys-create-form');
        if (form) {
            form.addEventListener('submit', submitCreateForm);
            // Delegated change listener — re-runs the shadow rule
            // on any scope checkbox flip so admin-on/off updates
            // the other tiles immediately.
            form.addEventListener('change', function (ev) {
                if (ev.target && ev.target.name === 'scope') {
                    syncScopeShadowing();
                }
            });
        }

        var copyBtn = document.getElementById('api-keys-copy-btn');
        if (copyBtn) copyBtn.addEventListener('click', copyPlaintext);

        var savedBtn = document.getElementById('api-keys-saved-btn');
        if (savedBtn) savedBtn.addEventListener('click', function () {
            closeCreateModal();
            refreshList();
        });

        // Generic modal-close handler for the overlay + cancel buttons.
        document.querySelectorAll('[data-modal-close="api-keys-create-modal"]').forEach(function (el) {
            el.addEventListener('click', closeCreateModal);
        });
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }
})();
