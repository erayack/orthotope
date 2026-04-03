use core::ptr::NonNull;
use std::vec::Vec;

use crate::allocator::Allocator;
use crate::central_pool::CentralPool;
use crate::config::AllocatorConfig;
use crate::free_list::{Batch, FreeList};
use crate::size_class::{NUM_CLASSES, SizeClass};
use crate::stats::{SizeClassStats, ThreadCacheStats};

/// Caller-owned per-thread cache for small-object reuse.
///
/// Use one `ThreadCache` per participating thread when calling [`Allocator`] methods
/// directly. The process-global convenience API manages this internally.
pub struct ThreadCache {
    classes: [LocalClassCache; NUM_CLASSES],
    config: AllocatorConfig,
    owner: Option<usize>,
}

struct LocalClassCache {
    slabs: Vec<LocalSlab>,
    available_slabs: Vec<usize>,
    shared: FreeList,
    shared_len: usize,
    total_len: usize,
}

struct LocalSlab {
    start: NonNull<u8>,
    end_addr: usize,
    block_size: usize,
    capacity: usize,
    next_fresh: usize,
    free: FreeList,
}

impl ThreadCache {
    /// Creates a caller-owned thread-local cache for use with a specific [`Allocator`].
    ///
    /// Pair one `ThreadCache` with one thread when using the instance-oriented API
    /// directly. The global convenience API manages this cache internally.
    #[must_use]
    pub fn new(config: AllocatorConfig) -> Self {
        Self {
            classes: core::array::from_fn(|_| LocalClassCache::new()),
            config,
            owner: None,
        }
    }

    pub(crate) fn bind_to_allocator(&mut self, allocator: &Allocator) {
        let owner = allocator.id();
        if self.owner == Some(owner) {
            return;
        }

        assert!(
            self.is_empty(),
            "thread cache cannot be reused across allocators while it still holds cached blocks"
        );

        self.config = *allocator.config();
        self.owner = Some(owner);
    }

    #[must_use]
    fn is_empty(&self) -> bool {
        self.classes.iter().all(LocalClassCache::is_empty)
    }

    #[must_use]
    pub(crate) fn pop(&mut self, class: SizeClass) -> Option<NonNull<u8>> {
        // SAFETY: `ThreadCache` has exclusive mutable access to the class cache.
        unsafe { self.classes[class.index()].pop_block() }
    }

    /// Pushes one block into the local free list for `class`.
    ///
    /// # Safety
    ///
    /// `block` must be a valid detached allocator block for `class`, large enough for
    /// the intrusive free-list node and not linked in any other list.
    pub(crate) unsafe fn push(&mut self, class: SizeClass, block: NonNull<u8>) {
        // SAFETY: the caller guarantees `block` is a valid detached node for this class,
        // and `&mut self` provides exclusive access to the destination class cache.
        unsafe { self.classes[class.index()].push_block(block) };
    }

    #[must_use]
    pub(crate) const fn needs_refill(&self, class: SizeClass) -> bool {
        self.classes[class.index()].is_empty()
    }

    pub(crate) fn refill_from_central(&mut self, class: SizeClass, central: &CentralPool) -> usize {
        let batch = central.take_batch(class, self.config.refill_count(class));
        let moved = batch.len();

        // SAFETY: the batch came from the central pool as a detached chain for this class,
        // and `&mut self` gives exclusive access to the destination class cache.
        unsafe { self.classes[class.index()].push_shared_batch(batch) };

        moved
    }

    #[must_use]
    pub(crate) const fn should_drain(&self, class: SizeClass) -> bool {
        self.classes[class.index()].len() > self.config.local_limit(class)
    }

    pub(crate) fn push_owned_slab(
        &mut self,
        class: SizeClass,
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
    ) {
        self.classes[class.index()].push_owned_slab(start, block_size, capacity);
    }

    /// Drains one configured batch from the local cache back to the central pool.
    ///
    /// # Safety
    ///
    /// Every node currently linked in the local list for `class` must be a valid block
    /// for that size class and must belong exclusively to this cache.
    pub(crate) unsafe fn drain_excess_to_central(
        &mut self,
        class: SizeClass,
        central: &CentralPool,
    ) -> usize {
        if !self.should_drain(class) {
            return 0;
        }

        // SAFETY: the caller guarantees the local class cache holds only valid nodes for
        // this class, and `&mut self` ensures exclusive access during detachment.
        let batch =
            unsafe { self.classes[class.index()].pop_batch(self.config.drain_count(class)) };
        let moved = batch.len();

        // SAFETY: the detached batch originated from this class list and remains valid
        // to splice into the matching class list in the central pool.
        unsafe {
            central.return_batch(class, batch);
        }

        moved
    }

