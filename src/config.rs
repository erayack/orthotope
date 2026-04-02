use crate::size_class::{BLOCK_ALIGNMENT, SizeClass};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocatorConfig {
    pub arena_size: usize,
    pub alignment: usize,
    pub refill_target_bytes: usize,
    pub local_cache_target_bytes: usize,
}

impl AllocatorConfig {
    #[must_use]
    pub const fn refill_count(&self, class: SizeClass) -> usize {
        let count = self.refill_target_bytes / class.block_size();
        if count == 0 { 1 } else { count }
    }

    #[must_use]
    pub const fn local_limit(&self, class: SizeClass) -> usize {
        let limit = self.local_cache_target_bytes / class.block_size();
        if limit < 2 { 2 } else { limit }
    }

    #[must_use]
    pub const fn drain_count(&self, class: SizeClass) -> usize {
        let count = self.refill_count(class) / 2;
        if count == 0 { 1 } else { count }
    }
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self {
            arena_size: 1 << 30,
            alignment: BLOCK_ALIGNMENT,
            refill_target_bytes: 32 << 10,
            local_cache_target_bytes: 64 << 10,
        }
    }
}
