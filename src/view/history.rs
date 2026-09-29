//! Back/forward over cursor jumps, like a browser's history (HI-1).

/// Entries kept in each direction.
const LIMIT: usize = 100;

/// Row paths behind and ahead of the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JumpList {
    back: Vec<Vec<u64>>,
    forward: Vec<Vec<u64>>,
}

impl JumpList {
    /// Remembers `from` as the place a jump left; the forward list is dropped.
    pub fn record(&mut self, from: Vec<u64>) {
        if self.back.last() != Some(&from) {
            self.back.push(from);
        }
        if self.back.len() > LIMIT {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    /// The previous place, leaving `current` to go forward to.
    pub fn back(&mut self, current: Vec<u64>) -> Option<Vec<u64>> {
        let to = self.back.pop()?;
        self.forward.push(current);
        Some(to)
    }

    /// The next place, leaving `current` to go back to.
    pub fn forward(&mut self, current: Vec<u64>) -> Option<Vec<u64>> {
        let to = self.forward.pop()?;
        self.back.push(current);
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
            let (mut list, mut cursor) = (JumpList::default(), vec![0]);
            let mut browser = Browser { places: vec![0], at: 0 };
            for op in &ops {
                match op {
                    Op::Jump(to) if vec![*to] != cursor => {
                        list.record(cursor.clone());
                        cursor = vec![*to];
                    }
                    Op::Back => cursor = list.back(cursor.clone()).unwrap_or(cursor),
                    Op::Forward => cursor = list.forward(cursor.clone()).unwrap_or(cursor),
                    Op::Jump(_) => {}
                }
                browser.apply(op);
                prop_assert_eq!(&cursor, &vec![browser.places[browser.at]], "after {:?}", op);
            }
        }
    }

    #[test]
    fn the_list_keeps_the_latest_hundred() {
        let mut list = JumpList::default();
        for i in 0..150 {
            list.record(vec![i]);
        }
        let mut cursor = vec![999];
        let mut steps = 0;
        while let Some(to) = list.back(cursor.clone()) {
            cursor = to;
            steps += 1;
        }
        assert_eq!((steps, cursor), (100, vec![50]));
    }
}
