use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;

use crate::allocator::Allocator;
use crate::central_pool::{CentralPool, CentralRefill, SlabFreshMode};
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
    cache_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockReuse {
    HotReuseRequestedOnly,
    FreshNeedsOwnerRefresh,
    NeedsHeaderRewrite,
}

struct LocalClassCache {
    hot_block: Option<NonNull<u8>>,
    slabs: Vec<LocalSlab>,
    available_slabs: Vec<usize>,
    recent_slab: Option<usize>,
    shared: FreeList,
    shared_len: usize,
    remote: FreeList,
    remote_len: usize,
    total_len: usize,
}

static NEXT_CACHE_ID: AtomicU32 = AtomicU32::new(1);

struct LocalSlab {
    start: NonNull<u8>,
    end_addr: usize,
    block_size: usize,
    capacity: usize,
    next_fresh: usize,
    free: FreeList,
    fresh_mode: SlabFreshMode,
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
            cache_id: NEXT_CACHE_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    #[must_use]
    pub(crate) const fn cache_id(&self) -> u32 {
        self.cache_id
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

        self.reset_for_rebind();
        self.config = *allocator.config();
        self.owner = Some(owner);
    }

    #[must_use]
    fn is_empty(&self) -> bool {
        self.classes.iter().all(LocalClassCache::is_empty)
    }

    #[must_use]
    pub(crate) fn pop(&mut self, class: SizeClass) -> Option<(NonNull<u8>, BlockReuse)> {
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
        match central.take_refill(class, self.config.refill_count(class)) {
            CentralRefill::Empty => 0,
            CentralRefill::Batch(batch) => {
                let moved = batch.len();

                // SAFETY: the batch came from the central pool as a detached chain for
                // this class, and `&mut self` gives exclusive access to the destination.
                unsafe { self.classes[class.index()].push_shared_batch(batch) };

                moved
            }
            CentralRefill::Slab {
                start,
                block_size,
                capacity,
                fresh_mode,
            } => {
                self.classes[class.index()]
                    .push_owned_slab(start, block_size, capacity, fresh_mode);
                capacity
            }
        }
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
        fresh_mode: SlabFreshMode,
    ) {
        self.classes[class.index()].push_owned_slab(start, block_size, capacity, fresh_mode);
    }

    /// Drains one configured batch from the local cache back to the central pool.
    ///
    /// For slab-backed capacity, this preserves the same reuse model as allocation:
    /// reclaimed slab blocks are drained before untouched fresh slab capacity, and a
    /// single drain batch may combine both sources to satisfy the configured count.
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
            // SAFETY: remote lists carry detached valid blocks for this class.
            unsafe {
                self.classes[class.index()].drain_remote_to_central(central, class);
            }

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

    /// Pushes one cross-thread-freed block into the class-local remote buffer and
    /// immediately flushes remote frees to the central pool so other threads can
    /// observe released capacity even when the freeing thread never accumulates a
    /// full batch.
    ///
    /// # Safety
    ///
    /// `block` must be a valid detached allocator block for `class`.
    pub(crate) unsafe fn push_remote_and_flush(
        &mut self,
        class: SizeClass,
        block: NonNull<u8>,
        central: &CentralPool,
    ) -> usize {
        let class_cache = &mut self.classes[class.index()];
        // SAFETY: caller guarantees the block is detached and valid for this class.
        unsafe {
            class_cache.push_remote(block);
        }
        // SAFETY: remote list contains detached valid blocks for this class. Flush
        // eagerly so a single cross-thread free cannot strand the only reusable block
        // behind a batch threshold in the freeing thread's cache.
        unsafe { class_cache.flush_remote_to_central(central, class, 1) }
    }

