//! Optional adapter for using Orthotope through Rust's `GlobalAlloc` trait.
//!
//! This adapter is opt-in and intended for downstream binaries to install via
//! `#[global_allocator]`.

use core::alloc::{GlobalAlloc, Layout};
use core::mem::{align_of, size_of};
use core::ptr::NonNull;
use std::alloc::System;
use std::process;
use std::thread;

use crate::error::FreeError;
use crate::header::HEADER_ALIGNMENT;
use crate::try_with_thread_cache;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Orthotope,
    System,
}

const FALLBACK_EMPTY: usize = 0;
const FALLBACK_MAGIC: usize = 0x4f52_5448_4653_5953;

#[derive(Clone, Copy)]
#[repr(C)]
struct SystemFallbackPrefix {
    magic: usize,
    raw_addr: usize,
    user_addr: usize,
}

const fn select_backend(layout: Layout) -> Backend {
    if layout.size() == 0 || layout.align() > HEADER_ALIGNMENT {
        Backend::System
    } else {
        Backend::Orthotope
    }
}

/// Opt-in process-global allocator shim for downstream binaries.
///
/// # Behavior
///
/// - Layouts with `size == 0` are delegated to [`System`].
/// - Layouts with `align() > 64` are delegated to [`System`].
/// - All other layouts use Orthotope's process-global allocator and TLS cache.
/// - If the Orthotope path is temporarily unavailable (for example, reentrant TLS-cache
///   borrow during unwind), allocation falls back to [`System`] with address tracking so
///   deallocation routes back to the same backend.
///
/// # Safety and failure semantics
///
/// Deallocating through this shim must follow the normal `GlobalAlloc` contract:
/// pointer/layout pairs must match prior successful allocations from this allocator.
/// If Orthotope detects an invalid free on its own path, the process aborts because
/// `GlobalAlloc::dealloc` cannot return an error. The only tolerated leak path is a
/// reentrant TLS-cache borrow during panic unwind, where retrying allocator work can
/// recurse and abort the process.
pub struct OrthotopeGlobalAlloc;

