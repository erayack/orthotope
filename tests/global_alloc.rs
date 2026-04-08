use core::alloc::GlobalAlloc;
use orthotope::{OrthotopeGlobalAlloc, SizeClass, global_stats};
use std::alloc::Layout;
use std::sync::{LazyLock, Mutex};

static SHIM: OrthotopeGlobalAlloc = OrthotopeGlobalAlloc::new();
static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[repr(align(128))]
struct OverAligned([u8; 32]);

#[test]
fn box_allocation_works_with_global_shim() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|error| panic!("expected global shim test lock: {error}"));
    let layout = Layout::new::<u64>();

    #[allow(clippy::cast_ptr_alignment)]
    // SAFETY: `layout` is valid and `SHIM` implements the global-allocation contract.
    let ptr = unsafe { SHIM.alloc(layout).cast::<u64>() };
    assert!(!ptr.is_null(), "expected shim allocation to succeed");

    // SAFETY: `ptr` points to writable memory for a `u64` allocation from `SHIM`.
    unsafe {
        ptr.write(42);
        assert_eq!(ptr.read(), 42);
        SHIM.dealloc(ptr.cast::<u8>(), layout);
    }
}

#[test]
fn vec_growth_and_writes_work_with_global_shim() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|error| panic!("expected global shim test lock: {error}"));
    let layout = Layout::array::<u32>(1024)
        .unwrap_or_else(|error| panic!("expected valid array layout: {error}"));

    #[allow(clippy::cast_ptr_alignment)]
    // SAFETY: `layout` is valid and `SHIM` implements the global-allocation contract.
    let ptr = unsafe { SHIM.alloc(layout).cast::<u32>() };
    assert!(!ptr.is_null(), "expected shim allocation to succeed");

    for i in 0..1024_usize {
        // SAFETY: the allocation above reserved space for 1024 `u32` values.
        unsafe {
            ptr.add(i)
                .write(u32::try_from(i).unwrap_or_else(|error| panic!("u32 index fits: {error}")));
        };
    }

    // SAFETY: every slot was initialized in the loop above.
    unsafe {
        assert_eq!(ptr.read(), 0);
        assert_eq!(ptr.add(1023).read(), 1023);
        SHIM.dealloc(ptr.cast::<u8>(), layout);
    }
}

#[test]
fn string_mutation_works_with_global_shim() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|error| panic!("expected global shim test lock: {error}"));
    let layout = Layout::array::<u8>(9)
        .unwrap_or_else(|error| panic!("expected valid byte layout: {error}"));

    // SAFETY: `layout` is valid and `SHIM` implements the global-allocation contract.
    let ptr = unsafe { SHIM.alloc(layout) };
    assert!(!ptr.is_null(), "expected shim allocation to succeed");

    // SAFETY: the allocation above reserved space for 9 bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(b"orthotope".as_ptr(), ptr, 9);
        let text = core::str::from_utf8(core::slice::from_raw_parts(ptr, 9))
            .unwrap_or_else(|error| panic!("expected utf-8 test payload: {error}"));
        assert_eq!(text, "orthotope");
        SHIM.dealloc(ptr, layout);
    }
}

#[test]
fn over_aligned_type_allocates_and_deallocates() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|error| panic!("expected global shim test lock: {error}"));
    let layout = Layout::new::<OverAligned>();

    #[allow(clippy::cast_ptr_alignment)]
    // SAFETY: `layout` is valid and should route to the shim's `System` fallback path.
    let ptr = unsafe { SHIM.alloc(layout).cast::<OverAligned>() };
    assert!(
        !ptr.is_null(),
        "expected over-aligned shim allocation to succeed"
    );
    assert_eq!(ptr.addr() % layout.align(), 0);

    // SAFETY: `ptr` points to writable memory for `OverAligned`.
    unsafe {
        ptr.write(OverAligned([7; 32]));
        assert_eq!((*ptr).0[0], 7);
        assert_eq!((*ptr).0[31], 7);
        SHIM.dealloc(ptr.cast::<u8>(), layout);
    }
}

#[test]
fn non_overaligned_large_allocation_updates_orthotope_stats() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|error| panic!("expected global shim test lock: {error}"));
    let request = SizeClass::max_small_request() + 1;
    let layout = Layout::from_size_align(request, 8)
        .unwrap_or_else(|error| panic!("expected valid large layout: {error}"));
    let before =
        global_stats().unwrap_or_else(|error| panic!("expected global stats to succeed: {error}"));

    // SAFETY: `layout` is valid and should route through Orthotope's large-allocation path.
    let ptr = unsafe { SHIM.alloc(layout) };
    assert!(!ptr.is_null(), "expected large shim allocation to succeed");

    let during =
        global_stats().unwrap_or_else(|error| panic!("expected global stats to succeed: {error}"));
    assert_eq!(
        during.large_live_allocations,
        before.large_live_allocations + 1
    );

    // SAFETY: `ptr` was allocated above with the same `layout` and is still live here.
    unsafe { SHIM.dealloc(ptr, layout) };

    let after =
        global_stats().unwrap_or_else(|error| panic!("expected global stats to succeed: {error}"));
    assert_eq!(after.large_live_allocations, before.large_live_allocations);
}
