//! The spilled index built on its own thread: the parser only queues events (NFR-12).

use std::io;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::{self, JoinHandle};

use crate::index::live::LiveStore;
use crate::index::spill::{SpillBuilder, SpillLimits, SpillSlot, SpillStore};
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

/// A [`SpillBuilder`] on its own thread; calls are batched and replayed in order.
#[derive(Debug)]
pub struct BackgroundSpill {
    batch: Vec<Event>,
    tx: Option<SyncSender<Message>>,
    spare: Receiver<Vec<Event>>,
    published: Receiver<()>,
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
        let worker = thread::spawn(move || replay(builder, &rx, &spare_tx, &published_tx));
        Self {
            batch: Vec::with_capacity(BATCH),
            tx: Some(tx),
            spare,
            published,
            worker: Some(worker),
        }
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
        if let Some(tx) = &self.tx {
            let _ = tx.send(Message::Events(batch));
        }
    }

    /// Makes everything before `frontier` visible to readers; returns once it is.
    pub fn publish(&mut self, frontier: u64, done: bool) {
        self.send();
        if let Some(tx) = &self.tx
            && tx.send(Message::Publish { frontier, done }).is_ok()
        {
            let _ = self.published.recv();
        }
    }

    /// Waits for the builder thread and opens the store for reading.
    ///
    /// # Errors
    /// Write failures, or the builder thread dying.
    pub fn finish(mut self) -> Result<SpillStore, SourceError> {
        self.send();
        self.tx = None;
        let worker = self.worker.take().ok_or_else(gone)?;
        worker.join().map_err(|_| gone())?
    }
}

fn gone() -> SourceError {
    io::Error::other("the index builder thread stopped").into()
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
    published: &Sender<()>,
) -> Result<SpillStore, SourceError> {
    let mut mark = None;
    for message in rx {
        match message {
            Message::Events(mut events) => {
                for event in events.drain(..) {
                    apply(&mut builder, &mut mark, event);
                }
                let _ = spare.send(events);
            }
            Message::Publish { frontier, done } => {
                builder.publish(frontier, done);
                let _ = published.send(());
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
        builder.publish(5_000_000, false);
        assert_eq!(store.view().frontier, 5_000_000);
        assert!(store.node_at(4_999_900).unwrap().is_some());
        builder.finish().unwrap();
    }
}
