//! Allocation-header layout and pointer conversions used for deallocation routing.

use core::mem::{align_of, size_of};
use core::ptr::NonNull;

use crate::error::FreeError;
use crate::size_class::SizeClass;

/// Alignment required for allocation headers and block starts.
pub const HEADER_ALIGNMENT: usize = 64;
/// Bytes reserved for allocator metadata at the start of every block.
pub const HEADER_SIZE: usize = 64;

const HEADER_MAGIC: u32 = 0x4f52_5448;
const LARGE_CLASS_SENTINEL: u8 = u8::MAX;

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
    padding: [u8; 48],
}

impl AllocationHeader {
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn new_small(class: SizeClass, requested_size: usize) -> Option<Self> {
        if requested_size == 0 || requested_size > class.payload_size() {
            return None;
        }
        let requested_size = u32::try_from(requested_size).ok()?;
        let usable_size = u32::try_from(class.payload_size()).ok()?;

        Some(Self {
            magic: HEADER_MAGIC,
            kind: AllocationKindTag::Small,
            class_index: size_class_to_index(class),
            reserved: [0; 2],
            requested_size,
            usable_size,
            padding: [0; 48],
        })
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn new_large(requested_size: usize, usable_size: usize) -> Option<Self> {
        if requested_size == 0 || usable_size < requested_size {
            return None;
        }
        let requested_size = u32::try_from(requested_size).ok()?;
        let usable_size = u32::try_from(usable_size).ok()?;

        Some(Self {
            magic: HEADER_MAGIC,
            kind: AllocationKindTag::Large,
            class_index: LARGE_CLASS_SENTINEL,
            reserved: [0; 2],
            requested_size,
            usable_size,
            padding: [0; 48],
        })
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
}

#[must_use]
#[allow(dead_code)]
pub(crate) const fn header_from_block_start(block_start: NonNull<u8>) -> NonNull<AllocationHeader> {
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
#[allow(dead_code)]
pub(crate) const fn block_start_from_user_ptr(user_ptr: NonNull<u8>) -> NonNull<u8> {
    header_from_user_ptr(user_ptr).cast()
}

#[must_use]
const fn size_class_to_index(class: SizeClass) -> u8 {
    match class {
        SizeClass::B64 => 0,
        SizeClass::B256 => 1,
        SizeClass::B4K => 2,
        SizeClass::B256K => 3,
        SizeClass::B1M => 4,
        SizeClass::B16M => 5,
    }
}

#[must_use]
const fn index_to_size_class(index: u8) -> Option<SizeClass> {
    match index {
        0 => Some(SizeClass::B64),
        1 => Some(SizeClass::B256),
        2 => Some(SizeClass::B4K),
        3 => Some(SizeClass::B256K),
        4 => Some(SizeClass::B1M),
        5 => Some(SizeClass::B16M),
        _ => None,
    }
}

const _: [(); HEADER_SIZE] = [(); size_of::<AllocationHeader>()];
const _: [(); HEADER_ALIGNMENT] = [(); align_of::<AllocationHeader>()];

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
