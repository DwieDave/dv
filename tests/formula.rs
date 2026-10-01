//! The Homebrew formula renderer fills every placeholder from the release checksums.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type Check = Result<(), String>;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/.github/scripts/render-formula.sh"
);
const VERSION: &str = "1.2.3";
const TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

/// A distinct fake SHA-256 per target, so a swapped checksum would show.
fn sha(index: usize) -> String {
    format!("{index}").repeat(64)
}

/// Writes `<archive>.sha256` files (as `shasum` prints them) for `targets` into `dist`.
fn checksums(dist: &Path, targets: &[&str]) -> Check {
    fs::create_dir_all(dist).map_err(|e| e.to_string())?;
    targets.iter().enumerate().try_for_each(|(i, target)| {
        let archive = format!("dv-{VERSION}-{target}.tar.gz");
        fs::write(
            dist.join(format!("{archive}.sha256")),
            format!("{}  {archive}\n", sha(i)),
        )
        .map_err(|e| e.to_string())
    })
}

fn render(dist: &Path) -> Result<Output, String> {
    Command::new("sh")
        .args([SCRIPT, VERSION])
        .arg(dist)
        .output()
        .map_err(|e| format!("spawn {SCRIPT}: {e}"))
}

#[test]
fn renders_version_and_each_targets_checksum() -> Check {
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    checksums(work.path(), &TARGETS)?;
    let output = render(work.path())?;
    let formula = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!formula.contains('@'), "placeholder left in:\n{formula}");
    for (i, target) in TARGETS.iter().enumerate() {
        let url = format!(
            "download/v{VERSION}/dv-{VERSION}-{target}.tar.gz\"\n      sha256 \"{}\"",
            sha(i)
        );
        assert!(
            formula.contains(&url),
            "{target} url/sha256 pair missing:\n{formula}"
        );
    }
    Ok(())
}

#[test]
fn fails_when_a_checksum_is_missing() -> Check {
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    checksums(work.path(), &TARGETS[..3])?;
    let output = render(work.path())?;
    assert!(!output.status.success(), "rendered without every checksum");
    assert!(output.stdout.is_empty(), "printed a partial formula");
    Ok(())
}
