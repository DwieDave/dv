//! Incremental re-formatting of validated JSON bytes.

/// Output layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Minify,
    Pretty,
}

/// Streams raw JSON through a token-level formatter; any chunking gives the same output.
#[derive(Debug, Clone)]
pub struct Formatter {
    style: Style,
    depth: usize,
    in_string: bool,
    escaped: bool,
    /// Just saw `{` or `[`: the newline waits until we know the container is not empty.
    pending_open: bool,
}

impl Formatter {
    #[must_use]
    pub fn new(style: Style) -> Self {
        Self {
            style,
            depth: 0,
            in_string: false,
            escaped: false,
            pending_open: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        for &b in chunk {
            if self.in_string {
                self.string_byte(b);
                out.push(b);
            } else {
                self.structural(b, out);
            }
        }
    }

    fn string_byte(&mut self, b: u8) {
        match (self.escaped, b) {
            (true, _) => self.escaped = false,
            (false, b'\\') => self.escaped = true,
            (false, b'"') => self.in_string = false,
            _ => {}
        }
    }

    fn structural(&mut self, b: u8, out: &mut Vec<u8>) {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'{' | b'[' => {
                self.flush_pending(out);
                out.push(b);
                self.depth += 1;
                self.pending_open = true;
            }
            b'}' | b']' => self.close(b, out),
            b',' => {
                out.push(b);
                self.newline(out);
            }
            b':' => out.extend_from_slice(if self.style == Style::Pretty {
                b": "
            } else {
                b":"
            }),
            _ => {
                self.flush_pending(out);
                self.in_string = b == b'"';
                out.push(b);
            }
        }
    }

    fn close(&mut self, b: u8, out: &mut Vec<u8>) {
        self.depth = self.depth.saturating_sub(1);
        if !std::mem::take(&mut self.pending_open) {
            self.newline(out);
        }
        out.push(b);
    }

    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        if std::mem::take(&mut self.pending_open) {
            self.newline(out);
        }
    }

    fn newline(&self, out: &mut Vec<u8>) {
        if self.style == Style::Pretty {
            out.push(b'\n');
            out.resize(out.len() + 2 * self.depth, b' ');
        }
    }
}

#[cfg(test)]
mod tests;
