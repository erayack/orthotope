use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;

use crate::allocator::Allocator;
use crate::central_pool::{CentralPool, CentralRefill};
use crate::config::AllocatorConfig;
use crate::free_list::{Batch, FreeList};
use crate::header::AllocationHeader;
use crate::size_class::{NUM_CLASSES, SizeClass};
use crate::stats::{SizeClassStats, ThreadCacheStats};

/// Caller-owned per-thread cache for small-object reuse.
///
/// Use one `ThreadCache` per participating thread when calling [`Allocator`] methods
/// directly. The process-global convenience API manages this internally.
pub struct ThreadCache {
    config: AllocatorConfig,
    class_config: [ClassCacheConfig; NUM_CLASSES],
    owner: Option<usize>,
    cache_id: u32,
    classes: [LocalClassCache; NUM_CLASSES],
    remote_returns: [RemoteReturnCache; NUM_CLASSES],
}

type SlabId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockReuse {
    HotReuseRequestedOnly,
    NeedsOwnerAndRequestedSizeRefresh,
    NeedsHeaderRewrite,
}

struct LocalClassCache {
    hot_block: Option<NonNull<u8>>,
    slabs: Vec<LocalSlab>,
    slabs_by_start: Vec<SlabId>,
    available_slabs: Vec<SlabId>,
    recent_slab: Option<SlabId>,
    shared: FreeList,
    shared_len: usize,
    total_len: usize,
}

#[derive(Clone, Copy)]
struct ClassCacheConfig {
    refill_count: usize,
    local_limit: usize,
    drain_count: usize,
}

struct RemoteReturnCache {
    pending: FreeList,
    pending_len: usize,
}

static NEXT_CACHE_ID: AtomicU32 = AtomicU32::new(1);

const fn class_config_table(config: AllocatorConfig) -> [ClassCacheConfig; NUM_CLASSES] {
    let mut entries = [ClassCacheConfig {
        refill_count: 0,
        local_limit: 0,
        drain_count: 0,
    }; NUM_CLASSES];
    let mut index = 0;
    while index < NUM_CLASSES {
        let class = SizeClass::ALL[index];
        entries[index] = ClassCacheConfig {
            refill_count: config.refill_count(class),
            local_limit: config.local_limit(class),
            drain_count: config.drain_count(class),
        };
        index += 1;
    }
    entries
}

struct LocalSlab {
    start: NonNull<u8>,
    end_addr: usize,
    block_size: usize,
    capacity: usize,
    next_fresh: usize,
    free: FreeList,
    available_slot: Option<usize>,
}

impl ThreadCache {
    /// Creates a caller-owned thread-local cache for use with a specific [`Allocator`].
    ///
    /// Pair one `ThreadCache` with one thread when using the instance-oriented API
    /// directly. The global convenience API manages this cache internally.
    #[must_use]
    pub fn new(config: AllocatorConfig) -> Self {
        Self {
            config,
            class_config: class_config_table(config),
            owner: None,
            cache_id: NEXT_CACHE_ID.fetch_add(1, Ordering::Relaxed),
            classes: core::array::from_fn(|_| LocalClassCache::new()),
            remote_returns: core::array::from_fn(|_| RemoteReturnCache::new()),
        }
    }

    #[must_use]
    pub(crate) const fn cache_id(&self) -> u32 {
        self.cache_id
    }

