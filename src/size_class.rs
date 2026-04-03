use crate::header::HEADER_SIZE;

/// Number of fixed small-request classes.
pub const NUM_CLASSES: usize = 12;
/// Default block-start alignment used for class sizing.
pub const BLOCK_ALIGNMENT: usize = 64;

/// Fixed small-allocation classes used by Orthotope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeClass {
    /// Requests in `1..=64`.
    B64,
    /// Requests in `65..=256`.
    B256,
    /// Requests in `257..=4096`.
    B4K,
    /// Requests in `4097..=6144`.
    B6K,
    /// Requests in `6145..=8192`.
    B8K,
    /// Requests in `8193..=16_384`.
    B16K,
    /// Requests in `16_385..=32_768`.
    B32K,
    /// Requests in `32_769..=65_536`.
    B64K,
    /// Requests in `65_537..=131_072`.
    B128K,
    /// Requests in `131_073..=262_144`.
    B256K,
    /// Requests in `262_145..=1_048_576`.
    B1M,
    /// Requests in `1_048_577..=16_777_216`.
    B16M,
}

impl SizeClass {
    /// All size classes in ascending order.
    pub const ALL: [Self; NUM_CLASSES] = [
        Self::B64,
        Self::B256,
        Self::B4K,
        Self::B6K,
        Self::B8K,
        Self::B16K,
        Self::B32K,
        Self::B64K,
        Self::B128K,
        Self::B256K,
        Self::B1M,
        Self::B16M,
    ];

    #[must_use]
    /// Maps a request size to its small-allocation class, or `None` for zero and large requests.
    pub const fn from_request(size: usize) -> Option<Self> {
        match size {
            1..=64 => Some(Self::B64),
            65..=256 => Some(Self::B256),
            257..=4096 => Some(Self::B4K),
            4097..=6144 => Some(Self::B6K),
            6145..=8192 => Some(Self::B8K),
            8193..=16_384 => Some(Self::B16K),
            16_385..=32_768 => Some(Self::B32K),
            32_769..=65_536 => Some(Self::B64K),
            65_537..=131_072 => Some(Self::B128K),
            131_073..=262_144 => Some(Self::B256K),
            262_145..=1_048_576 => Some(Self::B1M),
            1_048_577..=16_777_216 => Some(Self::B16M),
            _ => None,
        }
    }

    #[must_use]
    /// Returns the maximum user payload size, in bytes, for this class.
    pub const fn payload_size(self) -> usize {
        match self {
            Self::B64 => 64,
            Self::B256 => 256,
            Self::B4K => 4_096,
            Self::B6K => 6 * 1_024,
            Self::B8K => 8_192,
            Self::B16K => 16_384,
            Self::B32K => 32_768,
            Self::B64K => 65_536,
            Self::B128K => 131_072,
            Self::B256K => 262_144,
            Self::B1M => 1_048_576,
            Self::B16M => 16_777_216,
        }
    }

    #[must_use]
    /// Returns the full allocator block size for this class under the default 64-byte alignment.
    ///
    /// For allocator-specific capacity planning under a custom alignment, prefer
    /// [`Self::block_size_for_alignment`] or [`crate::AllocatorConfig::class_block_size`].
    pub const fn block_size(self) -> usize {
        align_up(self.payload_size() + HEADER_SIZE, BLOCK_ALIGNMENT)
    }

    #[must_use]
    /// Returns the full allocator block size for this class using `alignment`.
    ///
    /// Alignments below the crate's minimum block alignment are clamped up to `64`
    /// so this helper remains total even before allocator validation.
    pub const fn block_size_for_alignment(self, alignment: usize) -> usize {
        align_up(
            self.payload_size() + HEADER_SIZE,
            effective_block_alignment(alignment),
        )
    }

    #[must_use]
    /// Returns the stable dense array index for this class.
    pub const fn index(self) -> usize {
        match self {
            Self::B64 => 0,
            Self::B256 => 1,
            Self::B4K => 2,
            Self::B6K => 3,
            Self::B8K => 4,
            Self::B16K => 5,
            Self::B32K => 6,
            Self::B64K => 7,
            Self::B128K => 8,
            Self::B256K => 9,
            Self::B1M => 10,
            Self::B16M => 11,
        }
    }

    #[must_use]
    /// Returns the largest request still handled by the small-allocation path.
    pub const fn max_small_request() -> usize {
        Self::B16M.payload_size()
    }
}

const fn align_up(value: usize, alignment: usize) -> usize {
    let remainder = value % alignment;
    if remainder == 0 {
        value
    } else {
        value + (alignment - remainder)
    }
}

const fn effective_block_alignment(alignment: usize) -> usize {
    if alignment < BLOCK_ALIGNMENT {
        BLOCK_ALIGNMENT
    } else {
        alignment
    }
}
