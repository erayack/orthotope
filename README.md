# Orthotope

[![Crates.io](https://img.shields.io/crates/v/orthotope.svg)](https://crates.io/crates/orthotope)
[![Documentation](https://docs.rs/orthotope/badge.svg)](https://docs.rs/orthotope)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

See [`CHANGELOG`](CHANGELOG.md) for release-specific compatibility notes.

Orthotope is a Rust allocator library with:

- a pre-mapped arena
- fixed size classes up to `16 MiB`
- per-thread caches
- a shared central pool
- a tracked large-allocation path

It is aimed at allocation-heavy workloads such as ML inference, tensor pipelines,
batched embedding or reranking services, and other high-throughput systems.

## Installation

```sh
cargo add orthotope
```

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
- `global_stats()` returns a best-effort snapshot of the global allocator's shared state

Only free live pointers returned by Orthotope. Debug builds detect some small-object
double frees, but stale-pointer ABA cases remain outside guaranteed detection.

For direct instance-oriented use, the crate also exposes `Allocator`, `AllocatorConfig`,
`ThreadCache`, and `SizeClass` at the crate root. Use one `ThreadCache` per thread when
calling `Allocator::allocate_with_cache` or `Allocator::deallocate_with_cache` directly.
`Allocator::stats()` and `ThreadCache::stats()` expose the same best-effort snapshot model
for instance-oriented use. An empty cache may be rebound to a different allocator
instance, but reusing a non-empty cache across allocators panics instead of silently
rehoming cached blocks.

For opt-in drop-in usage in existing binaries, the crate also exposes
`OrthotopeGlobalAlloc`:

```rust
use orthotope::OrthotopeGlobalAlloc;

#[global_allocator]
static GLOBAL: OrthotopeGlobalAlloc = OrthotopeGlobalAlloc::new();
```

The shim intentionally falls back to `std::alloc::System` for layouts with `size == 0`
or `align() > 64`. It also has a best-effort fallback for rare reentrant TLS-cache
borrow cases. `global_stats()` only reports Orthotope-managed allocations, not
system-fallback allocations.

## Behavior

- small allocations use thread-local reuse first, then central-pool refill, then arena carving
- each thread cache owns class-local slabs carved from contiguous arena spans
- small-cache arena refill reserves one contiguous span, registers it as a local slab, and splits it into class-sized blocks
- fully central-resident small slabs may be retained as metadata-only central records and
  lazily marked reclaimable with `madvise(MADV_FREE)` when they stay cold
- frees are routed by a 64-byte allocation header
- small-allocation headers are refreshed in place on reuse instead of rebuilding a fresh header object
- requests above `16 MiB` use the large-allocation path
- default alignment is `64` bytes
- custom allocator alignment must be a power of two and at least `64` bytes
- the global convenience API uses `AllocatorConfig::default()`
- freed large allocations return to an arena-backed reusable pool for later
  same-size or smaller large requests, using smallest-fitting reuse first
- rebinding an empty caller-owned `ThreadCache` to another allocator clears stale
  local slab metadata before the new allocator starts carving fresh slabs

`AllocatorStats::arena_remaining` reports monotonic unreserved arena capacity, not RSS.
Pages from fully idle central slabs may remain in the arena address space even after they
become lazily reclaimable.

Small-object provenance in v1 is limited to header validation plus an arena-range
ownership check on the decoded block start, with debug-build freed-marker detection
for duplicate small frees while the marker survives in cached memory. Foreign pointers
are rejected where detectable, but same-arena pointer forgery, stale-pointer ABA, and
cold-page reclaim that discards freed markers are not guaranteed to be detected.

Large allocations are also tracked in a live registry. Duplicate large frees are rejected
when the pointer still decodes to a valid large-allocation header for the same live
allocation instance, and successful large frees return those arena-backed spans to
fit-based reuse for future same-size or smaller large requests.

Because large blocks may later be reused at the same address, stale large pointers after
address reuse are not guaranteed to be distinguishable by the raw-pointer free API.
Using such pointers still violates the `unsafe` contract.

When using `OrthotopeGlobalAlloc`, `GlobalAlloc::dealloc` cannot return typed errors.
If Orthotope detects an invalid free on the Orthotope-managed path, the shim aborts the
process instead of continuing in an invalid state. The only tolerated leak path is a
reentrant TLS-cache borrow during panic unwind.

Small-request classes:

- `1..=64`
- `65..=256`
- `257..=4096`
- `4097..=6144`
- `6145..=8192`
- `8193..=16_384`
- `16_385..=32_768`
- `32_769..=65_536`
- `65_537..=131_072`
- `131_073..=262_144`
- `262_145..=1_048_576`
- `1_048_577..=16_777_216`

## Benchmarking

Benchmark results are summarized in [`BENCHMARK`](BENCHMARK.md).

In the current local run, Orthotope was:

- fastest on 7 of 9 workloads against system, mimalloc, and jemalloc
- about `2.1x` faster than `mimalloc` and `2.1x` faster than `jemalloc` on `mixed_size_churn`
- about `8.8x` faster than `mimalloc` and `6.3x` faster than `jemalloc` on `large_path`
- about `49x` faster than `mimalloc` on `same_thread_small_churn/70000`

The [`bench/`](bench/) directory contains the harness used to produce these numbers. It runs each workload against Orthotope, the system allocator, mimalloc, and jemalloc, and prints a markdown table of medians.

```sh
just bench
```
