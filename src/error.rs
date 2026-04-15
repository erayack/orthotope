use thiserror::Error;

/// Failures while constructing an allocator instance or the global allocator.
#[derive(Debug, Error)]
pub enum InitError {
    /// Creating the arena mapping failed.
    #[error("failed to create arena mapping: {0}")]
    MapFailed(std::io::Error),
    /// The supplied allocator configuration violated a required invariant.
    #[error("invalid allocator configuration: {0}")]
    InvalidConfig(&'static str),
}

/// Failures that can occur while allocating memory.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AllocError {
    /// Zero-sized requests are rejected.
    #[error("allocation size must be greater than zero")]
    ZeroSize,
    /// The process-global allocator could not be initialized.
    #[error("global allocator initialization failed")]
    GlobalInitFailed,
    /// The allocator could not reserve enough space for the request.
    #[error("allocator out of memory: requested {requested} bytes, remaining {remaining} bytes")]
    OutOfMemory { requested: usize, remaining: usize },
}

/// Detectable failures while freeing memory.
///
/// Invalid frees can still be undefined behavior if the documented `unsafe`
/// preconditions of the free APIs are violated.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FreeError {
    /// The process-global allocator could not be initialized.
    #[error("global allocator initialization failed")]
    GlobalInitFailed,
    /// The pointer does not belong to this allocator instance.
    #[error("pointer does not belong to this allocator")]
    ForeignPointer,
    /// The compatibility size argument disagreed with the stored allocation size.
    #[error("provided size {provided} does not match recorded allocation size {recorded}")]
    SizeMismatch { provided: usize, recorded: usize },
    /// A small allocation was freed twice while its freed marker was still intact.
    #[error("small allocation was already freed")]
    DoubleFree,
    /// A large allocation was already freed or was never recorded as live.
    #[error("large allocation was already freed or is unknown")]
    AlreadyFreedOrUnknownLarge,
    /// The decoded allocation header was invalid.
    #[error("allocation header is corrupt")]
    CorruptHeader,
}
