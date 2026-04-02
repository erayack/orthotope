use orthotope::allocator::Allocator;
use orthotope::config::AllocatorConfig;
use orthotope::error::FreeError;
use orthotope::size_class::SizeClass;
use orthotope::thread_cache::ThreadCache;

const fn reuse_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 26,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }
}

fn allocator_and_cache() -> (Allocator, ThreadCache) {
    let allocator = match Allocator::new(reuse_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let cache = ThreadCache::new(*allocator.config());

    (allocator, cache)
}

#[test]
fn same_thread_small_free_reuses_identical_pointer() {
    let (allocator, mut cache) = allocator_and_cache();

    let first = match allocator.allocate_with_cache(&mut cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    };

    // SAFETY: `first` was allocated above and has not been freed yet.
    match unsafe { allocator.deallocate_with_cache(&mut cache, first) } {
        Ok(()) => {}
        Err(error) => panic!("expected small free to succeed: {error}"),
    }

    let second = match allocator.allocate_with_cache(&mut cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected second allocation to succeed: {error}"),
    };

    assert_eq!(first, second);
}

#[test]
fn reused_block_refreshes_requested_size_within_same_class() {
    let (allocator, mut cache) = allocator_and_cache();

    let first = match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    };

    // SAFETY: `first` was allocated above and has not been freed yet.
    match unsafe { allocator.deallocate_with_cache(&mut cache, first) } {
        Ok(()) => {}
        Err(error) => panic!("expected first free to succeed: {error}"),
    }

    let second = match allocator.allocate_with_cache(&mut cache, 64) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected same-class allocation to succeed: {error}"),
    };

    assert_eq!(first, second);

    // SAFETY: `second` is still live and was allocated by this allocator.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, second, 1) } {
        Err(FreeError::SizeMismatch { provided, recorded }) => {
            assert_eq!(provided, 1);
            assert_eq!(recorded, 64);
        }
        Err(error) => panic!("unexpected deallocation error: {error}"),
        Ok(()) => panic!("expected stale size check to fail after header refresh"),
    }

    // SAFETY: the mismatch path does not free the allocation, so `second` remains live.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, second, 64) } {
        Ok(()) => {}
        Err(error) => panic!("expected matching size check to succeed: {error}"),
    }
}

#[test]
fn class_boundary_crossing_does_not_reuse_smaller_block() {
    let (allocator, mut cache) = allocator_and_cache();

    let small = match allocator.allocate_with_cache(&mut cache, 64) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected small-class allocation to succeed: {error}"),
    };

    // SAFETY: `small` was allocated above and has not been freed yet.
    match unsafe { allocator.deallocate_with_cache(&mut cache, small) } {
        Ok(()) => {}
        Err(error) => panic!("expected small free to succeed: {error}"),
    }

    let medium = match allocator.allocate_with_cache(&mut cache, 65) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected next-class allocation to succeed: {error}"),
    };

    assert_ne!(small, medium);

    // SAFETY: `medium` is live and belongs to this allocator.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, medium, 65) } {
        Ok(()) => {}
        Err(error) => panic!("expected matching size-checked free to succeed: {error}"),
    }
}

#[test]
fn requests_above_small_limit_reuse_freed_large_block() {
    let (allocator, mut cache) = allocator_and_cache();
    let request = SizeClass::max_small_request() + 1;

    let first = match allocator.allocate_with_cache(&mut cache, request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected large allocation to succeed: {error}"),
    };

    // SAFETY: `first` is a live large allocation returned by this allocator.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, first, request) } {
        Ok(()) => {}
        Err(error) => panic!("expected large free to succeed: {error}"),
    }

    let second = match allocator.allocate_with_cache(&mut cache, request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected second large allocation to succeed: {error}"),
    };

    assert_eq!(first, second);

    // SAFETY: `second` is a live large allocation returned by this allocator.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, second, request) } {
        Ok(()) => {}
        Err(error) => panic!("expected second large free to succeed: {error}"),
    }
}
