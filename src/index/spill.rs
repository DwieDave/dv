//! A node store spilled to temporary files, for documents larger than memory (FR-23, D-14).

use std::cmp::Ordering;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use std::sync::{Arc, RwLock};

use tempfile::tempfile;

use crate::index::live::{LiveStore, LiveView, OpenNode, Shared};

use crate::index::store::{BigNode, Builder, CHECKPOINT_EVERY, Fanout, MIN_NODE_LEN, NodeStore};
use crate::index::to_usize;
use crate::index::u64file::U64File;
use crate::source::file::FileSource;
use crate::source::{Source, SourceError};

/// Bytes per node record: start, len, child count, first checkpoint.
pub const RECORD: u64 = 32;

/// Tuning for the builder's RAM use.
#[derive(Debug, Clone, Copy)]
pub struct SpillLimits {
    /// Records kept in RAM before a sequential flush.
    pub window: usize,
    /// Open checkpoints kept in RAM before spilling.
    pub stack: usize,
    /// Read-cache budget for the finished store.
    pub cache: u64,
}

impl Default for SpillLimits {
    fn default() -> Self {
        Self {
            window: 64 << 10,
            stack: 1 << 20,
            cache: 64 << 20,
        }
    }
}

/// A container being built; `Slot` for the parser.
#[derive(Debug)]
pub struct SpillSlot(u64);

/// Builder state captured by `mark`.
#[derive(Debug, Clone, Copy)]
pub struct SpillMark {
    next: u64,
    open: usize,
    stack: u64,
    cps: u64,
}

#[derive(Debug, Clone, Copy)]
struct Open {
    slot: u64,
    start: u64,
    count: u64,
    cp_base: u64,
}

/// Writes node records by preorder slot and checkpoints in close order.
#[derive(Debug)]
pub struct SpillBuilder {
    next: u64,
    open: Vec<Open>,
    records: Records,
    /// Checkpoints of open containers; each one's run is contiguous from its `cp_base`.
    stack: U64File,
    /// Checkpoints of finished big containers.
    cps: U64File,
    cache: u64,
    error: Option<io::Error>,
    /// Set when readers follow the build; all file writes then wait for `publish`.
    live: Option<Arc<Shared>>,
}

impl SpillBuilder {
    /// # Errors
    /// Temp file creation failures.
    pub fn new(limits: SpillLimits) -> Result<Self, SourceError> {
        Ok(Self {
            next: 0,
            open: Vec::new(),
            records: Records::new(tempfile()?, limits.window),
            stack: U64File::new(tempfile()?, limits.stack),
            cps: U64File::new(tempfile()?, limits.stack),
            cache: limits.cache,
            error: None,
            live: None,
        })
    }

    /// Flushes everything and opens the store for reading.
    ///
    /// # Errors
    /// The first write failure, or failures opening the readers.
    pub fn finish(mut self) -> Result<SpillStore, SourceError> {
        self.records
            .flush()
            .and_then(|()| self.cps.flush())
            .unwrap_or_else(|err| self.fail(err));
        if let Some(err) = self.error {
            return Err(err.into());
        }
        let nodes = FileSource::new(self.records.file, self.cache / 2)?;
        let cps = FileSource::new(self.cps.file, self.cache / 2)?;
        Ok(SpillStore {
            nodes,
            cps,
            count: self.next,
        })
    }

    fn fail(&mut self, err: io::Error) {
        self.error.get_or_insert(err);
    }

    fn record(&mut self, open: Open, end: u64) -> io::Result<()> {
        let first = if open.count > CHECKPOINT_EVERY {
            let run = self.stack.read(open.cp_base..self.stack.len())?;
            let first = self.cps.len();
            run.into_iter().try_for_each(|cp| self.cps.push(cp))?;
            first
        } else {
            0
        };
        self.records.put(
            open.slot,
            encode([open.start, end - open.start, open.count, first]),
        )
    }
}

impl Builder for SpillBuilder {
    type Slot = SpillSlot;
    type Mark = SpillMark;

    fn open(&mut self, start: u64) -> SpillSlot {
        let slot = self.next;
        self.next += 1;
        self.open.push(Open {
            slot,
            start,
            count: 0,
            cp_base: self.stack.len(),
        });
        if self.live.is_some() {
            let provisional = self.records.put(slot, encode([start, 0, 0, 0]));
            provisional.unwrap_or_else(|err| self.fail(err));
        }
        SpillSlot(slot)
    }

    fn start(&self, slot: &SpillSlot) -> u64 {
        self.open
            .iter()
            .rev()
            .find(|o| o.slot == slot.0)
            .map_or(0, |o| o.start)
    }

    fn add_child(&mut self, _slot: &SpillSlot, offset: u64) {
        let Some(top) = self.open.last_mut() else {
            return;
        };
        let checkpoint = top.count.is_multiple_of(CHECKPOINT_EVERY);
        top.count += 1;
        if checkpoint {
            let pushed = self.stack.push(offset);
            pushed.unwrap_or_else(|err| self.fail(err));
        }
    }

