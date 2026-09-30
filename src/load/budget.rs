//! Memory and pacing of a streaming load.

use crate::index::spill::SpillLimits;
use crate::index::to_usize;
use crate::json::stream::StreamLimits;

/// Memory and pacing for a streaming load.
#[derive(Debug, Clone, Copy)]
pub struct StreamBudget {
    /// Chunk cache of the parser's reads.
    pub parse_cache: u64,
    /// Chunk cache of each browsing view (live, then final).
    pub view_cache: u64,
    pub spill: SpillLimits,
    pub stream: StreamLimits,
    /// Input bytes between two publishes of the live index.
    pub publish_every: u64,
}

/// Input bytes between two publishes of the live index.
const PUBLISH_EVERY: u64 = 16 << 20;

/// Input bytes between two publishes of the live index in tests.
const PUBLISH_EVERY_TESTING: u64 = 16 << 10;

/// Streaming mode's default memory budget.
pub const DEFAULT_BUDGET: u64 = 512 << 20;

/// The smallest budget streaming honours: below it the longest-token buffer (1/4) would reject
/// ordinary documents.
pub const MIN_BUDGET: u64 = 64 << 20;

impl Default for StreamBudget {
    fn default() -> Self {
        Self::within(DEFAULT_BUDGET)
    }
}

impl StreamBudget {
    /// Caches and buffers sized to fit `total` bytes (`mode.memory_budget`): the parser's
    /// cache takes 1/16, each view 1/4, the spilled index 1/8, and the longest token 1/4.
    /// A `total` under [`MIN_BUDGET`] is raised to it.
    #[must_use]
    pub fn within(total: u64) -> Self {
        let total = total.max(MIN_BUDGET);
        let part = |n: u64| total / n;
        Self {
            parse_cache: part(16),
            view_cache: part(4),
            spill: SpillLimits {
                cache: part(8),
                ..SpillLimits::default()
            },
            stream: StreamLimits {
                initial: to_usize(part(128)),
                max: to_usize(part(4)),
            },
            publish_every: PUBLISH_EVERY,
        }
    }

    /// Small buffers and frequent publishes, for tests.
    #[must_use]
    pub fn testing() -> Self {
        Self {
            parse_cache: 1 << 20,
            view_cache: 1 << 20,
            spill: SpillLimits {
                window: 64,
                stack: 64,
                cache: 1 << 20,
            },
            stream: StreamLimits {
                initial: 4096,
                max: 1 << 20,
            },
            publish_every: PUBLISH_EVERY_TESTING,
        }
    }
}
