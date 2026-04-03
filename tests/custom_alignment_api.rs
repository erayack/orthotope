use orthotope::{Allocator, AllocatorConfig, ThreadCache};

const fn custom_alignment_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 128,
        refill_target_bytes: 640,
        local_cache_target_bytes: 640,
    }
}

#[test]
fn small_block_from_custom_aligned_allocator_can_be_freed() {
    let allocator = match Allocator::new(custom_alignment_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator.config());

    let ptr = match allocator.allocate_with_cache(&mut cache, 65) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected 65-byte allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` is the currently live allocation returned above.
    let result = unsafe { allocator.deallocate_with_cache(&mut cache, ptr) };

    assert_eq!(result, Ok(()));
}

#[test]
fn same_thread_reuse_still_hits_identical_pointer_with_custom_alignment() {
    let allocator = match Allocator::new(custom_alignment_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator.config());

    let first = match allocator.allocate_with_cache(&mut cache, 65) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected first 65-byte allocation to succeed: {error}"),
    };

    // SAFETY: `first` is live and belongs to this allocator.
    match unsafe { allocator.deallocate_with_cache(&mut cache, first) } {
        Ok(()) => {}
        Err(error) => panic!("expected first custom-aligned free to succeed: {error}"),
    }

    let second = match allocator.allocate_with_cache(&mut cache, 65) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected second 65-byte allocation to succeed: {error}"),
    };

    assert_eq!(first, second);

    // SAFETY: `second` is live and belongs to this allocator.
    let result = unsafe { allocator.deallocate_with_cache(&mut cache, second) };
    assert_eq!(result, Ok(()));
}
