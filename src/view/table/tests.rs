use proptest::prelude::*;

use super::*;
use crate::source::MemSource;
use crate::tree::MemTree;

fn tree_of(text: &str) -> MemTree {
    MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap()
}

/// An array of objects with the given members, as JSON text.
fn rows_text(rows: &[Vec<(String, u32)>]) -> String {
    let object = |row: &Vec<(String, u32)>| {
        let members: Vec<String> = row
            .iter()
            .map(|(key, value)| format!("{}: {value}", serde_json::to_string(key).unwrap()))
            .collect();
        format!("{{{}}}", members.join(", "))
    };
    format!(
        "[{}]",
        rows.iter().map(object).collect::<Vec<_>>().join(", ")
    )
}

/// Rows whose keys come from a small pool (so they overlap), unique within a row.
fn rows() -> impl Strategy<Value = Vec<Vec<(String, u32)>>> {
    let key = prop::sample::select(vec!["a", "b", "c", "id", "é x", "q\"t"]);
    let row = prop::collection::vec((key, 0..1000u32), 0..5).prop_map(|members| {
        let mut seen = Vec::new();
        members
            .into_iter()
            .filter(|(k, _)| {
                !seen.contains(k) && {
                    seen.push(*k);
                    true
                }
            })
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    });
    prop::collection::vec(row, 1..20)
}

fn keys(columns: &[Column]) -> Vec<&str> {
    columns.iter().map(|c| c.key.as_str()).collect()
}

proptest! {
    #[test]
    fn columns_are_the_ordered_union_and_cells_are_missing_exactly_where_keys_are(rows in rows()) {
        let tree = tree_of(&rows_text(&rows));
        let root = tree.root().unwrap();
        let columns = columns(&tree, root).unwrap();
        let mut union: Vec<&str> = Vec::new();
        for (key, _) in rows.iter().flatten() {
            if !union.contains(&key.as_str()) {
                union.push(key);
            }
        }
        prop_assert_eq!(keys(&columns), union);
        let children = tree.children(root, 0..rows.len() as u64).unwrap();
        for (row, child) in rows.iter().zip(&children) {
            let cells = row_cells(&tree, child.node(), &columns).unwrap();
            for (column, cell) in columns.iter().zip(&cells) {
                let expected = row.iter().find(|(k, _)| *k == column.key).map(|(_, v)| v.to_string());
                match (expected, cell) {
                    (None, Cell::Missing) => {}
                    (Some(v), Cell::Scalar { text, kind: Kind::Number }) => prop_assert_eq!(&v, text),
                    (expected, cell) => prop_assert!(false, "{expected:?} vs {cell:?}"),
                }
            }
        }
    }
}

#[test]
fn widths_fit_the_widest_cell_up_to_the_cap() {
    let long = "x".repeat(50);
    let tree = tree_of(&format!(
        r#"[{{"id": 1, "name": "{long}"}}, {{"id": 12345, "tags": [1, 2]}}]"#
    ));
    let columns = columns(&tree, tree.root().unwrap()).unwrap();
    let widths: Vec<(&str, usize)> = columns.iter().map(|c| (c.key.as_str(), c.width)).collect();
    assert_eq!(widths, [("id", 5), ("name", MAX_WIDTH), ("tags", 4)]);
}

#[test]
fn cells_show_scalars_badges_and_missing_values() {
    let tree = tree_of(r#"[{"s": "hi\nthere", "o": {"a": 1}, "n": null}, {}]"#);
    let root = tree.root().unwrap();
    let columns = columns(&tree, root).unwrap();
    let rows = tree.children(root, 0..2).unwrap();
    let texts = |i: usize| -> Vec<String> {
        row_cells(&tree, rows[i].node(), &columns)
            .unwrap()
            .iter()
            .map(Cell::text)
            .collect()
    };
    assert_eq!(texts(0), ["hi\\nthere", "{1}", "null"]);
    assert_eq!(texts(1), ["—", "—", "—"]);
}

#[test]
fn only_the_first_rows_are_sampled() {
    let rows: Vec<String> = (0..=SAMPLE)
        .map(|i| {
            if i == SAMPLE {
                r#"{"late": 1}"#.to_owned()
            } else {
                r#"{"a": 1}"#.to_owned()
            }
        })
        .collect();
    let tree = tree_of(&format!("[{}]", rows.join(",")));
    let columns = columns(&tree, tree.root().unwrap()).unwrap();
    assert_eq!(keys(&columns), ["a"]);
}

/// A value for the sorted key, or `None` for a missing key.
fn sort_value() -> impl Strategy<Value = Option<serde_json::Value>> {
    use serde_json::{Value, json};
    let value = prop_oneof![
        (-50i32..50).prop_map(|n| json!(n)),
        (-5.0f64..5.0).prop_map(|f| json!((f * 4.0).round() / 4.0)),
        prop::sample::select(vec!["", "a", "b", "ab", "é"]).prop_map(|s| json!(s)),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
        Just(json!([1])),
        Just(json!({"x": 1})),
    ];
    prop::option::weighted(0.8, value)
}

/// The TB-5 order, written independently over `serde_json` values.
fn reference(
    a: Option<&serde_json::Value>,
    b: Option<&serde_json::Value>,
    dir: SortDir,
) -> std::cmp::Ordering {
    use serde_json::Value;
    use std::cmp::Ordering;
    let rank = |v: &Value| match v {
        Value::Number(_) => 0,
        Value::String(_) => 1,
        Value::Bool(_) => 2,
        Value::Null => 3,
        _ => 4,
    };
    let within = |a: &Value, b: &Value| match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64().unwrap().total_cmp(&y.as_f64().unwrap()),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    };
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let order = rank(a).cmp(&rank(b)).then_with(|| within(a, b));
            if dir == SortDir::Desc {
                order.reverse()
            } else {
                order
            }
        }
    }
}

proptest! {
    #[test]
    fn sorting_agrees_with_the_reference_order_and_is_stable(
        values in prop::collection::vec(sort_value(), 0..40),
        desc: bool,
    ) {
        let dir = if desc { SortDir::Desc } else { SortDir::Asc };
        let rows: Vec<String> = values
            .iter()
            .map(|v| v.as_ref().map_or_else(|| r#"{"other": 1}"#.to_owned(), |v| format!(r#"{{"k": {v}}}"#)))
            .collect();
        let tree = tree_of(&format!("[{}]", rows.join(",")));
        let order = sort_order(&tree, tree.root().unwrap(), "k", dir, &|| false).unwrap().unwrap();
        let mut expected: Vec<usize> = (0..values.len()).collect();
        expected.sort_by(|&a, &b| reference(values[a].as_ref(), values[b].as_ref(), dir));
        let expected: Vec<u64> = expected.into_iter().map(|i| i as u64).collect();
        prop_assert_eq!(order, expected);
    }
}

#[test]
fn cancelled_sorts_return_nothing() {
    let tree = tree_of(r#"[{"k": 2}, {"k": 1}]"#);
    let order = sort_order(&tree, tree.root().unwrap(), "k", SortDir::Asc, &|| true).unwrap();
    assert_eq!(order, None);
}

#[test]
fn too_many_rows_cannot_be_sorted() {
    assert_eq!(sortable(MAX_SORT), Ok(()));
    assert_eq!(
        sortable(MAX_SORT + 1),
        Err("too many rows to sort (max 1M)")
    );
}
