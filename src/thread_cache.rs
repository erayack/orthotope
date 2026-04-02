use core::ptr::NonNull;

use crate::allocator::Allocator;
use crate::central_pool::CentralPool;
use crate::config::AllocatorConfig;
use crate::free_list::FreeList;
use crate::size_class::{NUM_CLASSES, SizeClass};

/// Caller-owned per-thread cache for small-object reuse.
///
/// Use one `ThreadCache` per participating thread when calling [`Allocator`] methods
/// directly. The process-global convenience API manages this internally.
pub struct ThreadCache {
    lists: [FreeList; NUM_CLASSES],
    config: AllocatorConfig,
}

impl ThreadCache {
    /// Creates a caller-owned thread-local cache for use with a specific [`Allocator`].
    ///
    /// Pair one `ThreadCache` with one thread when using the instance-oriented API
    /// directly. The global convenience API manages this cache internally.
    #[must_use]
    pub fn new(config: AllocatorConfig) -> Self {
        Self {
            lists: core::array::from_fn(|_| FreeList::new()),
            config,
        }
    }

    #[must_use]
    pub(crate) fn pop(&mut self, class: SizeClass) -> Option<NonNull<u8>> {
        // SAFETY: `ThreadCache` has exclusive mutable access to its per-class list.
        unsafe { self.lists[class.index()].pop_block() }
    }

    /// Pushes one block into the local free list for `class`.
    ///
    /// # Safety
    ///
    /// `block` must be a valid detached allocator block for `class`, large enough for
    /// the intrusive free-list node and not linked in any other list.
    pub(crate) unsafe fn push(&mut self, class: SizeClass, block: NonNull<u8>) {
        // SAFETY: the caller guarantees `block` is a valid detached node for this class,
        // and `&mut self` provides exclusive access to the destination free list.
        unsafe {
            self.lists[class.index()].push_block(block);
        }
    }

    #[must_use]
    pub(crate) const fn needs_refill(&self, class: SizeClass) -> bool {
        self.lists[class.index()].is_empty()
    }

    pub(crate) fn refill_from_central(&mut self, class: SizeClass, central: &CentralPool) -> usize {
        let batch = central.take_batch(class, self.config.refill_count(class));
        let moved = batch.len();

        // SAFETY: the batch came from the central pool as a detached chain for this class,
        // and `&mut self` gives exclusive access to the destination list.
        unsafe {
            self.lists[class.index()].push_batch(batch);
        }

        moved
    }

    #[must_use]
    pub(crate) const fn should_drain(&self, class: SizeClass) -> bool {
        self.lists[class.index()].len() > self.config.local_limit(class)
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

        let batch = {
            let list = &mut self.lists[class.index()];
            // SAFETY: the caller guarantees the local list holds only valid nodes for
            // this class, and `&mut self` ensures exclusive access during detachment.
            unsafe { list.pop_batch(self.config.drain_count(class)) }
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
        for class in SizeClass::ALL {
            let batch = {
                let list = &mut self.lists[class.index()];
                // SAFETY: `&mut self` guarantees exclusive access to each per-class list
                // while its full detached batch is removed.
                unsafe { list.pop_batch(list.len()) }
            };

            // SAFETY: each detached batch came from the matching local class list.
            unsafe {
                central.return_batch(class, batch);
            }
        }
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
}
