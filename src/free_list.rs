use core::ptr::NonNull;

use crate::header::{read_small_free_list_next, write_small_free_list_next};

// The reserved free-list link word lives in the allocation header. Its offset, size,
// and alignment are asserted in `src/header.rs`.

#[allow(dead_code)]
/// Intrusive LIFO list of detached allocator blocks.
pub(crate) struct FreeList {
    head: Option<NonNull<u8>>,
    len: usize,
}

#[allow(dead_code)]
/// Detached block chain used for O(1) transfers between lists.
pub(crate) struct Batch {
    head: Option<NonNull<u8>>,
    tail: Option<NonNull<u8>>,
    len: usize,
}

// SAFETY: `FreeList` is an intrusive container over allocator-owned block starts. Moving
// the list value between threads does not move or alias the underlying blocks; callers are
// still responsible for external synchronization when sharing mutable access.
unsafe impl Send for FreeList {}

// SAFETY: `Batch` is a detached intrusive chain with unique ownership while in transit.
// Sending the batch value transfers ownership of that detached chain to the receiver.
unsafe impl Send for Batch {}

#[allow(dead_code)]
impl FreeList {
    #[must_use]
    /// Creates an empty free list.
    pub(crate) const fn new() -> Self {
        Self { head: None, len: 0 }
    }

    #[must_use]
    /// Returns the number of linked blocks.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    /// Returns `true` when the list has no blocks.
    pub(crate) const fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// Pushes a free block onto the list.
    ///
    /// # Safety
    ///
    /// `block` must be the start of a valid allocator block, properly aligned,
    /// and not currently linked in any free list.
    #[allow(clippy::missing_const_for_fn)]
    pub(crate) unsafe fn push_block(&mut self, block: NonNull<u8>) {
        // SAFETY: the caller guarantees that `block` points to writable storage large
        // enough for the reserved small-block link word and that it is not currently
        // linked elsewhere.
        unsafe {
            write_small_free_list_next(block, self.head);
        }
        self.head = Some(block);
        self.len += 1;
    }

    /// Pops one free block from the list.
    ///
    /// # Safety
    ///
    /// Every node currently linked in the list must point to writable storage large
    /// enough for the reserved small-block link word and must belong exclusively to
    /// this list.
    #[must_use]
    pub(crate) unsafe fn pop_block(&mut self) -> Option<NonNull<u8>> {
        if self.len == 0 {
            return None;
        }

        // SAFETY: `self.len != 0` proves the list is non-empty and the intrusive list
        // invariant guarantees `self.head.is_some()` in that state.
        Some(unsafe { self.pop_block_unchecked() })
    }

    /// Pops one free block from the list without re-checking emptiness.
    ///
    /// # Safety
    ///
    /// The caller must ensure `self.len != 0`. As with [`Self::pop_block`], every
    /// linked node must point to writable storage large enough for the reserved
    /// small-block link word and must belong exclusively to this list.
    #[must_use]
    pub(crate) unsafe fn pop_block_unchecked(&mut self) -> NonNull<u8> {
        debug_assert!(self.len != 0, "unchecked pop requires a non-empty list");
        // SAFETY: the caller guarantees the list is non-empty, so the list invariant
        // guarantees `self.head.is_some()`.
        let head = unsafe { self.head.unwrap_unchecked() };
        // SAFETY: `head` is a node already linked in this list, so reading its next
        // pointer is valid under the list invariants.
        let next = unsafe { read_small_free_list_next(head) };
        self.head = next;
        self.len -= 1;
        // SAFETY: `head` has been detached from the list, so clearing the next pointer
        // maintains the invariant that detached nodes are single-block chains.
        unsafe {
            write_small_free_list_next(head, None);
        }
        head
    }

    /// Prepends a detached batch to this list while preserving batch order.
    ///
    /// # Safety
    ///
    /// `batch` must describe a valid detached chain whose nodes are not linked in any
    /// other free list and whose storage is large enough for the reserved small-block
    /// link word.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) unsafe fn push_batch(&mut self, batch: Batch) {
        let Batch { head, tail, len } = batch;
        if len == 0 {
            return;
        }

