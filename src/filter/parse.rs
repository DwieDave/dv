//! Lexing and recursive-descent parsing of filter expressions.

use super::{Expr, FilterError, Literal, Op, Path, Pattern, Step};
use crate::json::lex::hex4;
use crate::json::text::unescape;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Dot,
    Open,
    Close,
    LBracket,
    RBracket,
    Tilde,
    Op(Op),
    Word(String),
    Str(String),
    /// A number and its source text.
    Num(f64, String),
}

/// A token and the byte offset where it starts.
type Lexed = (Token, usize);

/// # Errors
/// The first problem, with its position.
pub fn parse(text: &str) -> Result<Expr, FilterError> {
    let mut parser = Parser {
        tokens: lex(text)?,
        at: 0,
        text,
    };
    let expr = parser.or()?;
    match parser.peek() {
        None => Ok(expr),
        Some(_) => parser.fail("expected `and` or `or`"),
    }
}

fn error(text: &str, message: &str, pos: usize) -> FilterError {
    let column = text.get(..pos).map_or(pos, |head| head.chars().count()) + 1;
    FilterError {
        message: message.to_owned(),
        pos,
        column,
    }
}

fn lex(text: &str) -> Result<Vec<Lexed>, FilterError> {
    let mut tokens = Vec::new();
    let mut pos = 0;
    while let Some(c) = text.get(pos..).and_then(|rest| rest.chars().next()) {
        if c.is_whitespace() {
            pos += c.len_utf8();
            continue;
        }
        let (token, end) = token(text, pos, c)?;
        tokens.push((token, pos));
        pos = end;
    }
    Ok(tokens)
}

/// The token starting with `c` at `pos`, and where it ends.
fn token(text: &str, pos: usize, c: char) -> Result<(Token, usize), FilterError> {
    let next = text.as_bytes().get(pos + 1).copied();
    let single = |token| Ok((token, pos + 1));
    let double = |token| Ok((token, pos + 2));
    match (c, next) {
        ('.', _) => single(Token::Dot),
        ('(', _) => single(Token::Open),
        (')', _) => single(Token::Close),
        ('[', _) => single(Token::LBracket),
        (']', _) => single(Token::RBracket),
        ('~', _) => single(Token::Tilde),
        ('=', Some(b'=')) => double(Token::Op(Op::Eq)),
        ('!', Some(b'=')) => double(Token::Op(Op::Ne)),
        ('<', Some(b'=')) => double(Token::Op(Op::Le)),
        ('>', Some(b'=')) => double(Token::Op(Op::Ge)),
        ('<', _) => single(Token::Op(Op::Lt)),
        ('>', _) => single(Token::Op(Op::Gt)),
        ('"', _) => string(text, pos),
        ('-' | '0'..='9', _) => number(text, pos),
        (c, _) if c.is_ascii_alphabetic() || c == '_' => Ok(word(text, pos)),
        ('=' | '!', _) => Err(error(text, "expected `==` or `!=`", pos)),
        _ => Err(error(text, "unexpected character", pos)),
    }
}

fn word(text: &str, pos: usize) -> (Token, usize) {
    let len = text[pos..]
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    (Token::Word(text[pos..pos + len].to_owned()), pos + len)
}

