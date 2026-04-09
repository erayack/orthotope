# Benchmark

These are local measurements, useful for relative direction rather than universal claims.

Method summary:

- 3 warmup samples, 9 measured samples, median reported
- `64`-byte alignment across all allocators
- Orthotope measured through `Allocator` plus one `ThreadCache` per participating thread
- `long_lived_handoff` measures steady-state end-to-end handoff time with persistent worker threads
- `large_path` measures warm reuse of a `20 MiB` request

## Overview

- Orthotope led 7 of 9 workloads, ranging from about `1.07x` to `49x` faster than the comparison allocators depending on size and competitor.
- Orthotope led `mixed_size_churn`, `large_path`, and `long_lived_handoff`, and all same-thread churn sizes except `/64` where mimalloc was `~8%` faster.
- On `embedding_batch`, mimalloc edged Orthotope by `~3%`; Orthotope still led system and jemalloc there.

## Summary

| Workload | Orthotope | System | mimalloc | jemalloc |
| --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | **8.18 ns** | 61.88 ns | 10.22 ns | 9.14 ns |
| `same_thread_small_churn/64` | 8.14 ns | 48.08 ns | **7.55 ns** | 9.10 ns |
| `same_thread_small_churn/65` | **8.27 ns** | 48.64 ns | 10.31 ns | 9.24 ns |
| `same_thread_small_churn/4096` | **8.51 ns** | 15.04 ns | 12.96 ns | 10.47 ns |
| `same_thread_small_churn/70000` | **9.68 ns** | 16.99 ns | 474.69 ns | 110.52 ns |
| `embedding_batch` | 105.16 ns | 251.96 ns | **101.76 ns** | 125.88 ns |
| `mixed_size_churn` | **95.74 ns** | 406.68 ns | 201.05 ns | 197.64 ns |
| `large_path` | **0.06 us** | 0.13 us | 0.53 us | 0.38 us |
| `long_lived_handoff` | **1.88 us** | 2.25 us | 2.01 us | 2.06 us |

## Interpretation Notes
- Orthotope's same-thread results align with the intended architecture: thread-local reuse, class-normalized slabs, and in-place header refresh on reuse.
- Relative framing for this capture:
  - same-thread hot-path reuse: Orthotope was best on `32`, `65`, `4096`, and `70000`; on `/64`, mimalloc was `~8%` faster (`7.55 ns` vs `8.14 ns`)
  - `same_thread_small_churn/70000`: Orthotope was about `49x` faster than `mimalloc` and `11.4x` faster than `jemalloc`
  - `mixed_size_churn`: Orthotope was about `2.10x` faster than `mimalloc` and `2.06x` faster than `jemalloc`
  - `large_path`: Orthotope was about `2.17x` faster than the system allocator, `6.33x` faster than `jemalloc`, and `8.83x` faster than `mimalloc`
  - `embedding_batch`: mimalloc was about `1.03x` faster than Orthotope; Orthotope still led system (`2.40x`) and jemalloc (`1.20x`)
  - `long_lived_handoff`: Orthotope was about `1.20x` faster than the system allocator, about `1.10x` faster than `jemalloc`, and about `1.07x` faster than `mimalloc`
- The `large_path` result is intentionally a warm-reuse measurement. It highlights fit-based reuse of freed large spans rather than cold allocation cost.

## Raw Runs

Full per-run data behind the medians above. 

