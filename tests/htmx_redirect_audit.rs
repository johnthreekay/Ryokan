//! hx-boost rollout — Phase C lint test.
//!
//! Per the hx-boost rollout plan's Phase C definition-of-done:
//! every `Redirect::to` callsite outside
//! the documented exceptions must go through
//! `crate::handlers::responses::htmx_aware_redirect` (or its
//! `*_from_req` variant for middleware). This test grep-asserts that
//! invariant against the source tree at test time so a new handler
//! adding a bare `Redirect::to` lands as a CI failure rather than a
//! latent boost-nesting regression.
//!
//! ### Documented exceptions
//!
//! - `src/handlers/auth/mod.rs::login_page` and `setup_page` — public
//!   pages, never reached via boost (the unauth `/login` doesn't have
//!   boost active; redirects from these are the FIRST page load).
//! - `src/handlers/auth/mod.rs::setup_submit::Ok(true)` branch and its
//!   lost-race `create_first_user` `Ok(None)` branch — same as above.
//! - `src/handlers/library/pages/mod.rs::needs_review_page` — 308 permanent
//!   for a moved URL; documented inline as boost-safe (target is a
//!   top-level page render, no form context to nest into).
//! - `src/handlers/responses.rs` — the helper itself uses
//!   `Redirect::to` internally on the non-HTMX branch.
//! - `src/handlers/downloads.rs::api_blocklist_remove` — already
//!   correctly branches on `is_htmx`; the `Redirect::to` only fires
//!   in the `else` arm.
//! - Test code inside `#[cfg(test)]` blocks.
//!
//! Anywhere else, a new `Redirect::to` is the bug Phase C exists to
//! prevent.

use std::path::PathBuf;

/// The expected count of `Redirect::to` and `Redirect::permanent`
/// callsites in the source tree, sorted by file. Each entry is
/// either a documented-exception location OR an inside-`if !is_htmx`
/// branch where the conditional already protects against the boost
/// nesting bug.
///
/// **If you add a new handler with a redirect**, the test below
/// will fail with a count mismatch. The fix is one of:
///   1. Migrate the handler to `htmx_aware_redirect` (the right
///      answer 99% of the time).
///   2. Add the new line to the EXPECTED_REDIRECTS table below with
///      a one-line comment explaining why it's exempt.
///
/// Numbers are inclusive: every match `grep -nP 'Redirect::(to|permanent)'`
/// finds in `src/` should be accounted for. The test counts matches
/// per file to keep the table reviewable.
const EXPECTED_REDIRECTS: &[(&str, usize)] = &[
    // Helper itself — `Redirect::to` is the non-HTMX branch.
    ("src/handlers/responses.rs", 1),
    // Auth: `login_page` (1), `setup_submit::Ok(true)` (1),
    // `setup_submit` lost race (1), `setup_page` (1).
    // All never reached via boost; documented above.
    ("src/handlers/auth/mod.rs", 4),
    // 308 permanent for moved /library/review URL — boost-safe.
    ("src/handlers/library/pages/mod.rs", 1),
    // Blocklist row removal — is_htmx branch is the empty 200 swap;
    // `Redirect::to` only fires in the `!is_htmx` arm.
    ("src/handlers/downloads.rs", 1),
    // CF deletes inside `if !is_htmx` arms.
    ("src/handlers/settings/custom_formats.rs", 2),
    // DC redirects inside `if !is_htmx` arms (upsert success, delete
    // success, set-default success, and delete-error fallback).
    ("src/handlers/settings/download_clients.rs", 4),
    // Indexers: `Redirect::to` only inside `!is_htmx` arms.
    ("src/handlers/settings/indexers.rs", 3),
    // Group delete inside `!is_htmx` arms.
    ("src/handlers/settings/mod.rs", 3),
];

#[test]
fn no_unaudited_redirect_callsites() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src");
    let mut total_per_file: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    walk(&src, &mut total_per_file);

    // Build the expected map for comparison.
    let expected: std::collections::BTreeMap<String, usize> = EXPECTED_REDIRECTS
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();

    let mut diffs: Vec<String> = Vec::new();
    for (path, count) in &total_per_file {
        match expected.get(path) {
            Some(expected_count) if expected_count == count => {}
            Some(expected_count) => {
                diffs.push(format!(
                    "{path}: expected {expected_count} Redirect::to/permanent callsite(s), got {count}"
                ));
            }
            None => {
                diffs.push(format!(
                    "{path}: NEW file with {count} Redirect::to/permanent callsite(s) — \
                     either migrate to htmx_aware_redirect or add an exception entry to \
                     tests/htmx_redirect_audit.rs::EXPECTED_REDIRECTS with a one-line comment"
                ));
            }
        }
    }
    for path in expected.keys() {
        if !total_per_file.contains_key(path) {
            diffs.push(format!(
                "{path}: file no longer present (or all redirects removed); drop the entry from \
                 tests/htmx_redirect_audit.rs::EXPECTED_REDIRECTS"
            ));
        }
    }

    assert!(
        diffs.is_empty(),
        "Phase C redirect audit failed — {} change(s):\n  {}\n\nBackground: \
         the hx-boost rollout plan's Phase C requires every Redirect::to in \
         handlers/ to either go through htmx_aware_redirect (boost-safe) or be \
         in the documented exceptions list.",
        diffs.len(),
        diffs.join("\n  ")
    );
}