    #[must_use]
    /// Returns a best-effort snapshot of this thread cache's local occupancy.
    pub fn stats(&self) -> ThreadCacheStats {
        let local = core::array::from_fn(|index| {
            let class = SizeClass::ALL[index];
            let blocks = self.classes[index].stats_len();
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

    fn reset_for_rebind(&mut self) {
        self.classes = core::array::from_fn(|_| LocalClassCache::new());
    }
}

impl LocalClassCache {
    const fn new() -> Self {
        Self {
            hot_block: None,
            slabs: Vec::new(),
            available_slabs: Vec::new(),
            recent_slab: None,
            shared: FreeList::new(),
            shared_len: 0,
            remote: FreeList::new(),
            remote_len: 0,
            total_len: 0,
        }
    }

    const fn len(&self) -> usize {
        self.total_len
    }

    const fn stats_len(&self) -> usize {
        self.total_len + self.remote_len
    }

    const fn is_empty(&self) -> bool {
        self.total_len == 0 && self.remote_len == 0
    }

    fn push_owned_slab(
        &mut self,
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
        fresh_mode: SlabFreshMode,
    ) {
        let start_addr = start.as_ptr().addr();
        let index = match self
            .slabs
            .binary_search_by_key(&start_addr, |slab| slab.start.as_ptr().addr())
        {
            Ok(index) => {
                self.slabs[index].reset(start, block_size, capacity, fresh_mode);
                index
            }
            Err(index) => {
                self.slabs.insert(
                    index,
                    LocalSlab::new(start, block_size, capacity, fresh_mode),
                );
                index
            }
        };

        self.available_slabs = self
            .slabs
            .iter()
            .enumerate()
            .filter_map(|(index, slab)| slab.has_available_blocks().then_some(index))
            .collect();
        self.recent_slab = Some(index);
        self.total_len = self
            .total_len
            .checked_add(capacity)
            .unwrap_or_else(|| unreachable!("local class cache size overflowed"));
    }

    unsafe fn pop_block(&mut self) -> Option<(NonNull<u8>, BlockReuse)> {
        if let Some(block) = self.hot_block.take() {
            self.total_len -= 1;
            return Some((block, BlockReuse::HotReuseRequestedOnly));
        }

        while let Some(&index) = self.available_slabs.last() {
            let slab = &mut self.slabs[index];
            // SAFETY: the slab owns its free storage exclusively while borrowed mutably here.
            if let Some((block, reuse)) = unsafe { slab.pop_block() } {
                self.recent_slab = Some(index);
                self.total_len -= 1;
                if !slab.has_available_blocks() {
                    let _ = self.available_slabs.pop();
                }
                return Some((block, reuse));
            }

            let _ = self.available_slabs.pop();
        }

        // SAFETY: the caller guarantees the shared list only contains valid detached nodes.
        if let Some(block) = unsafe { self.shared.pop_block() } {
            self.shared_len -= 1;
            self.total_len -= 1;
            return Some((block, BlockReuse::NeedsHeaderRewrite));
        }

        None
    }

    unsafe fn push_block(&mut self, block: NonNull<u8>) {
        if let Some(previous_hot) = self.hot_block.replace(block) {
            self.total_len += 1;
            // SAFETY: `previous_hot` was detached from the hot slot just above and
            // remains a valid block for normal class-cache insertion.
            unsafe { self.push_spilled_block(previous_hot) };
            return;
        }

        self.total_len += 1;
    }

    unsafe fn push_spilled_block(&mut self, block: NonNull<u8>) {
        if let Some(index) = self
            .recent_slab
            .filter(|&index| self.slabs[index].contains(block))
        {
            let slab = &mut self.slabs[index];
            let was_empty = !slab.has_available_blocks();
            // SAFETY: the caller guarantees `block` is detached and valid for this class.
            unsafe { slab.push_block(block) };
            if was_empty {
                self.available_slabs.push(index);
            }
            return;
        }

        if let Some(index) = self.find_slab(block) {
            self.recent_slab = Some(index);
            let slab = &mut self.slabs[index];
            let was_empty = !slab.has_available_blocks();
            // SAFETY: the caller guarantees `block` is detached and valid for this class.
            unsafe { slab.push_block(block) };
            if was_empty {
                self.available_slabs.push(index);
            }
            return;
        }

        // SAFETY: `block` remains a valid detached node for this class in the shared list.
        unsafe { self.shared.push_block(block) };
        self.shared_len += 1;
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

        if let Some(block) = self.hot_block.take() {
            let mut list = FreeList::new();
            // SAFETY: `block` was uniquely owned by the hot slot and is now detached.
            unsafe { list.push_block(block) };
            self.total_len -= 1;
            // SAFETY: the temporary list contains exactly one detached valid block.
            return unsafe { list.pop_batch(1) };
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

    unsafe fn push_remote(&mut self, block: NonNull<u8>) {
        // SAFETY: caller guarantees this block is detached and valid for the class.
        unsafe { self.remote.push_block(block) };
        self.remote_len += 1;
    }

    unsafe fn flush_remote_to_central(
        &mut self,
        central: &CentralPool,
        class: SizeClass,
        threshold: usize,
    ) -> usize {
        if self.remote_len < threshold {
            return 0;
        }

        // SAFETY: remote list stores detached valid blocks for this class.
        let batch = unsafe { self.remote.pop_batch(threshold) };
        let moved = batch.len();
        self.remote_len -= moved;

        // SAFETY: detached batch can be transferred to central for this class.
        unsafe { central.return_batch(class, batch) };
        moved
    }

    unsafe fn drain_remote_to_central(&mut self, central: &CentralPool, class: SizeClass) {
        while self.remote_len != 0 {
            // SAFETY: remote list stores detached valid blocks for this class.
            let batch = unsafe { self.remote.pop_batch(self.remote_len) };
            let moved = batch.len();
            self.remote_len -= moved;
            // SAFETY: detached batch can be transferred to central for this class.
            unsafe { central.return_batch(class, batch) };
        }
    }
}

impl LocalSlab {
    fn new(
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
        fresh_mode: SlabFreshMode,
    ) -> Self {
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
            fresh_mode,
        }
    }

    fn reset(
        &mut self,
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
        fresh_mode: SlabFreshMode,
    ) {
        debug_assert_eq!(self.start, start);
        debug_assert_eq!(self.block_size, block_size);
        debug_assert_eq!(self.capacity, capacity);
        self.next_fresh = 0;
        self.free = FreeList::new();
        self.fresh_mode = fresh_mode;
    }

    const fn has_available_blocks(&self) -> bool {
        self.next_fresh < self.capacity || !self.free.is_empty()
    }

    fn contains(&self, block: NonNull<u8>) -> bool {
        let addr = block.as_ptr().addr();
        let start = self.start.as_ptr().addr();

        addr >= start
            && addr < self.end_addr
            && slab_addr_alignment_matches(addr, start, self.block_size)
    }

    /// Pops one block from this slab, preferring reclaimed blocks before untouched
    /// fresh capacity so same-thread frees are reused immediately.
    unsafe fn pop_block(&mut self) -> Option<(NonNull<u8>, BlockReuse)> {
        // SAFETY: the slab owns this free list exclusively through `&mut self`.
        // Reclaimed slab entries were previously allocated from this cache, so owner
        // metadata remains valid and only requested-size refresh is needed.
        if let Some(block) = unsafe { self.free.pop_block() } {
            return Some((block, BlockReuse::HotReuseRequestedOnly));
        }

        if self.next_fresh < self.capacity {
            debug_assert!(self.next_fresh < self.capacity);
            let offset = self.next_fresh * self.block_size;
            self.next_fresh += 1;
            let block = self.start.as_ptr().wrapping_add(offset);
            // SAFETY: `offset` remains within the slab bounds and `start` is non-null.
            let reuse = match self.fresh_mode {
                SlabFreshMode::Preinitialized => BlockReuse::FreshNeedsOwnerRefresh,
                SlabFreshMode::MustRewrite => BlockReuse::NeedsHeaderRewrite,
            };
            // SAFETY: `offset` was derived from bounded slab capacity, so `block`
            // stays within the slab and cannot be null.
            return Some((unsafe { NonNull::new_unchecked(block) }, reuse));
        }

        None
    }

    unsafe fn push_block(&mut self, block: NonNull<u8>) {
        debug_assert!(self.contains(block));
        // SAFETY: the caller guarantees `block` is detached and valid for this slab.
        unsafe { self.free.push_block(block) };
    }

    /// Detaches up to `max` blocks from this slab for transfer to the central pool.
    ///
    /// Reclaimed blocks are drained first, but the returned batch may be topped up
    /// with untouched fresh capacity so over-limit drains can still return the full
    /// configured count.
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
            // SAFETY: the slab owns this free list exclusively through `&mut self`.
            let block = if let Some(block) = unsafe { self.free.pop_block() } {
                Some(block)
            } else if self.next_fresh < self.capacity {
                debug_assert!(self.next_fresh < self.capacity);
                let offset = self.next_fresh * self.block_size;
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

#[cfg(debug_assertions)]
const fn slab_addr_alignment_matches(addr: usize, start: usize, block_size: usize) -> bool {
    (addr - start).is_multiple_of(block_size)
}

#[cfg(not(debug_assertions))]
const fn slab_addr_alignment_matches(_addr: usize, _start: usize, _block_size: usize) -> bool {
    true
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
    use super::{BlockReuse, ThreadCache, ThreadCacheHandle};
    use crate::allocator::Allocator;
    use crate::arena::system_page_size;
    use crate::central_pool::SlabFreshMode;
    use crate::config::AllocatorConfig;
    use crate::header::block_start_from_user_ptr;
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
            arena_size: 4096,
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

    fn allocate_small_blocks(
        allocator: &Allocator,
        cache: &mut ThreadCache,
        count: usize,
    ) -> Vec<NonNull<u8>> {
        allocate_blocks_of_size(allocator, cache, count, 32)
    }

    fn allocate_blocks_of_size(
        allocator: &Allocator,
        cache: &mut ThreadCache,
        count: usize,
        requested_size: usize,
    ) -> Vec<NonNull<u8>> {
        let mut pointers = Vec::with_capacity(count);

        for _ in 0..count {
            let ptr = allocator
                .allocate_with_cache(cache, requested_size)
                .unwrap_or_else(|error| panic!("expected small allocation to succeed: {error}"));
            pointers.push(ptr);
        }

        pointers
    }

    fn sweepable_test_class() -> SizeClass {
        let page_size = system_page_size();
        SizeClass::ALL
            .into_iter()
            .find(|class| class.block_size() >= page_size)
            .unwrap_or_else(|| panic!("expected at least one size class to span a full page"))
    }

    fn test_config_for_class(class: SizeClass) -> AllocatorConfig {
        let block_size = test_config().class_block_size(class);
        AllocatorConfig {
            arena_size: block_size * 4,
            alignment: test_config().alignment,
            refill_target_bytes: block_size,
            local_cache_target_bytes: block_size * 2,
        }
    }

    #[test]
    fn empty_cache_needs_refill_and_refill_moves_configured_batch() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut source = ThreadCache::new(test_config());
        let mut destination = ThreadCache::new(test_config());
        let pointers = allocate_small_blocks(&allocator, &mut source, 2);

        for ptr in pointers {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut source, ptr)
                    .unwrap_or_else(|error| panic!("expected small free to succeed: {error}"));
            }
        }
        allocator.drain_thread_cache_on_exit(&mut source);

        assert!(destination.needs_refill(SizeClass::B64));
        let moved =
            allocator.refill_thread_cache_from_central_for_test(&mut destination, SizeClass::B64);
        assert_eq!(moved, 2);
        assert!(!destination.needs_refill(SizeClass::B64));
        assert_eq!(
            allocator.stats().small_central[SizeClass::B64.index()].blocks,
            0
        );
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
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());
        let pointers = allocate_small_blocks(&allocator, &mut cache, 4);

        for ptr in pointers {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut cache, ptr)
                    .unwrap_or_else(|error| panic!("expected small free to succeed: {error}"));
            }
        }

        assert_eq!(
            allocator.stats().small_central[SizeClass::B64.index()].blocks,
            1
        );

        let mut popped = 0;
        while cache.pop(SizeClass::B64).is_some() {
            popped += 1;
        }
        assert_eq!(popped, 3);
    }

