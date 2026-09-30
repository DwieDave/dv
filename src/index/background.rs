//! The spilled index built on its own thread: the parser only queues events (NFR-12).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::index::live::LiveStore;
use crate::index::spill::{SpillBuilder, SpillLimits, SpillSlot, SpillStore, copy_error};
use crate::index::store::Builder;
use crate::source::SourceError;

/// Events per batch handed to the builder thread.
const BATCH: usize = 8192;
/// Batches queued before the parser waits for the builder.
const QUEUED: usize = 4;

/// One builder call, replayed in order on the builder thread.
#[derive(Debug, Clone, Copy)]
enum Event {
    Open(u64),
    Child(u64),
    Close(u64),
    Mark,
    Rollback,
}

enum Message {
    Events(Vec<Event>),
    Publish { frontier: u64, done: bool },
}

/// The first failure of a [`BackgroundSpill`], latched so the loader can stop at once.
#[derive(Debug, Default)]
pub struct Failure {
    set: AtomicBool,
    error: Mutex<Option<io::Error>>,
}

impl Failure {
    /// Latches `err` unless a failure is already latched.
    pub fn set(&self, err: io::Error) {
        if let Ok(mut slot) = self.error.lock() {
            slot.get_or_insert(err);
        }
        self.set.store(true, Ordering::Release);
    }

    /// Whether the builder failed or died.
    pub fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    /// The latched error, if any; later calls see an equivalent copy.
    pub fn error(&self) -> Option<io::Error> {
        let slot = self.error.lock().ok()?;
        slot.as_ref().map(copy_error)
    }
}

/// A [`SpillBuilder`] on its own thread; calls are batched and replayed in order.
///
/// Dropping it stops the thread and waits for it.
#[derive(Debug)]
pub struct BackgroundSpill {
    batch: Vec<Event>,
    tx: Option<SyncSender<Message>>,
    spare: Receiver<Vec<Event>>,
    published: Receiver<io::Result<()>>,
    failure: Arc<Failure>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<SpillStore, SourceError>>>,
}

impl BackgroundSpill {
    /// # Errors
    /// Temp file creation failures.
    pub fn new(limits: SpillLimits) -> Result<Self, SourceError> {
        Ok(Self::spawn(SpillBuilder::new(limits)?))
    }

    /// A builder whose progress readers can follow through the returned [`LiveStore`].
    ///
    /// # Errors
    /// Temp file creation failures.
    pub fn live(limits: SpillLimits) -> Result<(Self, LiveStore), SourceError> {
        let (builder, store) = SpillBuilder::live(limits)?;
        Ok((Self::spawn(builder), store))
    }

    fn spawn(builder: SpillBuilder) -> Self {
        let (tx, rx) = sync_channel(QUEUED);
        let (spare_tx, spare) = channel();
        let (published_tx, published) = channel();
        let failure = Arc::new(Failure::default());
        let cancelled = Arc::new(AtomicBool::new(false));
        let (latch, stop) = (Arc::clone(&failure), Arc::clone(&cancelled));
        let worker =
            thread::spawn(move || replay(builder, &rx, &spare_tx, &published_tx, &latch, &stop));
        Self {
            batch: Vec::with_capacity(BATCH),
            tx: Some(tx),
            spare,
            published,
            failure,
            cancelled,
            worker: Some(worker),
        }
    }

    /// The latch set when the builder fails or its thread dies.
    #[must_use]
    pub fn failure(&self) -> Arc<Failure> {
        Arc::clone(&self.failure)
    }

    fn push(&mut self, event: Event) {
        self.batch.push(event);
        if self.batch.len() >= BATCH {
            self.send();
        }
    }

