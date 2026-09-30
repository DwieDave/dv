//! Repository guards that keep the no-unsafe rule enforced in the fuzz crate, which is outside
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

/// Planning IDs: a short capital prefix, a dash and a number, or a dotted task number.
/// Names like `UTF-8` and `BSD-3-Clause` are not IDs.
const PLANNING_ID: &str = r"\b[A-Z]{1,3}-[0-9]+\b|\bT[0-9]+\.[0-9]+\b|\bM[0-9]\b";
const NOT_IDS: [&str; 4] = ["UTF-", "BSD-", "SHA-", "MD-"];

/// Comments say why in plain words, not which planning document item asked for it.
#[test]
fn source_cites_no_planning_ids() {
    let pattern = regex::Regex::new(PLANNING_ID).expect("valid pattern");
    let offenders: Vec<String> = sources("src")
        .into_iter()
        .flat_map(|path| {
            let text = fs::read_to_string(&path).unwrap_or_default();
            text.lines()
                .enumerate()
                .flat_map(|(n, line)| {
                    pattern
                        .find_iter(line)
                        .filter(|m| !NOT_IDS.iter().any(|p| m.as_str().starts_with(p)))
                        .map(|m| format!("{}:{}: {}", path.display(), n + 1, m.as_str()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(offenders.is_empty(), "planning IDs in src: {offenders:#?}");
}