    #[inline]
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
        self.class_config = class_config_table(self.config);
        self.owner = Some(owner);
    }

    #[must_use]
    fn is_empty(&self) -> bool {
        self.classes.iter().all(LocalClassCache::is_empty)
            && self.remote_returns.iter().all(RemoteReturnCache::is_empty)
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn pop(&mut self, class: SizeClass) -> Option<(NonNull<u8>, BlockReuse)> {
        // SAFETY: `ThreadCache` has exclusive mutable access to the class cache.
        unsafe { self.classes[class.index()].pop_block() }
    }

    /// Pops one block from a class known to be non-empty.
    ///
    /// # Safety
    ///
    /// The local cache for `class` must contain at least one available block.
    #[must_use]
    #[inline]
    pub(crate) unsafe fn pop_available_unchecked(
        &mut self,
        class: SizeClass,
    ) -> (NonNull<u8>, BlockReuse) {
        // SAFETY: the caller guarantees the selected class cache is non-empty, and
        // `ThreadCache` has exclusive mutable access to it.
        unsafe { self.classes[class.index()].pop_block_unchecked() }
    }

    /// Pushes one block into the local free list for `class`.
    ///
    /// # Safety
    ///
    /// `block` must be a valid detached allocator block for `class`, large enough for
    /// the intrusive free-list node and not linked in any other list.
    #[inline]
    pub(crate) unsafe fn push(&mut self, class: SizeClass, block: NonNull<u8>) {
        // SAFETY: the caller guarantees `block` is a valid detached node for this class,
        // and `&mut self` provides exclusive access to the destination class cache.
        unsafe { self.classes[class.index()].push_block(block) };
    }

    /// Stages one block freed by a non-owner cache for batched publication to the
    /// central remote-return inbox.
    ///
    /// # Safety
    ///
    /// `block` must be a valid detached allocator block for `class`, large enough for
    /// the intrusive free-list node and not linked in any other list.
    pub(crate) unsafe fn push_remote_return(&mut self, class: SizeClass, block: NonNull<u8>) {
        // SAFETY: the caller guarantees `block` is detached and valid for the remote
        // return queue of this class, and `&mut self` provides exclusive cache access.
        unsafe { self.remote_returns[class.index()].push_block(block) };
    }

    #[must_use]
    #[inline]
    pub(crate) const fn should_flush_remote_returns(&self, class: SizeClass) -> bool {
        self.remote_returns[class.index()].len() >= self.class_config[class.index()].drain_count
    }

    /// Publishes pending remote frees for one class to the central pool.
    ///
    /// # Safety
    ///
    /// Every pending remote-return node for `class` must be a valid detached block for
    /// the allocator's central pool and must not be linked in any other list.
    pub(crate) unsafe fn flush_remote_returns_for_class(
        &mut self,
        class: SizeClass,
        central: &CentralPool,
    ) -> usize {
        let remote_returns = &mut self.remote_returns[class.index()];
        if remote_returns.is_empty() {
            return 0;
        }

        // SAFETY: the non-empty check above proves there is at least one pending node,
        // and this method's caller guarantees every pending node is detached and valid
        // for the matching central class.
        let batch = unsafe { remote_returns.take_all_unchecked() };
        let moved = batch.len();

        // SAFETY: the pending remote-return cache stores only detached valid blocks
        // for this class.
        unsafe {
            central.publish_remote_batch(class, batch);
        }
        moved
    }

    /// Publishes all pending remote frees to the central pool.
    ///
    /// # Safety
    ///
    /// Every pending remote-return node must be a valid detached block for its class
    /// and must not be linked in any other list.
    pub(crate) unsafe fn flush_all_remote_returns(&mut self, central: &CentralPool) {
        for class in SizeClass::ALL {
            // SAFETY: the cache invariant for each per-class remote-return list is the
            // same as this method's caller contract.
            unsafe {
                self.flush_remote_returns_for_class(class, central);
            }
        }
    }

    #[must_use]
    pub(crate) const fn needs_refill(&self, class: SizeClass) -> bool {
        self.classes[class.index()].is_empty()
    }

    #[inline]
    pub(crate) fn refill_from_central(&mut self, class: SizeClass, central: &CentralPool) -> usize {
        match central.take_refill(class, self.class_config[class.index()].refill_count) {
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
            } => {
                self.classes[class.index()].push_owned_slab(start, block_size, capacity);
                capacity
            }
        }
    }

    #[must_use]
    #[inline]
    pub(crate) const fn should_drain(&self, class: SizeClass) -> bool {
        self.classes[class.index()].len() > self.class_config[class.index()].local_limit
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
    /// For slab-backed capacity, this preserves the same reuse model as allocation:
    /// reclaimed slab blocks are drained before untouched fresh slab capacity, and a
    /// single drain batch may combine both sources to satisfy the configured count.
    ///
    /// # Safety
    ///
    /// Every node currently linked in the local list for `class` must be a valid block
    /// for that size class and must belong exclusively to this cache. The caller must
    /// have already established that [`Self::should_drain`] is true for `class`.
    pub(crate) unsafe fn drain_excess_to_central(
        &mut self,
        class: SizeClass,
        central: &CentralPool,
    ) -> usize {
        debug_assert!(self.should_drain(class));

        // SAFETY: the caller guarantees the local class cache holds only valid nodes for
        // this class, and `&mut self` ensures exclusive access during detachment.
        let batch = unsafe {
            self.classes[class.index()]
                .pop_batch(self.class_config[class.index()].drain_count, class)
        };
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
        // SAFETY: pending remote-return lists contain detached blocks for the matching
        // central class and are independent from the local reusable cache lists.
        unsafe {
            self.flush_all_remote_returns(central);
        }

        for class in SizeClass::ALL {
            loop {
                let len = self.classes[class.index()].len();
                if len == 0 {
                    break;
                }

                let batch = {
                    let class_cache = &mut self.classes[class.index()];
                    // SAFETY: `&mut self` guarantees exclusive access to the full class
                    // cache while detached batches are removed until it is empty.
                    unsafe { class_cache.pop_batch(len, class) }
                };

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
        self.remote_returns = core::array::from_fn(|_| RemoteReturnCache::new());
    }
}

impl RemoteReturnCache {
    const fn new() -> Self {
        Self {
            pending: FreeList::new(),
            pending_len: 0,
        }
    }

    const fn len(&self) -> usize {
        self.pending_len
    }

    const fn is_empty(&self) -> bool {
        self.pending_len == 0
    }

    unsafe fn push_block(&mut self, block: NonNull<u8>) {
        // SAFETY: the caller guarantees `block` is detached and valid for this class.
        unsafe {
            self.pending.push_block(block);
        }
        self.pending_len += 1;
    }

    unsafe fn take_all_unchecked(&mut self) -> Batch {
        debug_assert!(self.pending_len != 0);

        // SAFETY: `pending` exclusively owns exactly `pending_len` detached nodes.
        let batch = unsafe { self.pending.pop_batch_unchecked(self.pending_len) };
        self.pending_len = 0;
        batch
    }
}

impl LocalClassCache {
    const fn new() -> Self {
        Self {
            hot_block: None,
            slabs: Vec::new(),
            slabs_by_start: Vec::new(),
            available_slabs: Vec::new(),
            recent_slab: None,
            shared: FreeList::new(),
            shared_len: 0,
            total_len: 0,
        }
    }

    const fn len(&self) -> usize {
        self.total_len
    }

    const fn stats_len(&self) -> usize {
        self.total_len
    }

    const fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    fn push_owned_slab(&mut self, start: NonNull<u8>, block_size: usize, capacity: usize) {
        let start_addr = start.as_ptr().addr();
        let slab_id = match self
            .slabs_by_start
            .binary_search_by_key(&start_addr, |&slab_id| {
                self.slabs[slab_id].start.as_ptr().addr()
            }) {
            Ok(slot) => {
                let slab_id = self.slabs_by_start[slot];
                self.mark_slab_unavailable(slab_id);
                self.slabs[slab_id].reset(start, block_size, capacity);
                slab_id
            }
            Err(slot) => {
                let slab_id = self.slabs.len();
                self.slabs.push(LocalSlab::new(start, block_size, capacity));
                self.slabs_by_start.insert(slot, slab_id);
                slab_id
            }
        };

        self.mark_slab_available(slab_id);
        self.recent_slab = Some(slab_id);
        self.total_len = self
            .total_len
            .checked_add(capacity)
            .unwrap_or_else(|| unreachable!("local class cache size overflowed"));
    }

    #[cfg(test)]
    unsafe fn pop_block(&mut self) -> Option<(NonNull<u8>, BlockReuse)> {
        if self.total_len == 0 {
            None
        } else {
            // SAFETY: `self.total_len != 0` proves the cache has one backing source.
            Some(unsafe { self.pop_block_unchecked() })
        }
    }

    #[inline]
    unsafe fn pop_block_unchecked(&mut self) -> (NonNull<u8>, BlockReuse) {
        debug_assert!(
            self.total_len != 0,
            "unchecked pop requires a non-empty cache"
        );

        if let Some(block) = self.hot_block.take() {
            self.total_len -= 1;
            return (block, BlockReuse::HotReuseRequestedOnly);
        }

        if !self.available_slabs.is_empty() {
            // SAFETY: the guard guarantees the vector is non-empty.
            let slab_id = unsafe { *self.available_slabs.last().unwrap_unchecked() };
            let slab = &mut self.slabs[slab_id];
            debug_assert!(
                slab.has_available_blocks(),
                "available slab list must contain only non-empty slabs"
            );
            // SAFETY: membership in `available_slabs` guarantees this slab can return
            // a block while this cache holds exclusive mutable access.
            let (block, reuse) = unsafe { slab.pop_block_unchecked() };
            self.recent_slab = Some(slab_id);
            self.total_len -= 1;
            if !slab.has_available_blocks() {
                self.mark_slab_unavailable(slab_id);
            }
            return (block, reuse);
        }

        debug_assert!(
            self.shared_len != 0,
            "non-empty cache must have a backing source"
        );
        // SAFETY: `self.total_len != 0` and no hot/slab block imply the shared list is
        // non-empty by the local class-cache accounting invariant.
        let block = unsafe { self.shared.pop_block_unchecked() };
        self.shared_len -= 1;
        self.total_len -= 1;
        (block, BlockReuse::NeedsOwnerAndRequestedSizeRefresh)
    }

    #[inline]
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

    #[inline]
    unsafe fn push_spilled_block(&mut self, block: NonNull<u8>) {
        if let Some(slab_id) = self
            .recent_slab
            .filter(|&slab_id| self.slabs[slab_id].contains(block))
        {
            let slab = &mut self.slabs[slab_id];
            let was_empty = !slab.has_available_blocks();
            // SAFETY: the caller guarantees `block` is detached and valid for this class.
            unsafe { slab.push_block(block) };
            if was_empty {
                self.mark_slab_available(slab_id);
            }
            return;
        }

        if let Some(slab_id) = self.find_slab(block) {
            self.recent_slab = Some(slab_id);
            let slab = &mut self.slabs[slab_id];
            let was_empty = !slab.has_available_blocks();
            // SAFETY: the caller guarantees `block` is detached and valid for this class.
            unsafe { slab.push_block(block) };
            if was_empty {
                self.mark_slab_available(slab_id);
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

    unsafe fn pop_batch(&mut self, max: usize, class: SizeClass) -> Batch {
        debug_assert!(max != 0, "class-cache batch pop requires a non-zero max");
        debug_assert!(
            self.total_len != 0,
            "class-cache batch pop requires a non-empty cache"
        );

        if self.shared_len != 0 {
            // SAFETY: the shared list contains only valid detached nodes for this class.
            let batch = unsafe { self.shared.pop_batch_unchecked(max) };
            let len = batch.len();
            self.shared_len -= len;
            self.total_len -= len;
            return batch;
        }

        if !self.available_slabs.is_empty() {
            // SAFETY: the loop guard guarantees the vector is non-empty.
            let slab_id = unsafe { *self.available_slabs.last().unwrap_unchecked() };
            let slab = &mut self.slabs[slab_id];
            debug_assert!(
                slab.has_available_blocks(),
                "available slab list must contain only non-empty slabs"
            );
            // SAFETY: membership in `available_slabs` plus `max > 0` guarantees that
            // `slab.pop_batch(max)` returns a non-empty batch while this cache has
            // exclusive access to the slab.
            let batch = unsafe { slab.pop_batch(max, class) };
            let len = batch.len();
            debug_assert!(len != 0, "available slab must yield a non-empty batch");
            self.total_len -= len;
            if !slab.has_available_blocks() {
                self.mark_slab_unavailable(slab_id);
            }
            return batch;
        }

        if let Some(block) = self.hot_block.take() {
            let mut list = FreeList::new();
            // SAFETY: `block` was uniquely owned by the hot slot and is now detached.
            unsafe { list.push_block(block) };
            self.total_len -= 1;
            // SAFETY: the temporary list contains exactly one detached valid block.
            return unsafe { list.pop_batch_unchecked(1) };
        }

        debug_assert_eq!(
            self.total_len, 0,
            "non-empty class cache must have a backing source"
        );
        Batch::empty()
    }

    fn find_slab(&self, block: NonNull<u8>) -> Option<SlabId> {
        let addr = block.as_ptr().addr();
        let index = self
            .slabs_by_start
            .partition_point(|&slab_id| self.slabs[slab_id].start.as_ptr().addr() <= addr);
        if index == 0 {
            return None;
        }
        let slab_id = self.slabs_by_start[index - 1];
        self.slabs[slab_id].contains(block).then_some(slab_id)
    }

    fn mark_slab_available(&mut self, slab_id: SlabId) {
        if self.slabs[slab_id].available_slot.is_some() {
            return;
        }

        let slot = self.available_slabs.len();
        self.available_slabs.push(slab_id);
        self.slabs[slab_id].available_slot = Some(slot);
    }

    fn mark_slab_unavailable(&mut self, slab_id: SlabId) {
        let Some(slot) = self.slabs[slab_id].available_slot.take() else {
            return;
        };

        let removed = self.available_slabs.swap_remove(slot);
        debug_assert_eq!(removed, slab_id);
        if let Some(&moved_slab_id) = self.available_slabs.get(slot) {
            self.slabs[moved_slab_id].available_slot = Some(slot);
        }
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
            available_slot: None,
        }
    }

    fn reset(&mut self, start: NonNull<u8>, block_size: usize, capacity: usize) {
        debug_assert_eq!(self.start, start);
        debug_assert_eq!(self.block_size, block_size);
        debug_assert_eq!(self.capacity, capacity);
        self.next_fresh = 0;
        self.free = FreeList::new();
        self.available_slot = None;
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

    /// Pops one block from a slab known to have reclaimed or fresh capacity.
    ///
    /// # Safety
    ///
    /// The caller must ensure [`Self::has_available_blocks`] is true.
    #[inline]
    unsafe fn pop_block_unchecked(&mut self) -> (NonNull<u8>, BlockReuse) {
        debug_assert!(
            self.has_available_blocks(),
            "unchecked slab pop requires an available block"
        );

        // SAFETY: the slab owns this free list exclusively through `&mut self`.
        if !self.free.is_empty() {
            return (
                // SAFETY: `!self.free.is_empty()` proves the slab-local free list is non-empty.
                unsafe { self.free.pop_block_unchecked() },
                BlockReuse::HotReuseRequestedOnly,
            );
        }

        debug_assert!(self.next_fresh < self.capacity);
        let offset = self.next_fresh * self.block_size;
        self.next_fresh += 1;
        let block = self.start.as_ptr().wrapping_add(offset);
        let reuse = BlockReuse::NeedsHeaderRewrite;
        // SAFETY: the caller guarantees available capacity, so the computed block start
        // stays within this non-null slab.
        (unsafe { NonNull::new_unchecked(block) }, reuse)
    }

    #[inline]
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
    unsafe fn pop_batch(&mut self, max: usize, class: SizeClass) -> Batch {
        debug_assert!(max != 0, "slab batch pop requires a non-zero max");
        debug_assert!(
            self.has_available_blocks(),
            "slab batch pops only occur for slabs tracked as available"
        );

        let mut list = FreeList::new();
        let reclaimed = core::cmp::min(self.free.len(), max);
        for _ in 0..reclaimed {
            // SAFETY: `reclaimed` is bounded by the free-list length captured above.
            let block = unsafe { self.free.pop_block_unchecked() };
            // SAFETY: each reclaimed block is detached from `self.free` and valid for this slab.
            unsafe { list.push_block(block) };
        }

        let fresh = core::cmp::min(max - reclaimed, self.capacity - self.next_fresh);
        let fresh_start = self.next_fresh;
        self.next_fresh += fresh;
        let mut block = self
            .start
            .as_ptr()
            .wrapping_add(fresh_start * self.block_size);
        for _ in 0..fresh {
            // SAFETY: `block` starts at the first reserved fresh block and advances by
            // exactly `block_size` for `fresh` iterations, where `fresh` is bounded by
            // remaining slab capacity. Slab starts are non-null.
            let block_start = unsafe { NonNull::new_unchecked(block) };
            // SAFETY: fresh blocks that leave the local slab for the central pool need
            // initialized small metadata so a later partial-batch refill can refresh only
            // the hot owner/request fields.
            unsafe {
                AllocationHeader::initialize_small_to_block_unchecked(block_start, class);
            }
            // SAFETY: `block_start` is one of the fresh block starts proven above.
            unsafe { list.push_block(block_start) };
            block = block.wrapping_add(self.block_size);
        }

        let taken = reclaimed + fresh;
        debug_assert!(taken != 0, "available slab must yield a non-empty batch");
        // SAFETY: the temporary list owns exactly `taken` detached blocks, preserving the
        // previous LIFO batch order while avoiding per-iteration availability checks.
        unsafe { list.pop_batch_unchecked(taken) }
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
        self.owner.drain_thread_cache(&mut self.cache);
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockReuse, ThreadCache, ThreadCacheHandle};
    use crate::allocator::Allocator;
    use crate::arena::system_page_size;
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
        allocator.drain_thread_cache(&mut source);

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

        allocator.drain_thread_cache(&mut cache);

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
        allocator.drain_thread_cache(&mut source);

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
    fn partial_central_refill_only_needs_owner_and_requested_size_refresh() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut source = ThreadCache::new(test_config());
        let mut destination = ThreadCache::new(test_config());
        let class = SizeClass::B64;
        let first = allocator
            .allocate_with_cache(&mut source, 32)
            .unwrap_or_else(|error| panic!("expected first allocation to succeed: {error}"));
        let second = allocator
            .allocate_with_cache(&mut source, 32)
            .unwrap_or_else(|error| panic!("expected second allocation to succeed: {error}"));

        // SAFETY: `first` is live and is freed exactly once before the central refill.
        unsafe {
            allocator
                .deallocate_with_cache(&mut source, first)
                .unwrap_or_else(|error| panic!("expected first free to succeed: {error}"));
        }
        allocator.drain_thread_cache(&mut source);

        let moved = allocator.refill_thread_cache_from_central_for_test(&mut destination, class);
        assert_eq!(moved, 1);
        assert!(matches!(
            destination.pop(class),
            Some((_, BlockReuse::NeedsOwnerAndRequestedSizeRefresh))
        ));

        // SAFETY: `second` is the other live allocation and still belongs to `source`.
        unsafe {
            allocator
                .deallocate_with_cache(&mut source, second)
                .unwrap_or_else(|error| panic!("expected second free to succeed: {error}"));
        }
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
    fn drain_flushes_below_threshold_remote_returns_to_central_pool() {
        let config = AllocatorConfig {
            arena_size: 1 << 20,
            alignment: 64,
            refill_target_bytes: 32 << 10,
            local_cache_target_bytes: 64 << 10,
        };
        let allocator = Allocator::new(config)
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut source = ThreadCache::new(config);
        let mut remote = ThreadCache::new(config);
        let class = SizeClass::B64;
        let pointers = allocate_small_blocks(&allocator, &mut source, 2);

        for ptr in pointers {
            // SAFETY: each pointer is live and freeing through a different cache id
            // exercises the staged remote-return path.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut remote, ptr)
                    .unwrap_or_else(|error| panic!("expected remote free to succeed: {error}"));
            }
        }

        assert_eq!(
            allocator.stats().small_central[class.index()].blocks,
            0,
            "below-threshold remote frees stay staged in the freeing cache"
        );

        allocator.drain_thread_cache(&mut remote);

        assert_eq!(
            allocator.stats().small_central[class.index()].blocks,
            2,
            "draining the freeing cache publishes staged remote frees"
        );
    }

    #[test]
    fn whole_slab_reissue_reuses_local_slab_record_and_rewrites_header() {
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
        allocator.drain_thread_cache(&mut cache);

        assert_eq!(cache.classes[class.index()].slabs.len(), 1);

        let moved = allocator.refill_thread_cache_from_central_for_test(&mut cache, class);
        assert_eq!(moved, 2);
        assert_eq!(cache.classes[class.index()].slabs.len(), 1);
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
        allocator.drain_thread_cache(&mut source);
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
