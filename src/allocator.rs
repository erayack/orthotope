use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arena::Arena;
use crate::central_pool::CentralPool;
use crate::config::AllocatorConfig;
use crate::error::{AllocError, FreeError, InitError};
#[cfg(test)]
use crate::free_list::Batch;
use crate::header::{
    AllocationHeader, AllocationKind, HEADER_ALIGNMENT, HEADER_SIZE, block_start_from_user_ptr,
    user_ptr_from_block_start,
};
use crate::large_object::LargeObjectAllocator;
use crate::size_class::SizeClass;
use crate::stats::{AllocatorStats, SizeClassStats};
use crate::thread_cache::{BlockReuse, ThreadCache};

static NEXT_ALLOCATOR_ID: AtomicUsize = AtomicUsize::new(1);

/// Standalone allocator instance with its own arena, central pool, and large-object registry.
///
/// Share one allocator across threads and keep one [`ThreadCache`] per participating
/// thread when using the instance-oriented API directly.
pub struct Allocator {
    id: usize,
    config: AllocatorConfig,
    arena: Arena,
    central: CentralPool,
    large: LargeObjectAllocator,
}

// SAFETY: `Allocator` shares only immutable configuration plus internally synchronized
// components: `Arena` uses an atomic bump pointer, `CentralPool` is mutex-protected per
// class, and `LargeObjectAllocator` guards its registry with a mutex. Per-thread mutable
// state remains outside the allocator in `ThreadCache`.
unsafe impl Send for Allocator {}

// SAFETY: shared access is safe for the same reasons as above; mutable thread-local cache
// state is passed in explicitly and never stored inside the global allocator.
unsafe impl Sync for Allocator {}

impl Allocator {
    /// Creates a standalone allocator instance with its own arena and shared pools.
    ///
    /// # Errors
    ///
    /// Propagates arena initialization failures for invalid configuration or mapping
    /// errors.
    pub fn new(config: AllocatorConfig) -> Result<Self, InitError> {
        validate_allocator_config(&config)?;
        let arena = Arena::new(&config)?;

        Ok(Self {
            id: NEXT_ALLOCATOR_ID.fetch_add(1, Ordering::Relaxed),
            config,
            arena,
            central: CentralPool::new(),
            large: LargeObjectAllocator::new(),
        })
    }

    #[must_use]
    /// Returns the configuration used to construct this allocator.
    pub const fn config(&self) -> &AllocatorConfig {
        &self.config
    }

    #[must_use]
    pub(crate) const fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn drain_thread_cache_on_exit(&self, cache: &mut ThreadCache) {
        // SAFETY: thread-local teardown has exclusive access to the cache being drained,
        // and each cached block already belongs to the matching per-class local list.
        unsafe {
            cache.drain_all_to_central(&self.central);
        }
    }

    #[cfg(test)]
    pub(crate) fn take_central_batch_for_test(&self, class: SizeClass, max: usize) -> Batch {
        self.central.take_batch(class, max)
    }

    #[must_use]
    /// Returns a best-effort snapshot of allocator-wide shared state.
    ///
    /// This snapshot is assembled from independently synchronized subsystems.
    /// Under concurrent allocation or free traffic, different fields may reflect
    /// slightly different instants.
    pub fn stats(&self) -> AllocatorStats {
        let central_counts = self.central.block_counts();
        let small_central = core::array::from_fn(|index| {
            let class = SizeClass::ALL[index];
            let blocks = central_counts[index];
            SizeClassStats {
                class,
                blocks,
                bytes: blocks * self.config.class_block_size(class),
            }
        });
        let large = self.large.stats();

        AllocatorStats {
            arena_capacity: self.arena.capacity(),
            arena_remaining: self.arena.remaining(),
            small_central,
            large_live_allocations: large.live_allocations,
            large_live_bytes: large.live_bytes,
            large_free_blocks: large.free_blocks,
            large_free_bytes: large.free_bytes,
        }
    }

