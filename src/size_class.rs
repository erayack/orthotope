use crate::header::HEADER_SIZE;

pub const NUM_CLASSES: usize = 6;
pub const BLOCK_ALIGNMENT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeClass {
    B64,
    B256,
    B4K,
    B256K,
    B1M,
    B16M,
}

impl SizeClass {
    pub const ALL: [Self; NUM_CLASSES] = [
        Self::B64,
        Self::B256,
        Self::B4K,
        Self::B256K,
        Self::B1M,
        Self::B16M,
    ];

    #[must_use]
    pub const fn from_request(size: usize) -> Option<Self> {
        match size {
            1..=64 => Some(Self::B64),
            65..=256 => Some(Self::B256),
            257..=4096 => Some(Self::B4K),
            4097..=262_144 => Some(Self::B256K),
            262_145..=1_048_576 => Some(Self::B1M),
            1_048_577..=16_777_216 => Some(Self::B16M),
            _ => None,
        }
    }

    #[must_use]
    pub const fn payload_size(self) -> usize {
        match self {
            Self::B64 => 64,
            Self::B256 => 256,
            Self::B4K => 4_096,
            Self::B256K => 262_144,
            Self::B1M => 1_048_576,
            Self::B16M => 16_777_216,
        }
    }

    #[must_use]
    pub const fn block_size(self) -> usize {
        align_up(self.payload_size() + HEADER_SIZE, BLOCK_ALIGNMENT)
    }

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::B64 => 0,
            Self::B256 => 1,
            Self::B4K => 2,
            Self::B256K => 3,
            Self::B1M => 4,
            Self::B16M => 5,
        }
    }

    #[must_use]
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
