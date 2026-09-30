//! Reading ahead on a second thread: while the parser works through one buffer, the next one
//! is read and UTF-8-checked in parallel (NFR-12).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::Scope;
use std::time::Duration;

use crate::error::ParseError;
use crate::json::stream::Utf8;
use crate::source::{Growth, Source, SourceError};

/// Buffers read ahead but not yet taken by the parser.
const AHEAD: usize = 2;

/// A filled buffer: data at `buf[start..end]`, which starts at absolute offset `at`. The
/// `start` bytes in front are room for the previous buffer's unfinished token.
pub(crate) struct Chunk {
    pub(crate) buf: Vec<u8>,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) at: u64,
    pub(crate) eof: bool,
    /// The first invalid UTF-8 up to the end of this chunk, when validating.
    pub(crate) utf8_error: Option<ParseError>,
}

/// The parser's end of the read-ahead thread.
pub(crate) struct Prefetch {
    chunks: Receiver<Result<Chunk, SourceError>>,
    spare: Sender<Vec<u8>>,
    /// Tells a following reader to stop waiting for the file to grow.
    stop: Arc<AtomicBool>,
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Prefetch {
    /// Starts reading `source` from the beginning in chunks of `size` bytes, checking UTF-8.
    pub(crate) fn spawn<'scope, 'env, R: Source + Sync>(
        scope: &'scope Scope<'scope, 'env>,
        source: &'env R,
        size: usize,
    ) -> Self {
        Self::start(scope, source, size, Mode::Json, Arc::default())
    }

    /// Like [`Self::spawn`], but each chunk ends after its last newline (the rest opens the
    /// next one), and UTF-8 is left to the caller: blocks of whole NDJSON lines. With
    /// `follow`, the reader waits at the end for the file to grow instead of ending (FO-2),
    /// until that flag is set.
    pub(crate) fn lines<'scope, 'env, R: Source + Sync>(
        scope: &'scope Scope<'scope, 'env>,
        source: &'env R,
        size: usize,
        follow: Option<Arc<AtomicBool>>,
    ) -> Self {
        let mode = if follow.is_some() {
            Mode::Follow
        } else {
            Mode::Lines
        };
        Self::start(scope, source, size, mode, follow.unwrap_or_default())
    }

    fn start<'scope, 'env, R: Source + Sync>(
        scope: &'scope Scope<'scope, 'env>,
        source: &'env R,
        size: usize,
        mode: Mode,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let (tx, chunks) = sync_channel(AHEAD);
        let (spare, spares) = channel();
        let reader = Reader {
            size,
            mode,
            stop: Arc::clone(&stop),
        };
        scope.spawn(move || reader.run(source, &tx, &spares));
        Self {
            chunks,
            spare,
            stop,
        }
    }

    /// The next chunk; `None` once the reader has stopped.
    pub(crate) fn next(&self) -> Option<Result<Chunk, SourceError>> {
        self.chunks.recv().ok()
    }

    /// Like [`Self::next`] when `wait`; otherwise only a chunk that is ready now.
    pub(crate) fn poll(&self, wait: bool) -> Option<Result<Chunk, SourceError>> {
        if wait {
            self.next()
        } else {
            self.chunks.try_recv().ok()
        }
    }

    /// Hands a used buffer back for reuse (big allocations are slow).
    pub(crate) fn recycle(&self, buf: Vec<u8>) {
        let _ = self.spare.send(buf);
    }
}

/// What the reader does besides reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Validate UTF-8.
    Json,
    /// Cut chunks after their last newline.
    Lines,
    /// Like `Lines`, and wait for more at the end instead of ending.
    Follow,
}

/// How often a following reader looks for growth.
const POLL: Duration = Duration::from_millis(250);

/// The reading thread.
struct Reader {
    size: usize,
    mode: Mode,
    stop: Arc<AtomicBool>,
}

impl Reader {
    /// Reads chunks until the end of `source`, a read error, or the parser hanging up.
    fn run<R: Source>(
        &self,
        source: &R,
        tx: &SyncSender<Result<Chunk, SourceError>>,
        spares: &Receiver<Vec<u8>>,
    ) {
        let mut utf8 = (self.mode == Mode::Json).then(Utf8::default);
        let (mut at, mut utf8_error, mut carry) = (0u64, None, Vec::new());
        loop {
            let mut buf = spares.try_recv().unwrap_or_default();
            buf.resize(2 * self.size, 0);
            let len = match source.read_into(at, &mut buf[self.size..]) {
                Ok(0) if self.mode == Mode::Follow => match self.wait(source) {
                    Ok(true) => continue,
                    Ok(false) => return,
                    Err(err) => return drop(tx.send(Err(err))),
                },
                Ok(len) => len,
                Err(err) => return drop(tx.send(Err(err))),
            };
            let eof = self.mode != Mode::Follow && at + len as u64 >= source.len();
            let data = &buf[self.size..self.size + len];
            utf8_error = utf8_error.or_else(|| check(utf8.as_mut(), data, at, eof));
            let chunk = self.chunk(buf, len, at, eof, &mut carry, utf8_error);
            if tx.send(Ok(chunk)).is_err() || eof {
                return;
            }
            at += len as u64;
        }
    }

    /// Waits at the end of a followed file: `true` once it grew, `false` when told to stop.
    fn wait<R: Source>(&self, source: &R) -> Result<bool, SourceError> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(false);
            }
            match source.refresh()? {
                Growth::Grew => return Ok(true),
                Growth::Shrank => return Err(SourceError::Truncated),
                Growth::Rotated => return Err(SourceError::Rotated),
                Growth::Same => std::thread::sleep(POLL),
            }
        }
    }

    /// The chunk for `len` bytes just read into `buf`: the carried partial line in front, and
    /// (for lines) the bytes after the last newline carried to the next chunk.
    fn chunk(
        &self,
        mut buf: Vec<u8>,
        len: usize,
        at: u64,
        eof: bool,
        carry: &mut Vec<u8>,
        utf8_error: Option<ParseError>,
    ) -> Chunk {
        let start = self.size - carry.len();
        buf[start..self.size].copy_from_slice(carry);
        let chunk_at = at - carry.len() as u64;
        carry.clear();
        let mut end = self.size + len;
        if self.mode != Mode::Json
            && !eof
            && let Some(nl) = memchr::memrchr(b'\n', &buf[self.size..end])
        {
            carry.extend_from_slice(&buf[self.size + nl + 1..end]);
            end = self.size + nl + 1;
        }
        Chunk {
            buf,
            start,
            end,
            at: chunk_at,
            eof,
            utf8_error,
        }
    }
}

/// The first UTF-8 error in `data` (and, at the end, a sequence left incomplete).
fn check(utf8: Option<&mut Utf8>, data: &[u8], at: u64, eof: bool) -> Option<ParseError> {
    let utf8 = utf8?;
    utf8.feed(data, at)
        .err()
        .or_else(|| eof.then(|| utf8.finish().err()).flatten())
}
