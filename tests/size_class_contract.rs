use orthotope::{Allocator, AllocatorConfig, SizeClass, ThreadCache};

#[test]
fn config_class_block_size_matches_allocator_capacity_planning() {
    let class = SizeClass::B256;
    let config = AllocatorConfig {
        arena_size: 384 * 2,
        alignment: 128,
        refill_target_bytes: 384 * 2,
        local_cache_target_bytes: 384 * 2,
    };

    assert_eq!(config.class_block_size(class), 384);

    let allocator = match Allocator::new(config) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };
    let mut cache = ThreadCache::new(*allocator.config());

    let first = match allocator.allocate_with_cache(&mut cache, 65) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    };
    let second = match allocator.allocate_with_cache(&mut cache, 65) {
        Ok(ptr) => ptr,
        Err(error) => panic!(
            "expected second allocation to fit based on AllocatorConfig::class_block_size(): {error}"
        ),
    };

    // SAFETY: `first` was returned by this allocator above and has not been freed yet.
    let first_free = unsafe { allocator.deallocate_with_cache(&mut cache, first) };
    assert_eq!(first_free, Ok(()));

    // SAFETY: `second` was returned by this allocator above and has not been freed yet.
    let second_free = unsafe { allocator.deallocate_with_cache(&mut cache, second) };
    assert_eq!(second_free, Ok(()));
}
