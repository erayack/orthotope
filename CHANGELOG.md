# Changelog

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