        debug_assert!(head.is_some(), "non-empty batch must have a head");
        debug_assert!(tail.is_some(), "non-empty batch must have a tail");
        // SAFETY: a non-empty batch is constructed only with both head and tail set.
        let head = unsafe { head.unwrap_unchecked() };
        // SAFETY: same invariant as for `head` above.
        let tail = unsafe { tail.unwrap_unchecked() };

        // SAFETY: `tail` is the last node in the detached batch, so wiring it to this
        // list head splices the entire batch in front without disturbing batch order.
        unsafe {
            write_small_free_list_next(tail, self.head);
        }
        self.head = Some(head);
        self.len += len;
    }

    /// Detaches up to `max` blocks from the front of the list.
    ///
    /// # Safety
    ///
    /// Every node currently linked in the list must point to writable storage large
    /// enough for the reserved small-block link word and must belong exclusively to
    /// this list.
    #[must_use]
    pub(crate) unsafe fn pop_batch(&mut self, max: usize) -> Batch {
        if max == 0 || self.len == 0 {
            return Batch::empty();
        }

        // SAFETY: the guard above proves `max > 0` and the list is non-empty.
        unsafe { self.pop_batch_unchecked(max) }
    }

    /// Detaches up to `max` blocks from the front of a known non-empty list.
    ///
    /// # Safety
    ///
    /// The caller must ensure `max > 0` and `self.len != 0`. As with
    /// [`Self::pop_batch`], every linked node must point to writable storage large
    /// enough for the reserved small-block link word and must belong exclusively to
    /// this list.
    #[must_use]
    pub(crate) unsafe fn pop_batch_unchecked(&mut self, max: usize) -> Batch {
        debug_assert!(max != 0, "unchecked batch pop requires a non-zero max");
        debug_assert!(
            self.len != 0,
            "unchecked batch pop requires a non-empty list"
        );

        let take = core::cmp::min(max, self.len);
        debug_assert!(self.head.is_some(), "non-empty list must have a head");
        // SAFETY: `self.len != 0` implies `self.head.is_some()` by the list invariant.
        let head = unsafe { self.head.unwrap_unchecked() };
        let mut tail = head;

        for _ in 1..take {
            // SAFETY: we only walk within the first `take` nodes of a valid list.
            tail = unsafe { read_small_free_list_next(tail).unwrap_unchecked() };
        }

        // SAFETY: `tail` is the last node to detach, so taking its next pointer splits
        // the list into a detached batch and the remaining suffix.
        let remainder = unsafe { read_small_free_list_next(tail) };
        self.head = remainder;
        // SAFETY: `tail` is detached from the remaining list, so terminating the batch
        // with `None` preserves a valid detached chain.
        unsafe {
            write_small_free_list_next(tail, None);
        }
        self.len -= take;

        Batch {
            head: Some(head),
            tail: Some(tail),
            len: take,
        }
    }
}

