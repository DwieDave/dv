//! Repository guards that keep NFR-5 (no unsafe) enforced at the source level.

use std::fs;

const CRATE_ROOTS: [&str; 2] = ["src/lib.rs", "src/main.rs"];
const FORBID_UNSAFE: &str = "#![forbid(unsafe_code)]";

#[test]
fn every_crate_root_forbids_unsafe() {
    let missing: Vec<&str> = CRATE_ROOTS
        .into_iter()
        .filter(|path| {
            !fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .any(|line| line.trim() == FORBID_UNSAFE)
        })
        .collect();

    assert!(missing.is_empty(), "missing {FORBID_UNSAFE} in {missing:?}");
}

/// Every `.rs` file under `dir`, recursively.
fn sources(dir: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out
}

/// NFR-13: production code creates temp files only through `temp::file` (tests may unwrap).
#[test]
fn temp_files_come_from_one_place() {
    let offenders: Vec<String> = sources("src")
        .into_iter()
        .filter(|path| !path.ends_with("temp.rs"))
        .filter(|path| {
            let text = fs::read_to_string(path).unwrap_or_default();
            text.contains("use tempfile")
                || text
                    .match_indices("tempfile::tempfile()")
                    .any(|(at, m)| !text[at + m.len()..].starts_with(".unwrap()"))
        })
        .map(|path| path.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "create temp files with temp::file(): {offenders:?}"
    );
}
