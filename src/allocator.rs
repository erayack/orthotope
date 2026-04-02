use core::ptr::NonNull;

use crate::arena::Arena;
use crate::central_pool::CentralPool;
use crate::config::AllocatorConfig;
use crate::error::{AllocError, FreeError, InitError};
#[cfg(test)]
use crate::free_list::Batch;
use crate::header::{
    AllocationHeader, AllocationKind, HEADER_SIZE, block_start_from_user_ptr, header_from_user_ptr,
    user_ptr_from_block_start,
};
use crate::large_object::LargeObjectAllocator;
use crate::size_class::SizeClass;
use crate::thread_cache::ThreadCache;

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
        let arena = Arena::new(&config)?;

        Ok(Self {
            config,
            arena,
            central: CentralPool::new(),
            large: LargeObjectAllocator::new(),
        })
    }

    #[must_use]
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
    /// not have been freed already.
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
    /// not have been freed already.
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
        let block_size = class.block_size();
        let refill_count = self.config.refill_count(class);
        let refill_header =
            AllocationHeader::new_small(class, class.payload_size()).ok_or_else(|| {
                AllocError::OutOfMemory {
                    requested: class.payload_size(),
                    remaining: self.arena.remaining(),
                }
            })?;
        let mut carved = 0;

        for _ in 0..refill_count {
            let block_start = match self.arena.allocate_block(block_size) {
                Ok(block_start) => block_start,
                Err(AllocError::OutOfMemory { .. }) if carved > 0 => break,
                Err(AllocError::OutOfMemory { remaining, .. }) => {
                    return Err(AllocError::OutOfMemory {
                        requested: requested_size,
                        remaining,
                    });
                }
                Err(AllocError::GlobalInitFailed) => return Err(AllocError::GlobalInitFailed),
                Err(AllocError::ZeroSize) => return Err(AllocError::ZeroSize),
            };
            let _ = refill_header.write_to_block(block_start);

            // SAFETY: the arena returned a unique block start for this size class, the
            // header has been initialized, and the block is not linked in any list yet.
            unsafe {
                cache.push(class, block_start);
            }
            carved += 1;
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
            .record_live_allocation(user_ptr, block_start, block_size, requested_size);

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
                self.large.release_live_allocation(user_ptr)?;
                Ok(())
            }
        }
    }

    unsafe fn decode_header(
        user_ptr: NonNull<u8>,
    ) -> Result<(AllocationHeader, AllocationKind), FreeError> {
        let header_ptr = header_from_user_ptr(user_ptr);
        // SAFETY: `header_ptr` points to the allocation header immediately preceding
        // `user_ptr`; reading a copy lets validation inspect the stored metadata.
        let header = unsafe { header_ptr.as_ptr().read() };
        let kind = header.validate()?;
        Ok((header, kind))
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
