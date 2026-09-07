// Page lifecycle helper for the hx-boost rollout (Phase B).
//
// Why this exists: per-page scripts that started a `setInterval` at
// module scope (downloads.js, system.js) work fine on a fresh
// document load but leak when the user navigates AWAY via boost
// (interval keeps firing in the background, polling for elements
// that no longer exist) and double-leak when they navigate BACK
// (a second interval starts on top of the first, never the same
// reference). One `setInterval` per nav, never cleared.
//
// Pattern this exposes:
//
//   ryokanRegisterPageInit('downloads-queue', {
//       check: () => !!document.getElementById('queue-container'),
//       mount: () => { window.__downloadsQueuePoller = setInterval(loadQueue, 5000); },
//       unmount: () => {
//           clearInterval(window.__downloadsQueuePoller);
//           window.__downloadsQueuePoller = null;
//       },
//   });
//
// Each registration runs `check()` on every htmx.onLoad firing
// (initial document load + every boosted swap):
//   - was-active && !is-active  → call unmount() (page left)
//   - !was-active && is-active  → call mount() (page entered)
//   - is-active && was-active   → no-op (re-render of same page;
//                                  caller's mount must be idempotent
//                                  if it ever needs to run twice)
//
// The boot order is important: this script must load AFTER htmx
// (so `window.htmx.onLoad` exists) and BEFORE the per-page scripts
// that call `ryokanRegisterPageInit`. base.html loads it between
// htmx and `base.js`.

(function () {
    const registry = [];

    window.ryokanRegisterPageInit = function (name, options) {
        const reg = {
            name: name,
            wasActive: false,
            check: options.check,
            mount: options.mount || function () {},
            unmount: options.unmount || function () {},
        };
        // Dedupe by name. Per-page scripts re-execute on every boost-
        // nav, so without dedup each visit pushes a *new* registration
        // — after N visits, N copies of the same page mount fire on
        // every htmx.onLoad. Worse, the OLDEST registration's `mount`
        // captures stale closures from the first visit (e.g. a
        // grab_picker.js IIFE on visit 1 created `closeModal` /
        // `confirmGrab` referencing visit 1's `let session = null`;
        // on visit N, calling that `closeModal` operates on visit 1's
        // session variable while `window.openGrabPicker` mutates
        // visit N's). Because the per-element `dataset.bound` guards
        // make only the *first* bind actually attach handlers, the
        // user ends up with stale-closure handlers on a fresh DOM
        // element + a `window.foo` API talking to a different state
        // — every interaction silently does nothing.
        //
        // Replacing by name means the latest registration always
        // wins. Its closures match the currently-mutating
        // `window.foo` API, and stale registrations are GC'd.
        const existing = registry.findIndex(r => r.name === name);
        if (existing >= 0) {
            // Carry forward `wasActive` so a re-registration during
            // an already-mounted state doesn't trip a duplicate
            // mount on the next lifecycle pass. The new mount/unmount
            // closures take over from here.
            reg.wasActive = registry[existing].wasActive;
            registry[existing] = reg;
        } else {
            registry.push(reg);
        }
        // Immediate-reconcile: per-page scripts load AFTER
        // page_lifecycle.js (defer ordering). htmx.onLoad already
        // fired its initial-document pass before this registration
        // landed, so without an immediate check the page's mount
        // wouldn't fire until the FIRST boosted swap. That breaks
        // direct-URL loads where the page IS active right now —
        // the poller never starts. Run the check + mount inline.
        //
        // Skip the immediate mount if `wasActive` carried forward
        // from a prior registration of the same name — that means
        // the page is already mounted under the old closures and
        // we're just swapping in new ones; calling mount() again
        // here would double-attach (the per-element dataset guards
        // would no-op the bind, but the new closure-captured state
        // would still drift from the live mounted state).
        if (document.readyState !== 'loading' && !reg.wasActive) {
            try {
                if (reg.check()) {
                    reg.mount();
                    reg.wasActive = true;
                }
            } catch (e) {
                if (window.console && console.error) {
                    console.error(
                        'ryokanRegisterPageInit immediate-mount failed for',
                        reg.name, e
                    );
                }
            }
        }
    };

    function applyLifecycle() {
        for (const reg of registry) {
            // Wrap check() so a future page whose `check` throws
            // (e.g. `document.getElementById(...).dataset.bar`
            // dereferencing null) can't break sibling registrations
            // by aborting the loop. Mirrors the mount/unmount
            // try/catch posture below — `htmx.onLoad`'s own internal
            // error handling has the same shape.
            let isActive;
            try {
                isActive = !!reg.check();
            } catch (e) {
                if (window.console && console.error) {
                    console.error('ryokanRegisterPageInit check failed for', reg.name, e);
                }
                continue;
            }
            if (isActive && !reg.wasActive) {
                try { reg.mount(); } catch (e) {
                    // A page's mount throwing must not prevent
                    // sibling registrations from being processed.
                    // Log and continue.
                    if (window.console && console.error) {
                        console.error('ryokanRegisterPageInit mount failed for', reg.name, e);
                    }
                }
            } else if (!isActive && reg.wasActive) {
                try { reg.unmount(); } catch (e) {
                    if (window.console && console.error) {
                        console.error('ryokanRegisterPageInit unmount failed for', reg.name, e);
                    }
                }
            }
            reg.wasActive = isActive;
        }
    }

    // Wire to htmx.onLoad when available; falls back to a single
    // DOMContentLoaded firing for the rare case htmx never loads
    // (vendored asset 404, etc.) so the initial page still gets
    // its mount() call.
    if (window.htmx && typeof window.htmx.onLoad === 'function') {
        window.htmx.onLoad(applyLifecycle);
    } else {
        document.addEventListener('DOMContentLoaded', applyLifecycle);
    }
})();
