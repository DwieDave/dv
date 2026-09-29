use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;
use crate::source::MemSource;
use crate::tree::{MemTree, TreeIndex};

fn key() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec!["a", "b", "id", "and", "first name", "q\"t", "é"])
            .prop_map(str::to_owned),
        "[a-z_][a-z0-9_]{0,4}",
    ]
}

fn path() -> impl Strategy<Value = Path> {
    let step = prop_oneof![key().prop_map(Step::Key), (0u64..5).prop_map(Step::Index)];
    prop::collection::vec(step, 0..4).prop_map(Path)
}

fn literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        (-1e6f64..1e6).prop_map(Literal::Number),
        (-100i32..100).prop_map(|n| Literal::Number(f64::from(n))),
        "\\PC{0,6}".prop_map(Literal::String),
        any::<bool>().prop_map(Literal::Bool),
        Just(Literal::Null),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop::sample::select(vec![Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge])
}

fn expr() -> impl Strategy<Value = Expr> {
    let atom = prop_oneof![
        path().prop_map(Expr::Has),
        (path(), op(), literal()).prop_map(|(p, o, l)| Expr::Compare(p, o, l)),
        (path(), prop::sample::select(vec!["^a", "b+", "x|y", "é"]))
            .prop_map(|(p, re)| Expr::Matches(p, Pattern::new(re).unwrap())),
    ];
    atom.prop_recursive(4, 32, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(|e| Expr::Not(Box::new(e))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::And(Box::new(a), Box::new(b))),
            (inner.clone(), inner).prop_map(|(a, b)| Expr::Or(Box::new(a), Box::new(b))),
        ]
    })
}

proptest! {
    #[test]
    fn printing_then_parsing_gives_the_expression_back(expr in expr()) {
        let text = expr.to_string();
        prop_assert_eq!(parse(&text), Ok(expr), "{}", text);
    }

    #[test]
    fn every_parse_error_has_a_position_within_the_input(text in "[ .\\[\\]()a-z0-9=!<>~\"-]{0,24}") {
        if let Err(err) = parse(&text) {
            prop_assert!(err.pos <= text.len(), "{err:?} for {text:?}");
        }
    }
}

#[test]
fn errors_name_the_column() {
    let err = parse(".a == ").unwrap_err();
    assert_eq!(err.to_string(), "expected a value at 7");
    assert_eq!(parse(".a = 1").unwrap_err().pos, 3);
    assert!(
        parse(".a ~ \"(\"")
            .unwrap_err()
            .to_string()
            .contains("regex")
    );
    assert!(parse(".a == 1 )").is_err());
    assert!(parse("").is_err());
}

#[test]
fn precedence_and_grouping() {
    let e = parse("has(.a) or has(.b) and not has(.c)").unwrap();
    assert!(matches!(e, Expr::Or(_, _)));
    let e = parse("(has(.a) or has(.b)) and has(.c)").unwrap();
    assert!(matches!(e, Expr::And(_, _)));
    assert_eq!(
        parse(r#"."first name"[2] == "x""#).unwrap().to_string(),
        r#"."first name"[2] == "x""#
    );
    assert_eq!(parse(".[0] > -1.5").unwrap().to_string(), ".[0] > -1.5");
}

// --- evaluation against a reference over serde_json values ---

fn lookup<'v>(value: &'v Value, path: &Path) -> Option<&'v Value> {
    path.0.iter().try_fold(value, |v, step| match (step, v) {
        (Step::Key(k), Value::Object(map)) => map.get(k),
        (Step::Index(i), Value::Array(items)) => items.get(usize::try_from(*i).ok()?),
        _ => None,
    })
}

fn reference(value: &Value, expr: &Expr) -> bool {
    use std::cmp::Ordering;
    match expr {
        Expr::Or(a, b) => reference(value, a) || reference(value, b),
        Expr::And(a, b) => reference(value, a) && reference(value, b),
        Expr::Not(e) => !reference(value, e),
        Expr::Has(p) => lookup(value, p).is_some(),
        Expr::Matches(p, pattern) => {
            matches!(lookup(value, p), Some(Value::String(s)) if pattern.regex().is_match(s))
        }
        Expr::Compare(p, op, lit) => {
            let order = match (lookup(value, p), lit) {
                (Some(Value::Number(n)), Literal::Number(l)) => n.as_f64().unwrap().partial_cmp(l),
                (Some(Value::String(s)), Literal::String(l)) => Some(s.as_str().cmp(l)),
                (Some(Value::Bool(b)), Literal::Bool(l)) if matches!(op, Op::Eq | Op::Ne) => {
                    Some(b.cmp(l))
                }
                (Some(Value::Null), Literal::Null) if matches!(op, Op::Eq | Op::Ne) => {
                    Some(Ordering::Equal)
                }
                _ => None,
            };
            order.is_some_and(|o| match op {
                Op::Eq => o.is_eq(),
                Op::Ne => o.is_ne(),
                Op::Lt => o.is_lt(),
                Op::Le => o.is_le(),
                Op::Gt => o.is_gt(),
                Op::Ge => o.is_ge(),
            })
        }
    }
}

fn record() -> impl Strategy<Value = Value> {
    let scalar = prop_oneof![
        (-3i32..3).prop_map(|n| json!(n)),
        prop::sample::select(vec!["", "a", "ab", "by", "é"]).prop_map(|s| json!(s)),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
    ];
    let inner = prop_oneof![
        scalar.clone(),
        prop::collection::vec(scalar.clone(), 0..3).prop_map(Value::Array),
    ];
    prop::collection::vec((key(), inner), 0..4)
        .prop_map(|kv| Value::Object(kv.into_iter().collect()))
}

proptest! {
    #[test]
    fn evaluation_agrees_with_the_reference(records in prop::collection::vec(record(), 1..6), expr in expr()) {
        let text = serde_json::to_string(&Value::Array(records.clone())).unwrap();
        let tree = MemTree::parse(MemSource::new(text.into_bytes())).unwrap();
        let children = tree.children(tree.root().unwrap(), 0..records.len() as u64).unwrap();
        for (value, child) in records.iter().zip(&children) {
            prop_assert_eq!(matches(&tree, child, &expr).unwrap(), reference(value, &expr), "{} on {}", expr, value);
        }
    }
}

#[test]
fn comparisons_are_type_strict_and_missing_paths_never_compare() {
    let tree = MemTree::parse(MemSource::new(
        br#"[{"n": 1, "s": "1", "t": true, "z": null}]"#.to_vec(),
    ))
    .unwrap();
    let child = &tree.children(tree.root().unwrap(), 0..1).unwrap()[0];
    let eval = |text: &str| matches(&tree, child, &parse(text).unwrap()).unwrap();
    assert!(eval(".n == 1") && !eval(".s == 1") && eval(r#".s == "1""#));
    assert!(eval(".t == true") && eval(".z == null") && !eval(".z != null"));
    assert!(!eval(".missing != 1") && eval("not has(.missing)"));
    assert!(eval(r#".s ~ "^1$""#) && !eval(r#".n ~ "1""#));
    assert!(!eval(".t < true"), "booleans only compare for equality");
}
