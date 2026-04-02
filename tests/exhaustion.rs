use orthotope::allocator::Allocator;
use orthotope::config::AllocatorConfig;
use orthotope::error::AllocError;
use orthotope::header::HEADER_SIZE;
use orthotope::size_class::SizeClass;
use orthotope::thread_cache::ThreadCache;

const fn tiny_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 256,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }
}

const fn partial_refill_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 768,
        alignment: 256,
        refill_target_bytes: 384,
        local_cache_target_bytes: 128,
    }
}

fn allocator_and_cache() -> (Allocator, ThreadCache) {
    let allocator = match Allocator::new(tiny_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let cache = ThreadCache::new(*allocator.config());

    (allocator, cache)
}

fn allocator_and_cache_with(config: AllocatorConfig) -> (Allocator, ThreadCache) {
    let allocator = match Allocator::new(config) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let cache = ThreadCache::new(*allocator.config());

    (allocator, cache)
}

const fn align_up(value: usize, alignment: usize) -> usize {
    let remainder = value % alignment;
    if remainder == 0 {
        value
    } else {
        value + (alignment - remainder)
    }
}

#[test]
fn tiny_arena_exhausts_after_exact_number_of_small_blocks() {
    let (allocator, mut cache) = allocator_and_cache();

    for attempt in 0..2 {
        match allocator.allocate_with_cache(&mut cache, 1) {
            Ok(_) => {}
            Err(error) => {
                panic!("expected allocation {attempt} to succeed before exhaustion: {error}")
            }
        }
    }

    match allocator.allocate_with_cache(&mut cache, 1) {
        Err(AllocError::GlobalInitFailed) => {
            panic!("direct allocator path should never observe global allocator init failure")
        }
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            assert_eq!(requested, 1);
            assert_eq!(remaining, 0);
        }
        Err(AllocError::ZeroSize) => panic!("unexpected zero-size error for non-zero request"),
        Ok(_) => panic!("expected third allocation to exhaust the arena"),
    }
}

#[test]
fn failed_refill_does_not_poison_reuse_of_freed_block() {
    let (allocator, mut cache) = allocator_and_cache();

    let first = match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    };
    let second = match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected second allocation to succeed: {error}"),
    };

    match allocator.allocate_with_cache(&mut cache, 1) {
        Err(AllocError::OutOfMemory { .. }) => {}
        Err(error) => panic!("unexpected allocation error: {error}"),
        Ok(_) => panic!("expected allocator to be exhausted"),
    }

    // SAFETY: `first` is still live and belongs to this allocator.
    match unsafe { allocator.deallocate_with_cache(&mut cache, first) } {
        Ok(()) => {}
        Err(error) => panic!("expected free after exhaustion to succeed: {error}"),
    }

    let reused = match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected freed block to remain reusable: {error}"),
    };

    assert_eq!(reused, first);
    assert_ne!(reused, second);

    // SAFETY: both pointers are live and belong to this allocator.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, reused, 1) } {
        Ok(()) => {}
        Err(error) => panic!("expected reused block to free cleanly: {error}"),
    }
    // SAFETY: `second` is still the other live allocation from this allocator.
    match unsafe { allocator.deallocate_with_size_checked(&mut cache, second, 1) } {
        Ok(()) => {}
        Err(error) => panic!("expected second block to free cleanly: {error}"),
    }
}

#[test]
fn oversized_large_request_returns_typed_out_of_memory() {
    let (allocator, mut cache) = allocator_and_cache();
    let request = SizeClass::max_small_request() + 1;
    let normalized_block_size = align_up(HEADER_SIZE + request, tiny_config().alignment);

    match allocator.allocate_with_cache(&mut cache, request) {
        Err(AllocError::GlobalInitFailed) => {
            panic!("direct allocator path should never observe global allocator init failure")
        }
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            assert_eq!(requested, normalized_block_size);
            assert_eq!(remaining, tiny_config().arena_size);
        }
        Err(AllocError::ZeroSize) => panic!("unexpected zero-size error for non-zero request"),
        Ok(_) => panic!("expected oversized large request to exhaust the tiny arena"),
    }
}

#[test]
fn partial_refill_retries_smaller_batch_after_alignment_loss() {
    let (allocator, mut cache) = allocator_and_cache_with(partial_refill_config());

    match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(_) => {}
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    }
    match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(_) => {}
        Err(error) => panic!("expected second allocation to succeed: {error}"),
    }
    match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(_) => {}
        Err(error) => panic!("expected third allocation to succeed: {error}"),
    }

    match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(_) => {}
        Err(error) => panic!("expected partial-refill allocation to succeed: {error}"),
    }
    match allocator.allocate_with_cache(&mut cache, 1) {
        Ok(_) => {}
        Err(error) => panic!("expected second partial-refill allocation to succeed: {error}"),
    }

    match allocator.allocate_with_cache(&mut cache, 1) {
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            assert_eq!(requested, 1);
            assert_eq!(remaining, 0);
        }
        Err(error) => panic!("unexpected allocation error after partial refill: {error}"),
        Ok(_) => panic!("expected allocator to exhaust after the largest fitting partial refill"),
    }
}
