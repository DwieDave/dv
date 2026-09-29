//! End-to-end checks of the `dv` binary.

use std::io::{self, Write};
use std::process::{Command, Output};

fn dv(args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_dv")).args(args).output()
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
fn streaming_mode_is_reported_unsupported() {
    let file = temp_file(b"[]").unwrap();
    let out = dv(&["--mode", "stream", file.path().to_str().unwrap()]).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("streaming mode is not supported yet"),
        "{out:?}"
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
