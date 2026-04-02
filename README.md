# Orthotope

[![Crates.io](https://img.shields.io/crates/v/orthotope.svg)](https://crates.io/crates/orthotope)
[![Documentation](https://docs.rs/orthotope/badge.svg)](https://docs.rs/orthotope)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Orthotope is a Rust allocator library with:

## Installation

```sh
cargo add orthotope
```

## Features

- a pre-mapped arena
- fixed size classes up to `16 MiB`
- per-thread caches
- a shared central pool
- a tracked large-allocation path

It is aimed at allocation-heavy workloads such as ML inference, tensor pipelines,
batched embedding or reranking services, and other high-throughput systems.

## API

```rust
use orthotope::{allocate, deallocate};

let ptr = allocate(128)?;

unsafe {
    deallocate(ptr)?;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

- `allocate(size)` returns `Result<NonNull<u8>, AllocError>`
- `deallocate(ptr)` is the primary free path
- `deallocate_with_size(ptr, size)` validates the recorded size before freeing

Only free live pointers returned by Orthotope. Small-object double free remains undefined behavior.

For direct instance-oriented use, the crate also exposes `Allocator`, `AllocatorConfig`,
`ThreadCache`, and `SizeClass` at the crate root. Use one `ThreadCache` per thread when
calling `Allocator::allocate_with_cache` or `Allocator::deallocate_with_cache` directly.

## Behavior

- small allocations use thread-local reuse first, then central-pool refill, then arena carving
- frees are routed by a 64-byte allocation header
- requests above `16 MiB` use the large-allocation path
- default alignment is `64` bytes
- custom allocator alignment must be a power of two and at least `64` bytes
- the global convenience API uses `AllocatorConfig::default()`

Small-object provenance in v1 is limited to header validation plus an arena-range
ownership check on the decoded block start. Foreign pointers are rejected where
detectable, but small-object double free remains undefined behavior and same-arena
pointer forgery is not guaranteed to be detected.

Large allocations are also tracked in a live registry. Duplicate large frees are rejected
when the pointer still decodes to a valid large-allocation header.

Small-request classes:

- `1..=64`
- `65..=256`
- `257..=4096`
- `4097..=262_144`
- `262_145..=1_048_576`
- `1_048_577..=16_777_216`
