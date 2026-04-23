//! Allocation-header layout and pointer conversions used for deallocation routing.

use core::mem::{align_of, offset_of, size_of};
use core::ptr::NonNull;
use core::ptr::{self};

use crate::error::FreeError;
use crate::size_class::SizeClass;

/// Alignment required for allocation headers and block starts.
pub const HEADER_ALIGNMENT: usize = 64;
/// Bytes reserved for allocator metadata at the start of every block.
pub const HEADER_SIZE: usize = 64;

const HEADER_MAGIC: u32 = 0x4f52_5448;
const LARGE_CLASS_SENTINEL: u8 = u8::MAX;
#[cfg(debug_assertions)]
const SMALL_FREE_MARKER_SEED: usize = usize::MAX ^ (HEADER_MAGIC as usize);
const HEADER_SCRATCH_RESERVED_SIZE: usize = size_of::<u32>();
const HEADER_TAIL_PADDING_SIZE: usize = HEADER_SIZE
    - size_of::<AllocationHeaderPrefix>()
    - HEADER_SCRATCH_RESERVED_SIZE
    - size_of::<usize>()
    - size_of::<usize>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
struct AllocationHeaderPrefix {
    magic: u32,
    kind: AllocationKindTag,
    class_index: u8,
    reserved: [u8; 2],
    requested_size: u32,
    usable_size: u32,
    owner_cache_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
/// Decoded allocation kind stored in the header.
pub(crate) enum AllocationKind {
    Small(SizeClass),
    Large,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum AllocationKindTag {
    Small = 1,
    Large = 2,
}

#[derive(Clone, Copy, Debug)]
#[repr(C, align(64))]
#[allow(dead_code)]
/// Fixed header written at the start of every live allocation block.
pub(crate) struct AllocationHeader {
    magic: u32,
    kind: AllocationKindTag,
    class_index: u8,
    reserved: [u8; 2],
    requested_size: u32,
    usable_size: u32,
    owner_cache_id: u32,
    // Explicitly occupies the 4-byte gap that keeps the reserved free-list words
    // pointer-aligned within the first cache line on 64-bit targets.
    free_metadata_alignment_padding: u32,
    free_list_link: usize,
    small_free_marker: usize,
    tail_padding: [u8; HEADER_TAIL_PADDING_SIZE],
}

impl AllocationHeader {
    pub(crate) const PREFIX_SIZE: usize = size_of::<AllocationHeaderPrefix>();

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn new_small(class: SizeClass, requested_size: usize) -> Option<Self> {
        let (class_index, requested_size, usable_size) =
            encode_small_fields(class, requested_size)?;
        Some(Self::from_encoded_fields(
            AllocationKindTag::Small,
            class_index,
            requested_size,
            usable_size,
            0,
        ))
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn new_large(requested_size: usize, usable_size: usize) -> Option<Self> {
        let (requested_size, usable_size) = encode_large_fields(requested_size, usable_size)?;
        Some(Self::from_encoded_fields(
            AllocationKindTag::Large,
            LARGE_CLASS_SENTINEL,
            requested_size,
            usable_size,
            0,
        ))
    }

    /// Validates the stored header bytes and decodes the allocation routing kind.
    ///
    /// # Errors
    ///
    /// Returns [`FreeError::CorruptHeader`] when the magic value, size fields,
    /// or small-allocation class metadata are inconsistent.
    #[allow(dead_code)]
    pub(crate) fn validate(&self) -> Result<AllocationKind, FreeError> {
        if self.magic != HEADER_MAGIC {
            return Err(FreeError::CorruptHeader);
        }

        if self.requested_size == 0 || self.usable_size < self.requested_size {
            return Err(FreeError::CorruptHeader);
        }

        match self.kind {
            AllocationKindTag::Small => {
                let class =
                    index_to_size_class(self.class_index).ok_or(FreeError::CorruptHeader)?;
                if self.usable_size() != class.payload_size() {
                    return Err(FreeError::CorruptHeader);
                }
                Ok(AllocationKind::Small(class))
            }
            AllocationKindTag::Large => {
                if self.class_index != LARGE_CLASS_SENTINEL {
                    return Err(FreeError::CorruptHeader);
                }
                Ok(AllocationKind::Large)
            }
        }
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn requested_size(&self) -> usize {
        self.requested_size as usize
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn usable_size(&self) -> usize {
        self.usable_size as usize
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn small_owner_cache_id(&self) -> Option<u32> {
        match self.kind {
            AllocationKindTag::Small => Some(self.owner_cache_id),
            AllocationKindTag::Large => None,
        }
    }

    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn write_to_block(self, block_start: NonNull<u8>) -> NonNull<Self> {
        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);
        let header_ptr = header_from_block_start(block_start);
        // SAFETY: `header_ptr` is derived from a non-null block start and points to the
        // header storage at the beginning of that allocation block.
        unsafe {
            header_ptr.as_ptr().write(self);
        }
        header_ptr
    }

    #[allow(dead_code)]
    pub(crate) fn write_small_to_block(
        block_start: NonNull<u8>,
        class: SizeClass,
        requested_size: usize,
        owner_cache_id: u32,
    ) -> Option<NonNull<Self>> {
        let (class_index, requested_size, usable_size) =
            encode_small_fields(class, requested_size)?;
        let header_ptr = header_from_block_start(block_start);

        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);

        // SAFETY: `header_ptr` names the 64-byte header region at the start of a valid
        // allocator block, and writing these fields refreshes metadata in place.
        unsafe {
            Self::write_encoded_fields(
                header_ptr,
                AllocationKindTag::Small,
                class_index,
                requested_size,
                usable_size,
                owner_cache_id,
            );
        }

        Some(header_ptr)
    }

    /// Writes a small-block header without re-validating size-class/request encoding.
    ///
    /// # Safety
    ///
    /// `requested_size` must be in `1..=class.payload_size()`, `class.payload_size()`
    /// and `requested_size` must fit in `u32`, and `block_start` must name writable
    /// header storage for a valid allocator block start.
    pub(crate) unsafe fn write_small_to_block_unchecked(
        block_start: NonNull<u8>,
        class: SizeClass,
        requested_size: usize,
        owner_cache_id: u32,
    ) -> NonNull<Self> {
        debug_assert!(requested_size > 0);
        debug_assert!(requested_size <= class.payload_size());
        debug_assert!(u32::try_from(requested_size).is_ok());
        debug_assert!(u32::try_from(class.payload_size()).is_ok());

        let class_index = size_class_to_index(class);
        // SAFETY: the caller guarantees the request and class payload encode losslessly.
        let requested_size = unsafe { u32::try_from(requested_size).unwrap_unchecked() };
        // SAFETY: same invariant as above for built-in small-class payload size.
        let usable_size = unsafe { u32::try_from(class.payload_size()).unwrap_unchecked() };
        let header_ptr = header_from_block_start(block_start);

        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);

        // SAFETY: the caller provides valid writable header storage, and the encoded
        // fields have been proven valid by the function preconditions.
        unsafe {
            Self::write_encoded_fields(
                header_ptr,
                AllocationKindTag::Small,
                class_index,
                requested_size,
                usable_size,
                owner_cache_id,
            );
        }

        header_ptr
    }

    /// Initializes a fresh small-block header without re-validating the size-class encoding.
    ///
    /// # Safety
    ///
    /// `class` must be one of Orthotope's built-in size classes, which guarantees its
    /// payload size fits in `u32`. `block_start` must name writable header storage for a
    /// valid allocator block start.
    pub(crate) unsafe fn initialize_small_to_block_unchecked(
        block_start: NonNull<u8>,
        class: SizeClass,
    ) -> NonNull<Self> {
        debug_assert!(
            u32::try_from(class.payload_size()).is_ok(),
            "small size class payload must fit in header encoding"
        );
        let class_index = size_class_to_index(class);
        // SAFETY: `class` is one of Orthotope's built-in small classes. The largest
        // payload is 16 MiB, so this conversion is infallible by construction.
        let usable_size = unsafe { u32::try_from(class.payload_size()).unwrap_unchecked() };
        let header_ptr = header_from_block_start(block_start);

        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);

        // SAFETY: the caller provides a valid block start for this size class, and all
        // built-in small classes encode losslessly in the header fields.
        unsafe {
            Self::write_small_prefix(header_ptr, class_index, 0, usable_size);
        }

        header_ptr
    }

