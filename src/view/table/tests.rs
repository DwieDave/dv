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
