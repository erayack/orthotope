use core::ptr::NonNull;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use parking_lot::Mutex;

use crate::error::FreeError;
use crate::header::HEADER_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LargeAllocationRecord {
    pub(crate) block_size: usize,
    pub(crate) requested_size: usize,
}

pub(crate) struct LargeObjectAllocator {
    live: Mutex<HashMap<usize, LargeAllocationRecord>>,
}

impl LargeObjectAllocator {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn record_live_allocation(
        &self,
        user_ptr: NonNull<u8>,
        block_start: NonNull<u8>,
        block_size: usize,
        requested_size: usize,
    ) {
        debug_assert_ne!(user_ptr, block_start);
        debug_assert!(requested_size > 0);
        debug_assert!(block_size >= HEADER_SIZE + requested_size);

        let record = LargeAllocationRecord {
            block_size,
            requested_size,
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

    /// Releases the live-tracking record for a large allocation.
    ///
    /// # Errors
    ///
    /// Returns [`FreeError::AlreadyFreedOrUnknownLarge`] if `user_ptr` is not a
    /// currently tracked large allocation.
    pub(crate) fn release_live_allocation(
        &self,
        user_ptr: NonNull<u8>,
    ) -> Result<LargeAllocationRecord, FreeError> {
        self.live
            .lock()
            .remove(&user_ptr.as_ptr().addr())
            .ok_or(FreeError::AlreadyFreedOrUnknownLarge)
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
        block_size: usize,
    ) -> (NonNull<u8>, LargeAllocationRecord) {
        let block_start = block.block_start();
        let user_ptr = user_ptr_from_block_start(block_start);
        let record = LargeAllocationRecord {
            block_size,
            requested_size,
        };

        allocator.record_live_allocation(
            user_ptr,
            block_start,
            record.block_size,
            record.requested_size,
        );

        (user_ptr, record)
    }

    #[test]
    fn release_returns_exact_record_that_was_registered() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_217;
        let block = TestBlock::new(block_size);
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, block_size);

        let released = match allocator.release_live_allocation(user_ptr) {
            Ok(record) => record,
            Err(error) => panic!("expected live large allocation to be released: {error}"),
        };

        assert_eq!(released, expected);
        assert_eq!(allocator.live_len(), 0);
    }

    #[test]
    fn releasing_unknown_pointer_fails() {
        let allocator = LargeObjectAllocator::new();
        let block = TestBlock::new(HEADER_SIZE + 16_777_217);
        let user_ptr = user_ptr_from_block_start(block.block_start());

        let result = allocator.release_live_allocation(user_ptr);

        assert_eq!(result, Err(FreeError::AlreadyFreedOrUnknownLarge));
    }

    #[test]
    fn releasing_same_pointer_twice_detects_duplicate_free() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_280;
        let block = TestBlock::new(block_size);
        let (user_ptr, expected) = live_record(&allocator, &block, 16_777_217, block_size);

        let first = allocator.release_live_allocation(user_ptr);
        let second = allocator.release_live_allocation(user_ptr);

        assert_eq!(first, Ok(expected));
        assert_eq!(second, Err(FreeError::AlreadyFreedOrUnknownLarge));
    }

    #[test]
    fn records_are_isolated_per_pointer() {
        let allocator = LargeObjectAllocator::new();
        let first_block_size = HEADER_SIZE + 16_777_344;
        let second_block_size = HEADER_SIZE + 20_000_000;
        let first_block = TestBlock::new(first_block_size);
        let second_block = TestBlock::new(second_block_size);
        let (first_ptr, first_expected) =
            live_record(&allocator, &first_block, 16_777_280, first_block_size);
        let (second_ptr, second_expected) =
            live_record(&allocator, &second_block, 20_000_000, second_block_size);

        let first = allocator.release_live_allocation(first_ptr);

        assert_eq!(first, Ok(first_expected));
        assert_eq!(allocator.live_len(), 1);

        let second = allocator.release_live_allocation(second_ptr);
        assert_eq!(second, Ok(second_expected));
        assert_eq!(allocator.live_len(), 0);
    }

    #[test]
    #[should_panic(expected = "large allocation registered twice for the same user pointer")]
    fn duplicate_registration_panics_in_all_builds() {
        let allocator = LargeObjectAllocator::new();
        let block_size = HEADER_SIZE + 16_777_280;
        let block = TestBlock::new(block_size);
        let (user_ptr, _) = live_record(&allocator, &block, 16_777_217, block_size);

        allocator.record_live_allocation(user_ptr, block.block_start(), block_size, 16_777_217);
    }
}
