use orthotope::{AllocError, Allocator, AllocatorConfig, SizeClass, ThreadCache};

const fn align_up(value: usize, alignment: usize) -> usize {
    let remainder = value % alignment;
    if remainder == 0 {
        value
    } else {
        value + (alignment - remainder)
    }
}

const fn single_large_block_config() -> AllocatorConfig {
    let request = SizeClass::max_small_request() + 1;
    let block_size = align_up(request + orthotope::header::HEADER_SIZE, 64);

    AllocatorConfig {
        arena_size: block_size,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }
}

#[test]
fn freeing_a_large_block_restores_capacity_for_the_same_large_request() {
    let allocator = match Allocator::new(single_large_block_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator.config());
    let request = SizeClass::max_small_request() + 1;

    let ptr = match allocator.allocate_with_cache(&mut cache, request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected initial large allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` is the currently live large allocation returned above.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) } {
        Ok(()) => {}
        Err(error) => panic!("expected large free to succeed: {error}"),
    }

    match allocator.allocate_with_cache(&mut cache, request) {
        Ok(reused) => assert_eq!(reused, ptr),
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            panic!(
                "expected large free to restore capacity, but a second identical request \
                 exhausted the allocator instead (requested {requested} bytes, remaining \
                 {remaining} bytes)"
            );
        }
        Err(error) => panic!("unexpected allocation error after large free: {error}"),
    }
}

#[test]
fn reusing_a_larger_freed_block_preserves_its_full_future_reuse_capacity() {
    let large_request = 20_000_000;
    let medium_request = 17_000_000;
    let later_large_request = 19_000_000;
    let large_block_size = align_up(large_request + orthotope::header::HEADER_SIZE, 64);

    let allocator = match Allocator::new(AllocatorConfig {
        arena_size: large_block_size,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator.config());

    let first = match allocator.allocate_with_cache(&mut cache, large_request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected initial large allocation to succeed: {error}"),
    };

    // SAFETY: `first` is the currently live large allocation returned above.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, first, large_request) } {
        Ok(()) => {}
        Err(error) => panic!("expected first large free to succeed: {error}"),
    }

    let reused_for_medium = match allocator.allocate_with_cache(&mut cache, medium_request) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected medium request to reuse the freed large block: {error}"),
    };
    assert_eq!(reused_for_medium, first);

    // SAFETY: `reused_for_medium` is the currently live large allocation returned above.
    match unsafe {
        allocator.deallocate_with_size_checked(&mut cache, reused_for_medium, medium_request)
    } {
        Ok(()) => {}
        Err(error) => panic!("expected medium-sized large free to succeed: {error}"),
    }

    match allocator.allocate_with_cache(&mut cache, later_large_request) {
        Ok(reused_for_large) => assert_eq!(reused_for_large, first),
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            panic!(
                "expected the reused large block to keep its original capacity, but the later \
                 large request exhausted the allocator instead (requested {requested} bytes, \
                 remaining {remaining} bytes)"
            );
        }
        Err(error) => panic!("unexpected allocation error after medium reuse: {error}"),
    }
}

#[test]
fn large_reuse_prefers_smallest_fitting_freed_block() {
    let medium_request = 18_000_000;
    let large_request = 20_000_000;
    let target_request = 17_500_000;
    let arena_size = align_up(medium_request + orthotope::header::HEADER_SIZE, 64)
        + align_up(large_request + orthotope::header::HEADER_SIZE, 64);

    let allocator = match Allocator::new(AllocatorConfig {
        arena_size,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator.config());

    let medium = allocator
        .allocate_with_cache(&mut cache, medium_request)
        .unwrap_or_else(|error| panic!("expected medium large allocation to succeed: {error}"));
    let large = allocator
        .allocate_with_cache(&mut cache, large_request)
        .unwrap_or_else(|error| panic!("expected larger allocation to succeed: {error}"));

    // SAFETY: both pointers are still live large allocations returned by this allocator.
    unsafe {
        allocator
            .deallocate_with_size_checked(&mut cache, large, large_request)
            .unwrap_or_else(|error| panic!("expected larger free to succeed: {error}"));
        allocator
            .deallocate_with_size_checked(&mut cache, medium, medium_request)
            .unwrap_or_else(|error| panic!("expected medium free to succeed: {error}"));
    }

    let reused = allocator
        .allocate_with_cache(&mut cache, target_request)
        .unwrap_or_else(|error| panic!("expected target request to reuse a freed block: {error}"));

    assert_eq!(
        reused, medium,
        "large reuse should pick the smallest fitting freed block first"
    );
}
