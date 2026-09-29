//! End-to-end checks of the `dv` binary.

use std::io::{self, Write};
use std::process::{Command, Output, Stdio};

/// `dv` with no user config: the config directory does not exist.
fn dv_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dv"));
    command.env("XDG_CONFIG_HOME", "/nonexistent/dv-tests");
    command
}

fn dv(args: &[&str]) -> io::Result<Output> {
    dv_command().args(args).output()
}

fn temp_file(contents: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(contents)?;
    Ok(file)
}

#[test]
fn index_only_reports_indexed_bytes() {
    let file = temp_file(br#"{"a": [1, 2, 3]}"#).unwrap();
    let out = dv(&["--index-only", file.path().to_str().unwrap()]).unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "indexed 16 bytes"
    );
}

#[test]
fn index_only_reports_parse_errors_with_position() {
    let file = temp_file(b"{\n  \"a\": [1,\n}").unwrap();
    let out = dv(&["--index-only", file.path().to_str().unwrap()]).unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("3:1"),
        "{out:?}"
    );
}

#[test]
fn help_and_version_succeed() {
    assert!(dv(&["--help"]).unwrap().status.success());
    let out = dv(&["--version"]).unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("dv "));
}

#[test]
fn missing_files_fail_naming_the_path() {
    let out = dv(&["--index-only", "/no/such/file.json"]).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("dv: /no/such/file.json:"),
        "{out:?}"
    );
}

#[test]
fn streaming_mode_indexes_files() {
    let file = temp_file(br#"{"a": [1, 2, 3], "b": {"c": null}}"#).unwrap();
    let out = dv(&[
        "--index-only",
        "--mode",
        "stream",
        file.path().to_str().unwrap(),
    ])
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "indexed 34 bytes"
    );
}

#[test]
fn streaming_mode_indexes_ndjson_with_bad_lines() {
    let file = temp_file(b"{\"a\":1}\n{bad\n[2]\n").unwrap();
    let path = file.path().with_extension("ndjson");
    std::fs::copy(file.path(), &path).unwrap();
    let out = dv(&["--index-only", "--mode", "stream", path.to_str().unwrap()]).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "indexed 17 bytes"
    );
}

#[test]
fn index_only_handles_ndjson_with_bad_lines() {
    let file = temp_file(b"{\"a\":1}\n{bad\n[2]\n").unwrap();
    let path = file.path().with_extension("ndjson");
    std::fs::copy(file.path(), &path).unwrap();
    let out = dv(&["--index-only", path.to_str().unwrap()]).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(out.status.success(), "{out:?}");
}

fn dv_stdin(args: &[&str], input: &[u8]) -> io::Result<Output> {
    let mut child = dv_command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no stdin"))?
        .write_all(input)?;
    child.wait_with_output()
}

#[test]
fn stdin_is_read_with_dash_or_no_path() {
    for args in [&["--index-only", "-"][..], &["--index-only"][..]] {
        let out = dv_stdin(args, b"{\"a\": [1, 2]}").unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "indexed 13 bytes"
        );
    }
}

#[test]
fn piped_input_can_be_streamed() {
    let out = dv_stdin(&["--index-only", "--mode", "stream"], b"{\"a\": [1, 2]}").unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "indexed 13 bytes"
    );
    let lines = dv_stdin(&["--index-only", "--mode", "stream"], b"{\"a\":1}\n{bad\n").unwrap();
    assert!(lines.status.success(), "{lines:?}");
}

#[test]
fn streaming_never_leaves_names_in_the_temp_dir() {
    let dir = tempfile::tempdir().unwrap();
    let file = temp_file(br#"{"a": [1, 2, 3], "b": {"c": null}}"#).unwrap();
    let mut child = dv_command()
        .args(["--index-only", "--mode", "stream"])
        .env("TMPDIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"[1, 2, ").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let during = std::fs::read_dir(dir.path()).unwrap().count();
    drop(stdin);
    child.kill().unwrap();
    child.wait().unwrap();
    let out = dv_command()
        .args([
            "--index-only",
            "--mode",
            "stream",
            file.path().to_str().unwrap(),
        ])
        .env("TMPDIR", dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let after = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!((during, after), (0, 0));
}

#[test]
fn piped_ndjson_is_detected_from_content() {
    let out = dv_stdin(&["--index-only"], b"{\"a\":1}\n{bad\n{\"a\":2}\n").unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn the_config_threshold_decides_the_mode_and_bad_configs_only_warn() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let yaml = dir.path().join("doc.yaml");
    std::fs::write(&yaml, "a: 1\nb: [1, 2, 3]\n").unwrap();
    std::fs::write(&config, "[mode]\nthreshold = \"10B\"\n").unwrap();
    let args = |config: &std::path::Path| {
        let (c, y) = (
            config.to_str().unwrap().to_owned(),
            yaml.to_str().unwrap().to_owned(),
        );
        vec!["--index-only".to_owned(), "--config".to_owned(), c, y]
    };
    let small = dv(&args(&config).iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    assert!(
        String::from_utf8_lossy(&small.stderr).contains("cannot be streamed"),
        "{small:?}"
    );
    std::fs::write(&config, "theme = \"missing\"\n").unwrap();
    let bad = dv(&args(&config).iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    assert!(bad.status.success(), "{bad:?}");
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("config: unknown theme"),
        "{bad:?}"
    );
}

#[test]
fn yaml_cannot_be_streamed() {
    let file = temp_file(b"a: 1\n").unwrap();
    let path = file.path().with_extension("yaml");
    std::fs::copy(file.path(), &path).unwrap();
    let out = dv(&["--mode", "stream", path.to_str().unwrap()]).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be streamed"),
        "{out:?}"
    );
}

#[test]
fn follow_needs_an_ndjson_file() {
    let json = temp_file(br#"{"a": 1}"#).unwrap();
    let out = dv(&["--follow", json.path().to_str().unwrap()]).unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--follow needs an NDJSON file"), "{stderr}");
    let out = dv_stdin(&["--follow"], b"1\n2\n").unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--follow needs an NDJSON file"), "{stderr}");
}
