use std::ptr::NonNull;
use std::thread;

use orthotope::error::{AllocError, FreeError};
use orthotope::{SizeClass, allocate, deallocate, deallocate_with_size};

#[test]
fn public_api_allocates_and_deallocates_small_block() {
    let ptr = match allocate(32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected public small allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` was allocated above and has not been freed yet.
    match unsafe { deallocate(ptr) } {
        Ok(()) => {}
        Err(error) => panic!("expected public free to succeed: {error}"),
    }
}

#[test]
fn public_api_rejects_zero_sized_allocation() {
    match allocate(0) {
        Err(AllocError::ZeroSize) => {}
        Err(error) => panic!("unexpected allocation error: {error}"),
        Ok(_) => panic!("expected zero-sized allocation to be rejected"),
    }
}

#[test]
fn compatibility_free_validates_recorded_size() {
    let ptr = match allocate(64) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected public allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` is still live, and the mismatch path should validate only.
    match unsafe { deallocate_with_size(ptr, 1) } {
        Err(FreeError::SizeMismatch { provided, recorded }) => {
            assert_eq!(provided, 1);
            assert_eq!(recorded, 64);
        }
        Err(error) => panic!("unexpected deallocation error: {error}"),
        Ok(()) => panic!("expected mismatched compatibility free to fail"),
    }

    // SAFETY: the previous mismatch did not free the allocation.
    match unsafe { deallocate_with_size(ptr, 64) } {
        Ok(()) => {}
        Err(error) => panic!("expected matching compatibility free to succeed: {error}"),
    }
}

#[test]
fn same_thread_reuse_works_through_public_api() {
    let handle = thread::spawn(|| {
        let first = match allocate(32) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected first public allocation to succeed: {error}"),
        };

        // SAFETY: `first` was allocated above and has not been freed yet.
        match unsafe { deallocate(first) } {
            Ok(()) => {}
            Err(error) => panic!("expected first public free to succeed: {error}"),
        }

        let second = match allocate(32) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected second public allocation to succeed: {error}"),
        };

        assert_eq!(first, second);

        // SAFETY: `second` is the currently live allocation in this thread.
        match unsafe { deallocate(second) } {
            Ok(()) => {}
            Err(error) => panic!("expected second public free to succeed: {error}"),
        }
    });

    let join_result = handle.join();
    assert!(
        join_result.is_ok(),
        "expected reuse thread to complete successfully"
    );
}

#[test]
fn public_api_rejects_never_allocated_same_arena_pointer() {
    let handle = thread::spawn(|| {
        let ptr = match allocate(32) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected public allocation to succeed: {error}"),
        };
        let forged = NonNull::new(ptr.as_ptr().wrapping_add(SizeClass::B64.block_size()))
            .unwrap_or_else(|| panic!("forged in-arena user pointer should remain non-null"));

        // SAFETY: this intentionally forges a same-arena pointer to an untouched block in
        // the current thread's local slab to verify the public free API rejects it.
        let forged_result = unsafe { deallocate_with_size(forged, 1) };
        assert!(
            matches!(
                forged_result,
                Err(FreeError::CorruptHeader | FreeError::ForeignPointer)
            ),
            "forged never-allocated pointer was accepted: {forged_result:?}"
        );

        // SAFETY: `ptr` is still the only live allocation returned by this thread.
        match unsafe { deallocate(ptr) } {
            Ok(()) => {}
            Err(error) => panic!("expected public cleanup free to succeed: {error}"),
        }
    });

    let join_result = handle.join();
    assert!(
        join_result.is_ok(),
        "expected forged-pointer thread to complete successfully"
    );
}