    /// Drains all currently cached blocks from every size class into the central pool.
    ///
    /// # Safety
    ///
    /// Every node currently linked in this cache must be a valid allocator block large
    /// enough for the free-list node and linked in at most one list.
    #[allow(dead_code)]
    pub(crate) unsafe fn drain_all_to_central(&mut self, central: &CentralPool) {
        for class in SizeClass::ALL {
            loop {
                let batch = {
                    let class_cache = &mut self.classes[class.index()];
                    // SAFETY: `&mut self` guarantees exclusive access to the full class
                    // cache while detached batches are removed until it is empty.
                    unsafe { class_cache.pop_batch(class_cache.len()) }
                };

                if batch.is_empty() {
                    break;
                }

                // SAFETY: each detached batch came from the matching local class cache.
                unsafe {
                    central.return_batch(class, batch);
                }
            }
        }
    }

    #[must_use]
    /// Returns a best-effort snapshot of this thread cache's local occupancy.
    pub fn stats(&self) -> ThreadCacheStats {
        let local = core::array::from_fn(|index| {
            let class = SizeClass::ALL[index];
            let blocks = self.classes[index].len();
            SizeClassStats {
                class,
                blocks,
                bytes: blocks * self.config.class_block_size(class),
            }
        });

        ThreadCacheStats {
            is_bound: self.owner.is_some(),
            local,
        }
    }
}

impl LocalClassCache {
    const fn new() -> Self {
        Self {
            slabs: Vec::new(),
            available_slabs: Vec::new(),
            shared: FreeList::new(),
            shared_len: 0,
            total_len: 0,
        }
    }

    const fn len(&self) -> usize {
        self.total_len
    }

    const fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    fn push_owned_slab(&mut self, start: NonNull<u8>, block_size: usize, capacity: usize) {
        let index = self.slabs.len();
        self.slabs.push(LocalSlab::new(start, block_size, capacity));
        self.available_slabs.push(index);
        self.total_len = self
            .total_len
            .checked_add(capacity)
            .unwrap_or_else(|| unreachable!("local class cache size overflowed"));
    }

    unsafe fn pop_block(&mut self) -> Option<NonNull<u8>> {
        while let Some(&index) = self.available_slabs.last() {
            let slab = &mut self.slabs[index];
            // SAFETY: the slab owns its free storage exclusively while borrowed mutably here.
            if let Some(block) = unsafe { slab.pop_block() } {
                self.total_len -= 1;
                if !slab.has_available_blocks() {
                    let _ = self.available_slabs.pop();
                }
                return Some(block);
            }

            let _ = self.available_slabs.pop();
        }

        // SAFETY: the caller guarantees the shared list only contains valid detached nodes.
        let block = unsafe { self.shared.pop_block() };
        if block.is_some() {
            self.shared_len -= 1;
            self.total_len -= 1;
        }
        block
    }

    unsafe fn push_block(&mut self, block: NonNull<u8>) {
        if let Some(index) = self.find_slab(block) {
            let slab = &mut self.slabs[index];
            let was_empty = !slab.has_available_blocks();
            // SAFETY: the caller guarantees `block` is detached and valid for this class.
            unsafe { slab.push_block(block) };
            if was_empty {
                self.available_slabs.push(index);
            }
            self.total_len += 1;
            return;
        }

        // SAFETY: `block` remains a valid detached node for this class in the shared list.
        unsafe { self.shared.push_block(block) };
        self.shared_len += 1;
        self.total_len += 1;
    }

    unsafe fn push_shared_batch(&mut self, batch: Batch) {
        let len = batch.len();
        // SAFETY: the caller guarantees `batch` is a valid detached chain for this class.
        unsafe { self.shared.push_batch(batch) };
        self.shared_len += len;
        self.total_len += len;
    }

