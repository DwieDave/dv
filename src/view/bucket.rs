//! Splits huge child ranges into nested buckets of at most `BUCKET` rows.

use std::ops::Range;

/// Maximum rows per level; a multiple of the checkpoint stride.
pub const BUCKET: u64 = 1024;

/// One row of a level: a single child or a bucket of children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Child(u64),
    Bucket(Range<u64>),
}

/// The rows shown for a range of child indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    range: Range<u64>,
    step: u64,
}

impl Level {
    /// Picks the smallest step (`1`, `BUCKET`, `BUCKET²`, …) that leaves at most `BUCKET` rows.
    #[must_use]
    pub fn of(range: Range<u64>) -> Self {
        let width = range.end.saturating_sub(range.start);
        let mut step = 1u64;
        while width.div_ceil(step) > BUCKET {
            step = step.saturating_mul(BUCKET);
        }
        Self { range, step }
    }

    /// Rows per bucket at this level (1 when rows are children).
    #[must_use]
    pub fn step(&self) -> u64 {
        self.step
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.range
            .end
            .saturating_sub(self.range.start)
            .div_ceil(self.step)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The row holding child `k`, and that row's bucket range when it is a bucket.
    #[must_use]
    pub fn locate(&self, k: u64) -> Option<(u64, Option<Range<u64>>)> {
        if !self.range.contains(&k) {
            return None;
        }
        let row = (k - self.range.start) / self.step;
        let bucket = match self.row(row)? {
            Row::Bucket(range) => Some(range),
            Row::Child(_) => None,
        };
        Some((row, bucket))
    }

    #[must_use]
    pub fn row(&self, i: u64) -> Option<Row> {
        if i >= self.len() {
            return None;
        }
        let start = self.range.start + i * self.step;
        Some(match self.step {
            1 => Row::Child(start),
            step => Row::Bucket(start..(start + step).min(self.range.end)),
        })
    }
}

#[cfg(test)]
mod tests;