    /// Hands the current batch to the builder thread.
    fn send(&mut self) {
        let fresh = self
            .spare
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(BATCH));
        let batch = std::mem::replace(&mut self.batch, fresh);
        if let Some(tx) = &self.tx
            && tx.send(Message::Events(batch)).is_err()
        {
            self.failure.set(gone());
        }
    }

    /// Makes everything before `frontier` visible to readers; returns once it is.
    ///
    /// # Errors
    /// The builder's first failure, or its thread dying. Nothing is published after it.
    pub fn publish(&mut self, frontier: u64, done: bool) -> io::Result<()> {
        self.send();
        let sent = self
            .tx
            .as_ref()
            .is_some_and(|tx| tx.send(Message::Publish { frontier, done }).is_ok());
        let reply = if sent {
            self.published.recv().ok()
        } else {
            None
        };
        match reply {
            Some(Ok(())) => Ok(()),
            Some(Err(err)) => {
                self.failure.set(copy_error(&err));
                Err(err)
            }
            None => {
                self.failure.set(gone());
                Err(self.failure.error().unwrap_or_else(gone))
            }
        }
    }

    /// Waits for the builder thread and opens the store for reading.
    ///
    /// # Errors
    /// Write failures, or the builder thread dying.
    pub fn finish(mut self) -> Result<SpillStore, SourceError> {
        self.send();
        self.tx = None;
        let worker = self
            .worker
            .take()
            .ok_or_else(|| SourceError::from(gone()))?;
        worker.join().map_err(|_| SourceError::from(gone()))?
    }
}

fn gone() -> io::Error {
    io::Error::other("the index builder thread stopped")
}

