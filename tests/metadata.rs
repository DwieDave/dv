//! Crate metadata and CI workflow stay consistent with the toolchain policy.

use std::fs;
use toml::Table;

type Check = Result<(), String>;

fn read(relative: &str) -> Result<String, String> {
    let path = format!("{}/{relative}", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))
}

fn manifest() -> Result<Table, String> {
    read("Cargo.toml")?
        .parse()
        .map_err(|e| format!("parse Cargo.toml: {e}"))
}

#[test]
fn declares_a_minimum_rust_version() -> Check {
    let manifest = manifest()?;
    let declared = manifest["package"]["rust-version"]
        .as_str()
        .unwrap_or_default();
    let minor: u32 = declared
        .strip_prefix("1.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|minor| minor.parse().ok())
        .unwrap_or(0);
    assert!(
        minor >= 85,
        "rust-version must be set and at least 1.85, got {declared:?}"
    );
    Ok(())
}

#[test]
fn yaml_parsers_are_pinned_exactly() -> Check {
    let manifest = manifest()?;
    for name in ["saphyr", "saphyr-parser"] {
        let version = manifest["dependencies"][name].as_str().unwrap_or_default();
        assert!(
            version.starts_with('='),
            "{name} must be pinned with `=`, got {version:?}"
        );
    }
    Ok(())
}

#[test]
fn ci_runs_every_gate_on_arm_macos() -> Check {
    let ci = read(".github/workflows/ci.yml")?;
    for needle in [
        "macos-latest",
        "cargo fmt --all --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo nextest run",
        "cargo deny check",
        "taiki-e/install-action",
    ] {
        assert!(ci.contains(needle), "ci.yml is missing {needle:?}");
    }
    Ok(())
}

#[test]
fn is_dual_licensed_with_both_license_files() -> Check {
    let manifest = manifest()?;
    let license = manifest["package"]
        .get("license")
        .and_then(|l| l.as_str())
        .unwrap_or_default();
    assert_eq!(license, "MIT OR Apache-2.0");
    for file in ["LICENSE-MIT", "LICENSE-APACHE"] {
        assert!(!read(file)?.trim().is_empty(), "{file} is empty");
    }
    Ok(())
}

#[test]
fn points_at_the_public_repository() -> Check {
    let manifest = manifest()?;
    let repository = manifest["package"]
        .get("repository")
        .and_then(|r| r.as_str())
        .unwrap_or_default();
    assert_eq!(repository, "https://github.com/DwieDave/dv");
    Ok(())
}
