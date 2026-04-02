use crate::size_class::{BLOCK_ALIGNMENT, SizeClass};

/// Configuration for arena capacity, alignment, and small-object cache sizing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocatorConfig {
    /// Total usable bytes reserved for the allocator arena.
    pub arena_size: usize,
    /// Required block-start alignment. Must be a power of two and at least `64`.
    pub alignment: usize,
    /// Target bytes to refill per size class when a local cache runs empty.
    pub refill_target_bytes: usize,
    /// Target bytes to retain locally per size class before draining to the central pool.
    pub local_cache_target_bytes: usize,
}

impl AllocatorConfig {
    #[must_use]
    /// Returns the refill batch size, in blocks, for `class`.
    pub const fn refill_count(&self, class: SizeClass) -> usize {
        let count = self.refill_target_bytes / class.block_size_for_alignment(self.alignment);
        if count == 0 { 1 } else { count }
    }

    #[must_use]
    /// Returns the local per-class block limit before a drain is triggered.
    pub const fn local_limit(&self, class: SizeClass) -> usize {
        let limit = self.local_cache_target_bytes / class.block_size_for_alignment(self.alignment);
        if limit < 2 { 2 } else { limit }
    }

    #[must_use]
    /// Returns the number of blocks drained per class when the local limit is exceeded.
    pub const fn drain_count(&self, class: SizeClass) -> usize {
        let count = self.refill_count(class) / 2;
        if count == 0 { 1 } else { count }
    }
}

impl Default for AllocatorConfig {
    /// Builds the default 1 GiB, 64-byte aligned allocator configuration.
    fn default() -> Self {
        Self {
            arena_size: 1 << 30,
            alignment: BLOCK_ALIGNMENT,
            refill_target_bytes: 32 << 10,
            local_cache_target_bytes: 64 << 10,
        }
    }
}
