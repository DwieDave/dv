//! Supported input formats and their detection (FR-3).

use std::path::Path;

/// The syntax a document was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Ndjson,
    Yaml,
}

impl Format {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Ndjson => "NDJSON",
            Self::Yaml => "YAML",
        }
    }
}

/// Picks the format: `forced` wins, then the file extension, then the content of `head`.
#[must_use]
pub fn detect(path: Option<&Path>, head: &[u8], forced: Option<Format>) -> Format {
    forced
        .or_else(|| path.and_then(by_extension))
        .unwrap_or_else(|| sniff(head))
}

/// Bytes of the head inspected when sniffing.
pub const SNIFF_LEN: usize = 64 << 10;

fn by_extension(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "json" => Some(Format::Json),
        "ndjson" | "jsonl" => Some(Format::Ndjson),
        "yaml" | "yml" => Some(Format::Yaml),
        _ => None,
    }
}

/// JSON starts with a bracket, quote, digit or `-digit`; anything else is YAML.
fn sniff(head: &[u8]) -> Format {
    let head = &head[..head.len().min(SNIFF_LEN)];
    let start = head
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(head.len());
    match (head.get(start), head.get(start + 1)) {
        (Some(b'{' | b'['), _) if looks_line_delimited(head) => Format::Ndjson,
        (Some(b'{' | b'[' | b'"' | b'0'..=b'9'), _) | (Some(b'-'), Some(b'0'..=b'9')) => {
            Format::Json
        }
        _ => Format::Yaml,
    }
}

/// Heuristic: the first line is a complete bracketed value and another unindented line starts one.
fn looks_line_delimited(head: &[u8]) -> bool {
    let mut lines = head
        .split(|&b| b == b'\n')
        .filter(|line| !line.trim_ascii().is_empty());
    let complete = |line: &[u8]| matches!(line.trim_ascii().last(), Some(b'}' | b']'));
    let opens = |line: &&[u8]| matches!(line.first(), Some(b'{' | b'['));
    lines
        .next()
        .is_some_and(|first| opens(&first) && complete(first) && first.trim_ascii().len() > 1)
        && lines.any(|l| opens(&l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_decide_case_insensitively() {
        let table = [
            ("a.json", Format::Json),
            ("a.NDJSON", Format::Ndjson),
            ("a.jsonl", Format::Ndjson),
            ("a.yaml", Format::Yaml),
            ("a.YML", Format::Yaml),
        ];
        for (name, expected) in table {
            assert_eq!(detect(Some(Path::new(name)), b"", None), expected, "{name}");
        }
    }

    #[test]
    fn pretty_arrays_of_objects_stay_json() {
        let pretty = b"[\n  {\n    \"a\": 1\n  },\n  {\n    \"a\": 2\n  }\n]\n";
        assert_eq!(detect(None, pretty, None), Format::Json);
    }

    #[test]
    fn an_override_beats_the_extension() {
        assert_eq!(
            detect(Some(Path::new("a.json")), b"{}", Some(Format::Yaml)),
            Format::Yaml
        );
    }

    #[test]
    fn content_is_sniffed_without_a_known_extension() {
        let table: [(&[u8], Format); 7] = [
            (b"  {\"a\": 1}", Format::Json),
            (b"[\n  1,\n  2\n]\n", Format::Json),
            (b"{\"a\":1}\n{\"a\":2}\n", Format::Ndjson),
            (b"[1]\n\n[2]", Format::Ndjson),
            (b"a: 1\nb: [2]\n", Format::Yaml),
            (b"- a\n- b\n", Format::Yaml),
            (b"---\nkey: value\n", Format::Yaml),
        ];
        for (head, expected) in table {
            assert_eq!(
                detect(Some(Path::new("data.txt")), head, None),
                expected,
                "{:?}",
                String::from_utf8_lossy(head)
            );
            assert_eq!(detect(None, head, None), expected);
        }
    }
}
