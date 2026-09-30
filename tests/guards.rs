//! Repository guards that keep NFR-5 (no unsafe) enforced in the fuzz crate, which is outside
//! the workspace lints. Temp-file creation is guarded by clippy's `disallowed-methods`.

use std::fs;

const FORBID_UNSAFE: &str = "#![forbid(unsafe_code)]";

#[test]
fn every_fuzz_target_forbids_unsafe() {
    let fuzz_targets = sources("fuzz/fuzz_targets");
    assert!(!fuzz_targets.is_empty(), "fuzz targets not found");
    let missing: Vec<std::path::PathBuf> = fuzz_targets
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