    unsafe fn pop_batch(&mut self, max: usize) -> Batch {
        if max == 0 {
            return Batch::empty();
        }

        if self.shared_len != 0 {
            // SAFETY: the shared list contains only valid detached nodes for this class.
            let batch = unsafe { self.shared.pop_batch(max) };
            let len = batch.len();
            self.shared_len -= len;
            self.total_len -= len;
            return batch;
        }

        while let Some(&index) = self.available_slabs.last() {
            let slab = &mut self.slabs[index];
            if slab.has_available_blocks() {
                // SAFETY: each slab free storage belongs exclusively to this class cache.
                let batch = unsafe { slab.pop_batch(max) };
                let len = batch.len();
                self.total_len -= len;
                if !slab.has_available_blocks() {
                    let _ = self.available_slabs.pop();
                }
                return batch;
            }

            let _ = self.available_slabs.pop();
        }

        Batch::empty()
    }

    fn find_slab(&self, block: NonNull<u8>) -> Option<usize> {
        let addr = block.as_ptr().addr();
        let index = self
            .slabs
            .partition_point(|slab| slab.start.as_ptr().addr() <= addr);
        index
            .checked_sub(1)
            .filter(|&index| self.slabs[index].contains(block))
    }
}

impl LocalSlab {
    fn new(start: NonNull<u8>, block_size: usize, capacity: usize) -> Self {
        let slab_bytes = block_size
            .checked_mul(capacity)
            .unwrap_or_else(|| unreachable!("local slab size overflowed"));
        let end_addr = start
            .as_ptr()
            .addr()
            .checked_add(slab_bytes)
            .unwrap_or_else(|| unreachable!("local slab end overflowed"));

        Self {
            start,
            end_addr,
            block_size,
            capacity,
            next_fresh: 0,
            free: FreeList::new(),
        }
    }

    const fn has_available_blocks(&self) -> bool {
        self.next_fresh < self.capacity || !self.free.is_empty()
    }

    fn contains(&self, block: NonNull<u8>) -> bool {
        let addr = block.as_ptr().addr();
        let start = self.start.as_ptr().addr();

        addr >= start && addr < self.end_addr && (addr - start).is_multiple_of(self.block_size)
    }

    unsafe fn pop_block(&mut self) -> Option<NonNull<u8>> {
        if !self.free.is_empty() {
            // SAFETY: the slab owns this free list exclusively through `&mut self`.
            return unsafe { self.free.pop_block() };
        }

        if self.next_fresh < self.capacity {
            let offset = self
                .next_fresh
                .checked_mul(self.block_size)
                .unwrap_or_else(|| unreachable!("fresh slab offset overflowed"));
            self.next_fresh += 1;
            let block = self.start.as_ptr().wrapping_add(offset);
            // SAFETY: `offset` remains within the slab bounds and `start` is non-null.
            return Some(unsafe { NonNull::new_unchecked(block) });
        }

        None
    }

    unsafe fn push_block(&mut self, block: NonNull<u8>) {
        debug_assert!(self.contains(block));
        // SAFETY: the caller guarantees `block` is detached and valid for this slab.
        unsafe { self.free.push_block(block) };
    }

    unsafe fn pop_batch(&mut self, max: usize) -> Batch {
        if max == 0 {
            return Batch::empty();
        }

        if self.free.is_empty() && self.next_fresh == self.capacity {
            return Batch::empty();
        }

        let mut list = FreeList::new();
        let mut taken = 0;

        while taken < max {
            let block = if !self.free.is_empty() {
                // SAFETY: the slab owns this free list exclusively through `&mut self`.
                unsafe { self.free.pop_block() }
            } else if self.next_fresh < self.capacity {
                let offset = self
                    .next_fresh
                    .checked_mul(self.block_size)
                    .unwrap_or_else(|| unreachable!("fresh slab batch offset overflowed"));
                self.next_fresh += 1;
                let block = self.start.as_ptr().wrapping_add(offset);
                // SAFETY: the computed block start lies within this slab and is non-null.
                Some(unsafe { NonNull::new_unchecked(block) })
            } else {
                None
            };

            let Some(block) = block else {
                break;
            };

            // SAFETY: each detached block is valid for this slab's class and owned by `list`.
            unsafe { list.push_block(block) };
            taken += 1;
        }

        // SAFETY: the temporary list now owns exactly `taken` valid detached blocks.
        unsafe { list.pop_batch(taken) }
    }
}

pub(crate) struct ThreadCacheHandle {
    cache: ThreadCache,
    owner: &'static Allocator,
}

