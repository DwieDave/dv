use std::borrow::Cow;
use std::fmt::Write;

use proptest::prelude::*;
use saphyr::{LoadableYamlNode, Mapping, Scalar, Yaml, YamlEmitter};
use serde_json::{Value, json};

use super::*;
use crate::test_support::json_value;

fn go(yaml: &str) -> Result<Transcoded, TranscodeError> {
    transcode(yaml, budget(yaml.len()), |_| ControlFlow::Continue(()))
}

fn json_of(yaml: &str) -> Value {
    serde_json::from_slice(&go(yaml).unwrap().json).unwrap()
}

fn to_yaml(value: &Value) -> Yaml<'static> {
    match value {
        Value::Null => Yaml::Value(Scalar::Null),
        Value::Bool(b) => Yaml::Value(Scalar::Boolean(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Yaml::Value(Scalar::Integer(i)),
            None => Yaml::Value(Scalar::FloatingPoint(n.as_f64().unwrap().into())),
        },
        Value::String(s) => Yaml::Value(Scalar::String(Cow::Owned(s.clone()))),
        Value::Array(items) => Yaml::Sequence(items.iter().map(to_yaml).collect()),
        Value::Object(map) => {
            let mut mapping = Mapping::new();
            for (k, v) in map {
                mapping.insert(
                    Yaml::Value(Scalar::String(Cow::Owned(k.clone()))),
                    to_yaml(v),
                );
            }
            Yaml::Mapping(mapping)
        }
    }
}

/// The oracle: saphyr's own loader, mapped to JSON with the transcoder's conventions.
fn oracle(node: &Yaml) -> Value {
    match node {
        Yaml::Value(Scalar::Null) => Value::Null,
        Yaml::Value(Scalar::Boolean(b)) => json!(b),
        Yaml::Value(Scalar::Integer(i)) => json!(i),
        Yaml::Value(Scalar::FloatingPoint(f)) => json!(f.into_inner()),
        Yaml::Value(Scalar::String(s)) => json!(s),
        Yaml::Sequence(items) => items.iter().map(oracle).collect(),
        Yaml::Mapping(map) => map
            .iter()
            .map(|(k, v)| (k.as_str().unwrap().to_owned(), oracle(v)))
            .collect(),
        other => panic!("unexpected node {other:?}"),
    }
}

fn emit(value: &Value) -> String {
    let mut text = String::new();
    YamlEmitter::new(&mut text).dump(&to_yaml(value)).unwrap();
    text
}

proptest! {
    #[test]
    fn char_cursor_matches_char_indices(text in "[a-zé☃ \n]{0,40}", picks in proptest::collection::vec(0usize..45, 0..10)) {
        let mut cursor = CharToByte::new(&text);
        for pick in picks {
            let expected = text.char_indices().nth(pick).map_or(text.len(), |(b, _)| b);
            prop_assert_eq!(cursor.offset(pick), expected);
            prop_assert_eq!(cursor.bytes <= text.len(), true);
        }
    }

    #[test]
    fn matches_saphyr_loader(value in json_value()) {
        let text = emit(&value);
        let docs = Yaml::load_from_str(&text).unwrap();
        prop_assert_eq!(json_of(&text), oracle(&docs[0]), "yaml:\n{}", text);
    }
}

#[test]
fn documents_become_a_root_array() {
    assert_eq!(
        json_of("a: 1\n---\nb: [x, 2]\n"),
        json!([{"a": 1}, {"b": ["x", 2]}])
    );
    assert_eq!(json_of(""), Value::Null);
    assert_eq!(json_of("just text"), json!("just text"));
}

#[test]
fn empty_nodes_follow_the_saphyr_loader() {
    for text in ["---\n", "a:\n", "- \n- x\n", "a: ~\nb: null\n"] {
        let docs = Yaml::load_from_str(text).unwrap();
        assert_eq!(json_of(text), oracle(&docs[0]), "{text:?}");
    }
}

#[test]
fn aliases_expand_and_are_marked() {
    let out = go("base: &b {x: 1}\nuse: *b\nlist: [&s hi, *s]\n").unwrap();
    let text = String::from_utf8(out.json.clone()).unwrap();
    assert_eq!(text, r#"{"base":{"x":1},"use":{"x":1},"list":["hi","hi"]}"#);
    let marked: Vec<char> = out
        .aliases
        .iter()
        .map(|&o| text.as_bytes()[o as usize] as char)
        .collect();
    assert_eq!((out.aliases.len(), marked), (2, vec!['{', '"']));
    assert_eq!(&text[out.aliases[0] as usize..][..7], r#"{"x":1}"#);
}

#[test]
fn aliases_in_later_documents_shift_with_the_root_array() {
    let out = go("a: &x 1\n---\nb: *x\n");
    assert!(
        matches!(out, Err(TranscodeError::BadAlias { .. })),
        "anchors do not cross documents: {out:?}"
    );
    let out = go("- 0\n---\n- &y [1]\n- *y\n").unwrap();
    let text = String::from_utf8(out.json).unwrap();
    assert_eq!(text, "[[0],[[1],[1]]]");
    assert_eq!(&text[out.aliases[0] as usize..], "[1]]]");
}

#[test]
fn billion_laughs_hits_the_budget() {
    let mut yaml = String::from("a: &a [\"lol\",\"lol\",\"lol\",\"lol\",\"lol\"]\n");
    for (i, prev) in ('b'..='j').zip('a'..) {
        writeln!(yaml, "{i}: &{i} [*{prev},*{prev},*{prev},*{prev},*{prev}]").unwrap();
    }
    assert!(matches!(go(&yaml), Err(TranscodeError::Budget { .. })));
}

#[test]
fn complex_keys_become_strings_and_special_floats_stay_text() {
    assert_eq!(
        json_of("? [1, 2]\n: v\n1: one\ntrue: yes\n"),
        json!({"[1,2]": "v", "1": "one", "true": "yes"})
    );
    assert_eq!(
        json_of("x: .inf\ny: -.Inf\nz: .nan\n"),
        json!({"x": ".inf", "y": "-.Inf", "z": ".nan"})
    );
    assert_eq!(
        json_of("q: \"123\"\np: 0x1F\n"),
        json!({"q": "123", "p": 31})
    );
}

#[test]
fn scan_errors_carry_byte_offsets() {
    let err = go("é: [1, 2\nb: 3\n").unwrap_err();
    let TranscodeError::Scan { line, offset, .. } = err else {
        panic!("expected scan error: {err:?}")
    };
    assert!(line >= 1);
    assert!(
        "é: [1, 2\nb: 3\n".is_char_boundary(offset) && offset > 1,
        "offset {offset} must count é as 2 bytes"
    );
}

#[test]
fn transcoding_is_linear_in_the_input() {
    let mut yaml = String::new();
    for i in 0..12_000 {
        writeln!(yaml, "- name: é{i}\n  v: {i}").unwrap();
    }
    let started = std::time::Instant::now();
    go(&yaml).unwrap();
    assert!(
        started.elapsed().as_secs_f64() < 2.0,
        "took {:?} for {} bytes",
        started.elapsed(),
        yaml.len()
    );
}

#[test]
fn nested_complex_keys_stop_at_the_budget() {
    // Each level quotes the key below it again, doubling the backslashes (found by fuzzing).
    let text = "? ".repeat(60) + "x\r";
    let result = transcode(&text, budget(text.len()), |_| ControlFlow::Continue(()));
    assert!(
        matches!(result, Err(TranscodeError::Budget { .. })),
        "{:?}",
        result.map(|t| t.json.len())
    );
}
