//! Reading ahead on a second thread: while the parser works through one buffer, the next one
//! is read and UTF-8-checked in parallel (NFR-12).

use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::Scope;

use crate::error::ParseError;
use crate::json::stream::Utf8;
use crate::source::{Source, SourceError};

/// Buffers read ahead but not yet taken by the parser.
const AHEAD: usize = 2;

/// A filled buffer: data at `buf[head..head + len]`, which starts at absolute offset `at`. The
/// `head` bytes in front are room for the previous buffer's unfinished token.
pub(crate) struct Chunk {
    pub(crate) buf: Vec<u8>,
    pub(crate) head: usize,
    pub(crate) len: usize,
    pub(crate) at: u64,
    pub(crate) eof: bool,
    /// The first invalid UTF-8 up to the end of this chunk, when validating.
    pub(crate) utf8_error: Option<ParseError>,
}

/// The parser's end of the read-ahead thread.
pub(crate) struct Prefetch {
    chunks: Receiver<Result<Chunk, SourceError>>,
    spare: Sender<Vec<u8>>,
}

impl Prefetch {
    /// Starts reading `source` from the beginning in chunks of `size` bytes.
    pub(crate) fn spawn<'scope, 'env, R: Source + Sync>(
        scope: &'scope Scope<'scope, 'env>,
        source: &'env R,
        size: usize,
        validate: bool,
    ) -> Self {
        let (tx, chunks) = sync_channel(AHEAD);
        let (spare, spares) = channel();
        scope.spawn(move || read_ahead(source, size, validate, &tx, &spares));
        Self { chunks, spare }
    }

    /// The next chunk; `None` once the reader has stopped.
    pub(crate) fn next(&self) -> Option<Result<Chunk, SourceError>> {
        self.chunks.recv().ok()
    }

    /// Hands a used buffer back for reuse (big allocations are slow).
    pub(crate) fn recycle(&self, buf: Vec<u8>) {
        let _ = self.spare.send(buf);
    }
}

/// Reads chunks until the end of `source`, a read error, or the parser hanging up.
fn read_ahead<R: Source>(
    source: &R,
    size: usize,
    validate: bool,
    tx: &SyncSender<Result<Chunk, SourceError>>,
    spares: &Receiver<Vec<u8>>,
) {
    let mut utf8 = validate.then(Utf8::default);
    let (mut at, mut utf8_error) = (0u64, None);
    loop {
        let mut buf = spares.try_recv().unwrap_or_default();
        buf.resize(2 * size, 0);
        let len = match source.read_into(at, &mut buf[size..]) {
            Ok(len) => len,
            Err(err) => return drop(tx.send(Err(err))),
        };
        let eof = at + len as u64 >= source.len();
        utf8_error = utf8_error.or_else(|| check(utf8.as_mut(), &buf[size..size + len], at, eof));
        let chunk = Chunk {
            buf,
            head: size,
            len,
            at,
            eof,
            utf8_error,
        };
        if tx.send(Ok(chunk)).is_err() || eof {
            return;
        }
        at += len as u64;
    }
}

/// The first UTF-8 error in `data` (and, at the end, a sequence left incomplete).
fn check(utf8: Option<&mut Utf8>, data: &[u8], at: u64, eof: bool) -> Option<ParseError> {
    let utf8 = utf8?;
    utf8.feed(data, at)
        .err()
        .or_else(|| eof.then(|| utf8.finish().err()).flatten())
}
