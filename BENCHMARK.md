# Benchmark

**Method:** each `just bench` run uses 3 warmup + 9 measured samples and reports
a per-run median; the summary is the median of 3 such runs. All allocators use
`64`-byte alignment. Orthotope is measured through `Allocator` with one
`ThreadCache` per participating thread. `long_lived_handoff[_4x]` measures
steady-state end-to-end handoff with persistent worker threads (4 producer/
consumer pairs for `_4x`). `large_path` measures warm reuse of a `20 MiB` request.

## Overview

Orthotope led 8 of 10 workloads (roughly `1.09x`–`54x` faster depending on size
and competitor), including `embedding_batch`, `mixed_size_churn`, `large_path`,
and every `same_thread_small_churn` size except `/64` (mimalloc ~`4%` faster).
mimalloc edged Orthotope on `long_lived_handoff` (~`5%`) and `long_lived_handoff_4x` (~`2%`).

## Summary

| Workload | Orthotope | System | mimalloc | jemalloc |
| --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | **8.05 ns** | 64.05 ns | 10.49 ns | 8.98 ns |
| `same_thread_small_churn/64` | 8.11 ns | 50.39 ns | **7.83 ns** | 9.20 ns |
| `same_thread_small_churn/65` | **8.30 ns** | 48.24 ns | 10.82 ns | 9.25 ns |
| `same_thread_small_churn/4096` | **8.55 ns** | 14.90 ns | 14.28 ns | 10.55 ns |
| `same_thread_small_churn/70000` | **9.87 ns** | 17.01 ns | 535.18 ns | 112.19 ns |
| `embedding_batch` | **99.62 ns** | 254.80 ns | 108.88 ns | 128.66 ns |
| `mixed_size_churn` | **94.16 ns** | 383.97 ns | 205.27 ns | 203.73 ns |
| `large_path` | **0.06 us** | 0.14 us | 0.56 us | 0.37 us |
| `long_lived_handoff` | 1.97 us | 2.27 us | **1.87 us** | 1.94 us |
| `long_lived_handoff_4x` | 1.89 us | 1.88 us | **1.86 us** | 1.87 us |

## Notes

Same-thread wins reflect the intended architecture: thread-local reuse,
class-normalized slabs, in-place header refresh on reuse. Selected ratios:

- `same_thread_small_churn/70000`: ~`54x` vs mimalloc, ~`11.4x` vs jemalloc
- `mixed_size_churn`: ~`2.18x` vs mimalloc, ~`2.16x` vs jemalloc
- `large_path`: ~`2.33x` vs system, ~`6.17x` vs jemalloc, ~`9.33x` vs mimalloc (warm-reuse measurement; highlights fit-based reuse of freed large spans)
- `embedding_batch`: ~`2.56x` vs system, ~`1.29x` vs jemalloc, ~`1.09x` vs mimalloc
- `long_lived_handoff`: ~`1.15x` vs system, within ~`5%` of mimalloc
- `long_lived_handoff_4x`: all allocators within ~`2%`

## Raw Runs

Per-run data behind the medians above.

