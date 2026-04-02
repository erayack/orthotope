use orthotope::arena::Arena;
use orthotope::config::AllocatorConfig;
use orthotope::error::{AllocError, InitError};

const fn test_config(arena_size: usize, alignment: usize) -> AllocatorConfig {
    AllocatorConfig {
        arena_size,
        alignment,
        refill_target_bytes: 64,
        local_cache_target_bytes: 128,
    }
}

#[test]
fn allocations_are_aligned_to_configured_boundary() {
    let arena = match Arena::new(&test_config(256, 64)) {
        Ok(arena) => arena,
        Err(error) => panic!("expected arena to initialize: {error}"),
    };

    let first = match arena.allocate_block(1) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    };
    let second = match arena.allocate_block(17) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected second allocation to succeed: {error}"),
    };
    let third = match arena.allocate_block(63) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected third allocation to succeed: {error}"),
    };

    assert_eq!(first.as_ptr().addr() % 64, 0);
    assert_eq!(second.as_ptr().addr() % 64, 0);
    assert_eq!(third.as_ptr().addr() % 64, 0);
    assert_eq!(arena.alignment(), 64);
}

#[test]
fn allocations_honor_large_power_of_two_alignment() {
    let arena = match Arena::new(&test_config(16_384, 8_192)) {
        Ok(arena) => arena,
        Err(error) => panic!("expected arena to initialize: {error}"),
    };

    let first = match arena.allocate_block(1) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected aligned allocation to succeed: {error}"),
    };
    let second = match arena.allocate_block(64) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected second aligned allocation to succeed: {error}"),
    };

    assert_eq!(first.as_ptr().addr() % 8_192, 0);
    assert_eq!(second.as_ptr().addr() % 8_192, 0);
}

#[test]
fn remaining_tracks_reserved_bytes_including_alignment_padding() {
    let arena = match Arena::new(&test_config(256, 64)) {
        Ok(arena) => arena,
        Err(error) => panic!("expected arena to initialize: {error}"),
    };

    assert_eq!(arena.remaining(), 256);

    match arena.allocate_block(1) {
        Ok(_) => {}
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    }
    assert_eq!(arena.remaining(), 255);

    match arena.allocate_block(1) {
        Ok(_) => {}
        Err(error) => panic!("expected second allocation to succeed: {error}"),
    }
    assert_eq!(arena.remaining(), 191);

    match arena.allocate_block(1) {
        Ok(_) => {}
        Err(error) => panic!("expected third allocation to succeed: {error}"),
    }
    assert_eq!(arena.remaining(), 127);
}

#[test]
fn exact_fit_after_alignment_succeeds_then_exhausts() {
    let arena = match Arena::new(&test_config(128, 64)) {
        Ok(arena) => arena,
        Err(error) => panic!("expected arena to initialize: {error}"),
    };

    match arena.allocate_block(1) {
        Ok(_) => {}
        Err(error) => panic!("expected first allocation to succeed: {error}"),
    }
    match arena.allocate_block(64) {
        Ok(_) => {}
        Err(error) => panic!("expected exact-fit allocation to succeed: {error}"),
    }

    assert_eq!(arena.remaining(), 0);

    match arena.allocate_block(1) {
        Err(AllocError::GlobalInitFailed) => {
            panic!("arena allocation should never observe global allocator init failure")
        }
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            assert_eq!(requested, 1);
            assert_eq!(remaining, 0);
        }
        Err(AllocError::ZeroSize) => panic!("unexpected zero-size error for non-zero request"),
        Ok(_) => panic!("expected allocator to be exhausted"),
    }
}

#[test]
fn tiny_arena_exhausts_deterministically() {
    let arena = match Arena::new(&test_config(192, 64)) {
        Ok(arena) => arena,
        Err(error) => panic!("expected arena to initialize: {error}"),
    };

    for _ in 0..3 {
        match arena.allocate_block(1) {
            Ok(_) => {}
            Err(error) => panic!("expected allocation to succeed before exhaustion: {error}"),
        }
    }

    match arena.allocate_block(1) {
        Err(AllocError::GlobalInitFailed) => {
            panic!("arena allocation should never observe global allocator init failure")
        }
        Err(AllocError::OutOfMemory {
            requested,
            remaining,
        }) => {
            assert_eq!(requested, 1);
            assert_eq!(remaining, 63);
        }
        Err(AllocError::ZeroSize) => panic!("unexpected zero-size error for non-zero request"),
        Ok(_) => panic!("expected fourth allocation to exhaust the arena"),
    }
}

#[test]
fn invalid_alignment_is_rejected() {
    match Arena::new(&test_config(256, 24)) {
        Err(InitError::InvalidConfig(message)) => {
            assert_eq!(message, "alignment must be a power of two");
        }
        Ok(_) => panic!("expected invalid alignment to be rejected"),
        Err(error) => panic!("unexpected initialization error: {error}"),
    }
}

#[test]
fn zero_size_allocation_is_rejected() {
    let arena = match Arena::new(&test_config(256, 64)) {
        Ok(arena) => arena,
        Err(error) => panic!("expected arena to initialize: {error}"),
    };

    match arena.allocate_block(0) {
        Err(AllocError::ZeroSize) => {}
        Err(error) => panic!("unexpected allocation error: {error}"),
        Ok(_) => panic!("expected zero-sized allocation to be rejected"),
    }

    assert_eq!(arena.remaining(), 256);
}

#[test]
fn zero_or_too_small_arena_is_rejected() {
    match Arena::new(&test_config(0, 64)) {
        Err(InitError::InvalidConfig(message)) => {
            assert_eq!(message, "arena size must be greater than zero");
        }
        Ok(_) => panic!("expected zero-sized arena to be rejected"),
        Err(error) => panic!("unexpected initialization error: {error}"),
    }

    match Arena::new(&test_config(32, 64)) {
        Err(InitError::InvalidConfig(message)) => {
            assert_eq!(message, "arena size must be at least the alignment");
        }
        Ok(_) => panic!("expected too-small arena to be rejected"),
        Err(error) => panic!("unexpected initialization error: {error}"),
    }
}