impl ThreadCacheHandle {
    #[must_use]
    pub(crate) fn new(owner: &'static Allocator) -> Self {
        Self {
            cache: ThreadCache::new(*owner.config()),
            owner,
        }
    }

    pub(crate) fn with_parts<R>(&mut self, f: impl FnOnce(&Allocator, &mut ThreadCache) -> R) -> R {
        f(self.owner, &mut self.cache)
    }
}

impl Drop for ThreadCacheHandle {
    fn drop(&mut self) {
        self.owner.drain_thread_cache_on_exit(&mut self.cache);
    }
}

#[cfg(test)]
mod tests {
    use super::{ThreadCache, ThreadCacheHandle};
    use crate::allocator::Allocator;
    use crate::central_pool::CentralPool;
    use crate::config::AllocatorConfig;
    use crate::size_class::SizeClass;
    use core::ptr::NonNull;

    #[repr(align(64))]
    struct TestBlock<const N: usize>([u8; N]);

    impl<const N: usize> TestBlock<N> {
        const fn new() -> Self {
            Self([0; N])
        }

        fn as_ptr(&mut self) -> NonNull<u8> {
            NonNull::from(&mut self.0).cast()
        }
    }

    const fn test_config() -> AllocatorConfig {
        AllocatorConfig {
            arena_size: 512,
            alignment: 64,
            refill_target_bytes: 256,
            local_cache_target_bytes: 384,
        }
    }

    fn test_allocator() -> &'static Allocator {
        let allocator = match Allocator::new(test_config()) {
            Ok(allocator) => allocator,
            Err(error) => panic!("expected allocator to initialize: {error}"),
        };

