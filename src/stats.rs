use crate::size_class::{NUM_CLASSES, SizeClass};

/// Snapshot of cached blocks for one fixed size class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeClassStats {
    /// The size class described by this snapshot.
    pub class: SizeClass,
    /// Number of detached blocks currently cached for this class.
    pub blocks: usize,
    /// Total bytes currently cached for this class, using normalized block size.
    pub bytes: usize,
}

/// Snapshot of allocator-wide shared state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocatorStats {
    /// Total usable bytes reserved by the arena mapping.
    pub arena_capacity: usize,
    /// Unreserved bytes still available from the monotonic arena.
    pub arena_remaining: usize,
    /// Shared central-pool occupancy for each fixed size class.
    pub small_central: [SizeClassStats; NUM_CLASSES],
    /// Number of currently live large allocations.
    pub large_live_allocations: usize,
    /// Total block bytes held by currently live large allocations.
    pub large_live_bytes: usize,
    /// Number of reusable freed large blocks retained for future requests.
    pub large_free_blocks: usize,
    /// Total block bytes retained in the reusable large-block pool.
    pub large_free_bytes: usize,
}

impl AllocatorStats {
    #[must_use]
    /// Returns the total number of small cached blocks currently held in the shared central pool.
    pub fn total_small_central_blocks(&self) -> usize {
        self.small_central.iter().map(|stats| stats.blocks).sum()
    }

    #[must_use]
    /// Returns the total number of bytes represented by the shared central-pool caches.
    pub fn total_small_central_bytes(&self) -> usize {
        self.small_central.iter().map(|stats| stats.bytes).sum()
    }
}

/// Snapshot of one caller-owned thread cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadCacheStats {
    /// Whether this cache is currently bound to an allocator instance.
    pub is_bound: bool,
    /// Local cached blocks for each fixed size class.
    pub local: [SizeClassStats; NUM_CLASSES],
}

impl ThreadCacheStats {
    #[must_use]
    /// Returns the total number of small cached blocks currently held locally.
    pub fn total_local_blocks(&self) -> usize {
        self.local.iter().map(|stats| stats.blocks).sum()
    }

    #[must_use]
    /// Returns the total number of bytes represented by this cache's local block lists.
    pub fn total_local_bytes(&self) -> usize {
        self.local.iter().map(|stats| stats.bytes).sum()
    }
}
