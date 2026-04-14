use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::io;

use memmap2::MmapMut;

use crate::config::AllocatorConfig;
use crate::error::{AllocError, InitError};

/// Monotonic arena backed by one anonymous memory mapping.
///
/// Reservations are thread-safe and are never returned to the arena in v1.
pub struct Arena {
    _mapping: MmapMut,
    base: NonNull<u8>,
    len: usize,
    next: AtomicUsize,
    alignment: usize,
}

pub(crate) struct ReservedSpan {
    start: NonNull<u8>,
    size: usize,
}

// SAFETY: `Arena` owns its mapping, never exposes mutable aliases to that mapping, and
// coordinates shared allocation progress exclusively through the atomic `next` cursor.
unsafe impl Send for Arena {}

// SAFETY: shared references are safe because the only mutable shared state is the atomic
// bump pointer, and the mapping itself remains owned by the arena for its full lifetime.
unsafe impl Sync for Arena {}

impl Arena {
    /// Creates a new pre-mapped arena with monotonic bump allocation.
    ///
    /// # Errors
    ///
    /// Returns [`InitError::InvalidConfig`] when the arena size or alignment
    /// are invalid, or [`InitError::MapFailed`] when the anonymous mapping
    /// cannot be created.
    pub fn new(config: &AllocatorConfig) -> Result<Self, InitError> {
        validate_config(config)?;

        let mapping_len = config
            .arena_size
            .checked_add(config.alignment - 1)
            .ok_or(InitError::InvalidConfig("arena mapping size overflowed"))?;
        let mut mapping = MmapMut::map_anon(mapping_len).map_err(InitError::MapFailed)?;
        let base_ptr = mapping.as_mut_ptr();
        let aligned_addr = align_up(base_ptr.addr(), config.alignment)
            .ok_or(InitError::InvalidConfig("arena base alignment overflowed"))?;
        let aligned_ptr = aligned_addr as *mut u8;
        let base = NonNull::new(aligned_ptr)
            .ok_or(InitError::InvalidConfig("arena mapping returned null"))?;

        Ok(Self {
            _mapping: mapping,
            base,
            len: config.arena_size,
            next: AtomicUsize::new(0),
            alignment: config.alignment,
        })
    }

    /// Reserves `size` bytes from the arena and returns the aligned block start.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::OutOfMemory`] if the aligned reservation would
    /// exceed the arena bounds or if alignment arithmetic overflows.
    pub fn allocate_block(&self, size: usize) -> Result<NonNull<u8>, AllocError> {
        Ok(self.reserve_span(size)?.start())
    }

    /// Reserves one contiguous aligned span from the arena.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::OutOfMemory`] if the aligned reservation would
    /// exceed the arena bounds or if alignment arithmetic overflows.
    pub(crate) fn reserve_span(&self, size: usize) -> Result<ReservedSpan, AllocError> {
        if size == 0 {
            return Err(AllocError::ZeroSize);
        }

        loop {
            let current = self.next.load(Ordering::Relaxed);
            let aligned =
                align_up(current, self.alignment).ok_or_else(|| AllocError::OutOfMemory {
                    requested: size,
                    remaining: self.len.saturating_sub(current),
                })?;
            let end = aligned
                .checked_add(size)
                .ok_or_else(|| AllocError::OutOfMemory {
                    requested: size,
                    remaining: self.len.saturating_sub(current),
                })?;

            if end > self.len {
                return Err(AllocError::OutOfMemory {
                    requested: size,
                    remaining: self.len.saturating_sub(current),
                });
            }

            if self
                .next
                .compare_exchange_weak(current, end, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let ptr = self.base.as_ptr().wrapping_add(aligned);
                // SAFETY: `aligned <= self.len` and `end <= self.len` were proven above,
                // so the derived pointer stays within the mapped arena and cannot be null.
                let start = unsafe { NonNull::new_unchecked(ptr) };
                return Ok(ReservedSpan { start, size });
            }
        }
    }

