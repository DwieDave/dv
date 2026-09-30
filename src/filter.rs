//! The filter language: expressions over each child of a container.

use std::cmp::Ordering;
use std::fmt;

use regex::Regex;
use thiserror::Error;

use crate::index::children::Child;
use crate::index::{IndexError, to_usize};
use crate::json::lex::Kind;
use crate::json::text::{quote_into, unescape};
use crate::path::{Segment, render};
use crate::pulse::Pulse;
use crate::tree::{NodeRef, TreeIndex};

mod parse;

pub use parse::{MAX_NESTING, parse};

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Has(Path),
    Compare(Path, Op, Literal),
    Matches(Path, Pattern),
}

/// A path relative to the child: `.` is the child itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(pub Vec<Segment>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// An integer that fits `i64`, compared exactly against integer values.
    Integer(i64),
    Number(f64),
    String(String),
    Bool(bool),
    Null,
}

/// A compiled regex, compared by its source.
#[derive(Debug, Clone)]
pub struct Pattern {
    source: String,
    regex: Regex,
}

impl Pattern {
    /// # Errors
    /// An invalid regex.
    pub fn new(source: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            source: source.to_owned(),
            regex: Regex::new(source)?,
        })
    }

    #[must_use]
    pub fn regex(&self) -> &Regex {
        &self.regex
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

/// A parse error at a byte offset of the input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message} at {column}")]
pub struct FilterError {
    pub message: String,
    pub pos: usize,
    /// The 1-based column of `pos`.
    pub column: usize,
}

/// Binding strength: `or` < `and` < everything else.
fn precedence(expr: &Expr) -> u8 {
    match expr {
        Expr::Or(..) => 1,
        Expr::And(..) => 2,
        _ => 3,
    }
}

fn grouped(f: &mut fmt::Formatter<'_>, expr: &Expr, parens: bool) -> fmt::Result {
    if parens {
        write!(f, "({expr})")
    } else {
        write!(f, "{expr}")
    }
}

/// `a word b`, with parentheses where parsing would group differently (left-associative).
fn binary(
    f: &mut fmt::Formatter<'_>,
    level: u8,
    (a, word, b): (&Expr, &str, &Expr),
) -> fmt::Result {
    grouped(f, a, precedence(a) < level)?;
    write!(f, " {word} ")?;
    grouped(f, b, precedence(b) <= level)
}

fn quoted(s: &str) -> String {
    let mut out = Vec::new();
    quote_into(&mut out, s);
    String::from_utf8_lossy(&out).into_owned()
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Or(a, b) => binary(f, 1, (a, "or", b)),
            Self::And(a, b) => binary(f, 2, (a, "and", b)),
            Self::Not(e) => {
                write!(f, "not ")?;
                grouped(f, e, precedence(e) < 3)
            }
            Self::Has(path) => write!(f, "has({path})"),
            Self::Compare(path, op, literal) => write!(f, "{path} {op} {literal}"),
            Self::Matches(path, pattern) => write!(f, "{path} ~ {}", quoted(&pattern.source)),
        }
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render(&self.0))
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        })
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "{n}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(&quoted(s)),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Null => f.write_str("null"),
        }
    }
}

impl Op {
    fn holds(self, order: Ordering) -> bool {
        match self {
            Self::Eq => order.is_eq(),
            Self::Ne => order.is_ne(),
            Self::Lt => order.is_lt(),
            Self::Le => order.is_le(),
            Self::Gt => order.is_gt(),
            Self::Ge => order.is_ge(),
        }
    }
}

/// Whether `child` matches `expr`.
///
/// # Errors
/// Storage or lexing failures.
pub fn matches<T: TreeIndex + ?Sized>(
    tree: &T,
    child: &Child,
    expr: &Expr,
) -> Result<bool, IndexError> {
    Ok(match expr {
        Expr::Or(a, b) => matches(tree, child, a)? || matches(tree, child, b)?,
        Expr::And(a, b) => matches(tree, child, a)? && matches(tree, child, b)?,
        Expr::Not(e) => !matches(tree, child, e)?,
        Expr::Has(path) => resolve(tree, child, path)?.is_some(),
        Expr::Compare(path, op, literal) => match resolve(tree, child, path)? {
            Some(value) => compare(tree, &value, *op, literal)?,
            None => false,
        },
        Expr::Matches(path, pattern) => match resolve(tree, child, path)? {
            Some(value) if value.kind == Kind::String => pattern
                .regex
                .is_match(&unescape(&tree.bytes(value.value..value.end)?)),
            _ => false,
        },
    })
}

