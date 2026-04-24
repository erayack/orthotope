use core::fmt;
use core::ptr::NonNull;

use parking_lot::Mutex;

use crate::arena::{advise_free, page_aligned_inner_range};
use crate::free_list::{Batch, FreeList};
use crate::size_class::{NUM_CLASSES, SizeClass};

const SWEEP_INTERVAL: u64 = 64;
const COLD_EPOCHS: u64 = 4;
const SWEEP_SCAN_BUDGET: usize = 2;

type SlabId = usize;

pub(crate) enum CentralRefill {
    Empty,
    Batch(Batch),
    Slab {
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
    },
}

impl fmt::Debug for CentralRefill {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::Batch(batch) => f.debug_tuple("Batch").field(&batch.len()).finish(),
            Self::Slab {
                start,
                block_size,
                capacity,
            } => f
                .debug_struct("Slab")
                .field("start", &start)
                .field("block_size", block_size)
                .field("capacity", capacity)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlabState {
    Loaned,
    Partial,
    FullHot,
    FullCold,
}

struct SlabRecord {
    start: usize,
    end_addr: usize,
    block_size: usize,
    capacity: usize,
    central_free_len: usize,
    partial_free: FreeList,
    last_touched_epoch: u64,
    state: SlabState,
    bucket_slot: Option<usize>,
}

struct RemoteReturnInbox {
    pending: FreeList,
    pending_len: usize,
}

struct ClassPool {
    slabs: Vec<SlabRecord>,
    slabs_by_start: Vec<SlabId>,
    partial_slabs: Vec<SlabId>,
    full_hot_slabs: Vec<SlabId>,
    full_cold_slabs: Vec<SlabId>,
    epoch: u64,
    sweep_cursor: usize,
    recent_slab: Option<SlabId>,
}

/// Shared per-class slab registry and batch exchange between thread-local caches.
pub(crate) struct CentralPool {
    lists: [Mutex<ClassPool>; NUM_CLASSES],
    remote_inboxes: [Mutex<RemoteReturnInbox>; NUM_CLASSES],
}

impl CentralPool {
    #[must_use]
    /// Creates an empty central pool.
    pub(crate) fn new() -> Self {
        Self {
            lists: core::array::from_fn(|_| Mutex::new(ClassPool::new())),
            remote_inboxes: core::array::from_fn(|_| Mutex::new(RemoteReturnInbox::new())),
        }
    }

    pub(crate) fn register_slab(
        &self,
        class: SizeClass,
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
    ) {
        self.lists[class.index()]
            .lock()
            .register_slab(start, block_size, capacity);
    }

    #[must_use]
    pub(crate) fn take_refill(&self, class: SizeClass, max: usize) -> CentralRefill {
        let class_index = class.index();
        let mut pool = self.lists[class_index].lock();
        self.drain_remote_inbox(class_index, &mut pool);
        pool.take_refill(max)
    }

    /// Returns a detached batch to the shared pool for `class`.
    ///
    /// # Safety
    ///
    /// `batch` must describe a valid detached chain of allocator blocks belonging to
    /// previously registered slabs for `class`, and none of its nodes may be linked in
    /// any other free list.
    pub(crate) unsafe fn return_batch(&self, class: SizeClass, batch: Batch) {
        let class_index = class.index();
        let mut pool = self.lists[class_index].lock();
        self.drain_remote_inbox(class_index, &mut pool);
        // SAFETY: the caller guarantees `batch` contains detached valid blocks from
        // registered slabs for this class, and the class mutex serializes mutation.
        unsafe { pool.return_batch(batch) };
    }

    /// Publishes a detached remote-free batch for later class-pool resolution.
    ///
    /// # Safety
    ///
    /// `batch` must describe valid detached blocks belonging to registered slabs for
    /// `class`, and none of its nodes may be linked in any other free list.
    pub(crate) unsafe fn publish_remote_batch(&self, class: SizeClass, batch: Batch) {
        let mut inbox = self.remote_inboxes[class.index()].lock();
        // SAFETY: the caller guarantees `batch` is a valid detached chain for this class.
        unsafe { inbox.push_batch(batch) };
    }

    #[must_use]
    pub(crate) fn block_counts(&self) -> [usize; NUM_CLASSES] {
        core::array::from_fn(|index| {
            self.lists[index].lock().central_block_count()
                + self.remote_inboxes[index].lock().pending_len()
        })
    }

