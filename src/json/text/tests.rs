use proptest::prelude::*;

use super::*;

const BS: char = '\\';

proptest! {
    #[test]
    fn quote_into_round_trips_through_serde(s in any::<String>()) {
        let mut out = b"x".to_vec();
        quote_into(&mut out, &s);
        prop_assert_eq!(serde_json::from_slice::<String>(&out[1..]).unwrap(), s);
    }

    #[test]
    fn unescape_inverts_serialization(s in any::<String>()) {
        let raw = serde_json::to_string(&s).unwrap();
        prop_assert_eq!(unescape(raw.as_bytes()), s);
    }

    #[test]
    fn unescape_agrees_with_serde(body in r#"([a-z ]|\\[\\"/bfnrt]|\\u00[0-7][0-9a-f]|\\ud83d\\ude00)*"#) {
        let raw = format!("\"{body}\"");
        let expected: String = serde_json::from_str(&raw).unwrap();
        prop_assert_eq!(unescape(raw.as_bytes()), expected);
    }

    #[test]
    fn inline_is_single_line_and_bounded(s in any::<String>(), max in 1usize..40) {
        let raw = serde_json::to_string(&s).unwrap();
        let shown = inline(raw.as_bytes(), max);
        prop_assert!(shown.chars().count() <= max);
        prop_assert!(!shown.contains(['\n', '\r']));
    }
}

#[test]
fn inline_decodes_strings_and_keeps_numbers_raw() {
    let raw = format!("\"caf{BS}u00e9{BS}n\"");
    assert_eq!(inline(raw.as_bytes(), 20), format!("café{BS}n"));
    assert_eq!(inline(b"1.50e3", 20), "1.50e3");
    assert_eq!(inline(b"\"abcdef\"", 4), "abc…");
}

#[test]
fn unescape_borrows_without_escapes() {
    assert!(matches!(unescape(b"\"plain\""), Cow::Borrowed("plain")));
}