impl Drop for BackgroundSpill {
    fn drop(&mut self) {
        // Cancel: the thread skips what is still queued, then ends when the channel closes.
        if self.worker.is_some() {
            self.cancelled.store(true, Ordering::Release);
        }
        self.tx = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Builder for BackgroundSpill {
    type Slot = SpillSlot;
    type Mark = ();

    fn open(&mut self, start: u64) -> SpillSlot {
        self.push(Event::Open(start));
        SpillSlot
    }

    fn add_child(&mut self, _slot: &SpillSlot, offset: u64) {
        self.push(Event::Child(offset));
    }

    fn close(&mut self, _slot: SpillSlot, end: u64) {
        self.push(Event::Close(end));
    }

    fn mark(&mut self) {
        self.push(Event::Mark);
    }

    fn rollback(&mut self, (): ()) {
        self.push(Event::Rollback);
    }
}

/// The builder thread: applies batches in order until the parser hangs up.
fn replay(
    mut builder: SpillBuilder,
    rx: &Receiver<Message>,
    spare: &Sender<Vec<Event>>,
    published: &Sender<io::Result<()>>,
    failure: &Failure,
    cancelled: &AtomicBool,
) -> Result<SpillStore, SourceError> {
    let mut mark = None;
    for message in rx {
        match message {
            Message::Events(mut events) => {
                if !failure.is_set() && !cancelled.load(Ordering::Acquire) {
                    for event in events.drain(..) {
                        apply(&mut builder, &mut mark, event);
                    }
                    if let Some(err) = builder.failure() {
                        failure.set(err);
                    }
                }
                events.clear();
                let _ = spare.send(events);
            }
            Message::Publish { frontier, done } => {
                let reply = match failure.error() {
                    Some(err) => Err(err),
                    None => builder.publish(frontier, done),
                };
                if let Err(err) = &reply {
                    failure.set(copy_error(err));
                }
                let _ = published.send(reply);
            }
        }
    }
    builder.finish()
}

fn apply(
    builder: &mut SpillBuilder,
    mark: &mut Option<<SpillBuilder as Builder>::Mark>,
    event: Event,
) {
    match event {
        Event::Open(start) => {
            builder.open(start);
        }
        Event::Child(offset) => builder.add_child(&SpillSlot, offset),
        Event::Close(end) => builder.close(SpillSlot, end),
        Event::Mark => *mark = Some(builder.mark()),
        Event::Rollback => {
            if let Some(m) = *mark {
                builder.rollback(m);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use proptest::prelude::*;

    use super::*;
    use crate::index::lines::LineSpill;
    use crate::index::spill::SpillBuilder;
    use crate::index::store::NodeStore;
    use crate::json::lines_stream::parse_lines_stream;
    use crate::json::stream::{StreamLimits, parse_stream};
    use crate::source::MemSource;
    use crate::test_support::{json_value, layout, ndjson};

    const LIMITS: SpillLimits = SpillLimits {
        window: 4,
        stack: 4,
        cache: 4096,
    };

    fn same_nodes(a: &impl NodeStore, b: &impl NodeStore, len: usize) -> Result<(), TestCaseError> {
        for offset in 0..=len as u64 {
            let (x, y) = (a.node_at(offset).unwrap(), b.node_at(offset).unwrap());
            prop_assert_eq!(
                x.map(|n| (n.start, n.end, n.fanout.map(|f| f.count))),
                y.map(|n| (n.start, n.end, n.fanout.map(|f| f.count)))
            );
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn background_json_index_equals_the_direct_one(value in json_value()) {
            let (text, _) = layout(&value, " ");
            let source = MemSource::new(text.clone().into_bytes());
            let limits = StreamLimits { initial: 16, max: 1 << 20 };
            let go = |_| ControlFlow::Continue(());
            let direct = parse_stream(&source, SpillBuilder::new(LIMITS).unwrap(), limits, go, |_, _, _| {}).unwrap();
            let background = parse_stream(&source, BackgroundSpill::new(LIMITS).unwrap(), limits, go, |_, _, _| {}).unwrap();
            same_nodes(&direct.builder.finish().unwrap(), &background.builder.finish().unwrap(), text.len())?;
        }

        #[test]
        fn background_ndjson_index_equals_the_direct_one(bytes in ndjson()) {
            let source = MemSource::new(bytes.clone());
            let limits = StreamLimits { initial: 16, max: 1 << 20 };
            let go = |_| ControlFlow::Continue(());
            let lines = || LineSpill::new(4).unwrap();
            let direct = parse_lines_stream(&source, SpillBuilder::new(LIMITS).unwrap(), lines(), limits, go, |_, _, _, _| {}).unwrap();
            let background = parse_lines_stream(&source, BackgroundSpill::new(LIMITS).unwrap(), lines(), limits, go, |_, _, _, _| {}).unwrap();
            same_nodes(&direct.builder.finish().unwrap(), &background.builder.finish().unwrap(), bytes.len())?;
        }
    }

    #[test]
    fn publishing_waits_until_readers_see_the_frontier() {
        let (mut builder, store) = BackgroundSpill::live(LIMITS).unwrap();
        for i in 0..50_000u64 {
            builder.open(i * 100);
            builder.close(SpillSlot, i * 100 + 99);
        }
        builder.publish(5_000_000, false).unwrap();
        assert_eq!(store.view().frontier, 5_000_000);
        assert!(store.node_at(4_999_900).unwrap().is_some());
        builder.finish().unwrap();
    }

    #[test]
    fn a_write_failure_is_reported_by_publish_and_latched() {
        let (mut builder, _store) = SpillBuilder::live(LIMITS).unwrap();
        builder.break_writes();
        let mut background = BackgroundSpill::spawn(builder);
        let failure = background.failure();
        for i in 0..100u64 {
            background.open(i * 100);
            background.close(SpillSlot, i * 100 + 99);
        }
        assert!(background.publish(10_000, false).is_err());
        assert!(failure.is_set());
        assert!(background.publish(20_000, false).is_err());
        assert!(failure.error().is_some());
        assert!(background.finish().is_err());
    }

    #[test]
    fn a_dead_builder_thread_is_noticed_at_once() {
        let (tx, rx) = sync_channel(QUEUED);
        drop(rx);
        let (_spare_tx, spare) = channel();
        let (_published_tx, published) = channel();
        let mut background = BackgroundSpill {
            batch: Vec::new(),
            tx: Some(tx),
            spare,
            published,
            failure: Arc::new(Failure::default()),
            cancelled: Arc::new(AtomicBool::new(false)),
            worker: Some(thread::spawn(|| Err(gone().into()))),
        };
        let failure = background.failure();
        background.open(0);
        background.send();
        assert!(failure.is_set());
        assert!(background.publish(10, false).is_err());
    }

    #[test]
    fn dropping_the_builder_joins_its_thread() {
        let (mut background, _store) = BackgroundSpill::live(LIMITS).unwrap();
        let failure = background.failure();
        for i in 0..50_000u64 {
            background.open(i * 100);
            background.close(SpillSlot, i * 100 + 99);
        }
        drop(background);
        // The thread held the only other reference.
        assert_eq!(Arc::strong_count(&failure), 1);
    }
}