    /// Refreshes the requested-size field without re-validating header encoding.
    ///
    /// # Safety
    ///
    /// `requested_size` must be in `1..=current small header usable_size`, must fit in
    /// `u32`, and `block_start` must name an initialized writable small-allocation header.
    pub(crate) unsafe fn refresh_small_requested_size_unchecked(
        block_start: NonNull<u8>,
        requested_size: usize,
    ) -> NonNull<Self> {
        debug_assert!(requested_size > 0);
        debug_assert!(u32::try_from(requested_size).is_ok());
        // SAFETY: the caller guarantees the request size encodes losslessly.
        let requested_size = unsafe { u32::try_from(requested_size).unwrap_unchecked() };
        let header_ptr = header_from_block_start(block_start);

        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);
        // SAFETY: the caller guarantees `block_start` names an initialized small header;
        // reading `usable_size` only checks that the requested-size refresh preserves
        // the live-header invariant in debug builds.
        debug_assert!(unsafe { (*header_ptr.as_ptr()).usable_size >= requested_size });

        // SAFETY: `header_ptr` points to an initialized small-allocation header, so
        // updating only the requested-size field preserves the routing metadata.
        unsafe {
            ptr::addr_of_mut!((*header_ptr.as_ptr()).requested_size).write(requested_size);
        }

