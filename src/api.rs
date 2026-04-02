use core::ptr::NonNull;

use crate::error::{AllocError, FreeError};
use crate::with_thread_cache;

/// # Errors
///
/// Returns [`AllocError::GlobalInitFailed`] when the process-global allocator could not
/// be initialized.
///
/// Returns [`AllocError::ZeroSize`] for zero-sized requests and
/// [`AllocError::OutOfMemory`] when the global allocator cannot satisfy the request.
pub fn allocate(size: usize) -> Result<NonNull<u8>, AllocError> {
    with_thread_cache(|allocator, cache| allocator.allocate_with_cache(cache, size))
        .map_err(|_| AllocError::GlobalInitFailed)?
}

/// # Safety
///
/// `ptr` must be a live pointer returned by this crate's allocator and must not have
/// been deallocated already.
///
/// # Errors
///
/// Returns [`FreeError::GlobalInitFailed`] when the process-global allocator could not
/// be initialized.
///
/// Returns [`FreeError::CorruptHeader`] when the allocation metadata cannot be decoded,
/// [`FreeError::ForeignPointer`] when a decoded small block falls outside the current
/// allocator's arena-range ownership boundary, and
/// [`FreeError::AlreadyFreedOrUnknownLarge`] for unknown large frees.
///
/// Small-object provenance in v1 is limited to header validation plus that
/// arena-range/alignment check. Same-arena forgery and small double-free remain
/// outside guaranteed detection.
pub unsafe fn deallocate(ptr: NonNull<u8>) -> Result<(), FreeError> {
    with_thread_cache(|allocator, cache| {
        // SAFETY: the caller guarantees that `ptr` is a live allocation from this
        // allocator and has not already been freed.
        unsafe { allocator.deallocate_with_cache(cache, ptr) }
    })
    .map_err(|_| FreeError::GlobalInitFailed)?
}

/// # Safety
///
/// `ptr` must be a live pointer returned by this crate's allocator and must not have
/// been deallocated already. `size` must match the original requested allocation size.
///
/// # Errors
///
/// Returns [`FreeError::GlobalInitFailed`] when the process-global allocator could not
/// be initialized.
///
/// Returns [`FreeError::SizeMismatch`] when `size` does not match the recorded request
/// size, plus the same errors as [`deallocate`]. The size check is a compatibility
/// guard and does not provide stronger small-object provenance than the primary free
/// path.
pub unsafe fn deallocate_with_size(ptr: NonNull<u8>, size: usize) -> Result<(), FreeError> {
    with_thread_cache(|allocator, cache| {
        // SAFETY: the caller guarantees that `ptr` is a live allocation from this
        // allocator and the provided `size` describes that live allocation.
        unsafe { allocator.deallocate_with_size_checked(cache, ptr, size) }
    })
    .map_err(|_| FreeError::GlobalInitFailed)?
}