/// Recursive walker that counts `Redirect::to` / `Redirect::permanent`
/// callsites per file. Skips `#[cfg(test)]` test modules so test-only
/// uses (e.g., handler tests asserting the `Location` header on a 303
/// path) don't leak into the audit.
fn walk(dir: &std::path::Path, out: &mut std::collections::BTreeMap<String, usize>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let count = count_redirects_excluding_test_modules(&contents);
        if count == 0 {
            continue;
        }
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let rel = path
            .strip_prefix(&manifest_dir)
            .ok()
            .map(|p| p.display().to_string().replace('\\', "/"))
            .unwrap_or_else(|| path.display().to_string());
        out.insert(rel, count);
    }
}

/// Counts `Redirect::to` / `Redirect::permanent` outside test
/// modules. We do a simple-but-robust scan: walk lines, track
/// `#[cfg(test)]` then the next `mod ... {` brace depth to know when
/// we're "inside" test code, count outside.
fn count_redirects_excluding_test_modules(contents: &str) -> usize {
    let mut count = 0usize;
    let bytes = contents.as_bytes();

    // Pre-scan: find the byte offsets of `#[cfg(test)]` followed by
    // a `mod NAME {`. Mark the byte ranges enclosed by those module
    // braces as "in test scope" via a sorted Vec of (start, end).
    let mut test_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(rest) = contents.get(i..).and_then(|s| s.find("#[cfg(test)]")) {
            let attr_start = i + rest;
            // Find the next `{` that opens the test module body.
            if let Some(brace_offset) = contents[attr_start..].find('{') {
                let body_start = attr_start + brace_offset + 1;
                // Find the matching `}` by depth-counting.
                let mut d = 1i32;
                let mut j = body_start;
                while j < bytes.len() && d > 0 {
                    match bytes[j] {
                        b'{' => d += 1,
                        b'}' => d -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                test_ranges.push((attr_start, j));
                i = j;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // Helper: is this byte offset inside a comment line? Walk
    // backwards from `abs` to find the line start, then check
    // whether the prefix begins with `//` (line comment) — that
    // catches both `// regular` and `/// doc` and `//!` module-doc.
    //
    // Limitation: only matches LINE-PREFIX comments. A line shaped
    // like `let x = 1; // see Redirect::to docs` would (incorrectly)
    // count the `Redirect::to` in the trailing comment. No such
    // cases exist in the source today; if one ever surfaces as a
    // false-positive audit failure, either rewrite the comment to
    // not name the symbol or extend this filter to scan from the
    // line start for the first `//` and treat anything after as a
    // comment range.
    let line_starts_with_comment = |abs: usize| -> bool {
        let mut start = abs;
        while start > 0 && bytes[start - 1] != b'\n' {
            start -= 1;
        }
        let line_prefix = &contents[start..abs.min(contents.len())];
        let trimmed = line_prefix.trim_start();
        trimmed.starts_with("//")
    };

    // Now scan once for the redirect needles, skipping any byte
    // offset inside a test range OR on a comment line.
    for needle in &["Redirect::to", "Redirect::permanent"] {
        let mut search_start = 0;
        while let Some(rel) = contents[search_start..].find(needle) {
            let abs = search_start + rel;
            let inside_test = test_ranges.iter().any(|(s, e)| abs >= *s && abs < *e);
            if !inside_test && !line_starts_with_comment(abs) {
                count += 1;
            }
            search_start = abs + needle.len();
        }
    }

    count
}

#[cfg(test)]
mod self_tests {
    use super::*;

    /// Sanity: the counter excludes test-module redirects.
    #[test]
    fn ignores_redirects_in_cfg_test_modules() {
        let src = r#"
            use axum::response::Redirect;
            pub fn prod() -> Redirect { Redirect::to("/prod") }

            #[cfg(test)]
            mod tests {
                use super::*;
                #[test]
                fn t() {
                    let r = Redirect::to("/test1");
                    let r2 = Redirect::permanent("/test2");
                    let _ = (r, r2);
                }
            }
        "#;
        // Only the `pub fn prod` redirect counts; the two inside
        // `mod tests` are test-only.
        assert_eq!(count_redirects_excluding_test_modules(src), 1);
    }

    /// Sanity: a file with no redirects returns 0.
    #[test]
    fn returns_zero_for_clean_file() {
        let src = r#"
            pub fn nothing_here() -> i32 { 42 }
        "#;
        assert_eq!(count_redirects_excluding_test_modules(src), 0);
    }
}
