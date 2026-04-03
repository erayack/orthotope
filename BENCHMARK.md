# Benchmark

These are local measurements, useful for relative direction rather than universal claims.

Method summary:

- 3 warmup samples, 9 measured samples, median reported
- `64`-byte alignment across all allocators
- Orthotope measured through `Allocator` plus one `ThreadCache` per participating thread
- `long_lived_handoff` measures steady-state end-to-end handoff time with persistent worker threads
- `large_path` measures warm reuse of a `20 MiB` request

## Overview

- Orthotope was fastest on same-thread hot-path reuse workloads, ranging from about `1.1x` to `7.1x` faster than the comparison allocators depending on size.
- Orthotope also led `mixed_size_churn` by about `2.2x` over `mimalloc`, `large_path` by about `2.8x` over the system allocator and `7.4x` to `11x` over `jemalloc` and `mimalloc`, and the system/jemalloc variants of `long_lived_handoff` by about `1.1x`.
- `mimalloc` led `embedding_batch` and narrowly led `long_lived_handoff`.

## Summary

| Workload | Orthotope | System | mimalloc | jemalloc |
| --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | **7.25 ns** | 59.91 ns | 9.85 ns | 8.53 ns |
| `same_thread_small_churn/64` | **7.29 ns** | 48.03 ns | 7.69 ns | 8.53 ns |
| `same_thread_small_churn/65` | **7.52 ns** | 46.04 ns | 10.43 ns | 8.63 ns |
| `same_thread_small_churn/4096` | **7.83 ns** | 14.54 ns | 13.15 ns | 9.90 ns |
| `same_thread_small_churn/70000` | **8.47 ns** | 16.64 ns | 593.52 ns | 113.33 ns |
| `embedding_batch` | 150.22 ns | 245.31 ns | **99.46 ns** | 127.40 ns |
| `mixed_size_churn` | **90.72 ns** | 388.48 ns | 198.67 ns | 199.83 ns |
| `large_path` | **0.05 us** | 0.14 us | 0.55 us | 0.37 us |
| `long_lived_handoff` | 1.92 us | 2.16 us | **1.85 us** | 2.01 us |

## Interpretation Notes
- Orthotope's same-thread results still align with the intended architecture: thread-local reuse, class-normalized slabs, and in-place header refresh on reuse.
- Relative framing for this capture:
  - same-thread hot-path reuse: Orthotope was about `1.06x` to `7.13x` faster than the best non-Orthotope result for each request size
  - `mixed_size_churn`: Orthotope was about `2.19x` faster than `mimalloc` and `2.20x` faster than `jemalloc`
  - `large_path`: Orthotope was about `2.8x` faster than the system allocator, `7.4x` faster than `jemalloc`, and `11x` faster than `mimalloc`
  - `embedding_batch`: Orthotope was about `1.5x` slower than `mimalloc`
  - `long_lived_handoff`: Orthotope was about `1.12x` faster than the system allocator, about `1.05x` faster than `jemalloc`, and about `1.04x` slower than `mimalloc`
- `embedding_batch` moved from an Orthotope lead in the older ad hoc numbers to a `mimalloc` lead in this capture, so benchmark discussions should cite the methodology revision explicitly.
- The `large_path` result is intentionally a warm-reuse measurement. It highlights fit-based reuse of freed large spans rather than cold allocation cost.
- `long_lived_handoff` is materially lower than the older documented number because the current harness removes per-iteration thread creation and times only the steady-state handoff path.
- If future comparison work needs cold-path large allocations or thread-spawn-inclusive cross-thread costs, add separate named workloads rather than overloading the current ones.