    #[cfg(test)]
    pub(crate) fn force_first_full_hot_to_cold_for_test(&self, class: SizeClass) -> bool {
        let class_index = class.index();
        let mut pool = self.lists[class_index].lock();
        self.drain_remote_inbox(class_index, &mut pool);
        pool.force_first_full_hot_to_cold_for_test()
    }

    fn drain_remote_inbox(&self, class_index: usize, pool: &mut ClassPool) {
        let pending = {
            let mut inbox = self.remote_inboxes[class_index].lock();
            inbox.take_pending()
        };

        if pending.is_empty() {
            return;
        }

        // SAFETY: the per-class inbox stores only detached valid blocks for this class.
        unsafe {
            pool.return_batch(pending);
        }
    }
}

impl RemoteReturnInbox {
    const fn new() -> Self {
        Self {
            pending: FreeList::new(),
            pending_len: 0,
        }
    }

    unsafe fn push_batch(&mut self, batch: Batch) {
        let len = batch.len();
        if len == 0 {
            return;
        }

        // SAFETY: the caller guarantees `batch` is detached and valid for this class.
        unsafe { self.pending.push_batch(batch) };
        self.pending_len += len;
    }

    fn take_pending(&mut self) -> Batch {
        if self.pending_len == 0 {
            return Batch::empty();
        }

        // SAFETY: `pending` exclusively owns exactly `pending_len` detached valid nodes.
        let batch = unsafe { self.pending.pop_batch_unchecked(self.pending_len) };
        self.pending_len = 0;
        batch
    }

    const fn pending_len(&self) -> usize {
        self.pending_len
    }
}

impl ClassPool {
    const fn new() -> Self {
        Self {
            slabs: Vec::new(),
            slabs_by_start: Vec::new(),
            partial_slabs: Vec::new(),
            full_hot_slabs: Vec::new(),
            full_cold_slabs: Vec::new(),
            epoch: 0,
            sweep_cursor: 0,
            recent_slab: None,
        }
    }

    fn register_slab(&mut self, start: NonNull<u8>, block_size: usize, capacity: usize) {
        let epoch = self.bump_epoch();
        let start_addr = start.as_ptr().addr();

        match self
            .slabs_by_start
            .binary_search_by_key(&start_addr, |&slab_id| self.slabs[slab_id].start)
        {
            Ok(slot) => {
                let slab_id = self.slabs_by_start[slot];
                self.slabs[slab_id].reset(start, block_size, capacity, epoch);
                self.recent_slab = Some(slab_id);
            }
            Err(slot) => {
                let slab_id = self.slabs.len();
                self.slabs
                    .push(SlabRecord::new(start, block_size, capacity, epoch));
                self.slabs_by_start.insert(slot, slab_id);
                self.recent_slab = Some(slab_id);
            }
        }

        self.maybe_sweep();
    }

    fn take_refill(&mut self, max: usize) -> CentralRefill {
        if max == 0 {
            return CentralRefill::Empty;
        }

        let epoch = self.bump_epoch();

        if let Some(slab_id) = self.partial_slabs.last().copied() {
            let batch = self.slabs[slab_id].take_partial_batch(max);
            debug_assert!(
                !batch.is_empty(),
                "partial slab bucket must contain slabs with central capacity"
            );
            self.recent_slab = Some(slab_id);
            self.slabs[slab_id].last_touched_epoch = epoch;
            self.reconcile_bucket_state(slab_id, SlabState::Partial);
            self.maybe_sweep();
            return CentralRefill::Batch(batch);
        }

        if let Some(slab_id) = self.full_hot_slabs.last().copied() {
            let refill = self.loan_whole_slab(slab_id, epoch);
            self.maybe_sweep();
            return refill;
        }

        if let Some(slab_id) = self.full_cold_slabs.last().copied() {
            let refill = self.loan_whole_slab(slab_id, epoch);
            self.maybe_sweep();
            return refill;
        }

        self.maybe_sweep();
        CentralRefill::Empty
    }

    unsafe fn return_batch(&mut self, batch: Batch) {
        let blocks = batch.len();
        if blocks == 0 {
            return;
        }

        let epoch = self.bump_epoch();
        let mut batch = batch;

        for _ in 0..blocks {
            // SAFETY: `batch` starts with exactly `blocks` detached valid nodes, and
            // each iteration removes one node until the batch is exhausted.
            let block = unsafe { batch.pop_block_unchecked() };
            // SAFETY: `return_batch` accepts only detached blocks from slabs already
            // registered for this class, so lookup cannot fail for valid input.
            let slab_id = unsafe { self.find_slab_id_unchecked(block) };
            self.recent_slab = Some(slab_id);
            self.slabs[slab_id].last_touched_epoch = epoch;
            let previous_state = self.slabs[slab_id].state;
            // SAFETY: `find_slab_id` proved that `block` lies within this slab's range.
            unsafe {
                self.slabs[slab_id].record_returned_block(block);
            }
            self.reconcile_bucket_state(slab_id, previous_state);
        }

        self.maybe_sweep();
    }

