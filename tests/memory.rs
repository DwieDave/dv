//! NFR-3: peak heap during load + parse stays within a bounded ratio of the input size.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod support {
    pub mod mem;
}

use std::fmt::Write;

use dv::source::MemSource;
use dv::tree::MemTree;
use support::mem::peak_heap;

const TARGET: usize = 4_000_000;

fn repeat_until(open: &str, item: impl Fn(usize) -> String, sep: &str, close: &str) -> String {
    let mut out = String::from(open);
    let mut i = 0;
    while out.len() < TARGET {
        out.push_str(if i > 0 { sep } else { "" });
        out.push_str(&item(i));
        i += 1;
    }
    out + close
}

fn api(i: usize) -> String {
    format!(
        r#"{{"id":{i},"name":"user_{i}","active":true,"score":12.5,"tags":["a","b"],"address":{{"city":"c{i}","zip":"01234"}},"note":null}}"#
    )
}

fn shapes() -> Vec<(&'static str, String, f64)> {
    let deep = 100_000;
    let chain = 1_000_000;
    vec![
        ("api", repeat_until("[", api, ",", "]"), 3.0),
        (
            "dense",
            repeat_until("[", |i| (i % 10).to_string(), ",", "]"),
            3.0,
        ),
        (
            "small-objects",
            repeat_until("[", |i| format!(r#"{{"a":{}}}"#, i % 10), ",", "]"),
            3.0,
        ),
        (
            "wide",
            repeat_until("{", |i| format!(r#""k{i}":{i}"#), ",", "}"),
            3.0,
        ),
        (
            "escapes",
            repeat_until("[", |_| r#""a\n\"b\" é\t""#.to_owned(), ",", "]"),
            3.0,
        ),
        (
            "deep",
            format!("{}1{}", r#"{"a":"#.repeat(deep), "}".repeat(deep)),
            5.0,
        ),
        (
            "bracket-chain",
            format!("{}{}", "[".repeat(chain), "]".repeat(chain)),
            10.0,
        ),
    ]
}

#[allow(clippy::cast_precision_loss)] // ratios of sizes far below 2^52
fn ratio(peak: usize, len: usize) -> f64 {
    peak as f64 / len as f64
}

#[test]
fn peak_heap_is_bounded_for_every_shape() {
    let mut report = String::new();
    let mut failures = Vec::new();
    for (name, text, limit) in shapes() {
        let len = text.len();
        let (tree, peak) = peak_heap(|| {
            let source = MemSource::load(text.as_bytes(), u64::MAX, Some(len as u64)).unwrap();
            MemTree::parse(source).unwrap()
        });
        drop(tree);
        let r = ratio(peak, len);
        writeln!(report, "{name}: {r:.2}x of {len} bytes").unwrap();
        if r > limit {
            failures.push(format!("{name} {r:.2}x > {limit}x"));
        }
    }
    println!("{report}");
    assert!(failures.is_empty(), "{failures:?}\n{report}");
}

fn api_yaml(i: usize) -> String {
    format!(
        "- id: {i}\n  name: user_{i}\n  active: true\n  score: 12.5\n  tags:\n    - a\n    - b\n  address:\n    city: c{i}\n    zip: \"01234\"\n  note: null\n"
    )
}

fn loaded_peak(text: &str, path: &str) -> usize {
    use dv::load::{LoadEvent, Request, load};
    use std::sync::atomic::AtomicBool;
    let request = Request {
        path: Some(path.into()),
        size_hint: Some(text.len() as u64),
        max_len: u64::MAX,
        ..Request::default()
    };
    let (loaded, peak) = peak_heap(|| {
        let mut ok = false;
        load(
            text.as_bytes(),
            &request,
            &mut |e| ok |= matches!(e, LoadEvent::Loaded(Ok(_))),
            &AtomicBool::new(false),
        );
        ok
    });
    assert!(loaded, "{path} did not load");
    peak
}

#[test]
fn ndjson_and_yaml_loads_are_bounded() {
    let ndjson = repeat_until("", api, "\n", "\n");
    let yaml = repeat_until("", api_yaml, "", "");
    for (name, text) in [("x.ndjson", ndjson), ("x.yaml", yaml)] {
        let r = ratio(loaded_peak(&text, name), text.len());
        println!("{name}: {r:.2}x");
        assert!(r <= 3.0, "{name}: {r:.2}x");
    }
}
