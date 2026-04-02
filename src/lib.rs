use std::cell::RefCell;
use std::sync::LazyLock;

pub mod allocator;
pub mod api;
pub mod arena;
pub mod central_pool;
pub mod config;
pub mod error;
pub mod free_list;
pub mod header;
pub mod large_object;
pub mod size_class;
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

pub(crate) fn with_thread_cache<R>(
    f: impl FnOnce(&Allocator, &mut ThreadCache) -> R,
) -> Result<R, &'static InitError> {
    let allocator = global_allocator()?;

    THREAD_CACHE.with(|cache| {
        let mut handle = cache.borrow_mut();
        let handle = handle.get_or_insert_with(|| ThreadCacheHandle::new(allocator));
        Ok(handle.with_parts(f))
    })
}

pub use crate::allocator::Allocator;
pub use crate::api::{allocate, deallocate, deallocate_with_size};
pub use crate::config::AllocatorConfig;
pub use crate::error::{AllocError, FreeError, InitError};
pub use crate::size_class::SizeClass;
pub use crate::thread_cache::ThreadCache;