    fn central_block_count(&self) -> usize {
        self.slabs.iter().map(|slab| slab.central_free_len).sum()
    }

    fn bump_epoch(&mut self) -> u64 {
        self.epoch = self
            .epoch
            .checked_add(1)
            .unwrap_or_else(|| unreachable!("central pool epoch overflowed"));
        self.epoch
    }

    fn loan_whole_slab(&mut self, slab_id: SlabId, epoch: u64) -> CentralRefill {
        let (start, block_size, capacity, previous_state) = {
            let slab = &mut self.slabs[slab_id];
            debug_assert!(matches!(
                slab.state,
                SlabState::FullHot | SlabState::FullCold
            ));
            debug_assert_eq!(slab.central_free_len, slab.capacity);
            debug_assert!(slab.partial_free.is_empty());
            let previous_state = slab.state;
            slab.state = SlabState::Loaned;
            slab.central_free_len = 0;
            slab.last_touched_epoch = epoch;
            (
                slab.start_ptr(),
                slab.block_size,
                slab.capacity,
                previous_state,
            )
        };
        self.recent_slab = Some(slab_id);
        self.reconcile_bucket_transition(slab_id, previous_state, SlabState::Loaned);

        CentralRefill::Slab {
            start,
            block_size,
            capacity,
        }
    }

    unsafe fn find_slab_id_unchecked(&mut self, block: NonNull<u8>) -> SlabId {
        if let Some(slab_id) = self
            .recent_slab
            .filter(|&slab_id| self.slabs[slab_id].contains(block))
        {
            return slab_id;
        }

        let addr = block.as_ptr().addr();
        let index = self
            .slabs_by_start
            .partition_point(|&slab_id| self.slabs[slab_id].start <= addr);
        debug_assert!(
            index != 0,
            "returned block {addr:#x} must belong to a registered central slab"
        );
        let slab_id = self.slabs_by_start[index - 1];
        debug_assert!(
            self.slabs[slab_id].contains(block),
            "returned block {addr:#x} must belong to a registered central slab"
        );
        self.recent_slab = Some(slab_id);
        slab_id
    }

    fn maybe_sweep(&mut self) {
        if !self.epoch.is_multiple_of(SWEEP_INTERVAL) || self.full_hot_slabs.is_empty() {
            return;
        }

        let total = self.full_hot_slabs.len();
        let budget = core::cmp::min(SWEEP_SCAN_BUDGET, total);

        for _ in 0..budget {
            let bucket_slot = self.sweep_cursor % total;
            let slab_id = self.full_hot_slabs[bucket_slot];
            self.sweep_cursor = (bucket_slot + 1) % total;

            if !self.slabs[slab_id].is_sweep_candidate(self.epoch) {
                continue;
            }

            let Some((addr, len)) = page_aligned_inner_range(
                self.slabs[slab_id].start_ptr(),
                self.slabs[slab_id].span_len(),
            ) else {
                continue;
            };

            // SAFETY: the computed range is page-aligned and fully contained within one
            // still-mapped arena slab owned by this allocator instance.
            let advised = unsafe { advise_free(addr, len) };
            if matches!(advised, Ok(true)) {
                let previous_state = self.slabs[slab_id].state;
                self.slabs[slab_id].state = SlabState::FullCold;
                self.reconcile_bucket_transition(slab_id, previous_state, SlabState::FullCold);
            }
            break;
        }
    }

    fn reconcile_bucket_state(&mut self, slab_id: SlabId, previous_state: SlabState) {
        let next_state = self.slabs[slab_id].state;
        self.reconcile_bucket_transition(slab_id, previous_state, next_state);
    }

    fn reconcile_bucket_transition(
        &mut self,
        slab_id: SlabId,
        previous_state: SlabState,
        next_state: SlabState,
    ) {
        if previous_state == next_state {
            return;
        }

        self.remove_from_bucket(slab_id, previous_state);
        self.add_to_bucket(slab_id, next_state);
    }