    /// Reserves up to `target_blocks` contiguous `block_size` blocks from the arena.
    ///
    /// Returns `Ok(None)` when fewer than one block currently fits.
    pub(crate) fn reserve_block_span(
        &self,
        block_size: usize,
        target_blocks: usize,
    ) -> Result<Option<ReservedSpan>, AllocError> {
        if block_size == 0 || target_blocks == 0 {
            return Err(AllocError::ZeroSize);
        }

        loop {
            let current = self.next.load(Ordering::Relaxed);
            let aligned =
                align_up(current, self.alignment).ok_or_else(|| AllocError::OutOfMemory {
                    requested: block_size,
                    remaining: self.len.saturating_sub(current),
                })?;
            let remaining = self.len.saturating_sub(aligned);
            let available_blocks = remaining / block_size;
            let reserved_blocks = core::cmp::min(target_blocks, available_blocks);

            if reserved_blocks == 0 {
                return Ok(None);
            }

            let size = reserved_blocks
                .checked_mul(block_size)
                .ok_or(AllocError::OutOfMemory {
                    requested: block_size,
                    remaining,
                })?;
            let end = aligned.checked_add(size).ok_or(AllocError::OutOfMemory {
                requested: size,
                remaining,
            })?;

            if end > self.len {
                return Err(AllocError::OutOfMemory {
                    requested: size,
                    remaining,
                });
            }

            if self
                .next
                .compare_exchange_weak(current, end, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let ptr = self.base.as_ptr().wrapping_add(aligned);
                // SAFETY: `aligned < self.len` and `end <= self.len` keep the start inside
                // the mapped arena, and `base` is non-null for the arena lifetime.
                let start = unsafe { NonNull::new_unchecked(ptr) };
                return Ok(Some(ReservedSpan { start, size }));
            }
        }
    }

    #[must_use]
    /// Returns the remaining unreserved capacity in bytes.
    pub fn remaining(&self) -> usize {
        self.len.saturating_sub(self.next.load(Ordering::Relaxed))
    }

    #[must_use]
    /// Returns the total usable capacity reserved by this arena mapping.
    pub const fn capacity(&self) -> usize {
        self.len
    }

    #[must_use]
    /// Returns the configured block-start alignment.
    pub const fn alignment(&self) -> usize {
        self.alignment
    }

    #[must_use]
    pub(crate) fn contains_block_start(&self, block_start: NonNull<u8>) -> bool {
        let start = self.base.as_ptr().addr();
        let end = start + self.len;
        let addr = block_start.as_ptr().addr();

        // This is the v1 small-object ownership predicate: the decoded block start
        // must land inside this arena and respect the arena alignment. It does not
        // prove that the block is currently live or uniquely owned.
        addr >= start && addr < end && (addr - start).is_multiple_of(self.alignment)
    }
}

static SYSTEM_PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static ADVISE_FREE_OVERRIDE: core::sync::atomic::AtomicIsize =
    core::sync::atomic::AtomicIsize::new(-1);

#[must_use]
pub(crate) fn system_page_size() -> usize {
    let cached = SYSTEM_PAGE_SIZE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }

    let detected = detect_system_page_size().unwrap_or(4096);
    SYSTEM_PAGE_SIZE.store(detected, Ordering::Relaxed);
    detected
}

#[must_use]
pub(crate) fn page_aligned_inner_range(start: NonNull<u8>, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }

    let page_size = system_page_size();
    let start_addr = start.as_ptr().addr();
    let end_addr = start_addr.checked_add(len)?;
    let aligned_start = align_up(start_addr, page_size)?;
    let aligned_end = align_down(end_addr, page_size);

    if aligned_start >= aligned_end {
        return None;
    }

    Some((aligned_start, aligned_end - aligned_start))
}

/// Advises the operating system that the page-aligned arena range may be reclaimed lazily.
///
/// Returns `Ok(true)` when the advisory syscall ran successfully, `Ok(false)` on
/// unsupported targets or for empty ranges, and `Err` when the syscall itself failed.
///
/// # Safety
///
/// `addr..addr + len` must describe a valid mapped page-aligned range inside the arena.
pub(crate) unsafe fn advise_free(addr: usize, len: usize) -> io::Result<bool> {
    if len == 0 {
        return Ok(false);
    }

    #[cfg(test)]
    match ADVISE_FREE_OVERRIDE.load(Ordering::Relaxed) {
        -1 => {}
        0 => return Ok(false),
        1 => return Ok(true),
        other => unreachable!("unexpected advise_free override mode: {other}"),
    }

    // SAFETY: the caller guarantees `addr..addr + len` is a valid page-aligned mapping.
    unsafe { advise_free_impl(addr, len) }
}

#[cfg(test)]
pub(crate) struct AdviseFreeOverrideGuard {
    previous: isize,
}

