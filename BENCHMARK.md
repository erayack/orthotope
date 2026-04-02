# Benchmark

These are local measurements, useful for relative direction rather than universal claims.

## Overview

- Orthotope was fastest on same-thread hot-path reuse workloads.
- Orthotope also led `embedding_batch` and `mixed_size_churn`.
- The current `large_path` benchmark also favored the comparison allocators.

## Summary

| Workload | Orthotope | System | mimalloc | jemalloc |
| --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | `4.73 ns` | `61.04 ns` | `10.08 ns` | `12.39 ns` |
| `same_thread_small_churn/64` | `4.72 ns` | `41.95 ns` | `7.36 ns` | `11.78 ns` |
| `same_thread_small_churn/65` | `4.74 ns` | `46.52 ns` | `10.34 ns` | `12.10 ns` |
| `same_thread_small_churn/4096` | `4.97 ns` | `13.76 ns` | `13.22 ns` | `13.33 ns` |
| `same_thread_small_churn/70000` | `5.01 ns` | `15.70 ns` | `555.96 ns` | `66.96 ns` |
| `embedding_batch` | `76.94 ns` | `233.61 ns` | `83.11 ns` | `98.47 ns` |
| `mixed_size_churn` | `59.56 ns` | `2.17 us` to `4.01 us` | `273.21 ns` | `196.51 ns` |
| `large_path` | `15.01 us` | `133.86 ns` | `596.31 ns` | `392.90 ns` |
| `long_lived_handoff` | `4.3387 us` | `12.959 us` | `12.447 us` | `12.805 us` |

## Interpretation Notes
- Orthotope's strong same-thread results align with the intended architecture: thread-local caches, class-normalized reuse, and efficient hot-path header refresh.
- The cross-thread benchmark currently measures thread spawning, handoff, and allocator interaction collectively. Orthotope's per-iteration allocator recreation for that workload prevents the arena from exhausting during Criterion warmup, but it also means the benchmark is not a pure central-pool measurement.
- The large-allocation benchmark is not directly comparable to the small-object path. Orthotope's v1 large path does not reuse freed large allocations, so the current harness recreates the allocator state for Orthotope in that scenario as well.
- The mixed_size_churn/system run exhibited high variance in this capture. Consider its range as a noisy result rather than a stable point estimate.
