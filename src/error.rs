use thiserror::Error;

#[derive(Debug, Error)]
pub enum InitError {
    #[error("failed to create arena mapping: {0}")]
    MapFailed(std::io::Error),
    #[error("invalid allocator configuration: {0}")]
    InvalidConfig(&'static str),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AllocError {
    #[error("allocation size must be greater than zero")]
    ZeroSize,
    #[error("global allocator initialization failed")]
    GlobalInitFailed,
    #[error("allocator out of memory: requested {requested} bytes, remaining {remaining} bytes")]
    OutOfMemory { requested: usize, remaining: usize },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FreeError {
    #[error("global allocator initialization failed")]
    GlobalInitFailed,
    #[error("pointer does not belong to this allocator")]
    ForeignPointer,
    #[error("provided size {provided} does not match recorded allocation size {recorded}")]
    SizeMismatch { provided: usize, recorded: usize },
    #[error("large allocation was already freed or is unknown")]
    AlreadyFreedOrUnknownLarge,
    #[error("allocation header is corrupt")]
    CorruptHeader,
}