    /// Allocates a block using the provided thread-local cache for small objects.
    ///
    /// Callers should pair one mutable [`ThreadCache`] with one thread.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::ZeroSize`] for zero-byte requests and
    /// [`AllocError::OutOfMemory`] when the arena cannot satisfy the request.
    pub fn allocate_with_cache(
        &self,
        cache: &mut ThreadCache,
        requested_size: usize,
    ) -> Result<NonNull<u8>, AllocError> {
        cache.bind_to_allocator(self);

        if requested_size == 0 {
            return Err(AllocError::ZeroSize);
        }

        SizeClass::from_request(requested_size).map_or_else(
            || self.allocate_large(requested_size),
            |class| self.allocate_small_with_cache(cache, class, requested_size),
        )
    }

    /// Deallocates a previously allocated pointer using the provided cache.
    ///
    /// # Safety
    ///
    /// `user_ptr` must be a live pointer returned by this allocator instance and must
    /// not have been freed already. `cache` must be the caller's thread-local cache for
    /// this allocator participation.
    ///
    /// # Errors
    ///
    /// Returns [`FreeError::CorruptHeader`] when the allocation header cannot be
    /// validated, [`FreeError::ForeignPointer`] when a decoded small block falls
    /// outside this allocator's arena-range ownership boundary, and
    /// [`FreeError::AlreadyFreedOrUnknownLarge`] for unknown large frees.
    ///
    /// Small-object provenance in v1 is limited to header validation plus an
    /// arena-range/alignment check on the decoded block start. This intentionally
    /// does not promise full same-arena forgery, duplicate-free detection for small
    /// allocations, or stale large-pointer detection after address reuse.
    pub unsafe fn deallocate_with_cache(
        &self,
        cache: &mut ThreadCache,
        user_ptr: NonNull<u8>,
    ) -> Result<(), FreeError> {
        cache.bind_to_allocator(self);

        // SAFETY: the caller promises that `user_ptr` came from this allocator and is
        // valid to decode as an allocator block header.
        unsafe { self.deallocate_impl(cache, user_ptr, None) }
    }

    /// Deallocates a pointer while validating the caller-provided allocation size.
    ///
    /// # Safety
    ///
    /// `user_ptr` must be a live pointer returned by this allocator instance and must
    /// not have been freed already. `cache` must be the caller's thread-local cache for
    /// this allocator participation.
    ///
    /// # Errors
    ///
    /// Returns [`FreeError::SizeMismatch`] when `expected_size` differs from the
    /// recorded header size, plus the same errors as [`Self::deallocate_with_cache`].
    ///
    /// The additional size check does not strengthen small-object provenance beyond
    /// the v1 header-plus-arena-range ownership check.
    pub unsafe fn deallocate_with_size_checked(
        &self,
        cache: &mut ThreadCache,
        user_ptr: NonNull<u8>,
        expected_size: usize,
    ) -> Result<(), FreeError> {
        cache.bind_to_allocator(self);

        // SAFETY: the caller promises that `user_ptr` came from this allocator and is
        // valid to decode as an allocator block header.
        unsafe { self.deallocate_impl(cache, user_ptr, Some(expected_size)) }
    }

    fn allocate_small_with_cache(
        &self,
        cache: &mut ThreadCache,
        class: SizeClass,
        requested_size: usize,
    ) -> Result<NonNull<u8>, AllocError> {
        if cache.needs_refill(class) {
            let moved = cache.refill_from_central(class, &self.central);
            if moved == 0 {
                let carved = self.refill_cache_from_arena(cache, class, requested_size)?;
                if carved == 0 {
                    return Err(AllocError::OutOfMemory {
                        requested: requested_size,
                        remaining: self.arena.remaining(),
                    });
                }
            }
        }

        let (block_start, reuse) = cache.pop(class).ok_or_else(|| AllocError::OutOfMemory {
            requested: requested_size,
            remaining: self.arena.remaining(),
        })?;

        match reuse {
            BlockReuse::HotReuseRequestedOnly => {
                let _ = AllocationHeader::refresh_small_requested_size(block_start, requested_size)
                    .ok_or_else(|| AllocError::OutOfMemory {
                        requested: requested_size,
                        remaining: self.arena.remaining(),
                    })?;
            }
            BlockReuse::FreshNeedsOwnerRefresh => {
                let _ = AllocationHeader::refresh_small_requested_size_and_owner(
                    block_start,
                    requested_size,
                    cache.cache_id(),
                )
                .ok_or_else(|| AllocError::OutOfMemory {
                    requested: requested_size,
                    remaining: self.arena.remaining(),
                })?;
            }
            BlockReuse::NeedsHeaderRewrite => {
                let _ = AllocationHeader::write_small_to_block(
                    block_start,
                    class,
                    requested_size,
                    cache.cache_id(),
                )
                .ok_or_else(|| AllocError::OutOfMemory {
                    requested: requested_size,
                    remaining: self.arena.remaining(),
                })?;
            }
        }

        Ok(user_ptr_from_block_start(block_start))
    }

