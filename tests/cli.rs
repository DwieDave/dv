//! End-to-end checks of the `dv` binary.

use std::io::Write;
use std::process::Command;

fn dv(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dv"))
        .args(args)
        .output()
        .unwrap()
}

fn temp_file(contents: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(contents).unwrap();
    file
}

#[test]
fn index_only_reports_indexed_bytes() {
    let file = temp_file(br#"{"a": [1, 2, 3]}"#);
    let out = dv(&["--index-only", file.path().to_str().unwrap()]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "indexed 16 bytes"
    );
}

#[test]
fn index_only_reports_parse_errors_with_position() {
    let file = temp_file(b"{\n  \"a\": [1,\n}");
    let out = dv(&["--index-only", file.path().to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("3:1"),
        "{out:?}"
    );
}
