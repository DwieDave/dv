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