impl OrthotopeGlobalAlloc {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for OrthotopeGlobalAlloc {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: This adapter does not own allocator state. It dispatches each request
// to either `System` or Orthotope's existing process-global API, both of which
// uphold `GlobalAlloc` requirements under their documented contracts.
unsafe impl GlobalAlloc for OrthotopeGlobalAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match select_backend(layout) {
            Backend::System => {
                // SAFETY: delegated directly to the system allocator with caller-provided layout.
                unsafe { System.alloc(layout) }
            }
            Backend::Orthotope => {
                let orthotope = try_with_thread_cache(|allocator, cache| {
                    allocator.allocate_with_cache(cache, layout.size())
                });
                match orthotope {
                    Ok(Some(Ok(ptr))) => return ptr.as_ptr(),
                    Ok(Some(Err(_))) | Err(_) => {
                        return core::ptr::null_mut();
                    }
                    Ok(None) => {}
                }

                // Reentrant TLS-cache borrow failures can occur while unwinding. In that
                // case we fall back to `System` with an in-band prefix so deallocation
                // can route back to the same backend without global state.
                fallback_alloc(layout)
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match select_backend(layout) {
            Backend::System => {
                // SAFETY: delegated directly to the system allocator with the original layout.
                unsafe { System.dealloc(ptr, layout) };
            }
            Backend::Orthotope => {
                let Some(ptr) = NonNull::new(ptr) else {
                    return;
                };
                if let Some(raw_ptr) = fallback_raw_ptr(ptr.as_ptr()) {
                    let Some(raw_layout) = try_fallback_layout(layout) else {
                        process::abort();
                    };
                    // SAFETY: the fallback prefix proves `raw_ptr` came from `System.alloc`
                    // with this derived fallback layout for the same original request.
                    unsafe { System.dealloc(raw_ptr, raw_layout) };
                    return;
                }
                let result = try_with_thread_cache(|allocator, cache| {
                    // SAFETY: caller must pass the original pointer/layout pair from a prior alloc.
                    unsafe { allocator.deallocate_with_size_checked(cache, ptr, layout.size()) }
                });
                if let Err(error) = collapse_dealloc_result(result) {
                    // `GlobalAlloc::dealloc` has no error return path. Failing closed is
                    // preferable to continuing in a potentially corrupted process state.
                    let _ = error;
                    process::abort();
                }
            }
        }
    }
}

fn fallback_alloc(layout: Layout) -> *mut u8 {
    let Some(raw_layout) = try_fallback_layout(layout) else {
        return core::ptr::null_mut();
    };
    // SAFETY: delegated directly to the system allocator using the larger fallback layout.
    let raw_ptr = unsafe { System.alloc(raw_layout) };
    if raw_ptr.is_null() {
        return raw_ptr;
    }

    let prefix_size = size_of::<SystemFallbackPrefix>();
    let Some(aligned_addr) = align_up(raw_ptr.addr() + prefix_size, fallback_alignment(layout))
    else {
        // SAFETY: `raw_ptr` came from `System.alloc(raw_layout)` above.
        unsafe { System.dealloc(raw_ptr, raw_layout) };
        return core::ptr::null_mut();
    };
    let user_ptr = aligned_addr as *mut u8;
    #[allow(clippy::cast_ptr_alignment)]
    let prefix_ptr = user_ptr
        .wrapping_sub(prefix_size)
        .cast::<SystemFallbackPrefix>();

    // SAFETY: `user_ptr` is derived from `raw_ptr` within the allocated fallback block,
    // and the prefix lands entirely inside the reserved prefix space before it.
    unsafe {
        prefix_ptr.write(SystemFallbackPrefix {
            magic: FALLBACK_MAGIC,
            raw_addr: raw_ptr.addr(),
            user_addr: user_ptr.addr(),
        });
    }

    user_ptr
}

fn fallback_raw_ptr(ptr: *mut u8) -> Option<*mut u8> {
    let prefix_size = size_of::<SystemFallbackPrefix>();
    let prefix_addr = ptr.addr().checked_sub(prefix_size)?;
    if prefix_addr == FALLBACK_EMPTY {
        return None;
    }

    let prefix_ptr = prefix_addr as *const SystemFallbackPrefix;

    // SAFETY: both Orthotope-managed and fallback-managed allocations reserve readable
    // bytes before the user pointer, so inspecting the fixed-size prefix is valid.
    let prefix = unsafe { prefix_ptr.read() };
    if prefix.magic != FALLBACK_MAGIC || prefix.user_addr != ptr.addr() {
        return None;
    }

    Some(prefix.raw_addr as *mut u8)
}

const fn fallback_alignment(layout: Layout) -> usize {
    if layout.align() > align_of::<SystemFallbackPrefix>() {
        layout.align()
    } else {
        align_of::<SystemFallbackPrefix>()
    }
}

fn try_fallback_layout(layout: Layout) -> Option<Layout> {
    let alignment = fallback_alignment(layout);
    let prefix_size = size_of::<SystemFallbackPrefix>();
    let size = layout
        .size()
        .checked_add(prefix_size)
        .and_then(|size| size.checked_add(alignment - 1))?;
    Layout::from_size_align(size, alignment).ok()
}

const fn align_up(value: usize, alignment: usize) -> Option<usize> {
    let remainder = value % alignment;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

fn collapse_dealloc_result(
    result: Result<Option<Result<(), FreeError>>, &'static crate::InitError>,
) -> Result<(), FreeError> {
    match result {
        Ok(Some(Err(error))) => Err(error),
        Ok(Some(Ok(()))) => Ok(()),
        // A busy TLS cache is only tolerated while unwinding, where trying to
        // recover by re-entering allocator code can recurse and abort. Outside
        // panic unwind, fail closed instead of silently leaking a reachable free.
        Ok(None) if thread::panicking() => Ok(()),
        Ok(None) | Err(_) => Err(FreeError::GlobalInitFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Backend, FALLBACK_MAGIC, SystemFallbackPrefix, fallback_alloc, fallback_raw_ptr,
        select_backend,
    };
    use core::alloc::{GlobalAlloc, Layout};
    use core::mem::size_of;
    use std::alloc::System;

    #[test]
    fn routes_zero_size_layout_to_system() {
        let layout =
            Layout::from_size_align(0, 8).unwrap_or_else(|error| panic!("valid layout: {error}"));
        assert_eq!(select_backend(layout), Backend::System);
    }

    #[test]
    fn routes_alignments_up_to_header_alignment_to_orthotope() {
        let layout =
            Layout::from_size_align(16, 64).unwrap_or_else(|error| panic!("valid layout: {error}"));
        assert_eq!(select_backend(layout), Backend::Orthotope);
    }

    #[test]
    fn routes_over_aligned_layouts_to_system() {
        let layout = Layout::from_size_align(16, 128)
            .unwrap_or_else(|error| panic!("valid layout: {error}"));
        assert_eq!(select_backend(layout), Backend::System);
    }

    #[test]
    fn fallback_prefix_round_trips_raw_pointer() {
        let layout =
            Layout::from_size_align(32, 8).unwrap_or_else(|error| panic!("valid layout: {error}"));
        let ptr = fallback_alloc(layout);
        assert!(!ptr.is_null(), "fallback allocation should succeed");

        let raw_ptr = fallback_raw_ptr(ptr)
            .unwrap_or_else(|| panic!("fallback prefix should decode the original raw pointer"));
        let prefix_addr = ptr.addr() - size_of::<SystemFallbackPrefix>();
        // SAFETY: `ptr` came from `fallback_alloc(layout)` above, which writes a full prefix
        // immediately before the returned user pointer.
        let prefix = unsafe { (prefix_addr as *const SystemFallbackPrefix).read() };

        assert_eq!(prefix.magic, FALLBACK_MAGIC);
        assert_eq!(prefix.user_addr, ptr.addr());
        assert_eq!(prefix.raw_addr, raw_ptr.addr());

        // SAFETY: `raw_ptr` came from `fallback_alloc(layout)` above.
        let raw_layout = super::try_fallback_layout(layout)
            .unwrap_or_else(|| panic!("fallback allocation should preserve a valid layout"));
        // SAFETY: `raw_ptr` came from `fallback_alloc(layout)` above with `raw_layout`.
        unsafe { System.dealloc(raw_ptr, raw_layout) };
    }

    #[test]
    fn reentrant_huge_layout_returns_null_instead_of_panicking() {
        let shim = crate::OrthotopeGlobalAlloc::new();
        let layout = Layout::from_size_align(isize::MAX as usize - 7, 8)
            .unwrap_or_else(|error| panic!("expected valid near-limit layout: {error}"));

        let outcome = std::panic::catch_unwind(|| {
            crate::try_with_thread_cache(|_, _| {
                // SAFETY: this intentionally exercises the shim with a valid layout while the
                // thread cache is already mutably borrowed to simulate allocator reentrancy.
                unsafe { shim.alloc(layout) }
            })
            .unwrap_or_else(|error| panic!("expected global allocator init to succeed: {error}"))
            .unwrap_or_else(|| panic!("outer thread-cache borrow should succeed"))
        });

        assert!(
            outcome.is_ok(),
            "reentrant allocation should return null, not panic"
        );
        let ptr = outcome.unwrap_or_else(|_| unreachable!("assertion above ensures success"));
        assert!(
            ptr.is_null(),
            "near-limit reentrant allocation should fail with null"
        );
    }
}
