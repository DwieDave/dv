use proptest::prelude::*;
use serde_json::Value;

use std::ops::ControlFlow;

use super::*;
use crate::index::store::{CHECKPOINT_EVERY, MIN_NODE_LEN, NodeStore};
use crate::test_support::{Container, json_value, layout};

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
    many.then(|| {
        c.children
            .iter()
            .step_by(16)
            .map(|k| k.start as u64)
            .collect()
    })
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

fn big_doc() -> String {
    let items: Vec<String> = (0..1_500_000).map(|i| i.to_string()).collect();
    format!("[{}]", items.join(","))
}

#[test]
fn progress_is_monotonic_and_reaches_the_end() {
    let text = big_doc();
    let mut seen = Vec::new();
    parse_with(text.as_bytes(), |at| {
        seen.push(at);
        ControlFlow::Continue(())
    })
    .unwrap();
    assert!(seen.len() >= 2, "{seen:?}");
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(seen.last(), Some(&(text.len() as u64)));
}

#[test]
fn breaking_the_hook_cancels() {
    let text = big_doc();
    let err = parse_with(text.as_bytes(), |_| ControlFlow::Break(())).unwrap_err();
    assert_eq!(err.kind, ParseErrorKind::Cancelled);
}
