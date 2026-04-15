//! Orthotope is an arena-backed allocator with fixed size classes, per-thread caches,
//! a shared central pool, and a tracked large-allocation path.
//!
//! Use [`allocate`] and [`deallocate`] for the process-global convenience API, or
//! construct an [`Allocator`] plus one [`ThreadCache`] per participating thread for
//! direct instance-oriented use. Downstream binaries may also opt in to a
//! `#[global_allocator]` adapter via [`OrthotopeGlobalAlloc`].
//!
//! Only free live pointers returned by Orthotope. Debug builds detect some small-object
//! double frees, but same-arena pointer forgery and stale-pointer ABA cases are still
//! outside the allocator's guaranteed provenance boundary.

use std::cell::RefCell;
use std::sync::LazyLock;

/// Standalone allocator instances and the instance-oriented API.
pub mod allocator;
/// Process-global convenience allocation and free functions.
pub mod api;
/// Monotonic pre-mapped arena reservations.
pub mod arena;
/// Shared batch exchange between thread-local caches.
pub mod central_pool;
/// Allocator sizing and cache-tuning configuration.
pub mod config;
/// Typed initialization, allocation, and free failures.
pub mod error;
/// Intrusive free-list primitives used by allocator internals.
pub mod free_list;
/// Optional `GlobalAlloc` adapter for downstream binary opt-in.
pub mod global_alloc;
/// Allocation-header constants and pointer-layout helpers.
pub mod header;
/// Live tracking for allocations above the largest small class.
pub mod large_object;
/// Fixed request buckets and class sizing helpers.
pub mod size_class;
/// Allocator and thread-cache statistics snapshots.
pub mod stats;
/// Caller-owned per-thread cache state for the instance API.
pub mod thread_cache;

use crate::thread_cache::ThreadCacheHandle;
use crate::{allocator::Allocator as AllocatorType, error::InitError as InitErrorType};

static GLOBAL_ALLOCATOR: LazyLock<Result<AllocatorType, InitErrorType>> =
    LazyLock::new(|| AllocatorType::new(AllocatorConfig::default()));

thread_local! {
    static THREAD_CACHE: RefCell<Option<ThreadCacheHandle>> = const { RefCell::new(None) };
}

fn global_allocator() -> Result<&'static Allocator, &'static InitError> {
    GLOBAL_ALLOCATOR.as_ref()
}

pub(crate) fn try_with_thread_cache<R>(
    f: impl FnOnce(&Allocator, &mut ThreadCache) -> R,
) -> Result<Option<R>, &'static InitError> {
    let allocator = global_allocator()?;

    THREAD_CACHE
        .try_with(|cache| {
            let Ok(mut handle) = cache.try_borrow_mut() else {
                return Ok(None);
            };
            let handle = handle.get_or_insert_with(|| ThreadCacheHandle::new(allocator));
            Ok(Some(handle.with_parts(f)))
        })
        .unwrap_or(Ok(None))
}

pub use crate::allocator::Allocator;
pub use crate::api::{allocate, deallocate, deallocate_with_size, global_stats};
pub use crate::config::AllocatorConfig;
pub use crate::error::{AllocError, FreeError, InitError};
pub use crate::global_alloc::OrthotopeGlobalAlloc;
pub use crate::size_class::SizeClass;
pub use crate::stats::{AllocatorStats, SizeClassStats, ThreadCacheStats};
pub use crate::thread_cache::ThreadCache;
