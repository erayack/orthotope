# Benchmark

These are local measurements, useful for relative direction rather than universal claims.

Method summary:

- 3 warmup samples, 9 measured samples, median reported
- `64`-byte alignment across all allocators
- Orthotope measured through `Allocator` plus one `ThreadCache` per participating thread
- `long_lived_handoff` measures steady-state end-to-end handoff time with persistent worker threads
- `large_path` measures warm reuse of a `20 MiB` request

## Overview

- Orthotope led most same-thread hot-path reuse workloads, ranging from about `1.1x` to `7.1x` faster than the comparison allocators depending on size, while `mimalloc` narrowly led the `64`-byte case.
- Orthotope also led `embedding_batch`, `mixed_size_churn`, and `large_path`, and still edged out the system/jemalloc variants of `long_lived_handoff`.
- The medium-size class ladder change removed the previous `embedding_batch` regression without giving up the existing wins in the other local workloads captured here.

## Summary

| Workload | Orthotope | System | mimalloc | jemalloc |
| --- | ---: | ---: | ---: | ---: |
| `same_thread_small_churn/32` | **9.17 ns** | 63.01 ns | 10.41 ns | 9.27 ns |
| `same_thread_small_churn/64` | 9.11 ns | 48.40 ns | **7.81 ns** | 9.26 ns |
| `same_thread_small_churn/65` | **9.15 ns** | 47.85 ns | 10.71 ns | 9.38 ns |
| `same_thread_small_churn/4096` | **9.33 ns** | 14.04 ns | 14.18 ns | 10.55 ns |
| `same_thread_small_churn/70000` | **11.54 ns** | 17.73 ns | 557.19 ns | 112.05 ns |
| `embedding_batch` | **99.29 ns** | 254.37 ns | 103.32 ns | 127.06 ns |
| `mixed_size_churn` | **98.09 ns** | 403.98 ns | 203.27 ns | 212.12 ns |
| `large_path` | **0.06 us** | 0.14 us | 0.53 us | 0.40 us |
| `long_lived_handoff` | **1.90 us** | 2.27 us | 1.93 us | 2.03 us |

## Interpretation Notes
- Orthotope's same-thread results still align with the intended architecture: thread-local reuse, class-normalized slabs, and in-place header refresh on reuse.
- `embedding_batch` improved because `6,144`-byte requests no longer normalize into the old `262,144`-byte class. They now stay in a dedicated `6 KiB` class whose default refill and local-limit thresholds keep the full eight-object burst local.
- Relative framing for this capture:
  - same-thread hot-path reuse: Orthotope was best on `32`, `65`, `4096`, and `70000`, while `mimalloc` narrowly led `64`
  - `mixed_size_churn`: Orthotope was about `2.07x` faster than `mimalloc` and `2.16x` faster than `jemalloc`
  - `large_path`: Orthotope was about `2.33x` faster than the system allocator, `6.67x` faster than `jemalloc`, and `8.83x` faster than `mimalloc`
  - `embedding_batch`: Orthotope was about `1.04x` faster than `mimalloc`
  - `long_lived_handoff`: Orthotope was about `1.19x` faster than the system allocator, about `1.07x` faster than `jemalloc`, and about `1.02x` faster than `mimalloc`
- The previous `embedding_batch` loss was workload-specific class normalization overhead, not a general allocator weakness. The dedicated `6 KiB` class removes that mismatch without changing the arena, central-pool, or large-object architecture.
- The `large_path` result is intentionally a warm-reuse measurement. It highlights fit-based reuse of freed large spans rather than cold allocation cost.
- `long_lived_handoff` is materially lower than the older documented number because the current harness removes per-iteration thread creation and times only the steady-state handoff path.
- If future comparison work needs cold-path large allocations or thread-spawn-inclusive cross-thread costs, add separate named workloads rather than overloading the current ones.