        header_ptr
    }

    /// Refreshes requested size and owner without re-validating header encoding.
    ///
    /// # Safety
    ///
    /// `requested_size` must be in `1..=current small header usable_size`, must fit in
    /// `u32`, and `block_start` must name an initialized writable small-allocation header.
    pub(crate) unsafe fn refresh_small_requested_size_and_owner_unchecked(
        block_start: NonNull<u8>,
        requested_size: usize,
        owner_cache_id: u32,
    ) -> NonNull<Self> {
        debug_assert!(requested_size > 0);
        debug_assert!(u32::try_from(requested_size).is_ok());
        // SAFETY: the caller guarantees the request size encodes losslessly.
        let requested_size = unsafe { u32::try_from(requested_size).unwrap_unchecked() };
        let header_ptr = header_from_block_start(block_start);

        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);
        // SAFETY: the caller guarantees `block_start` names an initialized small header;
        // reading `usable_size` only checks that the hot-field refresh preserves the
        // live-header invariant in debug builds.
        debug_assert!(unsafe { (*header_ptr.as_ptr()).usable_size >= requested_size });

        // SAFETY: `header_ptr` points to an initialized small-allocation header, so
        // updating these hot-path fields preserves the routing metadata.
        unsafe {
            ptr::addr_of_mut!((*header_ptr.as_ptr()).requested_size).write(requested_size);
            ptr::addr_of_mut!((*header_ptr.as_ptr()).owner_cache_id).write(owner_cache_id);
        }

        header_ptr
    }

    #[allow(dead_code)]
    pub(crate) fn write_large_to_block(
        block_start: NonNull<u8>,
        requested_size: usize,
        usable_size: usize,
    ) -> Option<NonNull<Self>> {
        let (requested_size, usable_size) = encode_large_fields(requested_size, usable_size)?;
        let header_ptr = header_from_block_start(block_start);

        debug_assert_eq!(block_start.as_ptr().addr() % HEADER_ALIGNMENT, 0);

        // SAFETY: `header_ptr` names the 64-byte header region at the start of a valid
        // allocator block, and writing these fields refreshes metadata in place.
        unsafe {
            Self::write_encoded_fields(
                header_ptr,
                AllocationKindTag::Large,
                LARGE_CLASS_SENTINEL,
                requested_size,
                usable_size,
                0,
            );
        }

        Some(header_ptr)
    }