fn number(text: &str, pos: usize) -> Result<(Token, usize), FilterError> {
    let len = 1 + text[pos + 1..]
        .bytes()
        .take_while(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        .count();
    let source = &text[pos..pos + len];
    match source.parse::<f64>() {
        Ok(n) if n.is_finite() => Ok((Token::Num(n, source.to_owned()), pos + len)),
        _ => Err(error(text, "invalid number", pos)),
    }
}

/// A JSON string literal starting at `pos`.
fn string(text: &str, pos: usize) -> Result<(Token, usize), FilterError> {
    let bytes = text.as_bytes();
    let mut i = pos + 1;
    loop {
        match bytes.get(i) {
            None => return Err(error(text, "unterminated string", pos)),
            Some(b'"') => break,
            Some(b'\\') => {
                i += escape_len(bytes, i).ok_or_else(|| error(text, "invalid escape", i))?;
            }
            Some(b) if *b < 0x20 => return Err(error(text, "control character in string", i)),
            Some(_) => i += 1,
        }
    }
    let value = unescape(&bytes[pos..=i]).into_owned();
    Ok((Token::Str(value), i + 1))
}

/// Bytes of the valid escape at `i`, if it is one.
fn escape_len(bytes: &[u8], i: usize) -> Option<usize> {
    match bytes.get(i + 1)? {
        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Some(2),
        b'u' => hex4(bytes, i + 2).map(|_| 6),
        _ => None,
    }
}

/// What comes next in a path.
enum Next {
    Step(Step),
    /// A `.` with no key after it (the value itself, or before `[n]`).
    Dot,
    End,
}

struct Parser<'a> {
    tokens: Vec<Lexed>,
    at: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(token, _)| token)
    }

    fn pos(&self) -> usize {
        self.tokens
            .get(self.at)
            .map_or(self.text.len(), |(_, pos)| *pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.peek().cloned();
        self.at += usize::from(token.is_some());
        token
    }

    fn fail<T>(&self, message: &str) -> Result<T, FilterError> {
        Err(error(self.text, message, self.pos()))
    }

    fn eat(&mut self, token: &Token) -> bool {
        let found = self.peek() == Some(token);
        self.at += usize::from(found);
        found
    }

    fn keyword(&mut self, word: &str) -> bool {
        self.eat(&Token::Word(word.to_owned()))
    }

    fn expect(&mut self, token: &Token, message: &str) -> Result<(), FilterError> {
        if self.eat(token) {
            Ok(())
        } else {
            self.fail(message)
        }
    }

    fn or(&mut self) -> Result<Expr, FilterError> {
        let mut left = self.and()?;
        while self.keyword("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, FilterError> {
        let mut left = self.unary()?;
        while self.keyword("and") {
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, FilterError> {
        if self.keyword("not") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat(&Token::Open) {
            let expr = self.or()?;
            self.expect(&Token::Close, "expected `)`")?;
            return Ok(expr);
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, FilterError> {
        let has = self.peek() == Some(&Token::Word("has".to_owned()));
        if has && self.tokens.get(self.at + 1).map(|(t, _)| t) == Some(&Token::Open) {
            self.at += 2;
            let path = self.path()?;
            self.expect(&Token::Close, "expected `)`")?;
            return Ok(Expr::Has(path));
        }
        let path = self.path()?;
        let pos = self.pos();
        match self.bump() {
            Some(Token::Op(op)) => Ok(Expr::Compare(path, op, self.literal()?)),
            Some(Token::Tilde) => Ok(Expr::Matches(path, self.pattern()?)),
            _ => Err(error(
                self.text,
                "expected an operator (== != < <= > >= ~)",
                pos,
            )),
        }
    }

    fn path(&mut self) -> Result<Path, FilterError> {
        let start = self.pos();
        let mut steps = Vec::new();
        loop {
            match self.step()? {
                Next::Step(step) => steps.push(step),
                Next::Dot => {}
                Next::End => break,
            }
        }
        if self.pos() == start {
            return Err(error(self.text, "expected a path like .name", start));
        }
        Ok(Path(steps))
    }

    /// One `.key`, `[n]` or lone `.`.
    fn step(&mut self) -> Result<Next, FilterError> {
        if self.eat(&Token::Dot) {
            return Ok(match self.peek().cloned() {
                Some(Token::Word(key) | Token::Str(key)) => {
                    self.at += 1;
                    Next::Step(Step::Key(key))
                }
                _ => Next::Dot,
            });
        }
        if !self.eat(&Token::LBracket) {
            return Ok(Next::End);
        }
        let index = match self.peek() {
            Some(Token::Num(_, source)) => source.parse::<u64>().ok(),
            _ => None,
        };
        let Some(index) = index else {
            return self.fail("expected an index");
        };
        self.at += 1;
        self.expect(&Token::RBracket, "expected `]`")?;
        Ok(Next::Step(Step::Index(index)))
    }

    fn literal(&mut self) -> Result<Literal, FilterError> {
        let literal = match self.peek() {
            Some(Token::Num(n, _)) => Literal::Number(*n),
            Some(Token::Str(s)) => Literal::String(s.clone()),
            Some(Token::Word(w)) if w == "true" => Literal::Bool(true),
            Some(Token::Word(w)) if w == "false" => Literal::Bool(false),
            Some(Token::Word(w)) if w == "null" => Literal::Null,
            _ => return self.fail("expected a value"),
        };
        self.at += 1;
        Ok(literal)
    }

    fn pattern(&mut self) -> Result<Pattern, FilterError> {
        let pos = self.pos();
        let Some(Token::Str(source)) = self.bump() else {
            return Err(error(self.text, "expected a regex string", pos));
        };
        Pattern::new(&source).map_err(|_| error(self.text, "invalid regex", pos))
    }
}
