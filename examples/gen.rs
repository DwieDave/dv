//! Deterministic test-data generator.
//!
//! `cargo run --release --example gen -- <shape> <size> [out]`
//!
//! Size is bytes or a decimal `K`/`M`/`G` suffix. Output streams, so memory
//! stays constant for any size.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeqShape {
    Api,
    Dense,
    SmallObjects,
    Wide,
    Escapes,
    Ndjson,
    Yaml,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Seq(SeqShape),
    Deep,
}

const SHAPES: [(&str, Shape); 8] = [
    ("api", Shape::Seq(SeqShape::Api)),
    ("dense", Shape::Seq(SeqShape::Dense)),
    ("small-objects", Shape::Seq(SeqShape::SmallObjects)),
    ("wide", Shape::Seq(SeqShape::Wide)),
    ("escapes", Shape::Seq(SeqShape::Escapes)),
    ("ndjson", Shape::Seq(SeqShape::Ndjson)),
    ("yaml", Shape::Seq(SeqShape::Yaml)),
    ("deep", Shape::Deep),
];

/// JSON escape introducer, kept as a char so escapes stay literal in output.
const BS: char = '\\';

/// splitmix64: tiny, fast, deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let z = (self.0 ^ (self.0 >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Counting<W> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn generate(shape: Shape, target: u64, out: impl Write) -> io::Result<()> {
    let mut w = Counting {
        inner: out,
        written: 0,
    };
    match shape {
        Shape::Seq(seq) => write_sequence(seq, &mut w, target)?,
        Shape::Deep => write_deep(&mut w, target)?,
    }
    w.flush()
}

fn frame(shape: SeqShape) -> (&'static str, &'static str, &'static str) {
    match shape {
        SeqShape::Wide => ("{", ",", "}"),
        SeqShape::Ndjson => ("", "\n", "\n"),
        SeqShape::Yaml => ("", "", ""),
        _ => ("[", ",", "]"),
    }
}

fn write_sequence<W: Write>(shape: SeqShape, w: &mut Counting<W>, target: u64) -> io::Result<()> {
    let (open, sep, close) = frame(shape);
    let mut rng = Rng(0x5EED);
    w.write_all(open.as_bytes())?;
    let mut i = 0u64;
    while w.written < target {
        if i > 0 {
            w.write_all(sep.as_bytes())?;
        }
        write_item(shape, w, &mut rng, i)?;
        i += 1;
    }
    w.write_all(close.as_bytes())
}

fn write_item(shape: SeqShape, w: &mut impl Write, rng: &mut Rng, i: u64) -> io::Result<()> {
    match shape {
        SeqShape::Api | SeqShape::Ndjson => write_record_json(w, &Record::new(rng, i)),
        SeqShape::Yaml => write_record_yaml(w, &Record::new(rng, i)),
        SeqShape::Dense => write!(w, "{}", rng.below(1000)),
        SeqShape::SmallObjects => write!(w, "{{\"a\":{}}}", rng.below(100)),
        SeqShape::Wide => write!(w, "\"key_{i}\":{}", rng.below(1_000_000)),
        SeqShape::Escapes => write!(
            w,
            r#""line\nbreak \"q\" tab\t {BS}u00e9 snow ☃ emoji 😀 slash \\ pair {BS}ud83d{BS}ude00 #{}""#,
            rng.below(1_000_000)
        ),
    }
}

/// One realistic API record, rendered identically as JSON and YAML.
struct Record {
    id: u64,
    name: u64,
    active: bool,
    score: (u64, u64),
    tags: [u64; 2],
    city: u64,
    zip: u64,
}

impl Record {
    fn new(rng: &mut Rng, id: u64) -> Self {
        Self {
            id,
            name: rng.below(100_000),
            active: rng.below(2) == 1,
            score: (rng.below(100), rng.below(10)),
            tags: [rng.below(50), rng.below(50)],
            city: rng.below(500),
            zip: rng.below(100_000),
        }
    }
}

fn write_record_json(w: &mut impl Write, r: &Record) -> io::Result<()> {
    write!(
        w,
        r#"{{"id":{},"name":"user_{}","active":{},"score":{}.{},"tags":["t{}","t{}"],"address":{{"city":"c{}","zip":"{:05}"}},"note":null}}"#,
        r.id, r.name, r.active, r.score.0, r.score.1, r.tags[0], r.tags[1], r.city, r.zip
    )
}

fn write_record_yaml(w: &mut impl Write, r: &Record) -> io::Result<()> {
    write!(
        w,
        "- id: {}\n  name: user_{}\n  active: {}\n  score: {}.{}\n  tags:\n    - t{}\n    - t{}\n  address:\n    city: c{}\n    zip: \"{:05}\"\n  note: null\n",
        r.id, r.name, r.active, r.score.0, r.score.1, r.tags[0], r.tags[1], r.city, r.zip
    )
}

fn write_deep<W: Write>(w: &mut Counting<W>, target: u64) -> io::Result<()> {
    let depth = target.div_ceil(6).max(1);
    (0..depth).try_for_each(|_| w.write_all(b"{\"a\":"))?;
    w.write_all(b"1")?;
    (0..depth).try_for_each(|_| w.write_all(b"}"))
}

