use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

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
