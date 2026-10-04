//! Every all-caps constant a page script uses must be defined somewhere.
//!
//! The static scripts are plain files that share one global scope per
//! page, with no bundler or type checker in front of them, so a
//! constant deleted in one place and still used in another only fails
//! when that code path runs in a browser. 65b545e dropped
//! `STATUS_ICON_HAVE` / `_MISSING` / `_UNAIRED` and
//! `DL_PROGRESS_HTML_ZERO` from series.js while five uses stayed: every
//! import left its episode row on "Importing…" and every delete threw
//! before its Undo toast. This test reads static/js and the templates
//! and fails on any `UPPER_SNAKE` name that is used but never declared
//! (`var` / `let` / `const` / `function`, or `window.NAME =`).

use std::collections::BTreeSet;
use std::path::Path;

/// Blank out comments and string literals so names inside them
/// (`SQLite CURRENT_TIMESTAMP`, `URL_BASE set`) are not read as uses.
/// Template literals are blanked whole, `${...}` included: no use of
/// a constant hides there today, and the scanner stays simple.
fn code_only(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == b'\'' || c == b'"' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push_str("\"\"");
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

/// `UPPER_SNAKE` names (at least one underscore) not preceded by `.`
/// (`Number.MAX_SAFE_INTEGER` is a property, not a global).
fn upper_snake_uses(code: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let starts_word = c.is_ascii_uppercase()
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'$'
                    || bytes[i - 1] == b'.'));
        if !starts_word {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_uppercase() || bytes[i].is_ascii_digit() || bytes[i] == b'_')
        {
            i += 1;
        }
        let at_word_end = i >= bytes.len()
            || !(bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$');
        let name = &code[start..i];
        if at_word_end && name.contains('_') && !name.ends_with('_') {
            found.insert(name.to_string());
        }
    }
    found
}

/// Names declared with `var` / `let` / `const` / `function`, or
/// assigned as `window.NAME =`.
fn declarations(code: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let words: Vec<&str> = code
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'))
        .filter(|w| !w.is_empty())
        .collect();
    for pair in words.windows(2) {
        if matches!(pair[0], "var" | "let" | "const" | "function") {
            found.insert(pair[1].to_string());
        }
    }
    let mut rest = code;
    while let Some(at) = rest.find("window.") {
        let tail = &rest[at + "window.".len()..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if tail[name.len()..].trim_start().starts_with('=')
            && !tail[name.len()..].trim_start().starts_with("==")
        {
            found.insert(name);
        }
        rest = tail;
    }
    found
}

fn files(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

#[test]
fn every_constant_a_page_script_uses_is_defined() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut scripts = Vec::new();
    files(&root.join("static/js"), "js", &mut scripts);
    let mut templates = Vec::new();
    files(&root.join("templates"), "html", &mut templates);
    assert!(!scripts.is_empty(), "no scripts found under static/js");

    let mut declared = BTreeSet::new();
    for path in scripts.iter().chain(templates.iter()) {
        let src = std::fs::read_to_string(path).expect("readable source");
        declared.extend(declarations(&code_only(&src)));
    }

    let mut missing = Vec::new();
    for path in &scripts {
        let src = std::fs::read_to_string(path).expect("readable script");
        for name in upper_snake_uses(&code_only(&src)) {
            if !declared.contains(&name) {
                let rel = path.strip_prefix(root).unwrap_or(path);
                missing.push(format!("{} uses {name}", rel.display()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "used but never declared (a ReferenceError the first time that path runs):\n{}",
        missing.join("\n")
    );
}

#[test]
fn the_scanner_reads_code_not_comments_or_strings() {
    let code = code_only(
        "// SQLITE_NOTE in a comment\n/* BLOCK_NOTE */ var A_B = 1; f('URL_BASE set'); use(C_D, x.MAX_SAFE_INTEGER);",
    );
    let uses = upper_snake_uses(&code);
    assert_eq!(
        uses.into_iter().collect::<Vec<_>>(),
        vec!["A_B".to_string(), "C_D".to_string()]
    );
    assert!(declarations(&code).contains("A_B"));
    assert!(declarations("window.E_F = 2;").contains("E_F"));
    assert!(!declarations("if (window.E_F == 2) {}").contains("E_F"));
}