    const fn from_encoded_fields(
        kind: AllocationKindTag,
        class_index: u8,
        requested_size: u32,
        usable_size: u32,
        owner_cache_id: u32,
    ) -> Self {
        Self {
            magic: HEADER_MAGIC,
            kind,
            class_index,
            reserved: [0; 2],
            requested_size,
            usable_size,
            owner_cache_id,
            free_metadata_alignment_padding: 0,
            free_list_link: 0,
            small_free_marker: 0,
            tail_padding: [0; HEADER_TAIL_PADDING_SIZE],
        }
    }

    unsafe fn write_encoded_fields(
        header_ptr: NonNull<Self>,
        kind: AllocationKindTag,
        class_index: u8,
        requested_size: u32,
        usable_size: u32,
        owner_cache_id: u32,
    ) {
        let header = header_ptr.as_ptr();
        // SAFETY: the caller guarantees `header_ptr` points at writable header storage for
        // a valid allocator block, so these field writes update that header in place.
        unsafe {
            ptr::addr_of_mut!((*header).magic).write(HEADER_MAGIC);
            ptr::addr_of_mut!((*header).kind).write(kind);
            ptr::addr_of_mut!((*header).class_index).write(class_index);
            ptr::addr_of_mut!((*header).reserved).write([0; 2]);
            ptr::addr_of_mut!((*header).requested_size).write(requested_size);
            ptr::addr_of_mut!((*header).usable_size).write(usable_size);
            ptr::addr_of_mut!((*header).owner_cache_id).write(owner_cache_id);
            ptr::addr_of_mut!((*header).free_metadata_alignment_padding).write(0);
            ptr::addr_of_mut!((*header).free_list_link).write(0);
            ptr::addr_of_mut!((*header).small_free_marker).write(0);
            ptr::addr_of_mut!((*header).tail_padding).write([0; HEADER_TAIL_PADDING_SIZE]);
        }
    }

    unsafe fn write_small_prefix(
        header_ptr: NonNull<Self>,
        class_index: u8,
        requested_size: u32,
        usable_size: u32,
    ) {
        // SAFETY: the caller guarantees `header_ptr` points at writable header storage for
        // a valid small allocator block, so these field writes establish small-block metadata.
        unsafe {
            Self::write_encoded_fields(
                header_ptr,
                AllocationKindTag::Small,
                class_index,
                requested_size,
                usable_size,
                0,
            );
        }
    }

    pub(crate) unsafe fn read_from_user_ptr(
        user_ptr: NonNull<u8>,
    ) -> Result<(Self, AllocationKind), FreeError> {
        let header_ptr = header_from_user_ptr(user_ptr);
        let header_prefix = header_prefix_from_user_ptr(user_ptr);
        // SAFETY: the caller already established that `user_ptr` has a plausible header
        // address and alignment; reading just the prefix is enough for routing validation.
        let prefix = unsafe { header_prefix.as_ptr().read() };
        let kind = validate_prefix(prefix)?;
        // SAFETY: the same header address is valid for a full-header read once routing
        // validation has confirmed a well-formed live header prefix.
        let header = unsafe { header_ptr.as_ptr().read() };
        Ok((header, kind))
    }
}

fn encode_small_fields(class: SizeClass, requested_size: usize) -> Option<(u8, u32, u32)> {
    if requested_size == 0 || requested_size > class.payload_size() {
        return None;
    }

    Some((
        size_class_to_index(class),
        u32::try_from(requested_size).ok()?,
        u32::try_from(class.payload_size()).ok()?,
    ))
}

fn encode_large_fields(requested_size: usize, usable_size: usize) -> Option<(u32, u32)> {
    if requested_size == 0 || usable_size < requested_size {
        return None;
    }

    Some((
        u32::try_from(requested_size).ok()?,
        u32::try_from(usable_size).ok()?,
    ))
}