    fn refill_cache_from_arena(
        &self,
        cache: &mut ThreadCache,
        class: SizeClass,
        requested_size: usize,
    ) -> Result<usize, AllocError> {
        let block_size = class.block_size_for_alignment(self.config.alignment);
        let refill_count = self.config.refill_count(class);
        let Some(span) = self.reserve_refill_span(block_size, refill_count, requested_size)? else {
            return Err(AllocError::OutOfMemory {
                requested: requested_size,
                remaining: self.arena.remaining(),
            });
        };

        let carved = span.size() / block_size;
        initialize_small_span_headers(span.start(), block_size, carved, class)?;

        cache.push_owned_slab(class, span.start(), block_size, carved);

        Ok(carved)
    }

    fn allocate_large(&self, requested_size: usize) -> Result<NonNull<u8>, AllocError> {
        let total_size =
            HEADER_SIZE
                .checked_add(requested_size)
                .ok_or_else(|| AllocError::OutOfMemory {
                    requested: requested_size,
                    remaining: self.arena.remaining(),
                })?;
        let block_size = align_up_checked(total_size, self.config.alignment).ok_or_else(|| {
            AllocError::OutOfMemory {
                requested: requested_size,
                remaining: self.arena.remaining(),
            }
        })?;
        let usable_size =
            block_size
                .checked_sub(HEADER_SIZE)
                .ok_or_else(|| AllocError::OutOfMemory {
                    requested: requested_size,
                    remaining: self.arena.remaining(),
                })?;
        let (block_start, actual_block_size, usable_size) =
            self.large.take_reusable_block(block_size).map_or_else(
                || {
                    self.arena
                        .allocate_block(block_size)
                        .map(|block_start| (block_start, block_size, usable_size))
                        .map_err(|error| match error {
                            AllocError::OutOfMemory { remaining, .. } => AllocError::OutOfMemory {
                                requested: requested_size,
                                remaining,
                            },
                            other => other,
                        })
                },
                |block| Ok((block.block_start(), block.block_size, block.usable_size())),
            )?;
        let _ = AllocationHeader::write_large_to_block(block_start, requested_size, usable_size)
            .ok_or_else(|| AllocError::OutOfMemory {
                requested: requested_size,
                remaining: self.arena.remaining(),
            })?;

        let user_ptr = user_ptr_from_block_start(block_start);
        self.large.record_live_allocation(
            user_ptr,
            block_start,
            actual_block_size,
            requested_size,
            usable_size,
        );

        Ok(user_ptr)
    }

    unsafe fn deallocate_impl(
        &self,
        cache: &mut ThreadCache,
        user_ptr: NonNull<u8>,
        expected_size: Option<usize>,
    ) -> Result<(), FreeError> {
        // SAFETY: the caller guarantees `user_ptr` is intended to name an allocation
        // from this allocator, so decoding its header is the required validation step.
        let (header, kind) = unsafe { Self::decode_header(user_ptr)? };

        if let Some(expected_size) = expected_size {
            let recorded = header.requested_size();
            if expected_size != recorded {
                return Err(FreeError::SizeMismatch {
                    provided: expected_size,
                    recorded,
                });
            }
        }

        match kind {
            AllocationKind::Small(class) => {
                let block_start = block_start_from_user_ptr(user_ptr);
                // In v1, small-object ownership is proven only by a valid decoded
                // header plus membership in this allocator's aligned arena range.
                if !self.arena.contains_block_start(block_start) {
                    return Err(FreeError::ForeignPointer);
                }
                let owner_cache_id = header
                    .small_owner_cache_id()
                    .ok_or(FreeError::CorruptHeader)?;
                if owner_cache_id == cache.cache_id() {
                    // SAFETY: the decoded header proved that this user pointer belongs to a
                    // valid small allocation block for `class`.
                    unsafe {
                        cache.push(class, block_start);
                    }

                    if cache.should_drain(class) {
                        // SAFETY: the local cache for `class` contains only allocator-owned
                        // blocks for that class, including the block just returned above.
                        unsafe {
                            cache.drain_excess_to_central(class, &self.central);
                        }
                    }
                } else {
                    // SAFETY: this decoded block belongs to `class` and this allocator;
                    // remote frees are detached into a dedicated remote buffer before
                    // batched transfer into the central pool.
                    unsafe {
                        cache.push_remote_and_maybe_flush(class, block_start, &self.central);
                    }
                }

                Ok(())
            }
            AllocationKind::Large => {
                self.large.validate_and_release_live_allocation(
                    user_ptr,
                    header.requested_size(),
                    header.usable_size(),
                )?;
                Ok(())
            }
        }
    }

