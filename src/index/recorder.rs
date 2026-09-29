//! A builder that records its calls, to replay them into another builder later and in order
//! (parallel NDJSON parsing).

use crate::index::store::{Builder, MIN_NODE_LEN};

/// One recorded call; offsets are relative to the recorded block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Open(u64),
    Child(u64),
    Close(u64),
}

/// Records big containers only, like the real builders; small ones are dropped on close.
#[derive(Debug, Default)]
pub struct Recorder {
    events: Vec<Event>,
    /// Index of each open container's `Open` event.
    open: Vec<usize>,
}

impl Recorder {
    /// A recorder that reuses `events`' allocation.
    #[must_use]
    pub fn reusing(mut events: Vec<Event>) -> Self {
        events.clear();
        Self {
            events,
            open: Vec::new(),
        }
    }

    #[must_use]
    pub fn into_events(self) -> Vec<Event> {
        self.events
    }
}

impl Builder for Recorder {
    type Slot = ();
    type Mark = (usize, usize);

    fn open(&mut self, start: u64) {
        self.open.push(self.events.len());
        self.events.push(Event::Open(start));
    }

    fn add_child(&mut self, (): &(), offset: u64) {
        self.events.push(Event::Child(offset));
    }

    fn close(&mut self, (): (), end: u64) {
        let Some(at) = self.open.pop() else {
            return;
        };
        match self.events.get(at) {
            Some(Event::Open(start)) if end - start < MIN_NODE_LEN => self.events.truncate(at),
            _ => self.events.push(Event::Close(end)),
        }
    }

    fn mark(&mut self) -> (usize, usize) {
        (self.events.len(), self.open.len())
    }

    fn rollback(&mut self, (events, open): (usize, usize)) {
        self.events.truncate(events);
        self.open.truncate(open);
    }
}

/// Replays `events`, shifted by `base`, into `builder`.
pub fn replay<B: Builder>(builder: &mut B, events: &[Event], base: u64) {
    let mut slots = Vec::new();
    for event in events {
        match *event {
            Event::Open(start) => slots.push(builder.open(base + start)),
            Event::Child(offset) => {
                if let Some(slot) = slots.last() {
                    builder.add_child(slot, base + offset);
                }
            }
            Event::Close(end) => {
                if let Some(slot) = slots.pop() {
                    builder.close(slot, base + end);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use proptest::prelude::*;

    use super::*;
    use crate::index::store::{NodeStore, VecStoreBuilder};
    use crate::json::parse::{Parser, parse};
    use crate::test_support::{json_value, layout};

    proptest! {
        #[test]
        fn replayed_recordings_build_the_same_index(value in json_value(), pad in 0usize..5) {
            let (text, _) = layout(&value, " ");
            let expected = parse(text.as_bytes()).unwrap();
            let go = |_| ControlFlow::Continue(());
            let (_, recorder, _) = Parser::with_builder(text.as_bytes(), go, Recorder::default()).run_with().unwrap();
            let mut store = VecStoreBuilder::default();
            replay(&mut store, &recorder.into_events(), pad as u64);
            let store = store.finish();
            for offset in 0..text.len() as u64 {
                let (a, b) = (expected.store.node_at(offset).unwrap(), store.node_at(offset + pad as u64).unwrap());
                prop_assert_eq!(a.map(|n| (n.start + pad as u64, n.end + pad as u64)), b.map(|n| (n.start, n.end)));
            }
        }
    }
}
