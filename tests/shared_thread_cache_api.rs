use orthotope::{Allocator, AllocatorConfig, ThreadCache};

const fn test_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 64,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    }
}

#[test]
#[should_panic(
    expected = "thread cache cannot be reused across allocators while it still holds cached blocks"
)]
fn shared_thread_cache_panics_before_rehoming_blocks_across_allocators() {
    let allocator_a = match Allocator::new(test_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator A to initialize: {error}"),
    };
    let allocator_b = match Allocator::new(test_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator B to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator_a.config());

    let first = match allocator_a.allocate_with_cache(&mut cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected allocator A allocation to succeed: {error}"),
    };

    // SAFETY: `first` is still live and belongs to allocator A.
    match unsafe { allocator_a.deallocate_with_cache(&mut cache, first) } {
        Ok(()) => {}
        Err(error) => panic!("expected allocator A free to succeed: {error}"),
    }

    let _ = allocator_b.allocate_with_cache(&mut cache, 32);
}

#[test]
fn empty_shared_thread_cache_can_rebind_to_another_allocator() {
    let allocator_a = match Allocator::new(test_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator A to initialize: {error}"),
    };
    let allocator_b = match Allocator::new(test_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator B to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator_a.config());

    let ptr = match allocator_b.allocate_with_cache(&mut cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => {
            panic!("expected allocator B allocation to succeed with an empty cache: {error}")
        }
    };
    let second = match allocator_b.allocate_with_cache(&mut cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => {
            panic!("expected allocator B to keep allocating cleanly after rebinding: {error}")
        }
    };

    assert_ne!(
        ptr, second,
        "rebinding an empty cache should not retain stale slab metadata from allocator A"
    );

    // SAFETY: `ptr` was returned by allocator B just above.
    let result = unsafe { allocator_b.deallocate_with_cache(&mut cache, ptr) };
    assert_eq!(result, Ok(()));

    // SAFETY: `second` is the other live allocation returned by allocator B above.
    let second_result = unsafe { allocator_b.deallocate_with_cache(&mut cache, second) };
    assert_eq!(second_result, Ok(()));
}
