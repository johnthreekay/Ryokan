//! HTMX foundation tests.
//!
//! Pins the load-bearing scaffolding the rest of the HTMX surface
//! depends on:
//!
//!   1. The vendored htmx 4 core exists on disk at the expected path
//!      with the expected shape. If anyone renames / moves /
//!      accidentally deletes it, this fails before a user hits a
//!      missing-script 404 in production. There are no extensions:
//!      the progress toast streams over native `EventSource`, and the
//!      `<head>` is static across boosted navs (all CSS bundled in
//!      base.html), so neither the SSE nor the head-support extension
//!      of the htmx 2 days is needed.
//!   2. `templates/base.html` references the core before `base.js`
//!      so custom JS that reads the `htmx.*` global sees it on first
//!      paint.
//!   3. `<body hx-boost:inherited="true">`. htmx 4 inherits attributes
//!      only with the `:inherited` suffix; a bare `hx-boost="true"`
//!      would boost nothing below it and every nav would become a
//!      full document load.
//!   4. The `htmx-config` meta keeps two htmx 2 behaviors Ryokan was
//!      built against: `noSwap` covering 4xx/5xx (handlers return
//!      plain-text errors on those statuses and rely on the HX-Trigger
//!      toast) and no request timeout (interactive search can run
//!      past htmx 4's 60s default).
//!   5. Nothing in templates / JS still speaks htmx 2: the removed
//!      attributes, the old event names, and `htmx.ajax`'s `pushUrl`
//!      option all silently stop working under 4 rather than erroring.
//!
//! Per-handler `HxRequest` branching, fragment-vs-page response shape,
//! and DOM-state assertions live alongside their handlers
//! (`tests/htmx_browser_e2e*.rs` for the browser layer).
//!
//! Asserts against on-disk artifacts directly rather than going through
//! the test router (`handler_router` is a minimal test surface that
//! doesn't mount `/static` or page renderers, so a router-based test
//! would fail for unrelated reasons). The on-disk approach is also
//! strictly more robust — it catches a missing file regardless of
//! what the router happens to mount.

use std::path::Path;

const HTMX_CORE_PATH: &str = "static/vendor/htmx-4.0.0.min.js";

/// Vendored htmx core exists and looks like htmx 4 (not, e.g., a 404
/// page captured from a bad fetch, and not the 2.x bundle).
#[test]
fn htmx_core_vendored_with_expected_shape() {
    let body = std::fs::read_to_string(HTMX_CORE_PATH)
        .unwrap_or_else(|e| panic!("vendored htmx core missing at {HTMX_CORE_PATH}: {e}"));
    assert!(
        body.starts_with("var htmx="),
        "asset at {HTMX_CORE_PATH} doesn't look like htmx (first 32 chars: {:?})",
        &body[..body.len().min(32)]
    );
    assert!(
        body.contains("version=\"4.0.0\"") || body.contains("version='4.0.0'"),
        "asset at {HTMX_CORE_PATH} does not report version 4.0.0"
    );
    // The 4.0.0 minified bundle is ~37KB; the unminified one is ~100KB.
    let len = body.len();
    assert!(
        (25_000..=60_000).contains(&len),
        "vendored htmx core size {len} bytes is outside the expected 25-60KB minified range — \
         possibly the unminified variant or a truncated fetch"
    );
}

/// No htmx 2 assets linger in `static/vendor/`: a stale bundle next
/// to the live one invites a template to reference the wrong file.
#[test]
fn no_htmx_2_assets_remain_vendored() {
    let stale: Vec<String> = std::fs::read_dir("static/vendor")
        .expect("static/vendor exists")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("htmx-2") || n.starts_with("htmx-ext-"))
        .collect();
    assert!(
        stale.is_empty(),
        "htmx 2 era assets still vendored: {stale:?}"
    );
}

/// `templates/base.html` loads the vendored core before `base.js` and
/// loads no htmx extension scripts.
#[test]
fn base_template_wires_htmx_correctly() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");

    assert!(
        body.contains(&format!("/{HTMX_CORE_PATH}")),
        "base.html must include the vendored script tag for /{HTMX_CORE_PATH}"
    );
    let htmx_pos = body
        .find("htmx-4.0.0.min.js")
        .expect("htmx core tag present");
    let base_js_pos = body
        .find("/static/js/base.js")
        .expect("base.js tag present");
    assert!(
        htmx_pos < base_js_pos,
        "htmx script tag must appear before base.js so the htmx global is available \
         to any custom JS"
    );
    assert!(
        !body.contains("htmx-ext-"),
        "base.html must not load htmx extension scripts; none are needed under htmx 4"
    );
}

