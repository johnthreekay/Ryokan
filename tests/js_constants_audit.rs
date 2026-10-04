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

/// Whether a `/` written after `out` starts a regex literal rather than
/// a division: it does after an operator or opening punctuator, after
/// `return`, and at the start of a line. After a name, a number, `)`
/// or `]` it divides.
fn regex_can_start(out: &str) -> bool {
    let before = out.trim_end_matches([' ', '\t', '\r']);
    match before.as_bytes().last() {
        None | Some(b'\n') => true,
        Some(c) if b"(,=:[!&|?{};".contains(c) => true,
        Some(_) => {
            let word = before
                .trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$');
            &before[word.len()..] == "return"
        }
    }
}

/// The end (just past the flags) of the regex literal whose opening `/`
/// is at `start`, or `None` when the line ends first and the `/` was
/// not one after all. A `/` inside a `[...]` class does not close it.
fn regex_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return None,
            b'\\' => i += 1,
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Blank out comments, string literals and regex literals so names
/// inside them (`SQLite CURRENT_TIMESTAMP`, `URL_BASE set`) are not read
/// as uses, and so a quote inside a regex (`.replace(/"/g, ...)`) does
/// not open a string that swallows the rest of the file. Template
/// literals are blanked whole, `${...}` included: no use of a constant
/// hides there today, and the scanner stays simple.
fn code_only(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let regex =
            if c == b'/' && !matches!(bytes.get(i + 1), Some(b'/' | b'*')) && regex_can_start(&out)
            {
                regex_end(bytes, i)
            } else {
                None
            };
        if let Some(end) = regex {
            i = end;
            out.push_str("\"\"");
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
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

#[test]
fn a_quote_inside_a_regex_literal_does_not_open_a_string() {
    let uses = |src: &str| {
        upper_snake_uses(&code_only(src))
            .into_iter()
            .collect::<Vec<_>>()
    };
    assert_eq!(uses(".replace(/\"/g, '&quot;'); use(A_B)"), vec!["A_B"]);
    assert_eq!(uses(".replace(/'/g, '&#39;'); use(A_B)"), vec!["A_B"]);
    // A `/` or quote in a class or escaped does not end the literal, and
    // a name inside the pattern is not a use.
    assert_eq!(
        uses("s.split(/[/\"]+/); t.match(/\\/'NOT_USED/i); use(A_B)"),
        vec!["A_B"]
    );
    assert_eq!(
        uses("function f(s) {\n  return /\"/.test(s) && A_B;\n}"),
        vec!["A_B"]
    );
    assert_eq!(uses("var patterns = [\n  /\"/,\n  A_B,\n];"), vec!["A_B"]);
    // Comments still read as comments.
    assert_eq!(
        uses("x = 1; // NOT_USED \"\nuse(A_B) /* C_D */"),
        vec!["A_B"]
    );
}

#[test]
fn a_division_is_not_read_as_a_regex() {
    let uses = |src: &str| {
        upper_snake_uses(&code_only(src))
            .into_iter()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        uses("w = A_B / 2 / C_D; h = (E_F) / G_H; i = arr[0] / I_J;"),
        vec!["A_B", "C_D", "E_F", "G_H", "I_J"]
    );
    // An unclosed `/` that looked like a regex start leaves the line as code.
    assert_eq!(uses("ratio = (\n/ A_B);"), vec!["A_B"]);
}
