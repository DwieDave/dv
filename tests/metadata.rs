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
        minor >= 98,
        "rust-version must be set and at least 1.98, got {declared:?}"
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
fn ci_runs_every_gate_on_macos_and_linux() -> Check {
    let ci = read(".github/workflows/ci.yml")?;
    for needle in [
        "workflow_call:",
        "contents: read",
        "timeout-minutes:",
        "macos-26",
        "ubuntu-26.04",
        "ubuntu-26.04-arm",
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

#[test]
fn release_profile_optimizes_for_distribution() -> Check {
    let manifest = manifest()?;
    let release = manifest
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.as_table())
        .ok_or("Cargo.toml has no [profile.release]")?;
    let expected = [
        ("lto", toml::Value::from("fat")),
        ("codegen-units", toml::Value::from(1)),
        ("strip", toml::Value::from(true)),
    ];
    for (key, value) in expected {
        assert_eq!(release.get(key), Some(&value), "[profile.release] {key}");
    }
    Ok(())
}

#[test]
fn cargo_deny_checks_every_release_target() -> Check {
    let deny: Table = read("deny.toml")?
        .parse()
        .map_err(|e| format!("parse deny.toml: {e}"))?;
    let targets: Vec<&str> = deny["graph"]["targets"]
        .as_array()
        .map(|list| list.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    for target in RELEASE_TARGETS {
        assert!(targets.contains(&target), "deny.toml targets lack {target}");
    }
    Ok(())
}

fn workflow_files() -> Result<Vec<String>, String> {
    let dir = format!("{}/.github/workflows", env!("CARGO_MANIFEST_DIR"));
    let entries = fs::read_dir(&dir).map_err(|e| format!("read {dir}: {e}"))?;
    Ok(entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yml"))
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .map(|name| format!(".github/workflows/{name}"))
        .collect())
}

/// A `uses:` reference is pinned when it's local or names a full 40-hex commit.
fn is_pinned(reference: &str) -> bool {
    reference.starts_with("./")
        || reference
            .rsplit_once('@')
            .is_some_and(|(_, rev)| rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()))
}

#[test]
fn every_action_is_pinned_to_a_commit() -> Check {
    for file in workflow_files()? {
        let unpinned: Vec<String> = read(&file)?
            .lines()
            .filter_map(|line| line.trim().trim_start_matches("- ").strip_prefix("uses: "))
            .map(|rest| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|reference| !is_pinned(reference))
            .collect();
        assert!(
            unpinned.is_empty(),
            "{file} has unpinned actions: {unpinned:?}"
        );
    }
    Ok(())
}

#[test]
fn dependabot_keeps_actions_current() -> Check {
    let config = read(".github/dependabot.yml")?;
    for needle in ["package-ecosystem: github-actions", "interval: weekly"] {
        assert!(
            config.contains(needle),
            "dependabot.yml is missing {needle:?}"
        );
    }
    Ok(())
}

const RELEASE_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

#[test]
fn release_builds_every_target_after_the_gates() -> Check {
    let release = read(".github/workflows/release.yml")?;
    let needles = [
        "- 'v[0-9]+.[0-9]+.[0-9]+'",
        "workflow_dispatch:",
        "uses: ./.github/workflows/ci.yml",
        "cargo build --release --locked --target",
        "--version",
        "lipo -archs",
        ".github/scripts/package.sh",
        "actions/upload-artifact@",
    ];
    for needle in needles.into_iter().chain(RELEASE_TARGETS) {
        assert!(
            release.contains(needle),
            "release.yml is missing {needle:?}"
        );
    }
    Ok(())
}

#[test]
fn release_publishes_attested_archives_only_for_tags() -> Check {
    let release = read(".github/workflows/release.yml")?;
    for needle in [
        "if: github.event_name == 'push'",
        "contents: write",
        "id-token: write",
        "attestations: write",
        "actions/download-artifact@",
        "actions/attest-build-provenance@",
        "SHA256SUMS",
        "gh release create",
        "--verify-tag",
    ] {
        assert!(
            release.contains(needle),
            "release.yml is missing {needle:?}"
        );
    }
    Ok(())
}

#[test]
fn release_checks_the_formula_and_pushes_it_to_the_tap() -> Check {
    let release = read(".github/workflows/release.yml")?;
    for needle in [
        ".github/scripts/render-formula.sh",
        "brew style",
        "brew audit --strict",
        "repository: DwieDave/homebrew-tap",
        "secrets.HOMEBREW_TAP_TOKEN",
        "git push",
    ] {
        assert!(
            release.contains(needle),
            "release.yml is missing {needle:?}"
        );
    }
    Ok(())
}

#[test]
fn readme_explains_installing() -> Check {
    let readme = read("README.md")?;
    for needle in [
        "brew trust --tap DwieDave/tap",
        "brew install DwieDave/tap/dv",
    ] {
        assert!(readme.contains(needle), "README.md is missing {needle:?}");
    }
    Ok(())
}

/// Runner images we've moved past, plus floating labels that change under us.
const STALE_RUNNERS: [&str; 4] = ["macos-15", "ubuntu-24.04", "macos-latest", "ubuntu-latest"];

#[test]
fn workflows_use_current_pinned_runner_images() -> Check {
    for file in workflow_files()? {
        let text = read(&file)?;
        let stale: Vec<&str> = STALE_RUNNERS
            .into_iter()
            .filter(|label| text.contains(label))
            .collect();
        assert!(stale.is_empty(), "{file} uses stale runners: {stale:?}");
    }
    Ok(())
}