        Box::leak(Box::new(allocator))
    }

    fn fill_central<const N: usize>(
        pool: &CentralPool,
        class: SizeClass,
        blocks: &mut [TestBlock<N>],
    ) {
        let mut list = crate::free_list::FreeList::new();

        for ptr in blocks.iter_mut().map(TestBlock::as_ptr) {
            // SAFETY: test blocks are aligned, large enough for the intrusive node,
            // and linked only through this temporary list.
            unsafe {
                list.push_block(ptr);
            }
        }

        // SAFETY: the temporary list contains exactly these valid detached test blocks.
        let batch = unsafe { list.pop_batch(blocks.len()) };
        // SAFETY: the batch is detached and belongs to the requested class for the test.
        unsafe {
            pool.return_batch(class, batch);
        }
    }

    #[test]
    fn empty_cache_needs_refill_and_refill_moves_configured_batch() {
        let pool = CentralPool::new();
        let mut cache = ThreadCache::new(test_config());
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];

        fill_central(&pool, SizeClass::B64, &mut blocks);

        assert!(cache.needs_refill(SizeClass::B64));

        let moved = cache.refill_from_central(SizeClass::B64, &pool);

        assert_eq!(moved, 2);
        assert!(!cache.needs_refill(SizeClass::B64));

        let remaining = pool.take_batch(SizeClass::B64, 8);
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn should_drain_only_after_exceeding_local_limit() {
        let mut cache = ThreadCache::new(test_config());
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];

        for block in &mut blocks[..3] {
            // SAFETY: each test block is detached and valid for the local free list.
            unsafe {
                cache.push(SizeClass::B64, block.as_ptr());
            }
        }

        assert!(!cache.should_drain(SizeClass::B64));

        // SAFETY: the fourth test block is detached and valid for the local free list.
        unsafe {
            cache.push(SizeClass::B64, blocks[3].as_ptr());
        }

        assert!(cache.should_drain(SizeClass::B64));
    }

    #[test]
    fn drain_excess_returns_exact_drain_batch_to_central() {
        let pool = CentralPool::new();
        let mut cache = ThreadCache::new(test_config());
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];

        for block in &mut blocks {
            // SAFETY: each test block is detached and valid for the local free list.
            unsafe {
                cache.push(SizeClass::B64, block.as_ptr());
            }
        }

        // SAFETY: the local cache contains only valid detached test blocks.
        let moved = unsafe { cache.drain_excess_to_central(SizeClass::B64, &pool) };

        assert_eq!(moved, 1);

        let drained = pool.take_batch(SizeClass::B64, 8);
        assert_eq!(drained.len(), 1);

        let mut popped = 0;
        while cache.pop(SizeClass::B64).is_some() {
            popped += 1;
        }
        assert_eq!(popped, 3);
    }

    #[test]
    fn drain_excess_is_a_noop_below_limit() {
        let pool = CentralPool::new();
        let mut cache = ThreadCache::new(test_config());
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];

        for block in &mut blocks {
            // SAFETY: each test block is detached and valid for the local free list.
            unsafe {
                cache.push(SizeClass::B64, block.as_ptr());
            }
        }

        // SAFETY: the local cache contains only valid detached test blocks.
        let moved = unsafe { cache.drain_excess_to_central(SizeClass::B64, &pool) };

        assert_eq!(moved, 0);
        assert_eq!(pool.take_batch(SizeClass::B64, 8).len(), 0);
    }

    #[test]
    fn drain_all_moves_every_class_back_to_central() {
        let pool = CentralPool::new();
        let mut cache = ThreadCache::new(test_config());
        let mut small = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];
        let mut medium = [TestBlock::<{ SizeClass::B256.block_size() }>::new()];

        for block in &mut small {
            // SAFETY: each test block is detached and valid for the local free list.
            unsafe {
                cache.push(SizeClass::B64, block.as_ptr());
            }
        }
        // SAFETY: the test block is detached and valid for the local free list.
        unsafe {
            cache.push(SizeClass::B256, medium[0].as_ptr());
        }

        // SAFETY: the cache contains only valid detached test blocks.
        unsafe {
            cache.drain_all_to_central(&pool);
        }

        assert!(cache.needs_refill(SizeClass::B64));
        assert!(cache.needs_refill(SizeClass::B256));
        assert_eq!(pool.take_batch(SizeClass::B64, 8).len(), 2);
        assert_eq!(pool.take_batch(SizeClass::B256, 8).len(), 1);
    }

    #[test]
    fn blocks_drained_from_one_cache_can_refill_another() {
        let pool = CentralPool::new();
        let mut source = ThreadCache::new(test_config());
        let mut destination = ThreadCache::new(test_config());
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];
        let expected = blocks.each_mut().map(TestBlock::as_ptr);

        for block in &mut blocks {
            // SAFETY: each test block is detached and valid for the local free list.
            unsafe {
                source.push(SizeClass::B64, block.as_ptr());
            }
        }

        // SAFETY: the source cache contains only valid detached test blocks.
        unsafe {
            source.drain_all_to_central(&pool);
        }

        let moved = destination.refill_from_central(SizeClass::B64, &pool);
        assert_eq!(moved, 2);

        assert_eq!(destination.pop(SizeClass::B64), Some(expected[1]));
        assert_eq!(destination.pop(SizeClass::B64), Some(expected[0]));
        assert_eq!(destination.pop(SizeClass::B64), None);
    }

    #[test]
    fn thread_cache_handle_drop_drains_back_to_owner_central_pool() {
        let allocator = test_allocator();
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];

        {
            let mut handle = ThreadCacheHandle::new(allocator);
            handle.with_parts(|_, cache| {
                for block in &mut blocks {
                    // SAFETY: each test block is detached and valid for the local free list.
                    unsafe {
                        cache.push(SizeClass::B64, block.as_ptr());
                    }
                }
            });
        }

        let drained = allocator.take_central_batch_for_test(SizeClass::B64, 8);
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn stats_report_local_occupancy_by_size_class() {
        let mut cache = ThreadCache::new(test_config());
        let mut small = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];
        let mut medium = [TestBlock::<{ SizeClass::B256.block_size() }>::new()];

        for block in &mut small {
            // SAFETY: each test block is detached and valid for the local free list.
            unsafe {
                cache.push(SizeClass::B64, block.as_ptr());
            }
        }
        // SAFETY: the test block is detached and valid for the local free list.
        unsafe {
            cache.push(SizeClass::B256, medium[0].as_ptr());
        }

        let stats = cache.stats();

        assert!(!stats.is_bound);
        assert_eq!(stats.local[SizeClass::B64.index()].blocks, 2);
        assert_eq!(
            stats.local[SizeClass::B64.index()].bytes,
            2 * SizeClass::B64.block_size_for_alignment(test_config().alignment)
        );
        assert_eq!(stats.local[SizeClass::B256.index()].blocks, 1);
        assert_eq!(stats.total_local_blocks(), 3);
        assert_eq!(
            stats.total_local_bytes(),
            2 * SizeClass::B64.block_size_for_alignment(test_config().alignment)
                + SizeClass::B256.block_size_for_alignment(test_config().alignment)
        );
    }
}
