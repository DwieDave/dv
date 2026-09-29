use proptest::prelude::*;

use super::*;

fn segment() -> impl Strategy<Value = Segment> {
    prop_oneof![
        any::<u64>().prop_map(Segment::Index),
        "[A-Za-z_][A-Za-z0-9_]{0,8}".prop_map(Segment::Key),
        any::<String>().prop_map(Segment::Key),
    ]
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

proptest! {
    #[test]
    fn keys_render_bare_or_quoted(key in prop_oneof!["[A-Za-z_][A-Za-z0-9_]{0,8}", any::<String>()]) {
        let text = render(&[Segment::Key(key.clone())]);
        if is_identifier(&key) {
            prop_assert_eq!(text, format!(".{key}"));
        } else {
            let quoted = text.strip_prefix('.').unwrap();
            prop_assert_eq!(serde_json::from_str::<String>(quoted).unwrap(), key);
        }
    }

    #[test]
    fn paths_concatenate_segments(segments in proptest::collection::vec(segment(), 1..6)) {
        let joined: String = segments.iter().map(fragment).collect();
        let expected = if joined.starts_with('[') { format!(".{joined}") } else { joined };
        prop_assert_eq!(render(&segments), expected);
    }
}

#[test]
fn renders_root_and_indices() {
    assert_eq!(render(&[]), ".");
    assert_eq!(render(&[Segment::Index(1), Segment::Index(2)]), ".[1][2]");
    let path = [
        Segment::Key("users".into()),
        Segment::Index(3),
        Segment::Key("first name".into()),
    ];
    assert_eq!(render(&path), r#".users[3]."first name""#);
}

#[test]
fn segment_of_decodes_keys_and_uses_indices() {
    use crate::source::MemSource;
    use crate::tree::{MemTree, TreeIndex};
    let tree = MemTree::parse(MemSource::new(br#"{"a\"b": [7]}"#.to_vec())).unwrap();
    let member = tree.children(tree.root().unwrap(), 0..1).unwrap().remove(0);
    assert_eq!(
        Segment::of(&member, &tree).unwrap(),
        Segment::Key("a\"b".into())
    );
    let item = tree.children(member.node(), 0..1).unwrap().remove(0);
    assert_eq!(Segment::of(&item, &tree).unwrap(), Segment::Index(0));
}