    fn remove_from_bucket(&mut self, slab_id: SlabId, state: SlabState) {
        let Some(slot) = self.slabs[slab_id].bucket_slot.take() else {
            debug_assert_eq!(state, SlabState::Loaned);
            return;
        };

        let bucket = match state {
            SlabState::Loaned => return,
            SlabState::Partial => &mut self.partial_slabs,
            SlabState::FullHot => &mut self.full_hot_slabs,
            SlabState::FullCold => &mut self.full_cold_slabs,
        };
        let removed = bucket.swap_remove(slot);
        debug_assert_eq!(removed, slab_id);
        if let Some(&moved_slab_id) = bucket.get(slot) {
            self.slabs[moved_slab_id].bucket_slot = Some(slot);
        }
        self.normalize_sweep_cursor();
    }

    fn add_to_bucket(&mut self, slab_id: SlabId, state: SlabState) {
        let bucket = match state {
            SlabState::Loaned => {
                self.slabs[slab_id].bucket_slot = None;
                return;
            }
            SlabState::Partial => &mut self.partial_slabs,
            SlabState::FullHot => &mut self.full_hot_slabs,
            SlabState::FullCold => &mut self.full_cold_slabs,
        };
        let slot = bucket.len();
        bucket.push(slab_id);
        self.slabs[slab_id].bucket_slot = Some(slot);
        self.normalize_sweep_cursor();
    }

    const fn normalize_sweep_cursor(&mut self) {
        if self.full_hot_slabs.is_empty() {
            self.sweep_cursor = 0;
        } else {
            self.sweep_cursor %= self.full_hot_slabs.len();
        }
    }

    #[cfg(test)]
    fn force_first_full_hot_to_cold_for_test(&mut self) -> bool {
        let Some(&slab_id) = self.full_hot_slabs.last() else {
            return false;
        };

        let _guard = crate::arena::override_advise_free_for_test(Some(true));
        self.epoch = SWEEP_INTERVAL;
        self.slabs[slab_id].last_touched_epoch = SWEEP_INTERVAL - COLD_EPOCHS;
        self.sweep_cursor = self.full_hot_slabs.len() - 1;
        self.maybe_sweep();

        self.slabs[slab_id].state == SlabState::FullCold
    }
}

impl SlabRecord {
    fn new(start: NonNull<u8>, block_size: usize, capacity: usize, epoch: u64) -> Self {
        let start_addr = start.as_ptr().addr();
        let span_len = block_size
            .checked_mul(capacity)
            .unwrap_or_else(|| unreachable!("slab span length overflowed"));
        let end_addr = start_addr
            .checked_add(span_len)
            .unwrap_or_else(|| unreachable!("slab end overflowed"));

        Self {
            start: start_addr,
            end_addr,
            block_size,
            capacity,
            central_free_len: 0,
            partial_free: FreeList::new(),
            last_touched_epoch: epoch,
            state: SlabState::Loaned,
            bucket_slot: None,
        }
    }

    fn reset(&mut self, start: NonNull<u8>, block_size: usize, capacity: usize, epoch: u64) {
        let start_addr = start.as_ptr().addr();
        let span_len = block_size
            .checked_mul(capacity)
            .unwrap_or_else(|| unreachable!("slab span length overflowed"));
        let end_addr = start_addr
            .checked_add(span_len)
            .unwrap_or_else(|| unreachable!("slab end overflowed"));

        debug_assert_eq!(self.start, start_addr);
        debug_assert_eq!(self.block_size, block_size);
        debug_assert_eq!(self.capacity, capacity);

        self.end_addr = end_addr;
        self.central_free_len = 0;
        self.partial_free = FreeList::new();
        self.last_touched_epoch = epoch;
        self.state = SlabState::Loaned;
        self.bucket_slot = None;
    }

    fn contains(&self, block: NonNull<u8>) -> bool {
        let addr = block.as_ptr().addr();

        addr >= self.start
            && addr < self.end_addr
            && (addr - self.start).is_multiple_of(self.block_size)
    }

    const fn span_len(&self) -> usize {
        self.end_addr - self.start
    }

    fn start_ptr(&self) -> NonNull<u8> {
        debug_assert_ne!(self.start, 0, "registered slab start must be non-null");
        // SAFETY: slab registration only accepts `NonNull<u8>` starts, and `start`
        // is never mutated after construction except by replacing the whole record.
        unsafe { NonNull::new_unchecked(self.start as *mut u8) }
    }