    unsafe fn decode_header(
        user_ptr: NonNull<u8>,
    ) -> Result<(AllocationHeader, AllocationKind), FreeError> {
        let user_addr = user_ptr.as_ptr().addr();
        let Some(header_addr) = user_addr.checked_sub(HEADER_SIZE) else {
            return Err(FreeError::CorruptHeader);
        };
        if !header_addr.is_multiple_of(HEADER_ALIGNMENT) {
            return Err(FreeError::CorruptHeader);
        }

        // SAFETY: the checked subtraction and alignment guard above ensure the derived
        // header address is plausibly aligned for `AllocationHeader`; prefix validation
        // inspects only the routing fields before copying the full header.
        unsafe { AllocationHeader::read_from_user_ptr(user_ptr) }
    }

    fn reserve_refill_span(
        &self,
        block_size: usize,
        refill_count: usize,
        requested_size: usize,
    ) -> Result<Option<crate::arena::ReservedSpan>, AllocError> {
        let _ = block_size
            .checked_mul(refill_count)
            .ok_or_else(|| AllocError::OutOfMemory {
                requested: requested_size,
                remaining: self.arena.remaining(),
            })?;

        self.arena
            .reserve_block_span(block_size, refill_count)
            .map_err(|error| match error {
                AllocError::OutOfMemory { remaining, .. } => AllocError::OutOfMemory {
                    requested: requested_size,
                    remaining,
                },
                other => other,
            })
    }
}

fn initialize_small_span_headers(
    span_start: NonNull<u8>,
    block_size: usize,
    blocks: usize,
    class: SizeClass,
) -> Result<(), AllocError> {
    for index in 0..blocks {
        let offset = index
            .checked_mul(block_size)
            .ok_or(AllocError::OutOfMemory {
                requested: block_size,
                remaining: 0,
            })?;
        let block_start = span_start.as_ptr().wrapping_add(offset);
        // SAFETY: `offset` walks a bounded set of block starts inside one reserved span.
        let block_start = unsafe { NonNull::new_unchecked(block_start) };
        let _ =
            AllocationHeader::initialize_small_to_block(block_start, class).ok_or_else(|| {
                AllocError::OutOfMemory {
                    requested: class.payload_size(),
                    remaining: 0,
                }
            })?;
    }

    Ok(())
}

