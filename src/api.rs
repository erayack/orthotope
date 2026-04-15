use core::ptr::NonNull;

use crate::error::{AllocError, FreeError};
use crate::stats::AllocatorStats;
use crate::{InitError, global_allocator, try_with_thread_cache};

/// Allocates `size` bytes from the process-global allocator.
///
/// The returned pointer is non-null and points to the user-visible payload region of
/// an internal allocator block.
///
/// # Errors
///
/// Returns [`AllocError::GlobalInitFailed`] when the process-global allocator could not
/// be initialized, including the case where the current thread re-enters this
/// convenience API while its thread-local cache is already mutably borrowed.
///
/// Returns [`AllocError::ZeroSize`] for zero-sized requests and
/// [`AllocError::OutOfMemory`] when the global allocator cannot satisfy the request.
pub fn allocate(size: usize) -> Result<NonNull<u8>, AllocError> {
    try_with_thread_cache(|allocator, cache| allocator.allocate_with_cache(cache, size))
        .map_err(|_| AllocError::GlobalInitFailed)?
        .ok_or(AllocError::GlobalInitFailed)?
}

/// # Safety
///
/// `ptr` must be a live pointer returned by this crate's allocator and must not have
/// been deallocated already.
///
/// # Errors
///
/// Returns [`FreeError::GlobalInitFailed`] when the process-global allocator could not
/// be initialized, including the case where the current thread re-enters this
/// convenience API while its thread-local cache is already mutably borrowed.
///
/// Returns [`FreeError::CorruptHeader`] when the allocation metadata cannot be decoded,
/// [`FreeError::ForeignPointer`] when a decoded small block falls outside the current
/// allocator's arena-range ownership boundary, [`FreeError::DoubleFree`] for duplicate
/// small frees detected while the freed marker is still intact in debug builds, and
/// [`FreeError::AlreadyFreedOrUnknownLarge`] for unknown large frees.
///
/// Small-object provenance in v1 is limited to header validation plus that
/// arena-range/alignment check plus a debug-build freed marker. Same-arena forgery,
/// stale large pointers after address reuse, cold-page reclaim that discards freed
/// markers, and stale-pointer ABA cases remain outside guaranteed detection, so
/// violating the safety contract can still be UB even when an error is returned for
/// some invalid pointers.
pub unsafe fn deallocate(ptr: NonNull<u8>) -> Result<(), FreeError> {
    try_with_thread_cache(|allocator, cache| {
        // SAFETY: the caller guarantees that `ptr` is a live allocation from this
        // allocator and has not already been freed.
        unsafe { allocator.deallocate_with_cache(cache, ptr) }
    })
    .map_err(|_| FreeError::GlobalInitFailed)?
    .ok_or(FreeError::GlobalInitFailed)?
}

/// # Safety
///
/// `ptr` must be a live pointer returned by this crate's allocator and must not have
/// been deallocated already. `size` must match the original requested allocation size.
///
/// # Errors
///
/// Returns [`FreeError::GlobalInitFailed`] when the process-global allocator could not
/// be initialized, including the case where the current thread re-enters this
/// convenience API while its thread-local cache is already mutably borrowed.
///
/// Returns [`FreeError::SizeMismatch`] when `size` does not match the recorded request
/// size, plus the same errors as [`deallocate`]. The size check is a compatibility
/// guard and does not provide stronger small-object provenance than the primary free
/// path.
pub unsafe fn deallocate_with_size(ptr: NonNull<u8>, size: usize) -> Result<(), FreeError> {
    try_with_thread_cache(|allocator, cache| {
        // SAFETY: the caller guarantees that `ptr` is a live allocation from this
        // allocator and the provided `size` describes that live allocation.
        unsafe { allocator.deallocate_with_size_checked(cache, ptr, size) }
    })
    .map_err(|_| FreeError::GlobalInitFailed)?
    .ok_or(FreeError::GlobalInitFailed)?
}

/// Returns a best-effort snapshot of the process-global allocator's shared state.
///
/// The snapshot includes arena capacity and remaining space, central-pool occupancy,
/// and large-allocation tracking state. It does not include other threads' private
/// thread-cache occupancy.
///
/// The returned values are not collected atomically across all allocator subsystems.
/// Under concurrent allocation or free traffic, different fields may reflect slightly
/// different instants.
///
/// # Errors
///
/// Returns [`InitError`] when the process-global allocator could not be initialized.
pub fn global_stats() -> Result<AllocatorStats, &'static InitError> {
    let allocator = global_allocator()?;
    Ok(allocator.stats())
}