    unsafe fn record_returned_block(&mut self, block: NonNull<u8>) {
        debug_assert!(self.contains(block));
        debug_assert!(self.central_free_len < self.capacity);
        debug_assert!(!matches!(
            self.state,
            SlabState::FullHot | SlabState::FullCold
        ));

        // SAFETY: the caller guarantees `block` is detached and belongs to this slab.
        unsafe {
            self.partial_free.push_block(block);
        }
        self.central_free_len += 1;

        if self.central_free_len == self.capacity {
            self.state = SlabState::FullHot;
            self.partial_free = FreeList::new();
        } else {
            self.state = SlabState::Partial;
        }

        self.debug_assert_invariants();
    }

    fn take_partial_batch(&mut self, max: usize) -> Batch {
        debug_assert_eq!(self.state, SlabState::Partial);
        debug_assert!(self.central_free_len > 0);
        // SAFETY: `partial_free` is owned exclusively while the class mutex is held.
        let batch = unsafe { self.partial_free.pop_batch_unchecked(max) };
        let moved = batch.len();
        self.central_free_len -= moved;

        if self.central_free_len == 0 {
            self.state = SlabState::Loaned;
        }

        self.debug_assert_invariants();
        batch
    }

    fn is_sweep_candidate(&self, epoch: u64) -> bool {
        self.state == SlabState::FullHot
            && epoch.saturating_sub(self.last_touched_epoch) >= COLD_EPOCHS
    }

    fn debug_assert_invariants(&self) {
        debug_assert!(self.central_free_len <= self.capacity);

        match self.state {
            SlabState::Loaned => {
                debug_assert_eq!(self.central_free_len, 0);
            }
            SlabState::Partial => {
                debug_assert!(self.central_free_len > 0);
                debug_assert!(self.central_free_len < self.capacity);
                debug_assert!(!self.partial_free.is_empty());
            }
            SlabState::FullHot | SlabState::FullCold => {
                debug_assert_eq!(self.central_free_len, self.capacity);
                debug_assert!(self.partial_free.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CentralPool, CentralRefill, SlabState};
    use crate::arena::system_page_size;
    use crate::free_list::{Batch, FreeList};
    use crate::size_class::SizeClass;
    use core::ptr::NonNull;
    use memmap2::MmapMut;

    struct RegisteredSlab {
        _mapping: MmapMut,
        start: NonNull<u8>,
        block_size: usize,
        capacity: usize,
    }

    impl RegisteredSlab {
        fn new(pool: &CentralPool, class: SizeClass, capacity: usize) -> Self {
            let block_size = class.block_size();
            let mut mapping = MmapMut::map_anon(block_size * capacity)
                .unwrap_or_else(|error| panic!("expected anonymous mapping: {error}"));
            let start = NonNull::new(mapping.as_mut_ptr())
                .unwrap_or_else(|| panic!("expected non-null mapping"));
            pool.register_slab(class, start, block_size, capacity);

            Self {
                _mapping: mapping,
                start,
                block_size,
                capacity,
            }
        }

        fn block(&self, index: usize) -> NonNull<u8> {
            let offset = index
                .checked_mul(self.block_size)
                .unwrap_or_else(|| unreachable!("test block offset overflowed"));
            let ptr = self.start.as_ptr().wrapping_add(offset);
            NonNull::new(ptr)
                .unwrap_or_else(|| unreachable!("registered slab block must be non-null"))
        }

        fn batch(&self, indices: &[usize]) -> Batch {
            let mut list = FreeList::new();
            for &index in indices {
                // SAFETY: test helper only links detached block starts inside the registered slab.
                unsafe {
                    list.push_block(self.block(index));
                }
            }
            // SAFETY: the temporary list contains exactly these detached test blocks.
            unsafe { list.pop_batch(indices.len()) }
        }
    }

    fn collect_batch(batch: Batch) -> Vec<NonNull<u8>> {
        let mut list = FreeList::new();
        // SAFETY: `batch` is detached and valid for transfer into another list.
        unsafe {
            list.push_batch(batch);
        }

        let mut result = Vec::new();
        // SAFETY: `list` now owns the detached chain and can pop until empty.
        unsafe {
            while let Some(ptr) = list.pop_block() {
                result.push(ptr);
            }
        }
        result
    }

    fn sweepable_test_class() -> SizeClass {
        let page_size = system_page_size();
        SizeClass::ALL
            .into_iter()
            .find(|class| class.block_size() >= page_size)
            .unwrap_or_else(|| panic!("expected at least one size class to span a full page"))
    }

    #[test]
    fn empty_pool_returns_empty_refill() {
        let pool = CentralPool::new();

        assert!(matches!(
            pool.take_refill(SizeClass::B64, 4),
            CentralRefill::Empty
        ));
        assert_eq!(pool.block_counts()[SizeClass::B64.index()], 0);
    }

    #[test]
    fn partial_returns_can_be_taken_back_as_batches() {
        let pool = CentralPool::new();
        let slab = RegisteredSlab::new(&pool, SizeClass::B64, 4);

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.return_batch(SizeClass::B64, slab.batch(&[0, 1, 2]));
        }

        let refill = pool.take_refill(SizeClass::B64, 2);
        let batch = match refill {
            CentralRefill::Batch(batch) => batch,
            other => panic!("expected partial batch refill, got {other:?}"),
        };
        let returned = collect_batch(batch);

        assert_eq!(returned.len(), 2);
        assert_eq!(pool.block_counts()[SizeClass::B64.index()], 1);
    }

    #[test]
    fn remote_returns_are_visible_in_block_counts_before_drain() {
        let pool = CentralPool::new();
        let slab = RegisteredSlab::new(&pool, SizeClass::B64, 4);

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.publish_remote_batch(SizeClass::B64, slab.batch(&[0, 1]));
        }

        assert_eq!(pool.block_counts()[SizeClass::B64.index()], 2);
    }

    #[test]
    fn remote_returns_are_drained_before_partial_refill() {
        let pool = CentralPool::new();
        let slab = RegisteredSlab::new(&pool, SizeClass::B64, 4);

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.publish_remote_batch(SizeClass::B64, slab.batch(&[0, 1, 2]));
        }

        let refill = pool.take_refill(SizeClass::B64, 2);
        let batch = match refill {
            CentralRefill::Batch(batch) => batch,
            other => panic!("expected inbox-drained partial batch refill, got {other:?}"),
        };
        let returned = collect_batch(batch);

        assert_eq!(returned.len(), 2);
        assert_eq!(pool.block_counts()[SizeClass::B64.index()], 1);
    }

