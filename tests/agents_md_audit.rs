//! The `AGENTS.md` files have to stay small enough to be read.
//!
//! Claude Code loads the root file into every session in full, so its
//! size is paid on every task; it reached 167,000 characters (about
//! 42,000 tokens) before it was split. A nested file is loaded on
//! demand, when an agent reads or edits a file under its folder, and a
//! session with a 200K context window is given only the first 40,000
//! characters of one: the rest is silently not in context. So:
//!
//! - every nested `AGENTS.md` stays under 40,000 characters,
//! - the root one stays under its budget,
//! - every `AGENTS.md` has the sibling `CLAUDE.md` symlink that tools
//!   looking only for that name follow,
//! - and a path one of these files points at exists.
//!
//! When the first two fail, the fix is to move detail down a level: a
//! subsystem's notes belong in the nested file beside its code, with a
//! line and a pointer left where they were.

use std::path::{Path, PathBuf};

/// Claude Code's per-file limit for an instruction file that loads on
/// demand, in a session with a 200K context window.
const NESTED_LIMIT: usize = 40_000;

/// The root file is loaded whole whatever its size. This is a budget,
/// set with room above the 65,000 characters the split left it at.
const ROOT_BUDGET: usize = 80_000;

/// Folders that hold no project notes: build output, dependencies,
/// local state, and agent worktrees (whole checkouts of this repo).
const SKIP: &[&str] = &["target", "node_modules", "data", "site", ".git", ".claude"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIP.contains(&name.as_ref()) && !name.starts_with(".venv") {
                collect(&path, found);
            }
        } else if name == "AGENTS.md" {
            found.push(path);
        }
    }
}

/// Every `AGENTS.md` in the repository, the root one first.
fn agents_files() -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect(&repo_root(), &mut found);
    found.sort();
    assert!(
        found.contains(&repo_root().join("AGENTS.md")),
        "the root AGENTS.md was not found; is the walk skipping too much?"
    );
    found
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn chars(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .chars()
        .count()
}

#[test]
fn nested_agents_files_fit_the_on_demand_limit() {
    let root = repo_root().join("AGENTS.md");
    let over: Vec<String> = agents_files()
        .iter()
        .filter(|p| **p != root)
        .map(|p| (relative(p), chars(p)))
        .filter(|(_, n)| *n >= NESTED_LIMIT)
        .map(|(p, n)| format!("{p}: {n} characters"))
        .collect();
    assert!(
        over.is_empty(),
        "a nested AGENTS.md has to stay under {NESTED_LIMIT} characters: a Claude Code session with a \
         200K context window is given only the start of a longer one. Split these by subfolder, \
         or cut what the code already says:\n  {}",
        over.join("\n  ")
    );
}

#[test]
fn root_agents_file_stays_within_its_budget() {
    let n = chars(&repo_root().join("AGENTS.md"));
    assert!(
        n <= ROOT_BUDGET,
        "the root AGENTS.md is {n} characters, over its budget of {ROOT_BUDGET}. It is loaded \
         into every session in full. Move the subsystem detail into the nested AGENTS.md beside \
         that code and leave a line with a pointer."
    );
}

#[cfg(unix)]
#[test]
fn every_agents_file_has_a_claude_symlink() {
    let mut problems = Vec::new();
    for agents in agents_files() {
        let link = agents.with_file_name("CLAUDE.md");
        match std::fs::read_link(&link) {
            Ok(target) if target == Path::new("AGENTS.md") => {}
            Ok(target) => problems.push(format!(
                "{} points at {}, not AGENTS.md",
                relative(&link),
                target.display()
            )),
            Err(_) => problems.push(format!(
                "{} is missing or is not a symlink (ln -s AGENTS.md CLAUDE.md)",
                relative(&link)
            )),
        }
    }
    assert!(
        problems.is_empty(),
        "every AGENTS.md needs a sibling CLAUDE.md symlink:\n  {}",
        problems.join("\n  ")
    );
}

/// A `path/to/AGENTS.md` written in backticks is a pointer a reader
/// follows. The files point at each other a great deal since the root
/// was split, and a moved or renamed file should not leave one behind.
#[test]
fn pointers_between_agents_files_resolve() {
    let mut dangling = Vec::new();
    for agents in agents_files() {
        let text = std::fs::read_to_string(&agents).unwrap();
        for piece in text.split('`').skip(1).step_by(2) {
            if !piece.ends_with("/AGENTS.md") || piece.contains(' ') || piece.contains('*') {
                continue;
            }
            // Written from the repository root, or, in a nested file's
            // own title, from `src/`.
            let found = [repo_root().join(piece), repo_root().join("src").join(piece)]
                .iter()
                .any(|p| p.is_file());
            if !found {
                dangling.push(format!("{} names `{piece}`", relative(&agents)));
            }
        }
    }
    assert!(
        dangling.is_empty(),
        "an AGENTS.md points at a file that does not exist:\n  {}",
        dangling.join("\n  ")
    );
}
