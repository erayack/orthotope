# Orthotope

[![Crates.io](https://img.shields.io/crates/v/orthotope.svg)](https://crates.io/crates/orthotope)
[![Documentation](https://docs.rs/orthotope/badge.svg)](https://docs.rs/orthotope)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Orthotope is a Rust allocator with a pre-mapped arena, fixed size classes up to `16 MiB`,
per-thread caches, a shared central pool, and a tracked large-allocation path.
Aimed at allocation-heavy workloads like ML inference, tensor pipelines, and
batched embedding or reranking services.

See [`CHANGELOG`](CHANGELOG.md) for release-specific compatibility notes.

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

- `allocate(size) -> Result<NonNull<u8>, AllocError>`
- `deallocate(ptr)` — primary free path
- `deallocate_with_size(ptr, size)` — validates the recorded size before freeing
- `global_stats()` — best-effort snapshot of shared state

Only free live pointers returned by Orthotope. Debug builds detect some small-object
double frees; stale-pointer ABA cases are not guaranteed to be caught.

The crate also exposes `Allocator`, `AllocatorConfig`, `ThreadCache`, and
`SizeClass` for instance-oriented use. Use one `ThreadCache` per thread with
`Allocator::allocate_with_cache` / `deallocate_with_cache`. An empty cache may
be rebound to another allocator; reusing a non-empty cache across allocators panics.

### Drop-in global allocator

```rust
use orthotope::OrthotopeGlobalAlloc;

#[global_allocator]
static GLOBAL: OrthotopeGlobalAlloc = OrthotopeGlobalAlloc::new();
```

The shim falls back to `std::alloc::System` for `size == 0` or `align() > 64`,
and has a best-effort fallback for rare reentrant TLS-cache borrows.
`global_stats()` reports only Orthotope-managed allocations.

## Behavior

- Small allocations: thread-local reuse → central-pool refill → arena carving.
- Thread caches own class-local slabs carved from contiguous arena spans.
- Fully central-resident small slabs may be retained as metadata-only records
  and lazily marked reclaimable with `madvise(MADV_FREE)` when cold.
- Frees are routed by a 64-byte allocation header; small-object headers are
  refreshed in place on reuse.
- Requests above `16 MiB` use the large-allocation path, which returns freed
  spans to an arena-backed fit-based reuse pool.
- Default alignment is `64` bytes; custom alignment must be a power of two and
  at least `64` bytes.

`AllocatorStats::arena_remaining` reports monotonic unreserved arena capacity,
not RSS. Pages from idle central slabs may remain in the arena address space
even after becoming lazily reclaimable.

Small-object provenance is limited to header validation plus an arena-range
ownership check, with debug-build freed-marker detection for duplicate small
frees. Same-arena pointer forgery, stale-pointer ABA, and cold-page reclaim
that discards freed markers are not guaranteed to be detected. Large frees
are tracked in a live registry; duplicate large frees are rejected while the
header remains valid, but stale large pointers after address reuse are not
guaranteed to be distinguishable by the raw-pointer free API.

When using `OrthotopeGlobalAlloc`, an invalid free on the Orthotope-managed
path aborts the process, since `GlobalAlloc::dealloc` cannot return errors.

Small-request size classes: `1..=64`, `65..=256`, `257..=4096`, `4097..=6144`,
`6145..=8192`, `8193..=16_384`, `16_385..=32_768`, `32_769..=65_536`,
`65_537..=131_072`, `131_073..=262_144`, `262_145..=1_048_576`,
`1_048_577..=16_777_216`.

## Benchmarking

Full results in [`BENCHMARK`](BENCHMARK.md). In the current local run,
Orthotope was:

- fastest on 9 of 11 workloads against system, mimalloc, and jemalloc
- ~`3.2x` / `3.1x` faster than `mimalloc` / `jemalloc` on `mixed_size_churn`
- ~`10.6x` / `7.2x` faster than `mimalloc` / `jemalloc` on `large_path`
- ~`75x` faster than `mimalloc` on `same_thread_small_churn/70000`

Run the [`bench/`](bench/) harness against Orthotope, system, mimalloc, and jemalloc:

```sh
just bench
```
