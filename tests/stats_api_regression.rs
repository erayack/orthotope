use orthotope::{Allocator, AllocatorConfig, SizeClass, ThreadCache};

const fn draining_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 64,
        refill_target_bytes: 512,
        local_cache_target_bytes: 256,
    }
}

#[test]
fn freeing_into_a_slab_backed_cache_drains_a_full_batch_when_over_limit() {
    let config = draining_config();
    let allocator = Allocator::new(config)
        .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
    let mut cache = ThreadCache::new(config);
    let class = SizeClass::B64;
    let ptr = allocator
        .allocate_with_cache(&mut cache, 32)
        .unwrap_or_else(|error| panic!("expected allocation to succeed: {error}"));

    assert_eq!(config.refill_count(class), 4);
    assert_eq!(config.local_limit(class), 2);
    assert_eq!(config.drain_count(class), 2);

    // SAFETY: `ptr` is still live and is freed exactly once here.
    unsafe {
        allocator
            .deallocate_with_cache(&mut cache, ptr)
            .unwrap_or_else(|error| panic!("expected free to succeed: {error}"));
    }

    let local = cache.stats();
    let shared = allocator.stats();

    assert_eq!(
        local.local[class.index()].blocks,
        config.local_limit(class),
        "freeing into an over-limit cache should leave at most the configured local limit"
    );
    assert_eq!(
        shared.small_central[class.index()].blocks,
        config.drain_count(class),
        "the central pool should receive a full configured drain batch"
    );
}