    fn close(&mut self, _slot: SpillSlot, end: u64) {
        let Some(open) = self.open.pop() else { return };
        if end - open.start < MIN_NODE_LEN {
            if open.slot + 1 == self.next {
                self.next = open.slot;
                self.records.rewind(self.next);
            }
        } else {
            let recorded = self.record(open, end);
            recorded.unwrap_or_else(|err| self.fail(err));
        }
        self.stack.truncate(open.cp_base);
    }

    fn mark(&self) -> SpillMark {
        SpillMark {
            next: self.next,
            open: self.open.len(),
            stack: self.stack.len(),
            cps: self.cps.len(),
        }
    }

    fn rollback(&mut self, mark: SpillMark) {
        self.next = mark.next;
        self.records.rewind(mark.next);
        self.open.truncate(mark.open);
        self.stack.truncate(mark.stack);
        self.cps.truncate(mark.cps);
    }
}

fn encode(fields: [u64; 4]) -> [u8; 32] {
    let mut out = [0; 32];
    for (chunk, field) in out.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
        *chunk = field.to_le_bytes();
    }
    out
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    if let Some(src) = bytes.get(at..at + 8) {
        word.copy_from_slice(src);
    }
    u64::from_le_bytes(word)
}

/// Node records by slot: recent slots in RAM, flushed sequentially; late closes written in place.
#[derive(Debug)]
struct Records {
    file: File,
    /// First slot held in `window`.
    low: u64,
    window: Vec<[u8; 32]>,
    limit: usize,
    /// Hold every write until `flush` (live mode).
    defer: bool,
    /// Late closes waiting for `flush` in live mode.
    late: Vec<(u64, [u8; 32])>,
}

impl Records {
    fn new(file: File, limit: usize) -> Self {
        Self {
            file,
            low: 0,
            window: Vec::new(),
            limit: limit.max(1),
            defer: false,
            late: Vec::new(),
        }
    }

    fn put(&mut self, slot: u64, record: [u8; 32]) -> io::Result<()> {
        if slot < self.low && self.defer {
            self.late.push((slot, record));
            return Ok(());
        }
        if slot < self.low {
            return self.file.write_all_at(&record, slot * RECORD);
        }
        let i = to_usize(slot - self.low);
        if self.window.len() <= i {
            self.window.resize(i + 1, [0; 32]);
        }
        self.window[i] = record;
        if self.window.len() >= self.limit && !self.defer {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Slots from `next` on are free again.
    fn rewind(&mut self, next: u64) {
        if next < self.low {
            self.low = next;
            self.window.clear();
        } else {
            self.window.truncate(to_usize(next - self.low));
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        for (slot, record) in std::mem::take(&mut self.late) {
            self.file.write_all_at(&record, slot * RECORD)?;
        }
        let bytes: Vec<u8> = self.window.iter().flatten().copied().collect();
        self.file.write_all_at(&bytes, self.low * RECORD)?;
        self.low += self.window.len() as u64;
        self.window.clear();
        Ok(())
    }
}

/// The finished store, read through bounded caches.
#[derive(Debug)]
pub struct SpillStore {
    nodes: FileSource,
    cps: FileSource,
    count: u64,
}

impl SpillStore {
    fn record(&self, index: u64) -> Result<[u64; 4], SourceError> {
        let bytes = self.nodes.read(index * RECORD..(index + 1) * RECORD)?;
        Ok([0, 8, 16, 24].map(|at| u64_at(&bytes, at)))
    }
}

impl NodeStore for SpillStore {
    fn node_at(&self, start: u64) -> Result<Option<BigNode>, SourceError> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let [node_start, len, count, first] = self.record(mid)?;
            match node_start.cmp(&start) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => {
                    let fanout = (count > CHECKPOINT_EVERY).then(|| Fanout::new(count, first));
                    return Ok(Some(BigNode {
                        start,
                        end: start + len,
                        fanout,
                    }));
                }
            }
        }
        Ok(None)
    }

    fn checkpoint(&self, fanout: &Fanout, k: u64) -> Result<Option<u64>, SourceError> {
        if k >= fanout.checkpoints() {
            return Ok(None);
        }
        let at = (fanout.first() + k) * 8;
        Ok(Some(u64_at(&self.cps.read(at..at + 8)?, 0)))
    }
}

impl SpillBuilder {
    /// A builder whose progress readers can follow through the returned [`LiveStore`].
    ///
    /// # Errors
    /// Temp file creation failures.
    pub fn live(limits: SpillLimits) -> Result<(Self, LiveStore), SourceError> {
        let mut builder = Self::new(limits)?;
        builder.records.defer = true;
        builder.stack.defer = true;
        builder.cps.defer = true;
        let shared = Shared {
            view: RwLock::new(LiveView::default()),
            records: builder.records.file.try_clone()?,
            cps: builder.cps.file.try_clone()?,
            stack: builder.stack.file.try_clone()?,
        };
        let shared = Arc::new(shared);
        builder.live = Some(Arc::clone(&shared));
        Ok((builder, LiveStore { shared }))
    }