fn validate_prefix(prefix: AllocationHeaderPrefix) -> Result<AllocationKind, FreeError> {
    if prefix.magic != HEADER_MAGIC {
        return Err(FreeError::CorruptHeader);
    }

    if prefix.requested_size == 0 || prefix.usable_size < prefix.requested_size {
        return Err(FreeError::CorruptHeader);
    }

    match prefix.kind {
        AllocationKindTag::Small => {
            let class = index_to_size_class(prefix.class_index).ok_or(FreeError::CorruptHeader)?;
            if prefix.usable_size as usize != class.payload_size() {
                return Err(FreeError::CorruptHeader);
            }
            Ok(AllocationKind::Small(class))
        }
        AllocationKindTag::Large => {
            if prefix.class_index != LARGE_CLASS_SENTINEL {
                return Err(FreeError::CorruptHeader);
            }
            Ok(AllocationKind::Large)
        }
    }
}

#[must_use]
#[allow(dead_code)]
pub(crate) const fn header_from_block_start(block_start: NonNull<u8>) -> NonNull<AllocationHeader> {
    block_start.cast()
}

#[must_use]
const fn header_prefix_from_block_start(
    block_start: NonNull<u8>,
) -> NonNull<AllocationHeaderPrefix> {
    block_start.cast()
}

#[must_use]
#[allow(dead_code)]
pub(crate) const fn block_start_from_header(header: NonNull<AllocationHeader>) -> NonNull<u8> {
    header.cast()
}

#[must_use]
#[allow(dead_code)]
pub(crate) const fn user_ptr_from_block_start(block_start: NonNull<u8>) -> NonNull<u8> {
    let user_ptr = block_start.as_ptr().wrapping_add(HEADER_SIZE);
    // SAFETY: adding a fixed positive offset to a non-null pointer cannot produce null.
    unsafe { NonNull::new_unchecked(user_ptr) }
}

#[must_use]
#[allow(dead_code)]
pub(crate) const fn user_ptr_from_header(header: NonNull<AllocationHeader>) -> NonNull<u8> {
    user_ptr_from_block_start(block_start_from_header(header))
}

/// Converts a user pointer back to the header address at the start of the block.
///
/// The caller must pass the exact user pointer originally derived from this
/// allocator's block layout. Any other pointer may produce an invalid header
/// address that must not be dereferenced.
#[must_use]
#[allow(clippy::cast_ptr_alignment)]
#[allow(dead_code)]
pub(crate) const fn header_from_user_ptr(user_ptr: NonNull<u8>) -> NonNull<AllocationHeader> {
    let header_ptr = user_ptr
        .as_ptr()
        .wrapping_sub(HEADER_SIZE)
        .cast::<AllocationHeader>();
    // SAFETY: subtracting the header size from a valid user pointer yields the header address.
    unsafe { NonNull::new_unchecked(header_ptr) }
}

#[must_use]
const fn header_prefix_from_user_ptr(user_ptr: NonNull<u8>) -> NonNull<AllocationHeaderPrefix> {
    header_prefix_from_block_start(block_start_from_user_ptr(user_ptr))
}

#[must_use]
#[allow(dead_code)]
pub(crate) const fn block_start_from_user_ptr(user_ptr: NonNull<u8>) -> NonNull<u8> {
    header_from_user_ptr(user_ptr).cast()
}

pub(crate) const SMALL_FREE_LIST_LINK_OFFSET: usize = offset_of!(AllocationHeader, free_list_link);
pub(crate) const SMALL_FREE_MARKER_OFFSET: usize = offset_of!(AllocationHeader, small_free_marker);

pub(crate) unsafe fn read_small_free_list_next(block_start: NonNull<u8>) -> Option<NonNull<u8>> {
    let header = header_from_block_start(block_start).as_ptr();
    // SAFETY: `block_start` names readable header storage for an allocator block, and the
    // free-list link word is reserved exclusively for detached small-block linkage.
    let next_addr = unsafe { ptr::addr_of!((*header).free_list_link).read() };
    NonNull::new(next_addr as *mut u8)
}