    #[test]
    fn fully_central_slab_reissues_as_metadata_only_whole_slab() {
        let pool = CentralPool::new();
        let slab = RegisteredSlab::new(&pool, SizeClass::B64, 4);

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.return_batch(SizeClass::B64, slab.batch(&[0, 1, 2, 3]));
        }

        assert_eq!(pool.block_counts()[SizeClass::B64.index()], 4);
        let refill = pool.take_refill(SizeClass::B64, 2);

        match refill {
            CentralRefill::Slab {
                start,
                block_size,
                capacity,
            } => {
                assert_eq!(start, slab.start);
                assert_eq!(block_size, slab.block_size);
                assert_eq!(capacity, slab.capacity);
            }
            other => panic!("expected whole-slab refill, got {other:?}"),
        }
        assert_eq!(pool.block_counts()[SizeClass::B64.index()], 0);
    }

    #[test]
    fn slab_state_transitions_to_full_hot_when_all_blocks_return() {
        let pool = CentralPool::new();
        let slab = RegisteredSlab::new(&pool, SizeClass::B64, 2);

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.return_batch(SizeClass::B64, slab.batch(&[0]));
        }

        {
            let class_pool = pool.lists[SizeClass::B64.index()].lock();
            assert_eq!(class_pool.slabs[0].state, SlabState::Partial);
            drop(class_pool);
        }

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.return_batch(SizeClass::B64, slab.batch(&[1]));
        }

        let class_pool = pool.lists[SizeClass::B64.index()].lock();
        assert_eq!(class_pool.slabs[0].state, SlabState::FullHot);
        assert!(class_pool.slabs[0].partial_free.is_empty());
        drop(class_pool);
    }

    #[test]
    fn bounded_sweep_can_transition_full_hot_slab_to_full_cold() {
        let pool = CentralPool::new();
        let class = sweepable_test_class();
        let slab = RegisteredSlab::new(&pool, class, 1);

        // SAFETY: the batch consists only of detached blocks from the registered slab.
        unsafe {
            pool.return_batch(class, slab.batch(&[0]));
        }

        let mut class_pool = pool.lists[class.index()].lock();
        assert!(class_pool.force_first_full_hot_to_cold_for_test());

        assert_eq!(class_pool.slabs[0].state, SlabState::FullCold);
        drop(class_pool);
    }
}
