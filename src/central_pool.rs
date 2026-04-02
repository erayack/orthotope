use parking_lot::Mutex;

use crate::free_list::{Batch, FreeList};
use crate::size_class::{NUM_CLASSES, SizeClass};

/// Shared per-class batch exchange between thread-local caches.
pub(crate) struct CentralPool {
    lists: [Mutex<FreeList>; NUM_CLASSES],
}

impl CentralPool {
    #[must_use]
    /// Creates an empty central pool.
    pub(crate) fn new() -> Self {
        Self {
            lists: core::array::from_fn(|_| Mutex::new(FreeList::new())),
        }
    }

    #[must_use]
    /// Removes up to `max` detached blocks from the shared list for `class`.
    pub(crate) fn take_batch(&self, class: SizeClass, max: usize) -> Batch {
        let mut list = self.lists[class.index()].lock();
        // SAFETY: each class list is protected by its mutex, so this detached batch
        // operation has exclusive access to the intrusive chain for that class.
        unsafe { list.pop_batch(max) }
    }

    /// Returns a detached batch to the shared pool for `class`.
    ///
    /// # Safety
    ///
    /// `batch` must describe a valid detached chain of allocator blocks belonging to
    /// `class`, and none of its nodes may be linked in any other free list.
    pub(crate) unsafe fn return_batch(&self, class: SizeClass, batch: Batch) {
        let mut list = self.lists[class.index()].lock();
        // SAFETY: the caller guarantees that `batch` is a valid detached chain for
        // this size class, and the class mutex gives exclusive list access.
        unsafe {
            list.push_batch(batch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CentralPool;
    use crate::free_list::{Batch, FreeList};
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

    fn batch_from_blocks<const N: usize>(blocks: &mut [TestBlock<N>]) -> Batch {
        let mut list = FreeList::new();
        let ptrs = blocks.iter_mut().map(TestBlock::as_ptr);

        for ptr in ptrs {
            // SAFETY: test blocks are aligned, large enough for a free-list node,
            // and linked only through this temporary list.
            unsafe {
                list.push_block(ptr);
            }
        }

        // SAFETY: the list above contains exactly the provided valid test blocks.
        unsafe { list.pop_batch(blocks.len()) }
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

    #[test]
    fn empty_pool_returns_empty_batch() {
        let pool = CentralPool::new();
        let batch = pool.take_batch(SizeClass::B64, 4);

        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
    }

    #[test]
    fn returned_batch_can_be_taken_back_in_lifo_order() {
        let pool = CentralPool::new();
        let mut blocks = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];
        let expected = blocks.each_mut().map(TestBlock::as_ptr);
        let batch = batch_from_blocks(&mut blocks);

        // SAFETY: `batch` is detached from the temporary list and contains only valid test blocks.
        unsafe {
            pool.return_batch(SizeClass::B64, batch);
        }

        let returned = collect_batch(pool.take_batch(SizeClass::B64, 3));
        assert_eq!(returned, vec![expected[2], expected[1], expected[0]]);
    }

    #[test]
    fn size_classes_remain_isolated() {
        let pool = CentralPool::new();
        let mut small = [
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
            TestBlock::<{ SizeClass::B64.block_size() }>::new(),
        ];
        let mut medium = [TestBlock::<{ SizeClass::B256.block_size() }>::new()];
        let small_ptrs = small.each_mut().map(TestBlock::as_ptr);
        let medium_ptr = medium[0].as_ptr();

        // SAFETY: both batches are detached and contain valid test blocks.
        unsafe {
            pool.return_batch(SizeClass::B64, batch_from_blocks(&mut small));
            pool.return_batch(SizeClass::B256, batch_from_blocks(&mut medium));
        }

        let medium_batch = collect_batch(pool.take_batch(SizeClass::B256, 2));
        assert_eq!(medium_batch, vec![medium_ptr]);

        let small_batch = collect_batch(pool.take_batch(SizeClass::B64, 2));
        assert_eq!(small_batch, vec![small_ptrs[1], small_ptrs[0]]);
    }
}