pub(crate) unsafe fn write_small_free_list_next(
    block_start: NonNull<u8>,
    next: Option<NonNull<u8>>,
) {
    let header = header_from_block_start(block_start).as_ptr();
    let next_addr = next.map_or(0, |ptr| ptr.as_ptr().addr());
    // SAFETY: `block_start` names writable header storage for an allocator block, and the
    // free-list link word is reserved exclusively for detached small-block linkage.
    unsafe {
        ptr::addr_of_mut!((*header).free_list_link).write(next_addr);
    }
}

pub(crate) unsafe fn clear_small_free_metadata(block_start: NonNull<u8>) {
    // SAFETY: the caller ensures this block has been detached from any free list before its
    // free-list metadata is reset for live allocation use.
    unsafe {
        write_small_free_list_next(block_start, None);
        clear_small_block_freed(block_start);
    }
}

#[cfg(debug_assertions)]
fn small_free_marker_for(block_start: NonNull<u8>) -> usize {
    SMALL_FREE_MARKER_SEED ^ block_start.as_ptr().addr().rotate_left(17) ^ HEADER_SIZE
}

#[cfg(debug_assertions)]
pub(crate) unsafe fn small_block_is_marked_freed(block_start: NonNull<u8>) -> bool {
    let header = header_from_block_start(block_start).as_ptr();
    // SAFETY: `block_start` names readable header storage for an allocator block whose
    // freed marker lives in a reserved header word.
    unsafe {
        ptr::addr_of!((*header).small_free_marker).read() == small_free_marker_for(block_start)
    }
}

#[cfg(not(debug_assertions))]
pub(crate) const unsafe fn small_block_is_marked_freed(_block_start: NonNull<u8>) -> bool {
    false
}

#[cfg(debug_assertions)]
pub(crate) unsafe fn mark_small_block_freed(block_start: NonNull<u8>) {
    let header = header_from_block_start(block_start).as_ptr();
    // SAFETY: `block_start` names writable header storage for an allocator block whose
    // freed marker lives in a reserved header word.
    unsafe {
        ptr::addr_of_mut!((*header).small_free_marker).write(small_free_marker_for(block_start));
    }
}

#[cfg(not(debug_assertions))]
pub(crate) const unsafe fn mark_small_block_freed(_block_start: NonNull<u8>) {}

#[cfg(debug_assertions)]
pub(crate) unsafe fn clear_small_block_freed(block_start: NonNull<u8>) {
    let header = header_from_block_start(block_start).as_ptr();
    // SAFETY: `block_start` names writable header storage for an allocator block whose
    // freed marker lives in a reserved header word.
    unsafe {
        ptr::addr_of_mut!((*header).small_free_marker).write(0);
    }
}

#[cfg(not(debug_assertions))]
pub(crate) const unsafe fn clear_small_block_freed(_block_start: NonNull<u8>) {}

#[must_use]
const fn size_class_to_index(class: SizeClass) -> u8 {
    match class {
        SizeClass::B64 => 0,
        SizeClass::B256 => 1,
        SizeClass::B4K => 2,
        SizeClass::B6K => 3,
        SizeClass::B8K => 4,
        SizeClass::B16K => 5,
        SizeClass::B32K => 6,
        SizeClass::B64K => 7,
        SizeClass::B128K => 8,
        SizeClass::B256K => 9,
        SizeClass::B1M => 10,
        SizeClass::B16M => 11,
    }
}

#[must_use]
const fn index_to_size_class(index: u8) -> Option<SizeClass> {
    match index {
        0 => Some(SizeClass::B64),
        1 => Some(SizeClass::B256),
        2 => Some(SizeClass::B4K),
        3 => Some(SizeClass::B6K),
        4 => Some(SizeClass::B8K),
        5 => Some(SizeClass::B16K),
        6 => Some(SizeClass::B32K),
        7 => Some(SizeClass::B64K),
        8 => Some(SizeClass::B128K),
        9 => Some(SizeClass::B256K),
        10 => Some(SizeClass::B1M),
        11 => Some(SizeClass::B16M),
        _ => None,
    }
}