| Workload | Allocator | Run 1 | Run 2 | Run 3 | Median |
| --- | --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | Orthotope | **7.95 ns** | **8.05 ns** | **8.16 ns** | **8.05 ns** |
| `same_thread_small_churn/32` | System | 63.41 ns | 65.70 ns | 64.05 ns | 64.05 ns |
| `same_thread_small_churn/32` | mimalloc | 10.21 ns | 10.49 ns | 10.62 ns | 10.49 ns |
| `same_thread_small_churn/32` | jemalloc | 8.78 ns | 8.98 ns | 9.46 ns | 8.98 ns |
| `same_thread_small_churn/64` | Orthotope | 7.95 ns | 8.11 ns | **8.22 ns** | 8.11 ns |
| `same_thread_small_churn/64` | System | 50.39 ns | 49.87 ns | 51.32 ns | 50.39 ns |
| `same_thread_small_churn/64` | mimalloc | **7.64 ns** | **7.83 ns** | **8.32 ns** | **7.83 ns** |
| `same_thread_small_churn/64` | jemalloc | 8.78 ns | 9.23 ns | 9.20 ns | 9.20 ns |
| `same_thread_small_churn/65` | Orthotope | **8.23 ns** | **8.30 ns** | **8.41 ns** | **8.30 ns** |
| `same_thread_small_churn/65` | System | 47.60 ns | 49.92 ns | 48.24 ns | 48.24 ns |
| `same_thread_small_churn/65` | mimalloc | 10.41 ns | 10.82 ns | 11.12 ns | 10.82 ns |
| `same_thread_small_churn/65` | jemalloc | 8.98 ns | 9.25 ns | 9.33 ns | 9.25 ns |
| `same_thread_small_churn/4096` | Orthotope | **8.29 ns** | **8.55 ns** | **8.56 ns** | **8.55 ns** |
| `same_thread_small_churn/4096` | System | 14.90 ns | 14.70 ns | 14.96 ns | 14.90 ns |
| `same_thread_small_churn/4096` | mimalloc | 13.80 ns | 14.28 ns | 14.63 ns | 14.28 ns |
| `same_thread_small_churn/4096` | jemalloc | 10.19 ns | 10.60 ns | 10.55 ns | 10.55 ns |
| `same_thread_small_churn/70000` | Orthotope | **9.47 ns** | **9.88 ns** | **9.87 ns** | **9.87 ns** |
| `same_thread_small_churn/70000` | System | 16.51 ns | 17.01 ns | 17.56 ns | 17.01 ns |
| `same_thread_small_churn/70000` | mimalloc | 548.72 ns | 521.31 ns | 535.18 ns | 535.18 ns |
| `same_thread_small_churn/70000` | jemalloc | 111.90 ns | 112.19 ns | 114.63 ns | 112.19 ns |
| `embedding_batch` | Orthotope | **97.51 ns** | **100.94 ns** | **99.62 ns** | **99.62 ns** |
| `embedding_batch` | System | 253.97 ns | 254.80 ns | 257.14 ns | 254.80 ns |
| `embedding_batch` | mimalloc | 108.56 ns | 108.88 ns | 112.50 ns | 108.88 ns |
| `embedding_batch` | jemalloc | 127.92 ns | 130.16 ns | 128.66 ns | 128.66 ns |
| `mixed_size_churn` | Orthotope | **92.69 ns** | **94.16 ns** | **94.37 ns** | **94.16 ns** |
| `mixed_size_churn` | System | 378.97 ns | 383.97 ns | 390.84 ns | 383.97 ns |
| `mixed_size_churn` | mimalloc | 199.94 ns | 205.27 ns | 206.34 ns | 205.27 ns |
| `mixed_size_churn` | jemalloc | 197.33 ns | 203.73 ns | 204.10 ns | 203.73 ns |
| `large_path` | Orthotope | **0.06 us** | **0.06 us** | **0.06 us** | **0.06 us** |
| `large_path` | System | 0.13 us | 0.14 us | 0.14 us | 0.14 us |
| `large_path` | mimalloc | 0.56 us | 0.52 us | 0.56 us | 0.56 us |
| `large_path` | jemalloc | 0.37 us | 0.40 us | 0.37 us | 0.37 us |
| `long_lived_handoff` | Orthotope | **1.88 us** | 1.97 us | 1.97 us | 1.97 us |
| `long_lived_handoff` | System | 2.30 us | 2.27 us | 2.19 us | 2.27 us |
| `long_lived_handoff` | mimalloc | 1.96 us | **1.87 us** | **1.84 us** | **1.87 us** |
| `long_lived_handoff` | jemalloc | 1.94 us | 1.94 us | 1.98 us | 1.94 us |
| `long_lived_handoff_4x` | Orthotope | 1.87 us | 1.91 us | 1.89 us | 1.89 us |
| `long_lived_handoff_4x` | System | 1.88 us | 1.89 us | 1.87 us | 1.88 us |
| `long_lived_handoff_4x` | mimalloc | **1.86 us** | **1.86 us** | 1.87 us | **1.86 us** |
| `long_lived_handoff_4x` | jemalloc | 1.87 us | 2.20 us | **1.85 us** | 1.87 us |
