use orthotope::config::AllocatorConfig;
use orthotope::header::{HEADER_ALIGNMENT, HEADER_SIZE};
use orthotope::size_class::{BLOCK_ALIGNMENT, NUM_CLASSES, SizeClass};

#[test]
fn request_boundaries_map_to_expected_classes() {
    assert_eq!(SizeClass::from_request(0), None);
    assert_eq!(SizeClass::from_request(1), Some(SizeClass::B64));
    assert_eq!(SizeClass::from_request(64), Some(SizeClass::B64));
    assert_eq!(SizeClass::from_request(65), Some(SizeClass::B256));
    assert_eq!(SizeClass::from_request(256), Some(SizeClass::B256));
    assert_eq!(SizeClass::from_request(257), Some(SizeClass::B4K));
    assert_eq!(SizeClass::from_request(4_096), Some(SizeClass::B4K));
    assert_eq!(SizeClass::from_request(4_097), Some(SizeClass::B6K));
    assert_eq!(SizeClass::from_request(6_144), Some(SizeClass::B6K));
    assert_eq!(SizeClass::from_request(6_145), Some(SizeClass::B8K));
    assert_eq!(SizeClass::from_request(8_192), Some(SizeClass::B8K));
    assert_eq!(SizeClass::from_request(8_193), Some(SizeClass::B16K));
    assert_eq!(SizeClass::from_request(16_384), Some(SizeClass::B16K));
    assert_eq!(SizeClass::from_request(16_385), Some(SizeClass::B32K));
    assert_eq!(SizeClass::from_request(32_768), Some(SizeClass::B32K));
    assert_eq!(SizeClass::from_request(32_769), Some(SizeClass::B64K));
    assert_eq!(SizeClass::from_request(65_536), Some(SizeClass::B64K));
    assert_eq!(SizeClass::from_request(65_537), Some(SizeClass::B128K));
    assert_eq!(SizeClass::from_request(131_072), Some(SizeClass::B128K));
    assert_eq!(SizeClass::from_request(131_073), Some(SizeClass::B256K));
    assert_eq!(SizeClass::from_request(262_144), Some(SizeClass::B256K));
    assert_eq!(SizeClass::from_request(262_145), Some(SizeClass::B1M));
    assert_eq!(SizeClass::from_request(1_048_576), Some(SizeClass::B1M));
    assert_eq!(SizeClass::from_request(1_048_577), Some(SizeClass::B16M));
    assert_eq!(SizeClass::from_request(16_777_216), Some(SizeClass::B16M));
    assert_eq!(SizeClass::from_request(16_777_217), None);
}

#[test]
fn size_class_order_and_indices_are_stable() {
    assert_eq!(SizeClass::ALL.len(), NUM_CLASSES);

    for (expected_index, class) in SizeClass::ALL.into_iter().enumerate() {
        assert_eq!(class.index(), expected_index);
    }
}

#[test]
fn block_sizes_cover_header_and_payload_and_stay_aligned() {
    let mut last_block_size = 0;

    assert_eq!(HEADER_ALIGNMENT, BLOCK_ALIGNMENT);

    for class in SizeClass::ALL {
        let block_size = class.block_size();

        assert!(block_size >= HEADER_SIZE + class.payload_size());
        assert_eq!(block_size % BLOCK_ALIGNMENT, 0);
        assert!(block_size > last_block_size);

        last_block_size = block_size;
    }
}

#[test]
fn config_aware_block_sizes_expand_with_custom_alignment() {
    let config = AllocatorConfig {
        arena_size: 1 << 20,
        alignment: 128,
        refill_target_bytes: 1 << 10,
        local_cache_target_bytes: 1 << 10,
    };

    assert_eq!(config.class_block_size(SizeClass::B256), 384);
    assert_eq!(
        config.class_block_size(SizeClass::B256),
        SizeClass::B256.block_size_for_alignment(config.alignment)
    );
    assert!(config.class_block_size(SizeClass::B256) > SizeClass::B256.block_size());
}

#[test]
fn config_helpers_clamp_low_alignment_instead_of_panicking() {
    let config = AllocatorConfig {
        arena_size: 64,
        alignment: 0,
        refill_target_bytes: 128,
        local_cache_target_bytes: 256,
    };

    assert_eq!(
        config.class_block_size(SizeClass::B64),
        SizeClass::B64.block_size()
    );
    assert_eq!(config.refill_count(SizeClass::B64), 1);
    assert_eq!(config.local_limit(SizeClass::B64), 2);
}

#[test]
fn config_defaults_match_allocator_plan() {
    let config = AllocatorConfig::default();

    assert_eq!(config.arena_size, 1 << 30);
    assert_eq!(config.alignment, BLOCK_ALIGNMENT);
    assert_eq!(config.refill_target_bytes, 32 << 10);
    assert_eq!(config.local_cache_target_bytes, 64 << 10);
}

#[test]
fn config_derives_refill_and_drain_counts_per_class() {
    let config = AllocatorConfig::default();

    for class in SizeClass::ALL {
        assert!(config.refill_count(class) >= 1);
        assert!(config.local_limit(class) >= 2);
        assert!(config.drain_count(class) >= 1);
    }

    assert_eq!(config.refill_count(SizeClass::B64), 256);
    assert_eq!(config.local_limit(SizeClass::B64), 512);
    assert_eq!(config.drain_count(SizeClass::B64), 128);

    assert_eq!(config.refill_count(SizeClass::B6K), 5);
    assert_eq!(config.local_limit(SizeClass::B6K), 10);
    assert_eq!(config.drain_count(SizeClass::B6K), 2);

    assert_eq!(config.refill_count(SizeClass::B16M), 1);
    assert_eq!(config.local_limit(SizeClass::B16M), 2);
    assert_eq!(config.drain_count(SizeClass::B16M), 1);
}
