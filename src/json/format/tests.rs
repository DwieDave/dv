use proptest::prelude::*;

use super::*;
use crate::test_support::{json_value, layout};

fn run(style: Style, chunks: &[&[u8]]) -> String {
    let mut formatter = Formatter::new(style);
    let mut out = Vec::new();
    for chunk in chunks {
        formatter.feed(chunk, &mut out);
    }
    String::from_utf8(out).unwrap()
}

proptest! {
    #[test]
    fn minify_matches_serde(value in json_value()) {
        let (text, _) = layout(&value, " \n ");
        prop_assert_eq!(run(Style::Minify, &[text.as_bytes()]), serde_json::to_string(&value).unwrap());
    }

    #[test]
    fn pretty_matches_serde(value in json_value()) {
        let (text, _) = layout(&value, "");
        prop_assert_eq!(run(Style::Pretty, &[text.as_bytes()]), serde_json::to_string_pretty(&value).unwrap());
    }

    #[test]
    fn chunking_does_not_change_output(value in json_value(), at in any::<prop::sample::Index>()) {
        let (text, _) = layout(&value, " ");
        let bytes = text.as_bytes();
        let (a, b) = bytes.split_at(at.index(bytes.len() + 1));
        prop_assert_eq!(run(Style::Pretty, &[a, b]), run(Style::Pretty, &[bytes]));
    }
}