| Workload | Allocator | Run 1 | Run 2 | Run 3 | Median |
| --- | --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | Orthotope | **7.78 ns** | **8.18 ns** | **8.18 ns** | **8.18 ns** |
| `same_thread_small_churn/32` | System | 61.15 ns | 61.88 ns | 63.88 ns | 61.88 ns |
| `same_thread_small_churn/32` | mimalloc | 9.98 ns | 10.22 ns | 10.37 ns | 10.22 ns |
| `same_thread_small_churn/32` | jemalloc | 8.54 ns | 9.14 ns | 9.18 ns | 9.14 ns |
| `same_thread_small_churn/64` | Orthotope | **7.86 ns** | 8.14 ns | 8.26 ns | 8.14 ns |
| `same_thread_small_churn/64` | System | 45.32 ns | 48.51 ns | 48.08 ns | 48.08 ns |
| `same_thread_small_churn/64` | mimalloc | 7.36 ns | **7.55 ns** | **7.61 ns** | **7.55 ns** |
| `same_thread_small_churn/64` | jemalloc | 8.62 ns | 9.22 ns | 9.10 ns | 9.10 ns |
| `same_thread_small_churn/65` | Orthotope | **8.09 ns** | **8.27 ns** | **8.52 ns** | **8.27 ns** |
| `same_thread_small_churn/65` | System | 48.64 ns | 47.39 ns | 50.18 ns | 48.64 ns |
| `same_thread_small_churn/65` | mimalloc | 10.07 ns | 10.31 ns | 10.68 ns | 10.31 ns |
| `same_thread_small_churn/65` | jemalloc | 8.89 ns | 9.24 ns | 9.30 ns | 9.24 ns |
| `same_thread_small_churn/4096` | Orthotope | **8.29 ns** | **8.56 ns** | **8.51 ns** | **8.51 ns** |
| `same_thread_small_churn/4096` | System | 14.71 ns | 15.04 ns | 15.27 ns | 15.04 ns |
| `same_thread_small_churn/4096` | mimalloc | 12.90 ns | 12.96 ns | 13.43 ns | 12.96 ns |
| `same_thread_small_churn/4096` | jemalloc | 10.01 ns | 10.47 ns | 10.66 ns | 10.47 ns |
| `same_thread_small_churn/70000` | Orthotope | **9.07 ns** | **9.68 ns** | **9.72 ns** | **9.68 ns** |
| `same_thread_small_churn/70000` | System | 16.40 ns | 16.99 ns | 17.09 ns | 16.99 ns |
| `same_thread_small_churn/70000` | mimalloc | 451.62 ns | 512.61 ns | 474.69 ns | 474.69 ns |
| `same_thread_small_churn/70000` | jemalloc | 108.19 ns | 110.52 ns | 111.34 ns | 110.52 ns |
| `embedding_batch` | Orthotope | 105.16 ns | **103.79 ns** | 107.33 ns | 105.16 ns |
| `embedding_batch` | System | 243.69 ns | 256.90 ns | 251.96 ns | 251.96 ns |
| `embedding_batch` | mimalloc | **98.86 ns** | 102.34 ns | **101.76 ns** | **101.76 ns** |
| `embedding_batch` | jemalloc | 122.10 ns | 126.16 ns | 125.88 ns | 125.88 ns |
| `mixed_size_churn` | Orthotope | **93.20 ns** | **95.75 ns** | **95.74 ns** | **95.74 ns** |
| `mixed_size_churn` | System | 390.83 ns | 406.68 ns | 413.35 ns | 406.68 ns |
| `mixed_size_churn` | mimalloc | 193.54 ns | 201.05 ns | 203.13 ns | 201.05 ns |
| `mixed_size_churn` | jemalloc | 191.00 ns | 197.64 ns | 212.01 ns | 197.64 ns |
| `large_path` | Orthotope | **0.06 us** | **0.06 us** | **0.06 us** | **0.06 us** |
| `large_path` | System | 0.13 us | 0.13 us | 0.13 us | 0.13 us |
| `large_path` | mimalloc | 0.51 us | 0.54 us | 0.53 us | 0.53 us |
| `large_path` | jemalloc | 0.36 us | 0.39 us | 0.38 us | 0.38 us |
| `long_lived_handoff` | Orthotope | **1.87 us** | **1.88 us** | **2.00 us** | **1.88 us** |
| `long_lived_handoff` | System | 2.25 us | 2.20 us | 2.31 us | 2.25 us |
| `long_lived_handoff` | mimalloc | 2.01 us | 2.02 us | 1.96 us | 2.01 us |
| `long_lived_handoff` | jemalloc | 2.09 us | 2.06 us | 2.01 us | 2.06 us |