#[cfg(test)]
mod tests {
    use super::{allocate, deallocate};
    use crate::error::{AllocError, FreeError};
    use crate::try_with_thread_cache;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    struct AllocateDuringTlsTeardown {
        result: Arc<Mutex<Option<Result<(), AllocError>>>>,
    }

    impl Drop for AllocateDuringTlsTeardown {
        fn drop(&mut self) {
            let outcome = allocate(32).map(|_| ());
            *self.result.lock().unwrap_or_else(|error| {
                panic!("expected teardown result lock to succeed: {error}")
            }) = Some(outcome);
        }
    }

    thread_local! {
        static TEARDOWN_PROBE: RefCell<Option<AllocateDuringTlsTeardown>> = const { RefCell::new(None) };
    }

    fn reentrant_allocate_result() -> Result<core::ptr::NonNull<u8>, AllocError> {
        try_with_thread_cache(|_, _| allocate(32))
            .unwrap_or_else(|error| panic!("expected global allocator init to succeed: {error}"))
            .unwrap_or_else(|| panic!("expected outer thread-cache borrow to succeed"))
    }

    fn reentrant_deallocate_result() -> Result<(), FreeError> {
        let ptr = allocate(32)
            .unwrap_or_else(|error| panic!("expected initial allocation to succeed: {error}"));

        let outcome = try_with_thread_cache(|_, _| {
            // SAFETY: `ptr` is still live here and this helper intentionally exercises
            // the reentrant public deallocation path.
            unsafe { deallocate(ptr) }
        })
        .unwrap_or_else(|error| panic!("expected global allocator init to succeed: {error}"))
        .unwrap_or_else(|| panic!("expected outer thread-cache borrow to succeed"));

        // SAFETY: the inner call returned an error, so `ptr` is still live and requires cleanup.
        unsafe {
            deallocate(ptr).unwrap_or_else(|error| {
                panic!("expected cleanup deallocation to succeed: {error}")
            });
        }

        outcome
    }

    #[test]
    fn public_allocate_returns_typed_error_when_thread_cache_is_reentrantly_borrowed() {
        assert_eq!(
            reentrant_allocate_result(),
            Err(AllocError::GlobalInitFailed)
        );
    }

    #[test]
    fn public_deallocate_returns_typed_error_when_thread_cache_is_reentrantly_borrowed() {
        assert_eq!(
            reentrant_deallocate_result(),
            Err(FreeError::GlobalInitFailed)
        );
    }

    #[test]
    fn public_allocate_does_not_panic_when_thread_cache_is_reentrantly_borrowed() {
        let outcome = std::panic::catch_unwind(reentrant_allocate_result);

        assert!(
            outcome.is_ok(),
            "public allocate should return an error instead of panicking on reentrant thread-cache borrow"
        );
    }

    #[test]
    fn public_deallocate_does_not_panic_when_thread_cache_is_reentrantly_borrowed() {
        let outcome = std::panic::catch_unwind(reentrant_deallocate_result);

        assert!(
            outcome.is_ok(),
            "public deallocate should return an error instead of panicking on reentrant thread-cache borrow"
        );
    }

    #[test]
    fn public_allocate_returns_typed_error_after_thread_cache_tls_is_destroyed() {
        let teardown_result = Arc::new(Mutex::new(None));
        let thread_result = Arc::clone(&teardown_result);

        let handle = std::thread::spawn(move || {
            TEARDOWN_PROBE.with(|probe| {
                *probe.borrow_mut() = Some(AllocateDuringTlsTeardown {
                    result: thread_result,
                });
            });

            let ptr = allocate(32)
                .unwrap_or_else(|error| panic!("expected initial allocation to succeed: {error}"));
            // SAFETY: `ptr` is still live here and is freed exactly once before thread exit.
            unsafe {
                deallocate(ptr).unwrap_or_else(|error| {
                    panic!("expected cleanup deallocation to succeed: {error}")
                });
            }
        });

        handle
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload));

        let outcome = teardown_result
            .lock()
            .unwrap_or_else(|error| panic!("expected teardown result lock to succeed: {error}"))
            .take()
            .unwrap_or_else(|| panic!("expected teardown probe to record a result"));

        assert_eq!(outcome, Err(AllocError::GlobalInitFailed));
    }
}