    #[test]
    fn drain_excess_is_a_noop_below_limit() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());
        let pointers = allocate_small_blocks(&allocator, &mut cache, 2);

        for ptr in pointers {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut cache, ptr)
                    .unwrap_or_else(|error| panic!("expected small free to succeed: {error}"));
            }
        }

        assert_eq!(
            allocator.stats().small_central[SizeClass::B64.index()].blocks,
            0
        );
    }

    #[test]
    fn drain_all_moves_every_class_back_to_central() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());
        let small = allocate_small_blocks(&allocator, &mut cache, 2);
        let medium = allocator
            .allocate_with_cache(&mut cache, 128)
            .unwrap_or_else(|error| panic!("expected medium allocation to succeed: {error}"));

        for ptr in small {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut cache, ptr)
                    .unwrap_or_else(|error| panic!("expected small free to succeed: {error}"));
            }
        }
        // SAFETY: `medium` is still live and is freed exactly once here.
        unsafe {
            allocator
                .deallocate_with_cache(&mut cache, medium)
                .unwrap_or_else(|error| panic!("expected medium free to succeed: {error}"));
        }

        allocator.drain_thread_cache_on_exit(&mut cache);

        assert!(cache.needs_refill(SizeClass::B64));
        assert!(cache.needs_refill(SizeClass::B256));
        assert_eq!(
            allocator.stats().small_central[SizeClass::B64.index()].blocks,
            2
        );
        assert_eq!(
            allocator.stats().small_central[SizeClass::B256.index()].blocks,
            1
        );
    }

    #[test]
    fn blocks_drained_from_one_cache_can_refill_another() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut source = ThreadCache::new(test_config());
        let mut destination = ThreadCache::new(test_config());
        let pointers = allocate_small_blocks(&allocator, &mut source, 2);
        let expected = pointers
            .iter()
            .copied()
            .map(block_start_from_user_ptr)
            .collect::<Vec<_>>();

        for ptr in pointers {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut source, ptr)
                    .unwrap_or_else(|error| panic!("expected source free to succeed: {error}"));
            }
        }
        allocator.drain_thread_cache_on_exit(&mut source);

        let moved =
            allocator.refill_thread_cache_from_central_for_test(&mut destination, SizeClass::B64);
        assert_eq!(moved, 2);
        let first = destination.pop(SizeClass::B64);
        let second = destination.pop(SizeClass::B64);
        let observed = [first, second];

        assert!(observed.contains(&Some((expected[0], BlockReuse::NeedsHeaderRewrite))));
        assert!(observed.contains(&Some((expected[1], BlockReuse::NeedsHeaderRewrite))));
        assert_eq!(destination.pop(SizeClass::B64), None);
    }

    #[test]
    fn thread_cache_handle_drop_drains_back_to_owner_central_pool() {
        let allocator = test_allocator();

        {
            let mut handle = ThreadCacheHandle::new(allocator);
            handle.with_parts(|allocator, cache| {
                let pointers = allocate_small_blocks(allocator, cache, 2);
                for ptr in pointers {
                    // SAFETY: each pointer is still live and is freed exactly once here.
                    unsafe {
                        allocator
                            .deallocate_with_cache(cache, ptr)
                            .unwrap_or_else(|error| {
                                panic!("expected handle-owned small free to succeed: {error}")
                            });
                    }
                }
            });
        }

        assert_eq!(allocator.central_block_count_for_test(SizeClass::B64), 2);
    }

    #[test]
    fn whole_slab_reissue_reuses_local_slab_record_and_marks_it_must_rewrite() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());
        let class = SizeClass::B64;
        let pointers = allocate_small_blocks(&allocator, &mut cache, 2);

        for ptr in pointers {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut cache, ptr)
                    .unwrap_or_else(|error| panic!("expected local free to succeed: {error}"));
            }
        }
        allocator.drain_thread_cache_on_exit(&mut cache);

        assert_eq!(cache.classes[class.index()].slabs.len(), 1);

        let moved = allocator.refill_thread_cache_from_central_for_test(&mut cache, class);
        assert_eq!(moved, 2);
        assert_eq!(cache.classes[class.index()].slabs.len(), 1);
        assert_eq!(
            cache.classes[class.index()].slabs[0].fresh_mode,
            SlabFreshMode::MustRewrite
        );
        assert!(matches!(
            cache.pop(class),
            Some((_, BlockReuse::NeedsHeaderRewrite))
        ));
    }

    #[test]
    fn full_cold_slab_reissue_refreshes_owner_before_cross_thread_free() {
        let class = sweepable_test_class();
        let config = test_config_for_class(class);
        let request = class.payload_size();
        let allocator = Allocator::new(config)
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut source = ThreadCache::new(config);
        let mut destination = ThreadCache::new(config);
        let pointers = allocate_blocks_of_size(&allocator, &mut source, 1, request);

        for ptr in pointers {
            // SAFETY: each pointer is still live and is freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut source, ptr)
                    .unwrap_or_else(|error| panic!("expected source free to succeed: {error}"));
            }
        }
        allocator.drain_thread_cache_on_exit(&mut source);
        assert!(allocator.force_first_full_hot_slab_to_cold_for_test(class));

        let reused = allocator
            .allocate_with_cache(&mut destination, request)
            .unwrap_or_else(|error| panic!("expected full-cold slab reissue allocation: {error}"));

        // SAFETY: `reused` is live and freeing through the old owner cache should route
        // remotely only if the reissued header owner cache id was refreshed correctly.
        unsafe {
            allocator
                .deallocate_with_cache(&mut source, reused)
                .unwrap_or_else(|error| panic!("expected cross-thread free to succeed: {error}"));
        }

        assert_eq!(source.stats().total_local_blocks(), 0);
        assert_eq!(allocator.stats().small_central[class.index()].blocks, 1);
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

    #[test]
    fn rebinding_an_empty_cache_discards_stale_slab_metadata() {
        let allocator_a = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator A to initialize: {error}"));
        let allocator_b = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator B to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());
        let class = SizeClass::B64;

        let first = allocator_a
            .allocate_with_cache(&mut cache, 32)
            .unwrap_or_else(|error| panic!("expected first allocator A allocation: {error}"));
        let second = allocator_a
            .allocate_with_cache(&mut cache, 32)
            .unwrap_or_else(|error| panic!("expected second allocator A allocation: {error}"));

        assert_eq!(cache.classes[class.index()].slabs.len(), 1);
        assert!(cache.needs_refill(class));

        let _ = allocator_b
            .allocate_with_cache(&mut cache, 32)
            .unwrap_or_else(|error| panic!("expected allocator B allocation to rebind: {error}"));

        assert_eq!(
            cache.classes[class.index()].slabs.len(),
            1,
            "rebinding an empty cache should discard slab metadata from the previous allocator"
        );

        // SAFETY: both pointers are still live allocations from allocator A and are freed once.
        unsafe {
            allocator_a
                .deallocate_with_cache(&mut ThreadCache::new(test_config()), first)
                .unwrap_or_else(|error| panic!("expected first allocator A cleanup free: {error}"));
            allocator_a
                .deallocate_with_cache(&mut ThreadCache::new(test_config()), second)
                .unwrap_or_else(|error| {
                    panic!("expected second allocator A cleanup free: {error}")
                });
        }
    }
}
