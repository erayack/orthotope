use core::ptr::NonNull;

use crate::arena::Arena;
use crate::central_pool::CentralPool;
use crate::config::AllocatorConfig;
use crate::error::{AllocError, FreeError, InitError};
#[cfg(test)]
use crate::free_list::Batch;
use crate::header::{
    AllocationHeader, AllocationKind, HEADER_ALIGNMENT, HEADER_SIZE, block_start_from_user_ptr,
    header_from_user_ptr, user_ptr_from_block_start,
};
use crate::large_object::LargeObjectAllocator;
use crate::size_class::SizeClass;
use crate::thread_cache::ThreadCache;

/// Standalone allocator instance with its own arena, central pool, and large-object registry.
///
/// Share one allocator across threads and keep one [`ThreadCache`] per participating
/// thread when using the instance-oriented API directly.
pub struct Allocator {
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
    /// does not promise full same-arena forgery or duplicate-free detection.
    pub unsafe fn deallocate_with_cache(
        &self,
        cache: &mut ThreadCache,
        user_ptr: NonNull<u8>,
    ) -> Result<(), FreeError> {
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

        let block_start = cache.pop(class).ok_or_else(|| AllocError::OutOfMemory {
            requested: requested_size,
            remaining: self.arena.remaining(),
        })?;

        let header = AllocationHeader::new_small(class, requested_size).ok_or_else(|| {
            AllocError::OutOfMemory {
                requested: requested_size,
                remaining: self.arena.remaining(),
            }
        })?;
        let _ = header.write_to_block(block_start);

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
        let span_start = span.start().as_ptr();

        for index in 0..carved {
            let offset = index * block_size;
            let block_start = span_start.wrapping_add(offset);
            // SAFETY: `span` is an exclusive reservation from the arena and `offset`
            // advances in exact block-size steps within the reserved range.
            let block_start = unsafe { NonNull::new_unchecked(block_start) };
            // SAFETY: each split block is unique, detached, and large enough for the
            // intrusive free-list node. Small headers are materialized on live allocation.
            unsafe {
                cache.push(class, block_start);
            }
        }

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
        let block_start = self.arena.allocate_block(block_size)?;
        let header = AllocationHeader::new_large(requested_size, usable_size).ok_or_else(|| {
            AllocError::OutOfMemory {
                requested: requested_size,
                remaining: self.arena.remaining(),
            }
        })?;
        let _ = header.write_to_block(block_start);

        let user_ptr = user_ptr_from_block_start(block_start);
        self.large
            .record_live_allocation(user_ptr, requested_size, usable_size);

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

        let header_ptr = header_from_user_ptr(user_ptr);
        // SAFETY: the checked subtraction and alignment guard above ensure the derived
        // header address is plausibly aligned for `AllocationHeader`; reading a copy
        // lets validation inspect the stored metadata before any routing decision.
        let header = unsafe { header_ptr.as_ptr().read() };
        let kind = header.validate()?;
        Ok((header, kind))
    }

    fn reserve_refill_span(
        &self,
        block_size: usize,
        refill_count: usize,
        requested_size: usize,
    ) -> Result<Option<crate::arena::ReservedSpan>, AllocError> {
        let requested_span_size =
            block_size
                .checked_mul(refill_count)
                .ok_or_else(|| AllocError::OutOfMemory {
                    requested: requested_size,
                    remaining: self.arena.remaining(),
                })?;

        match self.arena.reserve_span(requested_span_size) {
            Ok(span) => Ok(Some(span)),
            Err(AllocError::OutOfMemory { remaining, .. }) => {
                let mut reduced_count = remaining / block_size;

                while reduced_count > 0 {
                    let reduced_span_size =
                        block_size
                            .checked_mul(reduced_count)
                            .ok_or(AllocError::OutOfMemory {
                                requested: requested_size,
                                remaining,
                            })?;
                    match self.arena.reserve_span(reduced_span_size) {
                        Ok(span) => return Ok(Some(span)),
                        Err(AllocError::OutOfMemory { .. }) => {
                            reduced_count -= 1;
                        }
                        Err(AllocError::GlobalInitFailed) => {
                            return Err(AllocError::GlobalInitFailed);
                        }
                        Err(AllocError::ZeroSize) => return Err(AllocError::ZeroSize),
                    }
                }

                Ok(None)
            }
            Err(AllocError::GlobalInitFailed) => Err(AllocError::GlobalInitFailed),
            Err(AllocError::ZeroSize) => Err(AllocError::ZeroSize),
        }
    }
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
