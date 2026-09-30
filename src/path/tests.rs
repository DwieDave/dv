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

fn as_step(segment: &Segment) -> Step {
    match segment {
        Segment::Key(k) => Step::Key(k.clone()),
        Segment::Index(i) => Step::Index(i64::try_from(*i).unwrap()),
        Segment::Items => Step::Slice(None, None),
    }
}

proptest! {
    #[test]
    fn parsing_inverts_rendering(segments in proptest::collection::vec(segment(), 0..6)) {
        let segments: Vec<Segment> = segments.into_iter().map(|s| match s {
            Segment::Index(i) => Segment::Index(i % (1 << 40)),
            other => other,
        }).collect();
        let steps: Vec<Step> = segments.iter().map(as_step).collect();
        prop_assert_eq!(parse(&render(&segments)), Ok(steps));
    }
}

#[test]
fn parses_every_form() {
    let key = |k: &str| Step::Key(k.to_owned());
    let table: Vec<(&str, Vec<Step>)> = vec![
        (".", vec![]),
        ("  .a.b  ", vec![key("a"), key("b")]),
        (".a[3]", vec![key("a"), Step::Index(3)]),
        (".[-1]", vec![Step::Index(-1)]),
        (".a[10:20]", vec![key("a"), Step::Slice(Some(10), Some(20))]),
        (".a[:5]", vec![key("a"), Step::Slice(None, Some(5))]),
        (".a[-3:]", vec![key("a"), Step::Slice(Some(-3), None)]),
        (".a[:]", vec![key("a"), Step::Slice(None, None)]),
        (r#".a."k y""#, vec![key("a"), key("k y")]),
        (r#".["k\"q"]"#, vec![key("k\"q")]),
        (
            r#".a[ "b" ][ 2 ]"#,
            vec![key("a"), key("b"), Step::Index(2)],
        ),
        ("._x1.Y_", vec![key("_x1"), key("Y_")]),
    ];
    for (input, expected) in table {
        assert_eq!(parse(input), Ok(expected), "{input}");
    }
}

#[test]
fn reports_errors_with_positions() {
    let table = [
        (".a.", 3),
        (".a[1", 4),
        (".a[x]", 3),
        ("a.b", 0),
        (".a b", 3),
        ("", 0),
        (r#".a["x]"#, 3),
    ];
    for (input, at) in table {
        let err = parse(input).unwrap_err();
        assert_eq!(err.at, at, "{input}: {err}");
    }
}

#[test]
fn error_offsets_count_chars_not_bytes() {
    let table = [(r#"."é"."#, 5), (r#"."é" x"#, 5), (r#"."é"é"#, 4), ("é", 0)];
    for (input, at) in table {
        let err = parse(input).unwrap_err();
        assert_eq!(err.at, at, "{input}: {err}");
    }
}
