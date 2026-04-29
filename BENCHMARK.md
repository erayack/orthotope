# Benchmark

**Method:** each `just bench` run uses 3 warmup + 9 measured samples and reports
a per-run median; the summary is the median of 3 such runs. All allocators use
`64`-byte alignment. Orthotope is measured through `Allocator` with one
`ThreadCache` per participating thread. `long_lived_handoff[_4x]` measures
steady-state end-to-end handoff with persistent worker threads (4 producer/
consumer pairs for `_4x`). `large_path` measures warm reuse of a `20 MiB`
request. `concurrent_large_4x` measures 4-thread concurrent `20 MiB`
allocate/free throughput.

## Overview

Orthotope led 9 of 11 workloads (roughly `1.37x`-`75x` faster depending on size
and competitor), including `embedding_batch`, `mixed_size_churn`, `large_path`,
`concurrent_large_4x`, and every `same_thread_small_churn` size. mimalloc kept
a slight edge on `long_lived_handoff` (about `1%`) and `long_lived_handoff_4x`
(about `0.5%`).

## Summary

| Workload | Orthotope | System | mimalloc | jemalloc |
| --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | **4.94 ns** | 64.15 ns | 10.25 ns | 9.14 ns |
| `same_thread_small_churn/64` | **4.96 ns** | 50.06 ns | 7.93 ns | 9.12 ns |
| `same_thread_small_churn/65` | **5.14 ns** | 48.98 ns | 11.00 ns | 9.13 ns |
| `same_thread_small_churn/4096` | **5.22 ns** | 15.66 ns | 13.44 ns | 10.34 ns |
| `same_thread_small_churn/70000` | **6.99 ns** | 17.66 ns | 527.17 ns | 113.38 ns |
| `embedding_batch` | **74.62 ns** | 258.70 ns | 102.54 ns | 128.53 ns |
| `mixed_size_churn` | **65.72 ns** | 390.74 ns | 209.11 ns | 203.09 ns |
| `large_path` | **0.05 us** | 0.14 us | 0.53 us | 0.36 us |
| `concurrent_large_4x` | **0.10 us** | 0.46 us | 0.25 us | 0.35 us |
| `long_lived_handoff` | 1.99 us | 2.32 us | **1.97 us** | 2.05 us |
| `long_lived_handoff_4x` | **1.86 us** | 1.87 us | 1.87 us | **1.86 us** |

## Notes

Same-thread wins reflect the intended architecture: thread-local reuse,
class-normalized slabs, in-place header refresh on reuse. Selected ratios:

- `same_thread_small_churn/70000`: ~`75x` vs mimalloc, ~`16.2x` vs jemalloc
- `mixed_size_churn`: ~`3.18x` vs mimalloc, ~`3.09x` vs jemalloc
- `large_path`: ~`2.8x` vs system, ~`7.2x` vs jemalloc, ~`10.6x` vs mimalloc (warm-reuse measurement; highlights fit-based reuse of freed large spans)
- `concurrent_large_4x`: ~`4.6x` vs system, ~`3.5x` vs jemalloc, ~`2.5x` vs mimalloc
- `embedding_batch`: ~`3.47x` vs system, ~`1.72x` vs jemalloc, ~`1.37x` vs mimalloc
- `long_lived_handoff`: ~`1.17x` vs system, within ~`1%` of mimalloc
- `long_lived_handoff_4x`: Orthotope and jemalloc lead at `1.86 us`; system and mimalloc are within about `0.5%`

## Raw Runs

Per-run data behind the medians above.

