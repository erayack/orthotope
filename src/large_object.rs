use core::ptr::NonNull;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use parking_lot::Mutex;

use crate::error::FreeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Live-registry entry for one large allocation.
pub(crate) struct LargeAllocationRecord {
    pub(crate) requested_size: usize,
    pub(crate) usable_size: usize,
}

/// Tracks live allocations larger than the largest small class.
pub(crate) struct LargeObjectAllocator {
    live: Mutex<HashMap<usize, LargeAllocationRecord>>,
}

impl LargeObjectAllocator {
    #[must_use]
    /// Creates an empty large-allocation registry.
    pub(crate) fn new() -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Records one newly allocated large block as live.
    pub(crate) fn record_live_allocation(
        &self,
        user_ptr: NonNull<u8>,
        requested_size: usize,
        usable_size: usize,
    ) {
        debug_assert!(requested_size > 0);
        debug_assert!(usable_size >= requested_size);

        let record = LargeAllocationRecord {
            requested_size,
            usable_size,
        };

        match self.live.lock().entry(user_ptr.as_ptr().addr()) {
            Entry::Vacant(entry) => {
                entry.insert(record);
            }
            Entry::Occupied(_) => {
                panic!("large allocation registered twice for the same user pointer");
            }
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
        let mut live = self.live.lock();
        let Some(record) = live.get(&user_ptr.as_ptr().addr()).copied() else {
            return Err(FreeError::AlreadyFreedOrUnknownLarge);
        };

        if record.requested_size != requested_size || record.usable_size != usable_size {
            return Err(FreeError::CorruptHeader);
        }

        let removed = live.remove(&user_ptr.as_ptr().addr());
        drop(live);
        debug_assert!(removed.is_some(), "validated live record must still exist");
        Ok(())
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn live_len(&self) -> usize {
        self.live.lock().len()
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
            requested_size,
            usable_size,
        };

        allocator.record_live_allocation(user_ptr, record.requested_size, record.usable_size);

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
    }

    #[test]
    #[should_panic(expected = "large allocation registered twice for the same user pointer")]
    fn duplicate_registration_panics_in_all_builds() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_280;
        let block = TestBlock::new(block_size);
        let usable_size = block_size - HEADER_SIZE;
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, usable_size);

        allocator.record_live_allocation(user_ptr, expected.requested_size, expected.usable_size);
    }
}
