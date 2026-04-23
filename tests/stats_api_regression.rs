use orthotope::{Allocator, AllocatorConfig, SizeClass, ThreadCache};

const fn align_up(value: usize, alignment: usize) -> usize {
    let remainder = value % alignment;
    if remainder == 0 {
        value
    } else {
        value + (alignment - remainder)
    }
}

const fn draining_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 64,
        refill_target_bytes: 512,
        local_cache_target_bytes: 256,
    }
}

const fn large_stats_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 64 << 20,
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

#[test]
fn large_reuse_stats_track_bucketed_free_blocks_correctly() {
    let config = large_stats_config();
    let allocator = Allocator::new(config)
        .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
    let mut cache = ThreadCache::new(config);
    let first_request = SizeClass::max_small_request() + 1;
    let second_request = first_request + 1_024;

    let first = allocator
        .allocate_with_cache(&mut cache, first_request)
        .unwrap_or_else(|error| panic!("expected first large allocation to succeed: {error}"));
    let second = allocator
        .allocate_with_cache(&mut cache, second_request)
        .unwrap_or_else(|error| panic!("expected second large allocation to succeed: {error}"));
    let first_block_size = align_up(first_request + orthotope::header::HEADER_SIZE, 64);
    let second_block_size = align_up(second_request + orthotope::header::HEADER_SIZE, 64);
    let live = allocator.stats();

    assert_eq!(live.large_live_allocations, 2);
    assert_eq!(live.large_live_bytes, first_block_size + second_block_size);

    // SAFETY: both pointers are still live large allocations returned by this allocator.
    unsafe {
        allocator
            .deallocate_with_size_checked(&mut cache, first, first_request)
            .unwrap_or_else(|error| panic!("expected first large free to succeed: {error}"));
        allocator
            .deallocate_with_size_checked(&mut cache, second, second_request)
            .unwrap_or_else(|error| panic!("expected second large free to succeed: {error}"));
    }

    let freed = allocator.stats();
    assert_eq!(freed.large_live_allocations, 0);
    assert_eq!(freed.large_live_bytes, 0);
    assert_eq!(freed.large_free_blocks, 2);
    assert!(freed.large_free_bytes > 0);

    let reused = allocator
        .allocate_with_cache(&mut cache, first_request)
        .unwrap_or_else(|error| panic!("expected large reuse allocation to succeed: {error}"));

    let after_reuse = allocator.stats();
    assert_eq!(after_reuse.large_live_allocations, 1);
    assert_eq!(after_reuse.large_live_bytes, first_block_size);
    assert_eq!(after_reuse.large_free_blocks, 1);
    assert!(after_reuse.large_free_bytes < freed.large_free_bytes);

    // SAFETY: `reused` is the currently live large allocation returned above.
    unsafe {
        allocator
            .deallocate_with_size_checked(&mut cache, reused, first_request)
            .unwrap_or_else(|error| panic!("expected reused large free to succeed: {error}"));
    }
}

#[test]
fn embedding_batch_sized_bursts_stay_local_with_default_medium_class() {
    let config = AllocatorConfig::default();
    let allocator = Allocator::new(config)
        .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
    let mut cache = ThreadCache::new(config);
    let class = SizeClass::from_request(6_144)
        .unwrap_or_else(|| panic!("expected embedding-sized request to use the small path"));
    let mut pointers = Vec::with_capacity(8);

    assert_eq!(class, SizeClass::B6K);
    assert_eq!(config.refill_count(class), 5);
    assert_eq!(config.local_limit(class), 10);
    assert_eq!(config.drain_count(class), 2);

    for _ in 0..8 {
        let ptr = allocator
            .allocate_with_cache(&mut cache, 6_144)
            .unwrap_or_else(|error| {
                panic!("expected embedding-sized allocation to succeed: {error}")
            });
        pointers.push(ptr);
    }

    while let Some(ptr) = pointers.pop() {
        // SAFETY: every pointer in `pointers` is live and is freed exactly once here.
        unsafe {
            allocator
                .deallocate_with_size_checked(&mut cache, ptr, 6_144)
                .unwrap_or_else(|error| {
                    panic!("expected embedding-sized free to succeed: {error}")
                });
        }
    }

    let local = cache.stats();
    let shared = allocator.stats();

    assert!(local.local[class.index()].blocks >= 8);
    assert_eq!(shared.small_central[class.index()].blocks, 0);
}

#[test]
fn remote_free_is_visible_in_shared_stats_before_refill_drains_the_inbox() {
    let config = AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 64,
        refill_target_bytes: 256,
        local_cache_target_bytes: 256,
    };
    let allocator = Allocator::new(config)
        .unwrap_or_else(|error| panic!("expected allocator to initialize: {error}"));
    let mut source_cache = ThreadCache::new(config);
    let mut remote_cache = ThreadCache::new(config);
    let class = SizeClass::B64;
    let ptr = allocator
        .allocate_with_cache(&mut source_cache, 32)
        .unwrap_or_else(|error| panic!("expected allocation to succeed: {error}"));

    // SAFETY: `ptr` is live and freeing through another cache id exercises the remote path.
    unsafe {
        allocator
            .deallocate_with_cache(&mut remote_cache, ptr)
            .unwrap_or_else(|error| panic!("expected remote free to succeed: {error}"));
    }

    let shared = allocator.stats();
    assert_eq!(shared.small_central[class.index()].blocks, 1);
}