/// `<body hx-boost:inherited="true">`, and nothing else htmx-related on
/// the tag: `hx-ext` was removed in 4 and would be dead text.
#[test]
fn base_body_declares_inherited_boost() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");

    let open = body.find("<body").expect("base.html has a <body> tag");
    let close = body[open..]
        .find('>')
        .map(|i| open + i)
        .expect("<body> open tag is closed");
    let body_tag = &body[open..=close];

    assert!(
        body_tag.contains(r#"hx-boost:inherited="true""#),
        "<body> must declare hx-boost:inherited=\"true\"; a bare hx-boost does not reach \
         descendants under htmx 4's explicit inheritance (got: {body_tag:?})"
    );
    assert!(
        !body_tag.contains("hx-ext"),
        "<body> must not carry hx-ext (removed in htmx 4) (got: {body_tag:?})"
    );
}

/// The `htmx-config` meta keeps the two htmx 2 behaviors Ryokan relies
/// on: error bodies never swap, and requests never time out.
#[test]
fn base_pins_htmx_2_compatible_config_via_meta() {
    let body =
        std::fs::read_to_string("templates/base.html").expect("templates/base.html must exist");
    // The Askama comment above the tag quotes the tag's name; anchor
    // on the attribute pair so the comment can't match first.
    let start = body
        .find(r#"<meta name="htmx-config" content='"#)
        .expect("base.html must declare a <meta name=\"htmx-config\" content='...'> tag");
    let tag_end = body[start..]
        .find('>')
        .map(|i| start + i)
        .unwrap_or(body.len());
    let tag = &body[start..tag_end];
    let content = tag
        .split("content='")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("htmx-config meta carries a single-quoted content attribute");
    let config: serde_json::Value =
        serde_json::from_str(content).expect("htmx-config content is valid JSON");

    let no_swap = config["noSwap"]
        .as_array()
        .expect("noSwap must be an array");
    for code in ["4xx", "5xx"] {
        assert!(
            no_swap.iter().any(|v| v == code),
            "noSwap must include {code:?} so error bodies never land in the page (got {no_swap:?})"
        );
    }
    assert_eq!(
        config["defaultTimeout"], 0,
        "defaultTimeout must be 0: htmx 4 aborts at 60s by default and interactive search can run longer"
    );
    assert!(
        config.get("historyEnableCache").is_none(),
        "historyEnableCache is an htmx 2 key; htmx 4 refetches on history navigation by default"
    );
}

/// htmx 2 vocabulary that htmx 4 silently ignores must not creep back
/// into templates or JS. Each entry is (needle, why it is wrong now).
#[test]
fn no_htmx_2_vocabulary_in_templates_or_js() {
    const BANNED: &[(&str, &str)] = &[
        ("hx-disabled-elt", "renamed to hx-disable"),
        ("hx-disinherit", "removed; inheritance is explicit"),
        ("hx-ext=", "removed; extension scripts load directly"),
        (
            "hx-on::response-error",
            "event names are colon-separated: hx-on::response:error",
        ),
        (
            "hx-on::after-request",
            "event names are colon-separated: hx-on::after:request",
        ),
        (
            "hx-on::after-swap",
            "event names are colon-separated: hx-on::after:swap",
        ),
        ("htmx:afterSwap", "renamed to htmx:after:swap"),
        ("htmx:afterSettle", "renamed to htmx:after:settle"),
        ("htmx:afterRequest", "renamed to htmx:after:request"),
        ("htmx:beforeRequest", "renamed to htmx:before:request"),
        ("htmx:responseError", "renamed to htmx:response:error"),
        ("htmx:configRequest", "renamed to htmx:config:request"),
        ("htmx:load'", "renamed to htmx:after:init"),
        ("pushUrl:", "htmx.ajax option renamed to push"),
        (
            "detail.successful",
            "htmx 4 has no detail.successful; check detail.ctx.response.status",
        ),
        (
            "historyEnableCache",
            "htmx 2 config key; htmx 4 has no history cache",
        ),
    ];
    let mut hits = Vec::new();
    for dir in ["templates", "static/js"] {
        visit(Path::new(dir), &mut |path, text| {
            for (line_no, line) in text.lines().enumerate() {
                for (needle, why) in BANNED {
                    if line.contains(needle) {
                        hits.push(format!(
                            "{}:{}: `{needle}` ({why})",
                            path.display(),
                            line_no + 1
                        ));
                    }
                }
            }
        });
    }
    assert!(
        hits.is_empty(),
        "htmx 2 vocabulary found (each silently does nothing under htmx 4):\n{}",
        hits.join("\n")
    );
}

fn visit(dir: &Path, f: &mut dyn FnMut(&Path, &str)) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            visit(&path, f);
        } else if path.extension().is_some_and(|e| e == "html" || e == "js")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            f(&path, &text);
        }
    }
}