const fn align_up_checked(value: usize, alignment: usize) -> Option<usize> {
    let remainder = value % alignment;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

const fn validate_allocator_config(config: &AllocatorConfig) -> Result<(), InitError> {
    if config.alignment < HEADER_ALIGNMENT {
        return Err(InitError::InvalidConfig(
            "allocator alignment must be at least 64 bytes",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Allocator;
    use crate::config::AllocatorConfig;
    use crate::size_class::SizeClass;
    use crate::thread_cache::ThreadCache;

    const fn test_config() -> AllocatorConfig {
        AllocatorConfig {
            arena_size: 64 * 1024 * 1024,
            alignment: 64,
            refill_target_bytes: 256,
            local_cache_target_bytes: 384,
        }
    }

    #[test]
    fn stats_report_arena_capacity_and_small_central_occupancy() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut source = ThreadCache::new(test_config());
        let mut pointers = [None; 4];

        for slot in &mut pointers {
            let ptr = allocator
                .allocate_with_cache(&mut source, 32)
                .unwrap_or_else(|error| panic!("expected small allocation to succeed: {error}"));
            *slot = Some(ptr);
        }

        for ptr in pointers.into_iter().flatten() {
            // SAFETY: each pointer was returned live by `allocator` and is freed exactly once.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut source, ptr)
                    .unwrap_or_else(|error| panic!("expected small free to succeed: {error}"));
            }
        }
        allocator.drain_thread_cache_on_exit(&mut source);

        let stats = allocator.stats();
        let class_stats = stats.small_central[SizeClass::B64.index()];

        assert_eq!(stats.arena_capacity, test_config().arena_size);
        assert!(stats.arena_remaining < stats.arena_capacity);
        assert_eq!(class_stats.class, SizeClass::B64);
        assert_eq!(class_stats.blocks, 4);
        assert_eq!(
            class_stats.bytes,
            4 * SizeClass::B64.block_size_for_alignment(64)
        );
        assert_eq!(stats.total_small_central_blocks(), 4);
        assert_eq!(stats.total_small_central_bytes(), class_stats.bytes);
    }

    #[test]
    fn stats_report_large_live_and_reusable_bytes() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());

        let requested = SizeClass::max_small_request() + 1;
        let ptr = allocator
            .allocate_with_cache(&mut cache, requested)
            .unwrap_or_else(|error| panic!("expected large allocation to succeed: {error}"));

        let live_stats = allocator.stats();
        assert_eq!(live_stats.large_live_allocations, 1);
        assert!(live_stats.large_live_bytes >= requested);
        assert_eq!(live_stats.large_free_blocks, 0);

        // SAFETY: `ptr` is a live large allocation returned by `allocator` and is freed once.
        unsafe {
            allocator
                .deallocate_with_cache(&mut cache, ptr)
                .unwrap_or_else(|error| panic!("expected large free to succeed: {error}"));
        }

        let freed_stats = allocator.stats();
        assert_eq!(freed_stats.large_live_allocations, 0);
        assert_eq!(freed_stats.large_free_blocks, 1);
        assert_eq!(freed_stats.large_free_bytes, live_stats.large_live_bytes);
    }

    #[test]
    fn slab_refill_counts_each_fresh_block_once_and_reuses_freed_block() {
        let allocator = Allocator::new(test_config())
            .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
        let mut cache = ThreadCache::new(test_config());
        let class = SizeClass::B64;
        let request = 32;
        let carved = test_config().refill_count(class);

        let mut allocated = Vec::with_capacity(carved);
        for _ in 0..carved {
            let ptr = allocator
                .allocate_with_cache(&mut cache, request)
                .unwrap_or_else(|error| panic!("expected slab allocation to succeed: {error}"));
            allocated.push(ptr);
        }

        let stats = cache.stats();
        assert_eq!(stats.local[class.index()].blocks, 0);
        assert!(cache.needs_refill(class));

        let freed = allocated[0];
        // SAFETY: `freed` is a live small allocation returned above and is freed exactly once.
        unsafe {
            allocator
                .deallocate_with_cache(&mut cache, freed)
                .unwrap_or_else(|error| panic!("expected slab free to succeed: {error}"));
        }

        let stats = cache.stats();
        assert_eq!(stats.local[class.index()].blocks, 1);
        assert!(!cache.needs_refill(class));

        let reused = allocator
            .allocate_with_cache(&mut cache, request)
            .unwrap_or_else(|error| panic!("expected freed slab block to be reused: {error}"));
        assert_eq!(reused, freed);

        for ptr in allocated.into_iter().skip(1) {
            // SAFETY: each remaining pointer is still live and freed exactly once here.
            unsafe {
                allocator
                    .deallocate_with_cache(&mut cache, ptr)
                    .unwrap_or_else(|error| {
                        panic!("expected remaining slab allocation free to succeed: {error}")
                    });
            }
        }

        // SAFETY: `reused` is the currently live allocation and is freed exactly once here.
        unsafe {
            allocator
                .deallocate_with_cache(&mut cache, reused)
                .unwrap_or_else(|error| {
                    panic!("expected reused slab block free to succeed: {error}")
                });
        }
    }
}