const _: [(); HEADER_SIZE] = [(); size_of::<AllocationHeader>()];
const _: [(); HEADER_ALIGNMENT] = [(); align_of::<AllocationHeader>()];
const _: [(); AllocationHeader::PREFIX_SIZE] = [(); size_of::<AllocationHeaderPrefix>()];
const _: [(); 1] = [(); (SMALL_FREE_LIST_LINK_OFFSET + size_of::<usize>() <= HEADER_SIZE) as usize];
const _: [(); 1] = [(); (SMALL_FREE_MARKER_OFFSET + size_of::<usize>() <= HEADER_SIZE) as usize];
const _: [(); 1] = [(); SMALL_FREE_LIST_LINK_OFFSET.is_multiple_of(align_of::<usize>()) as usize];
const _: [(); 1] = [(); SMALL_FREE_MARKER_OFFSET.is_multiple_of(align_of::<usize>()) as usize];

#[cfg(test)]
mod tests {
    use super::{
        AllocationHeader, AllocationKind, AllocationKindTag, HEADER_ALIGNMENT, HEADER_MAGIC,
        HEADER_SIZE, block_start_from_header, block_start_from_user_ptr, header_from_block_start,
        header_from_user_ptr, user_ptr_from_block_start, user_ptr_from_header,
    };
    use crate::size_class::SizeClass;
    use core::mem::{MaybeUninit, align_of, size_of};
    use core::ptr::NonNull;

    #[repr(C, align(64))]
    struct TestBlock {
        bytes: [u8; HEADER_SIZE + 256],
    }

    fn test_block_start(storage: &mut MaybeUninit<TestBlock>) -> NonNull<u8> {
        let ptr = storage.as_mut_ptr().cast::<u8>();
        // SAFETY: `MaybeUninit<TestBlock>` always has a non-null backing address.
        unsafe { NonNull::new_unchecked(ptr) }
    }

    fn small_header(class: SizeClass, requested_size: usize) -> AllocationHeader {
        AllocationHeader::new_small(class, requested_size)
            .unwrap_or_else(|| panic!("expected valid small header"))
    }

    fn large_header(requested_size: usize, usable_size: usize) -> AllocationHeader {
        AllocationHeader::new_large(requested_size, usable_size)
            .unwrap_or_else(|| panic!("expected valid large header"))
    }

    #[test]
    fn allocation_header_stays_64_bytes_and_64_aligned() {
        assert_eq!(size_of::<AllocationHeader>(), HEADER_SIZE);
        assert_eq!(align_of::<AllocationHeader>(), HEADER_ALIGNMENT);
    }

    #[test]
    fn specialized_small_writer_refreshes_requested_size() {
        let mut storage = MaybeUninit::<TestBlock>::uninit();
        let block_start = test_block_start(&mut storage);

        AllocationHeader::write_small_to_block(block_start, SizeClass::B64, 1, 11)
            .unwrap_or_else(|| panic!("expected specialized small writer to succeed"));
        AllocationHeader::write_small_to_block(block_start, SizeClass::B64, 64, 27)
            .unwrap_or_else(|| panic!("expected specialized small rewrite to succeed"));

        // SAFETY: `block_start` points at the test block's valid header storage.
        let header = unsafe { header_from_block_start(block_start).as_ref() };
        assert_eq!(header.validate(), Ok(AllocationKind::Small(SizeClass::B64)));
        assert_eq!(header.requested_size(), 64);
        assert_eq!(header.usable_size(), 64);
        assert_eq!(header.small_owner_cache_id(), Some(27));
    }

