//! The release packaging script builds the archive layout the Homebrew formula expects.

use std::fs;
use std::path::Path;
use std::process::Command;

type Check = Result<(), String>;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/scripts/package.sh");
const ARCHIVE: &str = "dv-1.2.3-aarch64-apple-darwin.tar.gz";

fn run(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|e| format!("spawn {command:?}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "{command:?} failed: {}{stdout}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Packages a stand-in binary into `out` and returns the archive's file listing.
fn package(work: &Path) -> Result<String, String> {
    let bin = work.join("dv");
    let out = work.join("dist");
    fs::write(&bin, "#!/bin/sh\n").map_err(|e| e.to_string())?;
    run(Command::new("sh")
        .arg(SCRIPT)
        .args(["aarch64-apple-darwin", "1.2.3"])
        .args([&bin, &out]))?;
    run(Command::new("tar").arg("tzf").arg(out.join(ARCHIVE)))
}

#[test]
fn archive_holds_the_binary_licenses_and_readme_at_its_root() -> Check {
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut listing: Vec<String> = package(work.path())?
        .lines()
        .map(|line| line.trim_start_matches("./").to_owned())
        .filter(|line| !line.is_empty())
        .collect();
    listing.sort();
    assert_eq!(
        listing,
        ["LICENSE-APACHE", "LICENSE-MIT", "README.md", "dv"]
    );
    Ok(())
}

#[test]
fn checksum_file_verifies_the_archive() -> Check {
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    package(work.path())?;
    let out = work.path().join("dist");
    let verified = run(Command::new("shasum")
        .args(["-a", "256", "-c"])
        .arg(format!("{ARCHIVE}.sha256"))
        .current_dir(&out))?;
    assert!(verified.contains(&format!("{ARCHIVE}: OK")), "{verified}");
    Ok(())
}
