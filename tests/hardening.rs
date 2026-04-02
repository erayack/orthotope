use core::ptr::NonNull;

use orthotope::{
    Allocator, AllocatorConfig, FreeError, InitError, SizeClass, ThreadCache, allocate, deallocate,
};

const fn test_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 26,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }
}

fn allocator_and_cache() -> (Allocator, ThreadCache) {
    let allocator = match Allocator::new(test_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let cache = ThreadCache::new(*allocator.config());

    (allocator, cache)
}

#[allow(clippy::missing_const_for_fn)]
fn overwrite_magic(user_ptr: NonNull<u8>) {
    let magic_ptr = user_ptr.as_ptr().wrapping_sub(64);
    // SAFETY: tests call this only for live allocator pointers whose header occupies the
    // first four bytes of the 64-byte block prefix, and byte writes avoid any extra
    // alignment requirements.
    unsafe {
        magic_ptr.write(0);
        magic_ptr.add(1).write(0);
        magic_ptr.add(2).write(0);
        magic_ptr.add(3).write(0);
    }
}

#[allow(clippy::missing_const_for_fn)]
fn read_header_bytes(user_ptr: NonNull<u8>) -> [u8; 64] {
    let mut header = [0; 64];
    let header_ptr = user_ptr.as_ptr().wrapping_sub(64);
    // SAFETY: tests call this only for live allocator pointers whose 64-byte header
    // immediately precedes the returned user pointer.
    unsafe {
        core::ptr::copy_nonoverlapping(header_ptr, header.as_mut_ptr(), header.len());
    }
    header
}

#[allow(clippy::missing_const_for_fn)]
fn write_header_bytes(user_ptr: NonNull<u8>, header: &[u8; 64]) {
    let header_ptr = user_ptr.as_ptr().wrapping_sub(64);
    // SAFETY: tests call this only for live allocator pointers whose 64-byte header
    // immediately precedes the returned user pointer.
    unsafe {
        core::ptr::copy_nonoverlapping(header.as_ptr(), header_ptr, header.len());
    }
}

#[test]
fn allocator_rejects_alignment_below_header_alignment() {
    let mut config = test_config();
    config.alignment = 32;

    match Allocator::new(config) {
        Err(InitError::InvalidConfig(message)) => {
            assert_eq!(message, "allocator alignment must be at least 64 bytes");
        }
        Ok(_) => panic!("expected allocator to reject sub-header alignment"),
        Err(error) => panic!("unexpected initialization error: {error}"),
    }
}

#[test]
fn corrupt_small_header_is_rejected() {
    let (allocator, mut cache) = allocator_and_cache();
    let ptr = match allocator.allocate_with_cache(&mut cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected small allocation to succeed: {error}"),
    };

    overwrite_magic(ptr);

    // SAFETY: `ptr` is still a live allocator pointer, but the test intentionally
    // corrupts its header to verify the free path rejects it.
    let result = unsafe { allocator.deallocate_with_cache(&mut cache, ptr) };
    assert_eq!(result, Err(FreeError::CorruptHeader));
}

#[test]
fn corrupt_large_header_is_rejected_without_consuming_live_record() {
    let (allocator, mut cache) = allocator_and_cache();
    let request = SizeClass::max_small_request() + 1;
    let ptr = match allocator.allocate_with_cache(&mut cache, request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected large allocation to succeed: {error}"),
    };
    let original_header = read_header_bytes(ptr);

    overwrite_magic(ptr);

    // SAFETY: `ptr` is still a live allocator pointer, but the test intentionally
    // corrupts its header to verify the large-object free path rejects it.
    let first = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) };
    assert_eq!(first, Err(FreeError::CorruptHeader));

    write_header_bytes(ptr, &original_header);

    // SAFETY: restoring the original header proves the live record was not consumed by
    // the earlier corruption failure.
    let second = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) };
    assert_eq!(second, Ok(()));
}

#[test]
fn large_duplicate_free_is_detected() {
    let (allocator, mut cache) = allocator_and_cache();
    let request = SizeClass::max_small_request() + 1;
    let ptr = match allocator.allocate_with_cache(&mut cache, request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected large allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` is live and belongs to this allocator.
    let first = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) };
    assert_eq!(first, Ok(()));

    // SAFETY: the pointer names a previously freed large allocation, which should be
    // rejected by the live-record tracker.
    let second = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) };
    assert_eq!(second, Err(FreeError::AlreadyFreedOrUnknownLarge));
}

#[test]
fn large_size_mismatch_does_not_free_allocation() {
    let (allocator, mut cache) = allocator_and_cache();
    let request = SizeClass::max_small_request() + 1;
    let ptr = match allocator.allocate_with_cache(&mut cache, request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected large allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` is still live and the mismatch path should validate only.
    let mismatch = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request + 1) };
    assert_eq!(
        mismatch,
        Err(FreeError::SizeMismatch {
            provided: request + 1,
            recorded: request,
        })
    );

    // SAFETY: the previous mismatch did not free the allocation.
    let success = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) };
    assert_eq!(success, Ok(()));
}

#[test]
fn crate_root_reexports_support_public_api_usage() {
    let ptr = match allocate(32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected public allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` was allocated above and has not been freed yet.
    let result = unsafe { deallocate(ptr) };
    assert_eq!(result, Ok(()));
}