    /// Makes everything parsed before `frontier` visible to readers.
    pub fn publish(&mut self, frontier: u64, done: bool) {
        let Some(shared) = self.live.clone() else {
            return;
        };
        let Ok(mut view) = shared.view.write() else {
            return;
        };
        let flushed = self
            .records
            .flush()
            .and_then(|()| self.cps.flush())
            .and_then(|()| self.stack.flush());
        flushed.unwrap_or_else(|err| self.fail(err));
        let opens = self
            .open
            .iter()
            .map(|o| OpenNode {
                slot: o.slot,
                start: o.start,
                count: o.count,
                cp_base: o.cp_base,
            })
            .collect();
        *view = LiveView {
            next: self.next,
            frontier,
            opens,
            done,
        };
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use proptest::prelude::*;

    use super::*;
    use crate::index::store::VecStoreBuilder;
    use crate::json::parse::Parser;
    use crate::test_support::{json_value, layout};

    fn both(text: &str, limits: SpillLimits) -> (crate::index::store::VecStore, SpillStore) {
        let hook = |_| ControlFlow::Continue(());
        let (_, vec, _) = Parser::with_builder(text.as_bytes(), hook, VecStoreBuilder::default())
            .run_with()
            .unwrap();
        let (_, spill, _) =
            Parser::with_builder(text.as_bytes(), hook, SpillBuilder::new(limits).unwrap())
                .run_with()
                .unwrap();
        (vec.finish(), spill.finish().unwrap())
    }

    fn checkpoints(store: &impl NodeStore, node: Option<BigNode>) -> Option<Vec<u64>> {
        let fanout = node?.fanout?;
        Some(
            (0..fanout.checkpoints())
                .map(|k| store.checkpoint(&fanout, k).unwrap().unwrap())
                .collect(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn spilled_store_equals_the_in_memory_store(value in json_value(), window in 1usize..8, stack in 1usize..8) {
            let (text, _) = layout(&value, " ");
            let (vec, spill) = both(&text, SpillLimits { window, stack, cache: 4096 });
            for offset in 0..=text.len() as u64 {
                let (a, b) = (vec.node_at(offset).unwrap(), spill.node_at(offset).unwrap());
                prop_assert_eq!(a.map(|n| (n.start, n.end)), b.map(|n| (n.start, n.end)), "offset {}", offset);
                prop_assert_eq!(a.and_then(|n| n.fanout.map(|f| f.count)), b.and_then(|n| n.fanout.map(|f| f.count)));
                prop_assert_eq!(checkpoints(&vec, a), checkpoints(&spill, b));
            }
        }
    }

    fn live_checks(text: &str, buffer: usize) {
        use crate::index::live::NodeState;
        use crate::json::stream::{StreamLimits, parse_stream};
        use crate::source::MemSource;
        let final_store = Parser::with_builder(
            text.as_bytes(),
            |_| ControlFlow::Continue(()),
            VecStoreBuilder::default(),
        )
        .run_with()
        .unwrap()
        .1
        .finish();
        let (builder, live) = SpillBuilder::live(SpillLimits {
            window: 2,
            stack: 2,
            cache: 4096,
        })
        .unwrap();
        let limits = StreamLimits {
            initial: buffer,
            max: 1 << 20,
        };
        let source = MemSource::new(text.as_bytes().to_vec());
        let publish = |b: &mut SpillBuilder, frontier: u64, last: bool| {
            b.publish(frontier, last);
            let view = live.view();
            assert_eq!(view.frontier, frontier);
            for (start, byte) in text
                .bytes()
                .enumerate()
                .take(crate::index::to_usize(frontier))
            {
                if !matches!(byte, b'{' | b'[') {
                    continue;
                }
                let start = start as u64;
                let expected = final_store.node_at(start).unwrap();
                match live.node(start).unwrap() {
                    Some(NodeState::Closed(node)) => {
                        assert_eq!(Some(node), expected, "closed node at {start}");
                    }
                    Some(NodeState::Open { .. }) => assert!(
                        view.opens.iter().any(|o| o.start == start),
                        "open node {start} not listed"
                    ),
                    None => assert!(
                        expected.is_none_or(|n| n.end > frontier),
                        "node at {start} closed before {frontier} but missing"
                    ),
                }
            }
        };
        let parsed = parse_stream(
            &source,
            builder,
            limits,
            |_| ControlFlow::Continue(()),
            publish,
        )
        .unwrap();
        let _ = parsed;
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn live_store_agrees_with_the_final_index(value in json_value(), buffer in 1usize..48) {
            let (text, _) = layout(&value, " ");
            live_checks(&text, buffer);
        }
    }
}