fn parse_size(text: &str) -> Option<u64> {
    let (digits, factor) = match text.as_bytes().last()? {
        b'K' => (&text[..text.len() - 1], 1_000),
        b'M' => (&text[..text.len() - 1], 1_000_000),
        b'G' => (&text[..text.len() - 1], 1_000_000_000),
        _ => (text, 1),
    };
    digits.parse::<u64>().ok()?.checked_mul(factor)
}

fn usage() -> ExitCode {
    let names: Vec<&str> = SHAPES.iter().map(|(name, _)| *name).collect();
    eprintln!("usage: gen <{}> <size[K|M|G]> [out]", names.join("|"));
    ExitCode::FAILURE
}

fn run(shape: Shape, size: u64, out: Option<&str>) -> io::Result<()> {
    match out {
        Some(path) => generate(shape, size, BufWriter::new(File::create(path)?)),
        None => generate(shape, size, BufWriter::new(io::stdout().lock())),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let shape = args
        .first()
        .and_then(|a| SHAPES.iter().find(|(n, _)| n == a))
        .map(|(_, s)| *s);
    let size = args.get(1).and_then(|a| parse_size(a));
    let (Some(shape), Some(size)) = (shape, size) else {
        return usage();
    };
    match run(shape, size, args.get(2).map(String::as_str)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("gen: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use saphyr::{LoadableYamlNode, Scalar, Yaml};
    use serde_json::Value;

    const JSON_SHAPES: [SeqShape; 5] = [
        SeqShape::Api,
        SeqShape::Dense,
        SeqShape::SmallObjects,
        SeqShape::Wide,
        SeqShape::Escapes,
    ];

    fn render(shape: Shape, target: u64) -> String {
        let mut out = Vec::new();
        generate(shape, target, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn yaml_to_json(node: &Yaml) -> Value {
        match node {
            Yaml::Value(Scalar::Null) => Value::Null,
            Yaml::Value(Scalar::Boolean(b)) => Value::from(*b),
            Yaml::Value(Scalar::Integer(i)) => Value::from(*i),
            Yaml::Value(Scalar::FloatingPoint(f)) => Value::from(f.into_inner()),
            Yaml::Value(Scalar::String(s)) => Value::from(s.as_ref()),
            Yaml::Sequence(items) => items.iter().map(yaml_to_json).collect(),
            Yaml::Mapping(map) => map
                .iter()
                .map(|(k, v)| (k.as_str().unwrap().to_owned(), yaml_to_json(v)))
                .collect(),
            other => panic!("unexpected yaml node {other:?}"),
        }
    }

    proptest! {
        #[test]
        fn json_shapes_parse_and_hit_target(
            shape in proptest::sample::select(JSON_SHAPES.to_vec()),
            target in 1u64..20_000,
        ) {
            let text = render(Shape::Seq(shape), target);
            prop_assert!(serde_json::from_str::<Value>(&text).is_ok(), "{shape:?} invalid");
            let len = text.len() as u64;
            prop_assert!((target..=target + 512).contains(&len), "{shape:?} len {len}");
        }

        #[test]
        fn parse_size_round_trips(n in 0u64..1_000_000, unit in 0usize..4) {
            let (suffix, factor) = [("", 1), ("K", 1_000), ("M", 1_000_000), ("G", 1_000_000_000)][unit];
            prop_assert_eq!(parse_size(&format!("{n}{suffix}")), Some(n * factor));
        }
    }

    #[test]
    fn ndjson_lines_each_parse() {
        let text = render(Shape::Seq(SeqShape::Ndjson), 50_000);
        assert!(text.lines().count() > 10);
        for line in text.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }

    #[test]
    fn yaml_matches_api_records() {
        let yaml = render(Shape::Seq(SeqShape::Yaml), 20_000);
        let docs = Yaml::load_from_str(&yaml).unwrap();
        let records = yaml_to_json(&docs[0]);
        let records = records.as_array().unwrap();
        let api: Value = serde_json::from_str(&render(Shape::Seq(SeqShape::Api), 60_000)).unwrap();
        assert!(records.len() > 10);
        assert_eq!(records[..], api.as_array().unwrap()[..records.len()]);
    }

    #[test]
    fn escapes_keep_literal_unicode_escapes() {
        let text = render(Shape::Seq(SeqShape::Escapes), 1_000);
        assert!(text.contains(&format!("{BS}u00e9")));
        assert!(text.contains(&format!("{BS}ud83d{BS}ude00")));
    }

    #[test]
    fn deep_has_exact_nesting() {
        let text = render(Shape::Deep, 600);
        let expected = format!("{}1{}", "{\"a\":".repeat(100), "}".repeat(100));
        assert_eq!(text, expected);
    }

    #[test]
    fn generation_is_deterministic() {
        let shape = Shape::Seq(SeqShape::Api);
        assert!(!render(shape, 5_000).is_empty());
        assert_eq!(render(shape, 5_000), render(shape, 5_000));
    }
}