#[allow(dead_code)]
impl Batch {
    #[must_use]
    /// Creates an empty detached batch.
    pub(crate) const fn empty() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }

    #[must_use]
    /// Returns the number of blocks in the batch.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    /// Creates a detached single-block batch.
    ///
    /// # Safety
    ///
    /// `block` must be a valid detached allocator block that is not currently linked
    /// in any free list.
    pub(crate) unsafe fn from_single(block: NonNull<u8>) -> Self {
        // SAFETY: the caller guarantees `block` is detached, so terminating it with
        // `None` forms a valid single-node detached chain.
        unsafe {
            write_small_free_list_next(block, None);
        }

        Self {
            head: Some(block),
            tail: Some(block),
            len: 1,
        }
    }

    #[must_use]
    /// Returns `true` when the batch has no blocks.
    pub(crate) const fn is_empty(&self) -> bool {
        self.head.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::{Batch, FreeList};
    use crate::header::{AllocationHeader, header_from_block_start};
    use crate::size_class::SizeClass;
    use core::ptr::NonNull;

    #[repr(align(64))]
    struct TestBlock([u8; 64]);

    impl TestBlock {
        const fn new() -> Self {
            Self([0; 64])
        }

        fn as_ptr(&mut self) -> NonNull<u8> {
            NonNull::from(&mut self.0).cast()
        }
    }

    fn initialize_test_header(block: &mut TestBlock, requested_size: usize, owner_cache_id: u32) {
        let block_start = block.as_ptr();
        // These test blocks only provide header-sized storage. That is enough because
        // `FreeList` touches only the reserved header words, and this helper stamps a
        // valid small-header prefix so tests can verify those reserved words do not
        // clobber the semantic routing fields.
        AllocationHeader::write_small_to_block(
            block_start,
            SizeClass::B64,
            requested_size,
            owner_cache_id,
        )
        .unwrap_or_else(|| panic!("expected test header initialization to succeed"));
    }

    #[test]
    fn push_and_pop_are_lifo() {
        let mut list = FreeList::new();
        let mut blocks = [TestBlock::new(), TestBlock::new(), TestBlock::new()];
        initialize_test_header(&mut blocks[0], 8, 11);
        initialize_test_header(&mut blocks[1], 16, 12);
        initialize_test_header(&mut blocks[2], 24, 13);
        let ptrs = blocks.each_mut().map(TestBlock::as_ptr);

        // SAFETY: test blocks are aligned, provide initialized header storage with the
        // reserved free-list metadata words, and are linked only through this list for
        // the duration of the test.
        unsafe {
            list.push_block(ptrs[0]);
            list.push_block(ptrs[1]);
            list.push_block(ptrs[2]);
        }

        assert_eq!(list.len(), 3);
        assert!(!list.is_empty());

        // SAFETY: the list contains only the test blocks linked above.
        unsafe {
            assert_eq!(list.pop_block(), Some(ptrs[2]));
            assert_eq!(list.pop_block(), Some(ptrs[1]));
            assert_eq!(list.pop_block(), Some(ptrs[0]));
        }

        assert_eq!(list.len(), 0);
        assert!(list.is_empty());
    }

    #[test]
    fn popping_empty_list_returns_none() {
        let mut list = FreeList::new();

        // SAFETY: the empty list contains no invalid nodes to traverse.
        unsafe {
            assert_eq!(list.pop_block(), None);
        }

        assert_eq!(list.len(), 0);
        assert!(list.is_empty());
    }

    #[test]
    fn pop_batch_detaches_requested_prefix() {
        let mut list = FreeList::new();
        let mut blocks = [
            TestBlock::new(),
            TestBlock::new(),
            TestBlock::new(),
            TestBlock::new(),
        ];
        initialize_test_header(&mut blocks[0], 8, 21);
        initialize_test_header(&mut blocks[1], 16, 22);
        initialize_test_header(&mut blocks[2], 24, 23);
        initialize_test_header(&mut blocks[3], 32, 24);
        let ptrs = blocks.each_mut().map(TestBlock::as_ptr);

        // SAFETY: test blocks are aligned, provide initialized header storage with the
        // reserved free-list metadata words, and are owned by this list.
        unsafe {
            list.push_block(ptrs[0]);
            list.push_block(ptrs[1]);
            list.push_block(ptrs[2]);
            list.push_block(ptrs[3]);
        }

        // SAFETY: the list contains only the valid chain created above.
        let batch = unsafe { list.pop_batch(2) };

        assert_eq!(batch.len(), 2);
        assert!(!batch.is_empty());
        assert_eq!(list.len(), 2);

        let mut receiver = FreeList::new();
        // SAFETY: `batch` was detached from `list` and remains a valid standalone chain.
        unsafe {
            receiver.push_batch(batch);
        }

        // SAFETY: both lists contain valid detached chains of the test blocks.
        unsafe {
            assert_eq!(receiver.pop_block(), Some(ptrs[3]));
            assert_eq!(receiver.pop_block(), Some(ptrs[2]));
            assert_eq!(receiver.pop_block(), None);

            assert_eq!(list.pop_block(), Some(ptrs[1]));
            assert_eq!(list.pop_block(), Some(ptrs[0]));
            assert_eq!(list.pop_block(), None);
        }
    }

    #[test]
    fn push_batch_preserves_batch_order_ahead_of_existing_nodes() {
        let mut source = FreeList::new();
        let mut destination = FreeList::new();
        let mut blocks = [
            TestBlock::new(),
            TestBlock::new(),
            TestBlock::new(),
            TestBlock::new(),
            TestBlock::new(),
        ];
        initialize_test_header(&mut blocks[0], 8, 31);
        initialize_test_header(&mut blocks[1], 16, 32);
        initialize_test_header(&mut blocks[2], 24, 33);
        initialize_test_header(&mut blocks[3], 32, 34);
        initialize_test_header(&mut blocks[4], 40, 35);
        let ptrs = blocks.each_mut().map(TestBlock::as_ptr);

        // SAFETY: test blocks are aligned, provide initialized header storage with the
        // reserved free-list metadata words, and each block is linked into at most one
        // list at a time in this test.
        unsafe {
            source.push_block(ptrs[0]);
            source.push_block(ptrs[1]);
            source.push_block(ptrs[2]);

            destination.push_block(ptrs[3]);
            destination.push_block(ptrs[4]);
        }

        // SAFETY: `source` contains a valid chain of three test blocks.
        let batch = unsafe { source.pop_batch(3) };
        // SAFETY: `batch` is detached and can be prepended to `destination`.
        unsafe {
            destination.push_batch(batch);
        }

        assert_eq!(source.len(), 0);
        assert_eq!(destination.len(), 5);

        // SAFETY: `destination` now contains one valid chain of all five blocks.
        unsafe {
            assert_eq!(destination.pop_block(), Some(ptrs[2]));
            assert_eq!(destination.pop_block(), Some(ptrs[1]));
            assert_eq!(destination.pop_block(), Some(ptrs[0]));
            assert_eq!(destination.pop_block(), Some(ptrs[4]));
            assert_eq!(destination.pop_block(), Some(ptrs[3]));
            assert_eq!(destination.pop_block(), None);
        }
    }

    #[test]
    fn zero_length_and_empty_batches_are_noops() {
        let mut list = FreeList::new();
        let mut block = TestBlock::new();
        initialize_test_header(&mut block, 8, 41);
        let ptr = block.as_ptr();

        // SAFETY: the test block is aligned, provides initialized header storage with the
        // reserved free-list metadata words, and is linked only through this list for
        // the duration of the test.
        unsafe {
            list.push_block(ptr);
        }

        // SAFETY: popping zero nodes is defined as a no-op on a valid list.
        let batch = unsafe { list.pop_batch(0) };
        assert!(batch.is_empty());
        assert_eq!(list.len(), 1);

        // SAFETY: pushing an empty batch to a valid list is a no-op.
        unsafe {
            list.push_batch(Batch::empty());
        }
        assert_eq!(list.len(), 1);

        // SAFETY: the list still contains the original single block.
        unsafe {
            assert_eq!(list.pop_block(), Some(ptr));
            assert_eq!(list.pop_block(), None);
        }
    }

    #[test]
    fn push_and_pop_preserve_semantic_header_fields() {
        let mut list = FreeList::new();
        let mut block = TestBlock::new();
        initialize_test_header(&mut block, 48, 77);
        let ptr = block.as_ptr();
        // SAFETY: `ptr` names the start of the test block's initialized header storage.
        let before = unsafe { header_from_block_start(ptr).as_ref() };
        let expected_requested = before.requested_size();
        let expected_usable = before.usable_size();
        let expected_owner = before.small_owner_cache_id();

        // SAFETY: the test block is linked only through `list` for this check.
        unsafe {
            list.push_block(ptr);
        }
        // SAFETY: `ptr` still names the same initialized header after the reserved link word changes.
        let during = unsafe { header_from_block_start(ptr).as_ref() };
        assert_eq!(
            during.validate(),
            Ok(crate::header::AllocationKind::Small(SizeClass::B64))
        );
        assert_eq!(during.requested_size(), expected_requested);
        assert_eq!(during.usable_size(), expected_usable);
        assert_eq!(during.small_owner_cache_id(), expected_owner);

        // SAFETY: the list contains exactly the single test block.
        unsafe {
            assert_eq!(list.pop_block(), Some(ptr));
        }
        // SAFETY: popping the block detaches it but leaves its initialized header storage intact.
        let after = unsafe { header_from_block_start(ptr).as_ref() };
        assert_eq!(
            after.validate(),
            Ok(crate::header::AllocationKind::Small(SizeClass::B64))
        );
        assert_eq!(after.requested_size(), expected_requested);
        assert_eq!(after.usable_size(), expected_usable);
        assert_eq!(after.small_owner_cache_id(), expected_owner);
    }
}