/// The value at `path` inside `child`, if there is one.
fn resolve<T: TreeIndex + ?Sized>(
    tree: &T,
    child: &Child,
    path: &Path,
) -> Result<Option<Child>, IndexError> {
    path.0
        .iter()
        .try_fold(Some(child.clone()), |at, step| match at {
            Some(at) => step_into(tree, &at, step),
            None => Ok(None),
        })
}

fn step_into<T: TreeIndex + ?Sized>(
    tree: &T,
    at: &Child,
    step: &Segment,
) -> Result<Option<Child>, IndexError> {
    match (step, at.kind) {
        (Segment::Index(i), Kind::Array) => Ok(tree
            .children(at.node(), *i..i.saturating_add(1))?
            .into_iter()
            .next()),
        (Segment::Key(key), Kind::Object) => member(tree, at, key),
        _ => Ok(None),
    }
}

/// Members read per batch while looking for a key.
const MEMBER_BATCH: u64 = 256;

/// The first member of the object `at` named `key`.
fn member<T: TreeIndex + ?Sized>(
    tree: &T,
    at: &Child,
    key: &str,
) -> Result<Option<Child>, IndexError> {
    let total = tree.child_count(at.node())?.available();
    for start in (0..total).step_by(to_usize(MEMBER_BATCH)) {
        let end = start.saturating_add(MEMBER_BATCH).min(total);
        for child in tree.children(at.node(), start..end)? {
            let Some(span) = child.key.clone() else {
                continue;
            };
            if unescape(&tree.bytes(span)?) == key {
                return Ok(Some(child));
            }
        }
    }
    Ok(None)
}

/// Integers compare exactly; a fractional or huge value falls back to `f64`.
fn compare_integer(raw: &[u8], literal: i64) -> Option<Ordering> {
    let text = std::str::from_utf8(raw).ok()?;
    if let Ok(n) = text.parse::<i64>() {
        return Some(n.cmp(&literal));
    }
    // Only reached for values that are not `i64`, where the rounding is immaterial.
    #[allow(clippy::cast_precision_loss)]
    let literal = literal as f64;
    text.parse::<f64>().ok()?.partial_cmp(&literal)
}

/// A type-strict comparison; booleans and null only compare for (in)equality.
fn compare<T: TreeIndex + ?Sized>(
    tree: &T,
    value: &Child,
    op: Op,
    literal: &Literal,
) -> Result<bool, IndexError> {
    let raw = || tree.bytes(value.value..value.end);
    let equality = matches!(op, Op::Eq | Op::Ne);
    let order = match (value.kind, literal) {
        (Kind::Number, Literal::Integer(l)) => compare_integer(&raw()?, *l),
        (Kind::Number, Literal::Number(l)) => std::str::from_utf8(&raw()?)
            .ok()
            .and_then(|t| t.parse::<f64>().ok())
            .and_then(|n| n.partial_cmp(l)),
        (Kind::String, Literal::String(l)) => Some(unescape(&raw()?).as_ref().cmp(l.as_str())),
        (Kind::Bool, Literal::Bool(l)) if equality => Some((raw()?.first() == Some(&b't')).cmp(l)),
        (Kind::Null, Literal::Null) if equality => Some(Ordering::Equal),
        _ => None,
    };
    Ok(order.is_some_and(|order| op.holds(order)))
}

/// Children scanned between progress reports.
pub const FILTER_BATCH: u64 = 65_536;

/// The most matches a filter keeps.
pub const MAX_MATCHES: usize = 1_000_000;

/// Matches not yet reported, and how far the scan got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub found: Vec<u64>,
    pub scanned: u64,
    pub total: u64,
    pub capped: bool,
}

/// Scans the children of `node` for matches of `expr`, keeping at most `max`, reporting
/// each batch through `pulse`. The result holds the matches `pulse` did not take; `None`
/// when cancelled.
///
/// # Errors
/// Storage or lexing failures.
pub fn scan<T: TreeIndex + ?Sized>(
    tree: &T,
    node: NodeRef,
    expr: &Expr,
    max: usize,
    pulse: &dyn Pulse,
) -> Result<Option<Scan>, IndexError> {
    let total = tree.child_count(node)?.available();
    let (mut found, mut reported, mut scanned) = (Vec::new(), 0, 0);
    while scanned < total && found.len() < max {
        if pulse.cancelled() {
            return Ok(None);
        }
        let end = scanned.saturating_add(FILTER_BATCH).min(total);
        for child in tree.children(node, scanned..end)? {
            if found.len() < max && matches(tree, &child, expr)? {
                found.push(child.index);
            }
        }
        scanned = end;
        if pulse.matched(&found[reported..], scanned, total) {
            reported = found.len();
        }
    }
    let capped = found.len() >= max;
    let found = found.split_off(reported);
    Ok(Some(Scan {
        found,
        scanned,
        total,
        capped,
    }))
}

#[cfg(test)]
mod tests;