    #[test]
    fn specialized_large_writer_refreshes_sizes() {
        let mut storage = MaybeUninit::<TestBlock>::uninit();
        let block_start = test_block_start(&mut storage);

        AllocationHeader::write_large_to_block(block_start, 128, 192)
            .unwrap_or_else(|| panic!("expected specialized large writer to succeed"));
        AllocationHeader::write_large_to_block(block_start, 96, 192)
            .unwrap_or_else(|| panic!("expected specialized large rewrite to succeed"));

        // SAFETY: `block_start` points at the test block's valid header storage.
        let header = unsafe { header_from_block_start(block_start).as_ref() };
        assert_eq!(header.validate(), Ok(AllocationKind::Large));
        assert_eq!(header.requested_size(), 96);
        assert_eq!(header.usable_size(), 192);
    }

    #[test]
    fn block_start_header_and_user_pointer_round_trip() {
        let mut storage = MaybeUninit::<TestBlock>::uninit();
        let block_start = test_block_start(&mut storage);
        let header = header_from_block_start(block_start);
        let user_ptr = user_ptr_from_block_start(block_start);

        assert_eq!(block_start_from_header(header), block_start);
        assert_eq!(header_from_user_ptr(user_ptr), header);
        assert_eq!(block_start_from_user_ptr(user_ptr), block_start);
        assert_eq!(user_ptr_from_header(header), user_ptr);
    }

    #[test]
    fn user_pointer_is_header_size_bytes_after_block_start() {
        let mut storage = MaybeUninit::<TestBlock>::uninit();
        let block_start = test_block_start(&mut storage);
        let user_ptr = user_ptr_from_block_start(block_start);

        assert_eq!(
            user_ptr.as_ptr().addr() - block_start.as_ptr().addr(),
            HEADER_SIZE
        );
    }

    #[test]
    fn small_header_construction_records_class_and_sizes() {
        let header = small_header(SizeClass::B256, 144);

        assert_eq!(header.requested_size(), 144);
        assert_eq!(header.usable_size(), SizeClass::B256.payload_size());
        assert_eq!(
            header.validate(),
            Ok(AllocationKind::Small(SizeClass::B256))
        );
    }

    #[test]
    fn large_header_construction_records_requested_and_usable_sizes() {
        let header = large_header(16_777_217, 16_777_280);

        assert_eq!(header.requested_size(), 16_777_217);
        assert_eq!(header.usable_size(), 16_777_280);
        assert_eq!(header.validate(), Ok(AllocationKind::Large));
    }

    #[test]
    fn small_header_validation_rejects_invalid_class_index() {
        let mut header = small_header(SizeClass::B64, 32);
        header.class_index = 9;

        assert_eq!(
            header.validate(),
            Err(crate::error::FreeError::CorruptHeader)
        );
    }

    #[test]
    fn validation_rejects_bad_magic() {
        let mut header = large_header(1, 64);
        header.magic = HEADER_MAGIC ^ 1;

        assert_eq!(
            header.validate(),
            Err(crate::error::FreeError::CorruptHeader)
        );
    }

    #[test]
    fn write_to_block_persists_header_at_block_start() {
        let mut storage = MaybeUninit::<TestBlock>::uninit();
        let block_start = test_block_start(&mut storage);
        let expected = small_header(SizeClass::B4K, 1024);
        let header_ptr = expected.write_to_block(block_start);

        // SAFETY: the block now contains an initialized `AllocationHeader` at block start.
        let actual = unsafe { header_ptr.as_ref() };
        assert_eq!(actual.requested_size(), 1024);
        assert_eq!(actual.usable_size(), SizeClass::B4K.payload_size());
        assert_eq!(actual.validate(), Ok(AllocationKind::Small(SizeClass::B4K)));
    }

    #[test]
    fn validation_rejects_mismatched_small_usable_size() {
        let mut header = small_header(SizeClass::B256, 128);
        header.kind = AllocationKindTag::Small;
        header.class_index = 1;
        header.usable_size = 64;

        assert_eq!(
            header.validate(),
            Err(crate::error::FreeError::CorruptHeader)
        );
    }
}
