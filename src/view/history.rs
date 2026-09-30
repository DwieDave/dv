//! Back/forward over cursor jumps, like a browser's history.

use std::collections::VecDeque;

use crate::view::place::Place;

/// Entries kept in each direction.
const LIMIT: usize = 100;

/// Places behind and ahead of the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JumpList {
    back: VecDeque<Place>,
    forward: Vec<Place>,
}

impl JumpList {
    /// Remembers `from` as the place a jump left; the forward list is dropped.
    pub fn record(&mut self, from: Place) {
        if self.back.back() != Some(&from) {
            self.back.push_back(from);
        }
        if self.back.len() > LIMIT {
            self.back.pop_front();
        }
        self.forward.clear();
    }

    /// The previous place, leaving `current` to go forward to.
    pub fn back(&mut self, current: Place) -> Option<Place> {
        let to = self.back.pop_back()?;
        self.forward.push(current);
        Some(to)
    }

    /// The next place, leaving `current` to go back to.
    pub fn forward(&mut self, current: Place) -> Option<Place> {
        let to = self.forward.pop()?;
        self.back.push_back(current);
        Some(to)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum Op {
        Jump(u64),
        Back,
        Forward,
    }

    /// A browser: a list of visited places and the index of the current one.
    struct Browser {
        places: Vec<u64>,
        at: usize,
    }

    impl Browser {
        fn apply(&mut self, op: &Op) {
            match op {
                Op::Jump(to) if *to != self.places[self.at] => {
                    self.places.truncate(self.at + 1);
                    self.places.push(*to);
                    self.at += 1;
                }
                Op::Back => self.at = self.at.saturating_sub(1),
                Op::Forward => self.at = (self.at + 1).min(self.places.len() - 1),
                Op::Jump(_) => {}
            }
        }
    }

    proptest! {
        #[test]
        fn jumps_behave_like_a_browser_history(ops in prop::collection::vec(prop_oneof![
            (0u64..5).prop_map(Op::Jump), Just(Op::Back), Just(Op::Forward)
        ], 0..60)) {
            let (mut list, mut cursor) = (JumpList::default(), Place(0));
            let mut browser = Browser { places: vec![0], at: 0 };
            for op in &ops {
                match op {
                    Op::Jump(to) if Place(*to) != cursor => {
                        list.record(cursor);
                        cursor = Place(*to);
                    }
                    Op::Back => cursor = list.back(cursor).unwrap_or(cursor),
                    Op::Forward => cursor = list.forward(cursor).unwrap_or(cursor),
                    Op::Jump(_) => {}
                }
                browser.apply(op);
                prop_assert_eq!(cursor, Place(browser.places[browser.at]), "after {:?}", op);
            }
        }
    }

    #[test]
    fn the_list_keeps_the_latest_hundred() {
        let mut list = JumpList::default();
        for i in 0..150 {
            list.record(Place(i));
        }
        let mut cursor = Place(999);
        let mut steps = 0;
        while let Some(to) = list.back(cursor) {
            cursor = to;
            steps += 1;
        }
        assert_eq!((steps, cursor), (100, Place(50)));
    }
}
