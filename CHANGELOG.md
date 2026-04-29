# Changelog

## 0.3.1

Performance:

- Inline hot `FreeList`/`Batch` operations and avoid clearing stale intrusive
  next pointers on single-node pops; only linked nodes' next pointers are
  semantically meaningful, and insertion overwrites stale values.
- Combine the same-owner thread-cache free push with its drain-limit check to
  avoid repeated class routing on the small-object free path.
- Inline local-slab availability/containment checks used during slab reuse.
- Decode frees from the validated header prefix without reloading the full
  64-byte allocation header.
- Cache per-size-class refill, drain, and local-limit counts in `ThreadCache`
  instead of recomputing them from `AllocatorConfig` on hot allocate/free
  paths.
- Stop eagerly clearing the small-block intrusive next-pointer word on
  reallocation; only the freed marker is reset because the next pointer is read
  exclusively while blocks are detached in free lists.
- Refresh only the requested size and owner cache ID when reissuing a central
  partial batch.
- Trim redundant `Option`/emptiness checks from free-list and slab refill/drain
  loops, enforcing those invariants at registration and in debug assertions.
- Initialize fresh small-block headers via an infallible
  `initialize_small_to_block_unchecked` path, dropping the dead `OutOfMemory`
  branch on every span refill, and walk block starts by pointer increment.

Tooling:

- Add a `long_lived_handoff_4x` benchmark (4 concurrent producer/consumer
  pairs) and `ORTHOTOPE_BENCH_WORKLOAD` / `ORTHOTOPE_BENCH_ALLOCATOR` env-var
  filters for running a single workload or allocator in isolation.

## 0.3.0

Architecture:

- Track small-object slabs by stable IDs in the thread cache and central pool,
  avoiding vector rebuilds on slab registration and refill.
- Replace remote-free buffering with a per-class central inbox; cross-thread
  frees publish cheaply and resolve slab ownership on the next drain/refill.
- Maintain large-object `live_bytes` incrementally so stats snapshots no longer
  rescan the live large-allocation registry.

## 0.2.0

- **Breaking**: `FreeError` now includes a `DoubleFree` variant. Downstream
  crates that exhaustively match on `FreeError` must handle the new arm.
- Detect small-object double frees at runtime via a header-resident freed
  marker; debug builds return `FreeError::DoubleFree` while the marker remains
  intact.

## 0.1.10

- Fix non-panicking behavior on allocation/deallocation failures without
  widening the public error enums; the public API now returns typed errors
  instead of panicking during TLS teardown.
- Fix cross-thread reuse bug in the small-object path where a block freed by
  thread B could be recycled with a stale owning-cache header when thread A
  reallocated it.

## 0.1.9

- Implement thread-local cache ownership tracking and remote-free buffering;
  blocks freed by a non-owning thread are queued in a per-class remote list
  and flushed to the central pool in batches.
- Add `OrthotopeGlobalAlloc`, a thin `GlobalAlloc` adapter that makes the
  allocator usable as `#[global_allocator]` with automatic thread-cache
  provisioning.
- Fix invalid reentrant `global_alloc` fallback layouts without panicking;
  re-entering the allocator during TLS init now returns a null pointer instead
  of aborting.

## 0.1.8

- Introduce a hot-block slot in the thread cache that short-circuits same-thread
  free→alloc cycles without touching the free list, improving tight-loop
  throughput.
- Fix initialization of small-block requested size to zero so untouched
  (never-allocated) blocks are correctly rejected during deallocation.

## 0.1.7

- Add medium-size classes covering the 4 KiB – 256 KiB range, reducing internal
  fragmentation for allocations that previously fell straight into the large
  object path.

## 0.1.6

- Fix thread-cache rebind state reset: switching a cache to a new allocator now
  properly clears stale header and slab metadata, along with a refactored
  header module and large-object tracker.

## 0.1.5

- Add `AllocatorStats` API (`stats()` method) with per-class slab counts,
  arena utilization, and large-object metrics.
- Refactor slab batch drain logic to prefer reclaimed free-list blocks over
  arena-fresh spans.

## 0.1.4

- Support custom allocator alignment; `AllocatorConfig` now threads a
  caller-specified alignment through to size-class and arena calculations.
- Add allocator ID and cache binding validation; a `ThreadCache` bound to one
  allocator rejects operations targeting another.
- Add `AllocatorConfig::class_block_size` helper to query the actual block
  size for a given payload size.
- Fix block alignment clamping: `block_size_for_alignment` now clamps to the
  configured minimum instead of silently under-aligning.
- Reuse freed large allocations on a best-fit basis via a per-allocator free
  pool of `FreeLargeBlock` entries.
- Fix `OutOfMemory` error to propagate the originally requested size rather
  than the rounded-up block size.

## 0.1.3

- Refill small caches via contiguous span reservation: the arena now hands out
  a single `ReservedSpan` covering multiple blocks, which the thread cache
  splits locally instead of issuing per-block bump allocations.
- Add comprehensive crate-level and per-module rustdoc documentation across all
  public and internal modules.

## 0.1.1

- Validate pointer alignment before decoding the allocation header, returning
  `FreeError::InvalidPointer` instead of reading a misaligned header.