#[cfg(test)]
impl Drop for AdviseFreeOverrideGuard {
    fn drop(&mut self) {
        ADVISE_FREE_OVERRIDE.store(self.previous, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) fn override_advise_free_for_test(result: Option<bool>) -> AdviseFreeOverrideGuard {
    let next = match result {
        None => -1,
        Some(false) => 0,
        Some(true) => 1,
    };
    let previous = ADVISE_FREE_OVERRIDE.swap(next, Ordering::Relaxed);

    AdviseFreeOverrideGuard { previous }
}

impl ReservedSpan {
    #[must_use]
    pub(crate) const fn start(&self) -> NonNull<u8> {
        self.start
    }

    #[must_use]
    pub(crate) const fn size(&self) -> usize {
        self.size
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn detect_system_page_size() -> Option<usize> {
    // SAFETY: `sysconf(_SC_PAGESIZE)` is a thread-safe libc query with no pointer arguments.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page_size)
        .ok()
        .filter(|size| size.is_power_of_two())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const fn detect_system_page_size() -> Option<usize> {
    Some(4096)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
unsafe fn advise_free_impl(addr: usize, len: usize) -> io::Result<bool> {
    let ptr = addr as *mut libc::c_void;
    // SAFETY: the caller guarantees that this range is page-aligned mapped memory that
    // remains valid for the duration of the advisory syscall.
    let status = unsafe { libc::madvise(ptr, len, libc::MADV_FREE) };
    if status == 0 {
        Ok(true)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
unsafe fn advise_free_impl(_addr: usize, _len: usize) -> io::Result<bool> {
    Ok(false)
}

const fn validate_config(config: &AllocatorConfig) -> Result<(), InitError> {
    if config.arena_size == 0 {
        return Err(InitError::InvalidConfig(
            "arena size must be greater than zero",
        ));
    }
    if config.alignment == 0 {
        return Err(InitError::InvalidConfig(
            "alignment must be greater than zero",
        ));
    }
    if !config.alignment.is_power_of_two() {
        return Err(InitError::InvalidConfig("alignment must be a power of two"));
    }
    if config.arena_size < config.alignment {
        return Err(InitError::InvalidConfig(
            "arena size must be at least the alignment",
        ));
    }

    Ok(())
}

const fn align_up(value: usize, alignment: usize) -> Option<usize> {
    let remainder = value % alignment;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

const fn align_down(value: usize, alignment: usize) -> usize {
    value - (value % alignment)
}

#[cfg(test)]
mod tests {
    use super::{advise_free, page_aligned_inner_range, system_page_size};
    use core::ptr::NonNull;
    use memmap2::MmapMut;

    #[test]
    fn system_page_size_is_positive_power_of_two() {
        let page_size = system_page_size();

        assert!(page_size > 0);
        assert!(page_size.is_power_of_two());
    }

    #[test]
    fn inner_range_excludes_partial_pages() {
        let page_size = system_page_size();
        let mut mapping = MmapMut::map_anon(page_size * 4)
            .unwrap_or_else(|error| panic!("expected anonymous mapping: {error}"));
        let base = NonNull::new(mapping.as_mut_ptr())
            .unwrap_or_else(|| panic!("expected non-null mapping base"));
        let start = base.as_ptr().wrapping_add(page_size / 2);
        let start = NonNull::new(start).unwrap_or_else(|| panic!("expected non-null start"));

        let inner = page_aligned_inner_range(start, page_size * 2)
            .unwrap_or_else(|| panic!("expected fully contained page range"));

        assert_eq!(inner.0, base.as_ptr().addr() + page_size);
        assert_eq!(inner.1, page_size);
    }

    #[test]
    fn inner_range_is_none_when_span_has_no_full_page() {
        let page_size = system_page_size();
        let mut mapping = MmapMut::map_anon(page_size * 2)
            .unwrap_or_else(|error| panic!("expected anonymous mapping: {error}"));
        let start = mapping.as_mut_ptr().wrapping_add(page_size / 2);
        let start = NonNull::new(start).unwrap_or_else(|| panic!("expected non-null start"));

        assert_eq!(page_aligned_inner_range(start, page_size - 1), None);
    }

    #[test]
    fn advise_free_is_supported_or_noop_for_mapped_memory() {
        let page_size = system_page_size();
        let mut mapping = MmapMut::map_anon(page_size * 3)
            .unwrap_or_else(|error| panic!("expected anonymous mapping: {error}"));
        let base = NonNull::new(mapping.as_mut_ptr())
            .unwrap_or_else(|| panic!("expected non-null mapping base"));
        let middle_page = base.as_ptr().wrapping_add(page_size).addr();

        // SAFETY: the chosen subrange is page-aligned and fully contained in the mapping.
        let advised = unsafe { advise_free(middle_page, page_size) }
            .unwrap_or_else(|error| panic!("expected advise_free result: {error}"));

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(advised);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(!advised);
    }
}
