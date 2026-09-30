//! Shared test fixtures: arbitrary JSON values and a serializer that records layout.

use proptest::prelude::*;
use serde_json::{Map, Value};

use crate::json::lex::Kind;
use crate::tree::{Count, NodeRef, TreeIndex};

/// A child as laid out: `start` is the key (objects) or the value (arrays).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaidChild {
    pub start: usize,
    pub key: Option<String>,
    pub value: usize,
    pub end: usize,
}

/// A container as laid out by [`layout`].
pub struct Container {
    pub start: usize,
    pub end: usize,
    pub children: Vec<LaidChild>,
}

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
            children.push(self.member(key, value));
        }
        self.out.push_str(self.ws);
        self.out.push(close);
        let end = self.out.len();
        self.containers.push(Container {
            start,
            end,
            children,
        });
    }

    fn member(&mut self, key: Option<&String>, value: &Value) -> LaidChild {
        let start = self.out.len();
        if let Some(key) = key {
            self.out.push_str(&serde_json::to_string(key).unwrap());
            self.out.push_str(self.ws);
            self.out.push(':');
            self.out.push_str(self.ws);
        }
        let value_at = self.out.len();
        self.value(value);
        let key = key.cloned();
        LaidChild {
            start,
            key,
            value: value_at,
            end: self.out.len(),
        }
    }
}

/// Serializes `value` with `ws` around tokens, returning the text and every container.
pub fn layout(value: &Value, ws: &str) -> (String, Vec<Container>) {
    let mut w = Writer {
        out: String::new(),
        ws,
        containers: Vec::new(),
    };
    w.value(value);
    (w.out, w.containers)
}

/// Arbitrary JSON values with enough breadth to exercise checkpoints.
pub fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-1e9f64..1e9).prop_map(Value::from),
        text(12).prop_map(Value::from),
    ];
    leaf.prop_recursive(6, 400, 40, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..40).prop_map(Value::Array),
            proptest::collection::vec((text(6), inner), 0..20)
                .prop_map(|kv| Value::Object(kv.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

/// Strings mixing printable text with escape-heavy content (controls, quotes, backslashes).
fn text(max: usize) -> impl Strategy<Value = String> {
    prop_oneof![
        proptest::string::string_regex(&format!("\\PC{{0,{max}}}")).unwrap(),
        proptest::string::string_regex(&format!("[\\x00-\\x1f\"\\\\a-z]{{0,{max}}}")).unwrap(),
    ]
}

/// Rebuilds the value at `node` through the public `TreeIndex` API only.
pub fn to_value(tree: &impl TreeIndex, node: NodeRef) -> Value {
    let Count::Known(n) = tree.child_count(node).unwrap() else {
        panic!("pending count")
    };
    let kids = tree.children(node, 0..n).unwrap();
    match node.kind {
        Kind::Array => kids.iter().map(|c| to_value(tree, c.node())).collect(),
        Kind::Object => kids
            .iter()
            .map(|c| {
                let raw = tree.bytes(c.key.clone().unwrap()).unwrap();
                (
                    crate::json::text::unescape(&raw).into_owned(),
                    to_value(tree, c.node()),
                )
            })
            .collect(),
        _ => serde_json::from_slice(
            &tree
                .bytes(node.offset..tree.value_end(node).unwrap())
                .unwrap(),
        )
        .unwrap(),
    }
}

/// Records (some spread over lines), blank lines, then bytes inserted anywhere.
pub fn ndjson() -> impl Strategy<Value = Vec<u8>> {
    let record = (
        json_value(),
        prop::sample::select(vec!["", " ", " \n", "\t\r"]),
    );
    let cut = prop::option::of(any::<prop::sample::Index>());
    let records = prop::collection::vec(((record, any::<bool>()), cut), 0..6);
    let noise = prop::sample::select(b"\n\xff\xe2{}[]\",: 1e-tn\r".to_vec());
    let inserts = prop::collection::vec((any::<prop::sample::Index>(), noise), 0..4);
    (records, inserts, any::<bool>()).prop_map(|(records, inserts, final_newline)| {
        let lines: Vec<String> = records
            .iter()
            .map(|(((value, ws), blank), cut)| {
                let (mut text, _) = layout(value, ws);
                if let Some(at) = cut {
                    let end = (0..=at.index(text.len()))
                        .rev()
                        .find(|&i| text.is_char_boundary(i));
                    text.truncate(end.unwrap_or(0));
                }
                if *blank { format!("{text}\n") } else { text }
            })
            .collect();
        let mut bytes = lines.join("\n").into_bytes();
        if final_newline {
            bytes.push(b'\n');
        }
        for (at, byte) in inserts {
            bytes.insert(at.index(bytes.len() + 1), byte);
        }
        bytes
    })
}

/// Any tree, answering as if it were read in streaming mode.
pub struct Streamed<T>(pub T);

impl<T: TreeIndex> TreeIndex for Streamed<T> {
    fn root(&self) -> Result<NodeRef, crate::index::IndexError> {
        self.0.root()
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, crate::index::IndexError> {
        self.0.child_count(node)
    }

    fn children(
        &self,
        node: NodeRef,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<crate::index::children::Child>, crate::index::IndexError> {
        self.0.children(node, range)
    }

    fn child_containing(
        &self,
        node: NodeRef,
        offset: u64,
    ) -> Result<Option<crate::index::children::Child>, crate::index::IndexError> {
        self.0.child_containing(node, offset)
    }

    fn bytes(
        &self,
        range: std::ops::Range<u64>,
    ) -> Result<std::borrow::Cow<'_, [u8]>, crate::index::IndexError> {
        self.0.bytes(range)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, crate::index::IndexError> {
        self.0.value_end(node)
    }

    fn stats(&self) -> crate::tree::Stats {
        self.0.stats()
    }

    fn format(&self) -> crate::format::Format {
        self.0.format()
    }

    fn streamed(&self) -> bool {
        true
    }
}
