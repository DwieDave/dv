use proptest::prelude::*;
use serde_json::Value;

use super::*;

const BS: char = '\\';

fn serde_accepts(text: &str, is_kind: fn(&Value) -> bool) -> Option<bool> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Some(is_kind(&v)),
        Err(e) if e.to_string().contains("out of range") => None,
        Err(_) => Some(false),
    }
}

fn scans_whole(scan: impl Fn(&[u8], usize) -> Result<usize, ParseError>, text: &str) -> bool {
    scan(text.as_bytes(), 0) == Ok(text.len())
}

/// Any UTF-16 code unit, biased toward the surrogate ranges.
fn code_unit() -> impl Strategy<Value = u16> {
    prop_oneof![any::<u16>(), 0xD700u16..0xE100]
}

proptest! {
    #[test]
    fn serialized_strings_scan_fully(s in any::<String>()) {
        let text = serde_json::to_string(&s).unwrap();
        prop_assert!(scans_whole(scan_string, &text));
    }

    #[test]
    fn strings_agree_with_serde(body in r#"([a-z"\\/ ]|\\[\\"/bfnrtux]|\\u[0-9a-fA-FdD]{0,4}|[\x00-\x1f])*"#) {
        let text = format!("\"{body}\"");
        prop_assert_eq!(scans_whole(scan_string, &text), serde_accepts(&text, Value::is_string).unwrap());
    }

    #[test]
    fn unicode_escape_pairs_agree_with_serde(hi in code_unit(), lo in code_unit()) {
        let text = format!("\"{BS}u{hi:04x}{BS}u{lo:04x}\"");
        prop_assert_eq!(scans_whole(scan_string, &text), serde_accepts(&text, Value::is_string).unwrap());
    }

    #[test]
    fn numbers_agree_with_serde(text in "[-+0-9.eE]{1,12}") {
        if let Some(expected) = serde_accepts(&text, Value::is_number) {
            prop_assert_eq!(scans_whole(scan_number, &text), expected, "{}", text);
        }
    }

    #[test]
    fn literals_agree_with_serde(text in "(true|false|null|[truefalsn]{1,6})") {
        let is_literal = |v: &Value| v.is_boolean() || v.is_null();
        let lit: &[u8] = match text.as_bytes()[0] { b't' => b"true", b'f' => b"false", _ => b"null" };
        let scanned = scan_literal(text.as_bytes(), 0, lit) == Ok(text.len());
        prop_assert_eq!(scanned, serde_accepts(&text, is_literal).unwrap());
    }

    #[test]
    fn skip_ws_stops_at_first_non_ws(ws in "[ \t\r\n]{0,8}", rest in "[a-z]{0,3}") {
        let text = format!("{ws}{rest}");
        prop_assert_eq!(skip_ws(text.as_bytes(), 0), ws.len());
    }
}

#[test]
fn scalar_dispatch_reports_kind_and_end() {
    let cases: [(&str, Kind, usize); 6] = [
        ("null,", Kind::Null, 4),
        ("true]", Kind::Bool, 4),
        ("-1.5e3}", Kind::Number, 6),
        ("\"a\\\"b\":", Kind::String, 6),
        ("{", Kind::Object, 0),
        ("[", Kind::Array, 0),
    ];
    for (text, kind, end) in cases {
        assert_eq!(scan_scalar(text.as_bytes(), 0), Ok((kind, end)), "{text}");
    }
}

#[test]
fn unterminated_string_reports_eof() {
    let err = scan_string(b"\"abc", 0).unwrap_err();
    assert_eq!(err.kind, ParseErrorKind::UnexpectedEof);
}

#[test]
fn truncated_unicode_escapes_need_more_input() {
    for cut in [
        format!("\"{BS}u00"),
        format!("\"{BS}ud83d"),
        format!("\"{BS}ud83d{BS}ude"),
    ] {
        let err = scan_string(cut.as_bytes(), 0).unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::UnexpectedEof, "{cut}");
    }
}

#[test]
fn truncated_numbers_need_more_input() {
    for cut in ["-", "1.", "1e", "1e+", "-0."] {
        assert_eq!(
            scan_number(cut.as_bytes(), 0).unwrap_err().kind,
            ParseErrorKind::UnexpectedEof,
            "{cut}"
        );
    }
    assert_eq!(
        scan_number(b"-x", 0).unwrap_err().kind,
        ParseErrorKind::InvalidNumber
    );
}

/// The string scanner as a plain byte loop: the reference for the word-at-a-time one.
fn scan_string_bytewise(bytes: &[u8], pos: usize) -> Result<usize, ParseError> {
    let mut i = pos + 1;
    loop {
        match bytes.get(i) {
            None => return Err(fail(ParseErrorKind::UnexpectedEof, bytes.len())),
            Some(b'"') => return Ok(i + 1),
            Some(b'\\') => i = scan_escape(bytes, i)?,
            Some(&b) if b < 0x20 => return Err(fail(ParseErrorKind::ControlInString, i)),
            Some(_) => i += 1,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]
    #[test]
    fn strings_scan_like_a_byte_loop(
        body in prop::collection::vec(prop::sample::select(b"ab\"\\\x01\x1f \x7f\xff\xe2un0/".to_vec()), 0..48),
        prefix in 0usize..9,
    ) {
        let mut bytes = vec![b' '; prefix];
        bytes.push(b'"');
        bytes.extend(body);
        prop_assert_eq!(scan_string(&bytes, prefix), scan_string_bytewise(&bytes, prefix));
    }
}