| Workload | Allocator | Run 1 | Run 2 | Run 3 | Median |
| --- | --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | Orthotope | **4.86 ns** | **4.94 ns** | **4.99 ns** | **4.94 ns** |
| `same_thread_small_churn/32` | System | 64.23 ns | 64.15 ns | 63.60 ns | 64.15 ns |
| `same_thread_small_churn/32` | mimalloc | 9.90 ns | 10.25 ns | 10.86 ns | 10.25 ns |
| `same_thread_small_churn/32` | jemalloc | 8.91 ns | 9.14 ns | 9.31 ns | 9.14 ns |
| `same_thread_small_churn/64` | Orthotope | **4.88 ns** | **4.96 ns** | **4.98 ns** | **4.96 ns** |
| `same_thread_small_churn/64` | System | 47.98 ns | 50.11 ns | 50.06 ns | 50.06 ns |
| `same_thread_small_churn/64` | mimalloc | 7.60 ns | 7.93 ns | 8.08 ns | 7.93 ns |
| `same_thread_small_churn/64` | jemalloc | 8.94 ns | 9.12 ns | 9.45 ns | 9.12 ns |
| `same_thread_small_churn/65` | Orthotope | **5.11 ns** | **5.14 ns** | **5.24 ns** | **5.14 ns** |
| `same_thread_small_churn/65` | System | 47.34 ns | 49.25 ns | 48.98 ns | 48.98 ns |
| `same_thread_small_churn/65` | mimalloc | 10.35 ns | 11.00 ns | 11.24 ns | 11.00 ns |
| `same_thread_small_churn/65` | jemalloc | 9.01 ns | 9.13 ns | 9.36 ns | 9.13 ns |
| `same_thread_small_churn/4096` | Orthotope | **5.13 ns** | **5.23 ns** | **5.22 ns** | **5.22 ns** |
| `same_thread_small_churn/4096` | System | 15.30 ns | 15.66 ns | 16.12 ns | 15.66 ns |
| `same_thread_small_churn/4096` | mimalloc | 13.27 ns | 13.44 ns | 14.03 ns | 13.44 ns |
| `same_thread_small_churn/4096` | jemalloc | 10.22 ns | 10.34 ns | 10.49 ns | 10.34 ns |
| `same_thread_small_churn/70000` | Orthotope | **6.10 ns** | **7.05 ns** | **6.99 ns** | **6.99 ns** |
| `same_thread_small_churn/70000` | System | 17.16 ns | 17.66 ns | 17.92 ns | 17.66 ns |
| `same_thread_small_churn/70000` | mimalloc | 519.19 ns | 527.17 ns | 534.28 ns | 527.17 ns |
| `same_thread_small_churn/70000` | jemalloc | 114.03 ns | 113.37 ns | 113.38 ns | 113.38 ns |
| `embedding_batch` | Orthotope | **73.08 ns** | **78.48 ns** | **74.62 ns** | **74.62 ns** |
| `embedding_batch` | System | 252.13 ns | 258.70 ns | 260.69 ns | 258.70 ns |
| `embedding_batch` | mimalloc | 99.71 ns | 105.79 ns | 102.54 ns | 102.54 ns |
| `embedding_batch` | jemalloc | 125.04 ns | 128.53 ns | 128.82 ns | 128.53 ns |
| `mixed_size_churn` | Orthotope | **64.69 ns** | **66.44 ns** | **65.72 ns** | **65.72 ns** |
| `mixed_size_churn` | System | 395.52 ns | 390.74 ns | 382.26 ns | 390.74 ns |
| `mixed_size_churn` | mimalloc | 209.21 ns | 206.80 ns | 209.11 ns | 209.11 ns |
| `mixed_size_churn` | jemalloc | 211.80 ns | 200.12 ns | 203.09 ns | 203.09 ns |
| `large_path` | Orthotope | **0.05 us** | 0.06 us | **0.05 us** | **0.05 us** |
| `large_path` | System | 0.13 us | 0.14 us | 0.14 us | 0.14 us |
| `large_path` | mimalloc | 0.51 us | 0.53 us | 0.55 us | 0.53 us |
| `large_path` | jemalloc | 0.36 us | 0.36 us | 0.39 us | 0.36 us |
| `concurrent_large_4x` | Orthotope | **0.10 us** | **0.10 us** | **0.09 us** | **0.10 us** |
| `concurrent_large_4x` | System | 0.46 us | 0.47 us | 0.45 us | 0.46 us |
| `concurrent_large_4x` | mimalloc | 0.24 us | 0.38 us | 0.25 us | 0.25 us |
| `concurrent_large_4x` | jemalloc | 0.79 us | 0.35 us | 0.32 us | 0.35 us |
| `long_lived_handoff` | Orthotope | 1.99 us | **1.87 us** | 2.01 us | 1.99 us |
| `long_lived_handoff` | System | 2.32 us | 2.29 us | 2.32 us | 2.32 us |
| `long_lived_handoff` | mimalloc | **1.99 us** | 1.97 us | **1.97 us** | **1.97 us** |
| `long_lived_handoff` | jemalloc | 2.06 us | 2.03 us | 2.05 us | 2.05 us |
| `long_lived_handoff_4x` | Orthotope | **1.86 us** | **1.86 us** | 1.88 us | **1.86 us** |
| `long_lived_handoff_4x` | System | 1.87 us | 1.87 us | 1.87 us | 1.87 us |
| `long_lived_handoff_4x` | mimalloc | 1.86 us | 1.87 us | 1.87 us | 1.87 us |
| `long_lived_handoff_4x` | jemalloc | 1.87 us | **1.86 us** | **1.86 us** | **1.86 us** |
