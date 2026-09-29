//! jq-style paths (`.users[3]."first name"`) for display and copy.

use crate::index::IndexError;
use crate::index::children::Child;
use crate::json::text::{quote_into, unescape};
use crate::tree::TreeIndex;

/// One step from a container to a child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(u64),
}

impl Segment {
    /// The step that reaches `child` from its parent.
    ///
    /// # Errors
    /// Storage failures while reading the key.
    pub fn of(child: &Child, tree: &impl TreeIndex) -> Result<Self, IndexError> {
        match &child.key {
            Some(span) => Ok(Self::Key(unescape(&tree.bytes(span.clone())?).into_owned())),
            None => Ok(Self::Index(child.index)),
        }
    }
}

/// Renders `segments` as a jq path; the root is `.`, and a leading index reads `.[i]`.
#[must_use]
pub fn render(segments: &[Segment]) -> String {
    let joined: String = segments.iter().map(fragment).collect();
    if joined.is_empty() || joined.starts_with('[') {
        format!(".{joined}")
    } else {
        joined
    }
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A JSON string literal for `key`.
fn quote(key: &str) -> String {
    let mut out = Vec::with_capacity(key.len() + 2);
    quote_into(&mut out, key);
    String::from_utf8(out).unwrap_or_default()
}

/// One segment as it appears after its predecessor.
fn fragment(segment: &Segment) -> String {
    match segment {
        Segment::Index(i) => format!("[{i}]"),
        Segment::Key(k) if is_identifier(k) => format!(".{k}"),
        Segment::Key(k) => format!(".{}", quote(k)),
    }
}

#[cfg(test)]
mod tests;
