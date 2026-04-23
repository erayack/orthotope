use core::ptr::NonNull;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use parking_lot::Mutex;

use crate::error::FreeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LargeObjectStats {
    pub(crate) live_allocations: usize,
    pub(crate) live_bytes: usize,
    pub(crate) free_blocks: usize,
    pub(crate) free_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Live-registry entry for one large allocation.
pub(crate) struct LargeAllocationRecord {
    pub(crate) block_addr: usize,
    pub(crate) block_size: usize,
    pub(crate) requested_size: usize,
    pub(crate) usable_size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreeLargeBlock {
    pub(crate) block_addr: usize,
    pub(crate) block_size: usize,
}

impl FreeLargeBlock {
    #[must_use]
    pub(crate) const fn usable_size(self) -> usize {
        self.block_size - crate::header::HEADER_SIZE
    }

    #[must_use]
    pub(crate) fn block_start(self) -> NonNull<u8> {
        let ptr = self.block_addr as *mut u8;
        NonNull::new(ptr).unwrap_or_else(|| panic!("stored free large block address was null"))
    }
}

#[derive(Default)]
struct LargeObjectState {
    live: HashMap<usize, LargeAllocationRecord>,
    live_bytes: usize,
    free: BTreeMap<usize, Vec<FreeLargeBlock>>,
    free_blocks: usize,
    free_bytes: usize,
}

impl LargeObjectState {
    fn insert_free_block(&mut self, block: FreeLargeBlock) {
        self.free.entry(block.block_size).or_default().push(block);
        self.free_blocks += 1;
        self.free_bytes += block.block_size;
    }

    fn take_best_fit_block(&mut self, minimum_block_size: usize) -> Option<FreeLargeBlock> {
        let (&bucket_size, _) = self.free.range(minimum_block_size..).next()?;
        let mut blocks = self
            .free
            .remove(&bucket_size)
            .unwrap_or_else(|| unreachable!("selected reusable large-block bucket must exist"));
        let block = blocks.pop().unwrap_or_else(|| {
            unreachable!("selected reusable large-block bucket must be non-empty")
        });
        if !blocks.is_empty() {
            let replaced = self.free.insert(bucket_size, blocks);
            debug_assert!(
                replaced.is_none(),
                "large-object free bucket should not be reinserted twice"
            );
        }
        self.free_blocks -= 1;
        self.free_bytes -= block.block_size;
        Some(block)
    }
}

/// Tracks live allocations larger than the largest small class.
pub(crate) struct LargeObjectAllocator {
    state: Mutex<LargeObjectState>,
}

impl LargeObjectAllocator {
    #[must_use]
    /// Creates an empty large-allocation registry.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(LargeObjectState::default()),
        }
    }

    #[must_use]
    pub(crate) fn take_reusable_block(&self, minimum_block_size: usize) -> Option<FreeLargeBlock> {
        let mut state = self.state.lock();
        let block = state.take_best_fit_block(minimum_block_size)?;
        drop(state);
        Some(block)
    }

    #[must_use]
    pub(crate) fn stats(&self) -> LargeObjectStats {
        let state = self.state.lock();
        LargeObjectStats {
            live_allocations: state.live.len(),
            live_bytes: state.live_bytes,
            free_blocks: state.free_blocks,
            free_bytes: state.free_bytes,
        }
    }

    /// Records one newly allocated large block as live.
    pub(crate) fn record_live_allocation(
        &self,
        user_ptr: NonNull<u8>,
        block_start: NonNull<u8>,
        block_size: usize,
        requested_size: usize,
        usable_size: usize,
    ) {
        debug_assert!(requested_size > 0);
        debug_assert!(usable_size >= requested_size);

        let record = LargeAllocationRecord {
            block_addr: block_start.as_ptr().addr(),
            block_size,
            requested_size,
            usable_size,
        };

        let mut state = self.state.lock();
        if let Entry::Vacant(entry) = state.live.entry(user_ptr.as_ptr().addr()) {
            entry.insert(record);
            state.live_bytes += block_size;
            drop(state);
        } else {
            panic!("large allocation registered twice for the same user pointer");
        }
    }

    /// Validates and releases the live-tracking record for a large allocation.
    ///
    /// # Errors
    ///
    /// Returns [`FreeError::AlreadyFreedOrUnknownLarge`] if `user_ptr` is not a
    /// currently tracked large allocation, or [`FreeError::CorruptHeader`] when the
    /// decoded header disagrees with the live registry entry.
    pub(crate) fn validate_and_release_live_allocation(
        &self,
        user_ptr: NonNull<u8>,
        requested_size: usize,
        usable_size: usize,
    ) -> Result<(), FreeError> {
        let mut state = self.state.lock();
        let Some(record) = state.live.get(&user_ptr.as_ptr().addr()).copied() else {
            return Err(FreeError::AlreadyFreedOrUnknownLarge);
        };

        if record.requested_size != requested_size || record.usable_size != usable_size {
            return Err(FreeError::CorruptHeader);
        }

        let removed = state.live.remove(&user_ptr.as_ptr().addr());
        state.live_bytes -= record.block_size;
        state.insert_free_block(FreeLargeBlock {
            block_addr: record.block_addr,
            block_size: record.block_size,
        });
        drop(state);
        debug_assert!(removed.is_some(), "validated live record must still exist");
        Ok(())
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn live_len(&self) -> usize {
        self.state.lock().live.len()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn free_len(&self) -> usize {
        self.state.lock().free_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::{LargeAllocationRecord, LargeObjectAllocator};
    use crate::error::FreeError;
    use crate::header::{HEADER_SIZE, user_ptr_from_block_start};
    use core::alloc::Layout;
    use core::ptr::NonNull;
    use std::alloc::{alloc, dealloc};

    struct TestBlock {
        ptr: NonNull<u8>,
        size: usize,
    }

    impl TestBlock {
        fn new(size: usize) -> Self {
            let layout = match Layout::from_size_align(size, 64) {
                Ok(layout) => layout,
                Err(error) => panic!("expected valid test layout: {error}"),
            };
            // SAFETY: `layout` has non-zero size and valid alignment for the test allocation.
            let ptr = unsafe { alloc(layout) };
            let Some(ptr) = NonNull::new(ptr) else {
                panic!("expected heap allocation for test block to succeed");
            };

            Self { ptr, size }
        }

        fn block_start(&self) -> NonNull<u8> {
            self.ptr
        }
    }

    impl Drop for TestBlock {
        fn drop(&mut self) {
            let layout = match Layout::from_size_align(self.size, 64) {
                Ok(layout) => layout,
                Err(error) => panic!("expected valid test layout during drop: {error}"),
            };

            // SAFETY: `self.ptr` was allocated with `alloc(layout)` in `TestBlock::new`
            // and has not been deallocated yet.
            unsafe {
                dealloc(self.ptr.as_ptr(), layout);
            }
        }
    }

    fn live_record(
        allocator: &LargeObjectAllocator,
        block: &TestBlock,
        requested_size: usize,
        usable_size: usize,
    ) -> (NonNull<u8>, LargeAllocationRecord) {
        let block_start = block.block_start();
        let user_ptr = user_ptr_from_block_start(block_start);
        let record = LargeAllocationRecord {
            block_addr: block_start.as_ptr().addr(),
            block_size: block.size,
            requested_size,
            usable_size,
        };

        allocator.record_live_allocation(
            user_ptr,
            block_start,
            record.block_size,
            record.requested_size,
            record.usable_size,
        );

        (user_ptr, record)
    }

    #[test]
    fn matching_release_succeeds_and_removes_live_record() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_217;
        let block = TestBlock::new(block_size);
        let usable_size = block_size - HEADER_SIZE;
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, usable_size);

        let released = allocator.validate_and_release_live_allocation(
            user_ptr,
            expected.requested_size,
            expected.usable_size,
        );

        assert_eq!(released, Ok(()));
        assert_eq!(allocator.live_len(), 0);
        assert_eq!(allocator.free_len(), 1);
    }

    #[test]
    fn releasing_unknown_pointer_fails() {
        let allocator = LargeObjectAllocator::new();
        let block = TestBlock::new(HEADER_SIZE + 16_777_217);
        let user_ptr = user_ptr_from_block_start(block.block_start());

        let result =
            allocator.validate_and_release_live_allocation(user_ptr, 16_777_217, 16_777_217);

        assert_eq!(result, Err(FreeError::AlreadyFreedOrUnknownLarge));
    }

    #[test]
    fn releasing_same_pointer_twice_detects_duplicate_free() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_280;
        let block = TestBlock::new(block_size);
        let usable_size = block_size - HEADER_SIZE;
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, usable_size);

        let first = allocator.validate_and_release_live_allocation(
            user_ptr,
            expected.requested_size,
            expected.usable_size,
        );
        let second = allocator.validate_and_release_live_allocation(
            user_ptr,
            expected.requested_size,
            expected.usable_size,
        );

        assert_eq!(first, Ok(()));
        assert_eq!(second, Err(FreeError::AlreadyFreedOrUnknownLarge));
    }

    #[test]
    fn mismatched_header_sizes_fail_without_consuming_live_record() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_280;
        let block = TestBlock::new(block_size);
        let usable_size = block_size - HEADER_SIZE;
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, usable_size);

        let mismatch = allocator.validate_and_release_live_allocation(
            user_ptr,
            expected.requested_size + 1,
            expected.usable_size,
        );
        assert_eq!(mismatch, Err(FreeError::CorruptHeader));
        assert_eq!(allocator.live_len(), 1);

        let release = allocator.validate_and_release_live_allocation(
            user_ptr,
            expected.requested_size,
            expected.usable_size,
        );
        assert_eq!(release, Ok(()));
        assert_eq!(allocator.live_len(), 0);
        assert_eq!(allocator.free_len(), 1);
    }

    #[test]
    fn records_are_isolated_per_pointer() {
        let allocator = LargeObjectAllocator::new();
        let first_block_size = HEADER_SIZE + 16_777_344;
        let second_block_size = HEADER_SIZE + 20_000_000;
        let first_block = TestBlock::new(first_block_size);
        let second_block = TestBlock::new(second_block_size);
        let first_usable_size = first_block_size - HEADER_SIZE;
        let second_usable_size = second_block_size - HEADER_SIZE;
        let (first_ptr, first_expected) =
            live_record(&allocator, &first_block, 16_777_280, first_usable_size);
        let (second_ptr, second_expected) =
            live_record(&allocator, &second_block, 20_000_000, second_usable_size);

        let first = allocator.validate_and_release_live_allocation(
            first_ptr,
            first_expected.requested_size,
            first_expected.usable_size,
        );

        assert_eq!(first, Ok(()));
        assert_eq!(allocator.live_len(), 1);

        let second = allocator.validate_and_release_live_allocation(
            second_ptr,
            second_expected.requested_size,
            second_expected.usable_size,
        );
        assert_eq!(second, Ok(()));
        assert_eq!(allocator.live_len(), 0);
        assert_eq!(allocator.free_len(), 2);
    }

    #[test]
    #[should_panic(expected = "large allocation registered twice for the same user pointer")]
    fn duplicate_registration_panics_in_all_builds() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_280;
        let block = TestBlock::new(block_size);
        let usable_size = block_size - HEADER_SIZE;
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, usable_size);

        allocator.record_live_allocation(
            user_ptr,
            NonNull::new(expected.block_addr as *mut u8)
                .unwrap_or_else(|| panic!("expected live record block address to remain non-null")),
            expected.block_size,
            expected.requested_size,
            expected.usable_size,
        );
    }
}
