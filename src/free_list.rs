use core::mem::size_of;
use core::ptr::NonNull;

#[repr(C)]
struct FreeBlock {
    next: Option<NonNull<Self>>,
}

#[allow(dead_code)]
/// Intrusive LIFO list of detached allocator blocks.
pub(crate) struct FreeList {
    head: Option<NonNull<FreeBlock>>,
    len: usize,
}

#[allow(dead_code)]
/// Detached block chain used for O(1) transfers between lists.
pub(crate) struct Batch {
    head: Option<NonNull<FreeBlock>>,
    tail: Option<NonNull<FreeBlock>>,
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
    /// large enough to hold a `FreeBlock`, and not currently linked in any free list.
    #[allow(clippy::missing_const_for_fn)]
    pub(crate) unsafe fn push_block(&mut self, block: NonNull<u8>) {
        let mut block = block.cast::<FreeBlock>();
        // SAFETY: the caller guarantees that `block` points to writable storage large
        // enough for `FreeBlock` and that it is not currently linked elsewhere.
        unsafe {
            block.as_mut().next = self.head;
        }
        self.head = Some(block);
        self.len += 1;
    }

    /// Pops one free block from the list.
    ///
    /// # Safety
    ///
    /// Every node currently linked in the list must point to writable storage large
    /// enough for `FreeBlock` and must belong exclusively to this list.
    #[must_use]
    pub(crate) unsafe fn pop_block(&mut self) -> Option<NonNull<u8>> {
        let mut head = self.head?;
        // SAFETY: `head` is a node already linked in this list, so reading its next
        // pointer is valid under the list invariants.
        let next = unsafe { head.as_ref().next };
        self.head = next;
        self.len -= 1;
        // SAFETY: `head` has been detached from the list, so clearing the next pointer
        // maintains the invariant that detached nodes are single-block chains.
        unsafe {
            head.as_mut().next = None;
        }
        Some(head.cast())
    }

    /// Prepends a detached batch to this list while preserving batch order.
    ///
    /// # Safety
    ///
    /// `batch` must describe a valid detached chain whose nodes are not linked in any
    /// other free list and whose storage is large enough for `FreeBlock`.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) unsafe fn push_batch(&mut self, batch: Batch) {
        let Batch { head, tail, len } = batch;
        if len == 0 {
            return;
        }

        let head = head.unwrap_or_else(|| unreachable!("non-empty batch must have a head"));
        let mut tail = tail.unwrap_or_else(|| unreachable!("non-empty batch must have a tail"));

        // SAFETY: `tail` is the last node in the detached batch, so wiring it to this
        // list head splices the entire batch in front without disturbing batch order.
        unsafe {
            tail.as_mut().next = self.head;
        }
        self.head = Some(head);
        self.len += len;
    }

    /// Detaches up to `max` blocks from the front of the list.
    ///
    /// # Safety
    ///
    /// Every node currently linked in the list must point to writable storage large
    /// enough for `FreeBlock` and must belong exclusively to this list.
    #[must_use]
    pub(crate) unsafe fn pop_batch(&mut self, max: usize) -> Batch {
        if max == 0 || self.is_empty() {
            return Batch::empty();
        }

        let take = core::cmp::min(max, self.len);
        let head = self
            .head
            .unwrap_or_else(|| unreachable!("non-empty list must have a head"));
        let mut tail = head;

        for _ in 1..take {
            // SAFETY: we only walk within the first `take` nodes of a valid list.
            tail = unsafe {
                tail.as_ref()
                    .next
                    .unwrap_or_else(|| unreachable!("free list shorter than recorded length"))
            };
        }

        // SAFETY: `tail` is the last node to detach, so taking its next pointer splits
        // the list into a detached batch and the remaining suffix.
        let remainder = unsafe { tail.as_ref().next };
        self.head = remainder;
        // SAFETY: `tail` is detached from the remaining list, so terminating the batch
        // with `None` preserves a valid detached chain.
        unsafe {
            tail.as_mut().next = None;
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
    /// Returns `true` when the batch has no blocks.
    pub(crate) const fn is_empty(&self) -> bool {
        self.head.is_none()
    }
}

const _: [(); size_of::<FreeBlock>()] = [(); size_of::<Option<NonNull<FreeBlock>>>()];

#[cfg(test)]
mod tests {
    use super::{Batch, FreeList};
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

    #[test]
    fn push_and_pop_are_lifo() {
        let mut list = FreeList::new();
        let mut blocks = [TestBlock::new(), TestBlock::new(), TestBlock::new()];
        let ptrs = blocks.each_mut().map(TestBlock::as_ptr);

        // SAFETY: test blocks are aligned, large enough for a `FreeBlock`, and linked
        // only through this list for the duration of the test.
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
        let ptrs = blocks.each_mut().map(TestBlock::as_ptr);

        // SAFETY: test blocks are aligned, large enough for a `FreeBlock`, and owned by this list.
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
        let ptrs = blocks.each_mut().map(TestBlock::as_ptr);

        // SAFETY: test blocks are aligned, large enough for a `FreeBlock`, and each block
        // is linked into at most one list at a time in this test.
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
        let ptr = block.as_ptr();

        // SAFETY: the test block is aligned, large enough for a `FreeBlock`, and linked
        // only through this list for the duration of the test.
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
}
