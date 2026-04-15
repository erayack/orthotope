#![cfg(debug_assertions)]

use orthotope::{Allocator, AllocatorConfig, FreeError, ThreadCache};

const fn test_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 64,
        refill_target_bytes: 256,
        local_cache_target_bytes: 256,
    }
}

fn allocator_and_cache() -> (Allocator, ThreadCache) {
    let allocator = Allocator::new(test_config())
        .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
    let cache = ThreadCache::new(*allocator.config());

    (allocator, cache)
}

#[test]
fn same_thread_hot_block_duplicate_free_is_detected() {
    let (allocator, mut cache) = allocator_and_cache();
    let ptr = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected allocation to succeed: {error}"));

    // SAFETY: `ptr` is live and has not been freed yet.
    let first = unsafe { allocator.deallocate_with_cache(&mut cache, ptr) };
    assert_eq!(first, Ok(()));

    // SAFETY: this intentionally reuses the same pointer to verify duplicate-free detection.
    let second = unsafe { allocator.deallocate_with_cache(&mut cache, ptr) };
    assert_eq!(second, Err(FreeError::DoubleFree));
}

#[test]
fn duplicate_free_wins_over_size_mismatch_for_small_blocks() {
    let (allocator, mut cache) = allocator_and_cache();
    let ptr = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected allocation to succeed: {error}"));

    // SAFETY: `ptr` is live and is freed exactly once before the duplicate-free probe.
    unsafe {
        allocator
            .deallocate_with_cache(&mut cache, ptr)
            .unwrap_or_else(|error| panic!("expected first free to succeed: {error}"));
    }

    // SAFETY: this intentionally reuses the same pointer with the wrong size to prove the
    // duplicate-free check runs before the compatibility size check on small blocks.
    let duplicate = unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, 16) };
    assert_eq!(duplicate, Err(FreeError::DoubleFree));
}

#[test]
fn duplicate_free_after_hot_block_spills_into_slab_storage_is_detected() {
    let (allocator, mut cache) = allocator_and_cache();
    let first = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected first allocation to succeed: {error}"));
    let second = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected second allocation to succeed: {error}"));

    // SAFETY: both pointers are live and each is freed once here before the duplicate check.
    unsafe {
        allocator
            .deallocate_with_cache(&mut cache, first)
            .unwrap_or_else(|error| panic!("expected first free to succeed: {error}"));
        allocator
            .deallocate_with_cache(&mut cache, second)
            .unwrap_or_else(|error| panic!("expected second free to succeed: {error}"));
    }

    // SAFETY: `first` was already freed above and should now be rejected even after it left
    // the hot slot for slab/shared storage.
    let duplicate = unsafe { allocator.deallocate_with_cache(&mut cache, first) };
    assert_eq!(duplicate, Err(FreeError::DoubleFree));
}

#[test]
fn duplicate_remote_free_is_detected_after_flush_to_central() {
    let allocator = Allocator::new(test_config())
        .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
    let mut source_cache = ThreadCache::new(*allocator.config());
    let mut remote_cache = ThreadCache::new(*allocator.config());
    let ptr = allocator
        .allocate_with_cache(&mut source_cache, 32)
        .unwrap_or_else(|error| panic!("expected allocation to succeed: {error}"));

    // SAFETY: `ptr` is live here. Using a different cache id forces the remote-free path,
    // including the eager remote-buffer flush into central.
    let first = unsafe { allocator.deallocate_with_cache(&mut remote_cache, ptr) };
    assert_eq!(first, Ok(()));

    // SAFETY: this intentionally retries the same remote free to verify the preserved header
    // marker catches duplicates before the block is routed again.
    let second = unsafe { allocator.deallocate_with_cache(&mut remote_cache, ptr) };
    assert_eq!(second, Err(FreeError::DoubleFree));
}

#[test]
fn aba_reallocation_reuses_pointer_without_triggering_double_free() {
    let (allocator, mut cache) = allocator_and_cache();
    let first = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected first allocation to succeed: {error}"));

    // SAFETY: `first` is live and is freed exactly once before reuse.
    unsafe {
        allocator
            .deallocate_with_cache(&mut cache, first)
            .unwrap_or_else(|error| panic!("expected first free to succeed: {error}"));
    }

    let reused = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected reuse allocation to succeed: {error}"));
    assert_eq!(reused, first);

    // SAFETY: this uses the stale pointer value from the prior allocation instance. Because
    // the block was reallocated at the same address, the stale raw pointer is indistinguishable
    // from the current live allocation and therefore frees successfully instead of reporting
    // `DoubleFree`. This documents the intended ABA boundary.
    let aba = unsafe { allocator.deallocate_with_cache(&mut cache, first) };
    assert_eq!(aba, Ok(()));
}
