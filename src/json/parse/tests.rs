use proptest::prelude::*;
use serde_json::{Map, Value};

use super::*;
use crate::index::store::{CHECKPOINT_EVERY, MIN_NODE_LEN, NodeStore};

/// A container as laid out by the test serializer.
struct Container {
    start: usize,
    end: usize,
    children: Vec<usize>,
}

/// Serializes `value` with `ws` around tokens, recording every container's layout.
struct Writer<'a> {
    out: String,
    ws: &'a str,
    containers: Vec<Container>,
}

impl Writer<'_> {
    fn value(&mut self, value: &Value) {
        match value {
            Value::Array(items) => self.container('[', ']', items.iter().map(|v| (None, v))),
            Value::Object(map) => self.container('{', '}', map.iter().map(|(k, v)| (Some(k), v))),
            scalar => self.out.push_str(&scalar.to_string()),
        }
    }

    fn container<'v>(
        &mut self,
        open: char,
        close: char,
        kids: impl Iterator<Item = (Option<&'v String>, &'v Value)>,
    ) {
        let start = self.out.len();
        self.out.push(open);
        let mut children = Vec::new();
        for (i, (key, value)) in kids.enumerate() {
            self.out.push_str(if i > 0 { "," } else { "" });
            self.out.push_str(self.ws);
            children.push(self.out.len());
            self.member(key, value);
        }
        self.out.push_str(self.ws);
        self.out.push(close);
        self.containers.push(Container {
            start,
            end: self.out.len(),
            children,
        });
    }

    fn member(&mut self, key: Option<&String>, value: &Value) {
        if let Some(key) = key {
            self.out.push_str(&serde_json::to_string(key).unwrap());
            self.out.push_str(self.ws);
            self.out.push(':');
            self.out.push_str(self.ws);
        }
        self.value(value);
    }
}

fn layout(value: &Value, ws: &str) -> (String, Vec<Container>) {
    let mut w = Writer {
        out: String::new(),
        ws,
        containers: Vec::new(),
    };
    w.value(value);
    (w.out, w.containers)
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-1e9f64..1e9).prop_map(Value::from),
        "\\PC{0,12}".prop_map(Value::from),
    ];
    leaf.prop_recursive(6, 400, 40, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..40).prop_map(Value::Array),
            proptest::collection::vec(("\\PC{0,6}", inner), 0..20)
                .prop_map(|kv| Value::Object(kv.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

fn serde_verdict(text: &[u8]) -> Option<bool> {
    match serde_json::from_slice::<Value>(text) {
        Ok(_) => Some(true),
        Err(e) if e.to_string().contains("out of range") || e.to_string().contains("recursion") => {
            None
        }
        Err(_) => Some(false),
    }
}

fn expected_checkpoints(c: &Container) -> Option<Vec<u64>> {
    let many = c.children.len() as u64 > CHECKPOINT_EVERY;
    many.then(|| c.children.iter().step_by(16).map(|&o| o as u64).collect())
}

fn assert_indexed(store: &VecStore, c: &Container) {
    let node = store.node_at(c.start as u64).unwrap();
    if ((c.end - c.start) as u64) < MIN_NODE_LEN {
        assert_eq!(node, None, "small container at {}", c.start);
        return;
    }
    let node = node.unwrap();
    assert_eq!(node.end, c.end as u64);
    let cps = node.fanout.map(|f| {
        (0..f.checkpoints())
            .map(|k| store.checkpoint(&f, k).unwrap().unwrap())
            .collect()
    });
    assert_eq!(cps, expected_checkpoints(c), "checkpoints at {}", c.start);
}

proptest! {
    #[test]
    fn serialized_values_are_accepted(value in json_value(), padded in any::<bool>()) {
        let (text, _) = layout(&value, if padded { " \n" } else { "" });
        prop_assert!(parse(text.as_bytes()).is_ok());
    }

    #[test]
    fn mutations_agree_with_serde(value in json_value(), at in any::<prop::sample::Index>(), byte in prop::sample::select(b"{}[],:\"\\ 0-e.tn".to_vec())) {
        let (text, _) = layout(&value, "");
        let mut bytes = text.into_bytes();
        let i = at.index(bytes.len() + 1);
        if i < bytes.len() && byte == b' ' { bytes.remove(i); } else { bytes.insert(i, byte); }
        if let Some(expected) = serde_verdict(&bytes) {
            prop_assert_eq!(parse(&bytes).is_ok(), expected);
        }
    }

    #[test]
    fn spans_and_checkpoints_match_layout(value in json_value(), padded in any::<bool>()) {
        let (text, containers) = layout(&value, if padded { " \n" } else { "" });
        let parsed = parse(text.as_bytes()).unwrap();
        prop_assert_eq!(parsed.root, 0);
        for c in &containers {
            assert_indexed(&parsed.store, c);
        }
    }
}

#[test]
fn deep_nesting_does_not_overflow() {
    let depth = 200_000;
    let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert!(parse(text.as_bytes()).is_ok());
}

#[test]
fn errors_carry_kind_and_offset() {
    let cases: [(&[u8], ParseErrorKind, u64); 4] = [
        (b"[1] 2", ParseErrorKind::TrailingData, 4),
        (b"[1,", ParseErrorKind::UnexpectedEof, 3),
        (b"  ", ParseErrorKind::UnexpectedEof, 2),
        (b"[\"\xff\"]", ParseErrorKind::InvalidUtf8, 2),
    ];
    for (text, kind, offset) in cases {
        assert_eq!(
            parse(text).unwrap_err(),
            ParseError { kind, offset },
            "{text:?}"
        );
    }
}

#[test]
fn root_offset_skips_leading_whitespace() {
    assert_eq!(parse(b"  \n [1]").unwrap().root, 4);
}

#[test]
fn lengths_beyond_u32_are_too_large() {
    let too_big = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
    assert_eq!(
        ensure_addressable(too_big).unwrap_err().kind,
        ParseErrorKind::TooLarge
    );
    assert!(ensure_addressable(too_big - 1).is_ok());
}
