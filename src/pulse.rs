//! Feedback from long-running jobs: cancellation checks and interim results.

use crate::schema::Collected;
use crate::search::Scanned;

/// Consulted while a job runs: whether to stop, and where interim results go.
pub trait Pulse {
    fn cancelled(&self) -> bool;

    /// How far a search has scanned.
    fn scanned(&self, _scanned: Scanned) {}

    /// The schema paths collected so far.
    fn schema(&self, _partial: Collected) {}

    /// Rows read so far while sorting a table.
    fn sorting(&self, _done: u64, _total: u64) {}
}

impl<F: Fn() -> bool> Pulse for F {
    fn cancelled(&self) -> bool {
        self()
    }
}
