use core::alloc::{GlobalAlloc, Layout};
use core::hint::black_box;
use core::mem::size_of;
use core::ptr::NonNull;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::OnceLock;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use jemallocator::Jemalloc;
use mimalloc::MiMalloc;
use orthotope::{Allocator, AllocatorConfig, ThreadCache};

pub const ALIGNMENT: usize = 64;
pub const EMBEDDING_BATCH_SIZE: usize = 8;
pub const EMBEDDING_BYTES: usize = 1_536 * size_of::<f32>();
pub const LARGE_REQUEST_BYTES: usize = 20 * 1024 * 1024;
pub const SAME_THREAD_SIZES: [usize; 5] = [32, 64, 65, 4_096, 70_000];
pub const MIXED_SIZES: [usize; 8] = [32, 64, 65, 128, 512, 4_096, 16_384, 70_000];
pub const LONG_LIVED_HANDOFF_ITERATIONS: usize = 80_000;
pub const LONG_LIVED_HANDOFF_4X_PAIRS: usize = 4;
pub const CONCURRENT_LARGE_4X_THREADS: usize = 4;
pub const CONCURRENT_LARGE_ITERATIONS: usize = 1_200;
pub const WARMUP_SAMPLES: usize = 3;
pub const MEASURE_SAMPLES: usize = 9;
pub const DEFAULT_FLAMEGRAPH_REPETITIONS: usize = 400;
pub const DEFAULT_FLAMEGRAPH_ALLOCATOR: AllocatorKind = AllocatorKind::Orthotope;
pub const DEFAULT_FLAMEGRAPH_WORKLOAD: &str = "mixed_size_churn";

static SYSTEM_ALLOCATOR: std::alloc::System = std::alloc::System;
static MIMALLOC_ALLOCATOR: MiMalloc = MiMalloc;
static JEMALLOC_ALLOCATOR: Jemalloc = Jemalloc;
static WORKLOADS: OnceLock<Vec<Workload>> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AllocatorKind {
    Orthotope,
    System,
    MiMalloc,
    Jemalloc,
}

impl AllocatorKind {
    pub const ALL: [Self; 4] = [
        Self::Orthotope,
        Self::System,
        Self::MiMalloc,
        Self::Jemalloc,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Orthotope => "Orthotope",
            Self::System => "System",
            Self::MiMalloc => "mimalloc",
            Self::Jemalloc => "jemalloc",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Orthotope" | "orthotope" => Some(Self::Orthotope),
            "System" | "system" => Some(Self::System),
            "mimalloc" | "MiMalloc" => Some(Self::MiMalloc),
            "jemalloc" | "Jemalloc" => Some(Self::Jemalloc),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum TimeUnit {
    Nanoseconds,
    Microseconds,
}

impl TimeUnit {
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Nanoseconds => "ns",
            Self::Microseconds => "us",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Workload {
    pub name: &'static str,
    pub operations: usize,
    pub unit: TimeUnit,
    pub kind: WorkloadKind,
}

pub struct ResultRow {
    pub workload: &'static str,
    pub allocator: &'static str,
    pub value: f64,
    pub unit: TimeUnit,
}

#[derive(Debug)]
pub enum BenchError {
    Alloc(String),
    Thread(String),
    Io(std::io::Error),
    Config(String),
}

#[derive(Clone, Copy)]
pub enum WorkloadKind {
    SameThreadSmallChurn(usize),
    EmbeddingBatch,
    MixedSizeChurn,
    LargePath,
    ConcurrentLarge4x,
    LongLivedHandoff,
    LongLivedHandoff4x,
}

impl Display for BenchError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alloc(message) | Self::Thread(message) | Self::Config(message) => {
                f.write_str(message)
            }
            Self::Io(error) => Display::fmt(error, f),
        }
    }
}

impl Error for BenchError {}

impl From<std::io::Error> for BenchError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

trait AllocationApi {
    fn allocate(&mut self, size: usize) -> Result<NonNull<u8>, BenchError>;

    unsafe fn deallocate(&mut self, ptr: NonNull<u8>, size: usize) -> Result<(), BenchError>;
}

struct RawBackend {
    allocator: &'static dyn GlobalAlloc,
}

impl RawBackend {
    const fn new(allocator: &'static dyn GlobalAlloc) -> Self {
        Self { allocator }
    }
}

impl AllocationApi for RawBackend {
    fn allocate(&mut self, size: usize) -> Result<NonNull<u8>, BenchError> {
        let layout = layout_for(size)?;
        // SAFETY: the layout is validated above and delegated to the selected
        // global allocator implementation.
        let ptr = unsafe { self.allocator.alloc(layout) };
        NonNull::new(ptr)
            .ok_or_else(|| BenchError::Alloc(format!("allocation failed for {size} bytes")))
    }

    unsafe fn deallocate(&mut self, ptr: NonNull<u8>, size: usize) -> Result<(), BenchError> {
        let layout = layout_for(size)?;
        // SAFETY: callers pair this with a matching successful allocation from
        // the same backend and request size.
        unsafe {
            self.allocator.dealloc(ptr.as_ptr(), layout);
        }
        Ok(())
    }
}

struct OrthotopeBackend {
    allocator: Allocator,
    cache: ThreadCache,
}

impl OrthotopeBackend {
    fn new() -> Result<Self, BenchError> {
        let config = AllocatorConfig::default();
        let allocator = Allocator::new(config)
            .map_err(|error| BenchError::Alloc(format!("orthotope init failed: {error}")))?;
        let cache = ThreadCache::new(config);
        Ok(Self { allocator, cache })
    }
}

impl AllocationApi for OrthotopeBackend {
    fn allocate(&mut self, size: usize) -> Result<NonNull<u8>, BenchError> {
        self.allocator
            .allocate_with_cache(&mut self.cache, size)
            .map_err(|error| BenchError::Alloc(format!("orthotope allocation failed: {error}")))
    }

    unsafe fn deallocate(&mut self, ptr: NonNull<u8>, size: usize) -> Result<(), BenchError> {
        // SAFETY: callers provide a live pointer returned by the paired
        // allocation route plus its exact requested size.
        unsafe {
            self.allocator
                .deallocate_with_size_checked(&mut self.cache, ptr, size)
        }
        .map_err(|error| BenchError::Alloc(format!("orthotope deallocation failed: {error}")))
    }
}

pub fn workloads() -> Vec<Workload> {
    workload_table().to_vec()
}

pub fn workload_names() -> Vec<&'static str> {
    workload_table()
        .iter()
        .map(|workload| workload.name)
        .collect()
}

pub fn workload_by_name(name: &str) -> Option<Workload> {
    workload_table()
        .iter()
        .copied()
        .find(|workload| workload.name == name)
}

pub fn measure(workload: Workload, allocator: AllocatorKind) -> Result<Duration, BenchError> {
    for _ in 0..WARMUP_SAMPLES {
        let _ = run_workload(workload.kind, allocator)?;
    }

    let mut samples = Vec::with_capacity(MEASURE_SAMPLES);
    for _ in 0..MEASURE_SAMPLES {
        samples.push(run_workload(workload.kind, allocator)?);
    }
    samples.sort_unstable();
    Ok(samples[samples.len() / 2])
}

pub fn run_repeated_workload(
    workload: Workload,
    allocator: AllocatorKind,
    repetitions: usize,
) -> Result<Duration, BenchError> {
    let start = Instant::now();
    for _ in 0..repetitions {
        black_box(run_workload(workload.kind, allocator)?);
    }
    Ok(start.elapsed())
}

pub fn per_operation(duration: Duration, operations: usize, unit: TimeUnit) -> f64 {
    let divisor = operations as f64;
    match unit {
        TimeUnit::Nanoseconds => duration.as_secs_f64() * 1_000_000_000.0 / divisor,
        TimeUnit::Microseconds => duration.as_secs_f64() * 1_000_000.0 / divisor,
    }
}

fn workload_table() -> &'static [Workload] {
    WORKLOADS.get_or_init(|| {
        let mut workloads = Vec::new();

        for size in SAME_THREAD_SIZES {
            workloads.push(Workload {
                name: Box::leak(format!("same_thread_small_churn/{size}").into_boxed_str()),
                operations: same_thread_iterations(size),
                unit: TimeUnit::Nanoseconds,
                kind: WorkloadKind::SameThreadSmallChurn(size),
            });
        }

        workloads.push(Workload {
            name: "embedding_batch",
            operations: 200_000,
            unit: TimeUnit::Nanoseconds,
            kind: WorkloadKind::EmbeddingBatch,
        });
        workloads.push(Workload {
            name: "mixed_size_churn",
            operations: 120_000,
            unit: TimeUnit::Nanoseconds,
            kind: WorkloadKind::MixedSizeChurn,
        });
        workloads.push(Workload {
            name: "large_path",
            operations: 1_200,
            unit: TimeUnit::Microseconds,
            kind: WorkloadKind::LargePath,
        });
        workloads.push(Workload {
            name: "concurrent_large_4x",
            operations: CONCURRENT_LARGE_ITERATIONS,
            unit: TimeUnit::Microseconds,
            kind: WorkloadKind::ConcurrentLarge4x,
        });
        workloads.push(Workload {
            name: "long_lived_handoff",
            operations: LONG_LIVED_HANDOFF_ITERATIONS,
            unit: TimeUnit::Microseconds,
            kind: WorkloadKind::LongLivedHandoff,
        });
        workloads.push(Workload {
            name: "long_lived_handoff_4x",
            operations: LONG_LIVED_HANDOFF_ITERATIONS,
            unit: TimeUnit::Microseconds,
            kind: WorkloadKind::LongLivedHandoff4x,
        });

        workloads
    })
}

const fn same_thread_iterations(size: usize) -> usize {
    match size {
        0..=64 => 6_000_000,
        65..=4_096 => 3_000_000,
        _ => 600_000,
    }
}

fn run_workload(kind: WorkloadKind, allocator: AllocatorKind) -> Result<Duration, BenchError> {
    match kind {
        WorkloadKind::SameThreadSmallChurn(size) => run_same_thread_small_churn(allocator, size),
        WorkloadKind::EmbeddingBatch => run_embedding_batch(allocator),
        WorkloadKind::MixedSizeChurn => run_mixed_size_churn(allocator),
        WorkloadKind::LargePath => run_large_path(allocator),
        WorkloadKind::ConcurrentLarge4x => run_concurrent_large_4x(allocator),
        WorkloadKind::LongLivedHandoff => run_long_lived_handoff(allocator),
        WorkloadKind::LongLivedHandoff4x => run_long_lived_handoff_4x(allocator),
    }
}

fn run_same_thread_small_churn(
    allocator: AllocatorKind,
    size: usize,
) -> Result<Duration, BenchError> {
    let iterations = same_thread_iterations(size);
    with_backend(allocator, |backend| {
        let start = Instant::now();
        for _ in 0..iterations {
            let ptr = backend.allocate(size)?;
            black_box(ptr);
            // SAFETY: `ptr` came from the paired backend allocation above and
            // `size` matches the request.
            unsafe {
                backend.deallocate(ptr, size)?;
            }
        }
        Ok(start.elapsed())
    })
}

fn run_embedding_batch(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    const ITERATIONS: usize = 200_000;
    with_backend(allocator, |backend| {
        let mut batch = Vec::with_capacity(EMBEDDING_BATCH_SIZE);
        let start = Instant::now();

        for _ in 0..ITERATIONS {
            batch.clear();
            for _ in 0..EMBEDDING_BATCH_SIZE {
                batch.push(backend.allocate(EMBEDDING_BYTES)?);
            }
            black_box(&batch);

            while let Some(ptr) = batch.pop() {
                // SAFETY: `ptr` was allocated in the batch above using the same
                // backend and size.
                unsafe {
                    backend.deallocate(ptr, EMBEDDING_BYTES)?;
                }
            }
        }

        Ok(start.elapsed())
    })
}

fn run_mixed_size_churn(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    const ITERATIONS: usize = 120_000;
    with_backend(allocator, |backend| {
        let mut live = Vec::with_capacity(MIXED_SIZES.len());
        let start = Instant::now();

        for _ in 0..ITERATIONS {
            live.clear();
            for size in MIXED_SIZES {
                live.push((size, backend.allocate(size)?));
            }
            black_box(&live);

            while let Some((size, ptr)) = live.pop() {
                // SAFETY: each pointer is deallocated with the exact request
                // size recorded alongside it.
                unsafe {
                    backend.deallocate(ptr, size)?;
                }
            }
        }

        Ok(start.elapsed())
    })
}

fn run_large_path(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    const ITERATIONS: usize = 1_200;
    with_backend(allocator, |backend| {
        let start = Instant::now();

        for _ in 0..ITERATIONS {
            let ptr = backend.allocate(LARGE_REQUEST_BYTES)?;
            black_box(ptr);
            // SAFETY: `ptr` came from the paired backend allocation above and
            // `LARGE_REQUEST_BYTES` matches the request.
            unsafe {
                backend.deallocate(ptr, LARGE_REQUEST_BYTES)?;
            }
        }

        Ok(start.elapsed())
    })
}

fn run_concurrent_large_4x(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    match allocator {
        AllocatorKind::Orthotope => run_concurrent_large_orthotope(),
        AllocatorKind::System | AllocatorKind::MiMalloc | AllocatorKind::Jemalloc => {
            run_concurrent_large_raw(allocator)
        }
    }
}

fn run_concurrent_large_raw(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    let iterations_per_thread = CONCURRENT_LARGE_ITERATIONS / CONCURRENT_LARGE_4X_THREADS;
    let barrier = Arc::new(Barrier::new(CONCURRENT_LARGE_4X_THREADS + 1));
    let mut handles = Vec::with_capacity(CONCURRENT_LARGE_4X_THREADS);

    for _ in 0..CONCURRENT_LARGE_4X_THREADS {
        let worker_barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), BenchError> {
            let mut backend = raw_backend(allocator);
            worker_barrier.wait();
            for _ in 0..iterations_per_thread {
                let ptr = backend.allocate(LARGE_REQUEST_BYTES)?;
                black_box(ptr);
                // SAFETY: `ptr` came from the paired backend allocation above.
                unsafe {
                    backend.deallocate(ptr, LARGE_REQUEST_BYTES)?;
                }
            }
            Ok(())
        }));
    }

    barrier.wait();
    let start = Instant::now();

    for handle in handles {
        handle
            .join()
            .map_err(|_| BenchError::Thread("concurrent large thread panicked".to_string()))??;
    }

    Ok(start.elapsed())
}

fn run_concurrent_large_orthotope() -> Result<Duration, BenchError> {
    let allocator = Arc::new(
        Allocator::new(AllocatorConfig::default())
            .map_err(|error| BenchError::Alloc(format!("orthotope init failed: {error}")))?,
    );
    let iterations_per_thread = CONCURRENT_LARGE_ITERATIONS / CONCURRENT_LARGE_4X_THREADS;
    let barrier = Arc::new(Barrier::new(CONCURRENT_LARGE_4X_THREADS + 1));
    let mut handles = Vec::with_capacity(CONCURRENT_LARGE_4X_THREADS);

    for _ in 0..CONCURRENT_LARGE_4X_THREADS {
        let worker_allocator = Arc::clone(&allocator);
        let worker_barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), BenchError> {
            let mut cache = ThreadCache::new(*worker_allocator.config());
            worker_barrier.wait();
            for _ in 0..iterations_per_thread {
                let ptr = worker_allocator
                    .allocate_with_cache(&mut cache, LARGE_REQUEST_BYTES)
                    .map_err(|error| {
                        BenchError::Alloc(format!("orthotope large allocation failed: {error}"))
                    })?;
                black_box(ptr);
                // SAFETY: `ptr` came from the allocator call above and the size
                // matches the request.
                unsafe {
                    worker_allocator
                        .deallocate_with_size_checked(&mut cache, ptr, LARGE_REQUEST_BYTES)
                        .map_err(|error| {
                            BenchError::Alloc(format!("orthotope large free failed: {error}"))
                        })?;
                }
            }
            Ok(())
        }));
    }

    barrier.wait();
    let start = Instant::now();

    for handle in handles {
        handle
            .join()
            .map_err(|_| BenchError::Thread("concurrent large thread panicked".to_string()))??;
    }

    Ok(start.elapsed())
}

fn run_long_lived_handoff(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    run_long_lived_handoff_with_pairs(allocator, 1, LONG_LIVED_HANDOFF_ITERATIONS)
}

fn run_long_lived_handoff_4x(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    run_long_lived_handoff_with_pairs(
        allocator,
        LONG_LIVED_HANDOFF_4X_PAIRS,
        LONG_LIVED_HANDOFF_ITERATIONS / LONG_LIVED_HANDOFF_4X_PAIRS,
    )
}

fn run_long_lived_handoff_with_pairs(
    allocator: AllocatorKind,
    pair_count: usize,
    iterations_per_pair: usize,
) -> Result<Duration, BenchError> {
    match allocator {
        AllocatorKind::Orthotope => {
            run_long_lived_handoff_orthotope(pair_count, iterations_per_pair)
        }
        AllocatorKind::System | AllocatorKind::MiMalloc | AllocatorKind::Jemalloc => {
            run_long_lived_handoff_raw(allocator, pair_count, iterations_per_pair)
        }
    }
}

fn run_long_lived_handoff_raw(
    allocator: AllocatorKind,
    pair_count: usize,
    iterations_per_pair: usize,
) -> Result<Duration, BenchError> {
    let barrier = Arc::new(Barrier::new(pair_count * 2 + 1));
    let mut handles = Vec::with_capacity(pair_count * 2);

    for _ in 0..pair_count {
        let (sender, receiver) = sync_channel::<usize>(0);

        let producer_barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), BenchError> {
            let mut backend = raw_backend(allocator);
            producer_barrier.wait();
            for _ in 0..iterations_per_pair {
                let ptr = backend.allocate(256)?;
                sender
                    .send(ptr.as_ptr() as usize)
                    .map_err(|error| BenchError::Thread(format!("send failed: {error}")))?;
            }
            Ok(())
        }));

        let consumer_barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), BenchError> {
            let mut backend = raw_backend(allocator);
            consumer_barrier.wait();
            for _ in 0..iterations_per_pair {
                let address = receiver
                    .recv()
                    .map_err(|error| BenchError::Thread(format!("receive failed: {error}")))?;
                // SAFETY: producer sends only addresses from successful `NonNull`
                // allocations on the paired channel.
                let ptr = unsafe { NonNull::new_unchecked(address as *mut u8) };
                // SAFETY: producer/consumer pair uses the same backend and fixed
                // allocation size for the transferred pointer.
                unsafe {
                    backend.deallocate(ptr, 256)?;
                }
            }
            Ok(())
        }));
    }

    barrier.wait();
    let start = Instant::now();

    for handle in handles {
        handle
            .join()
            .map_err(|_| BenchError::Thread("handoff thread panicked".to_string()))??;
    }

    Ok(start.elapsed())
}

fn run_long_lived_handoff_orthotope(
    pair_count: usize,
    iterations_per_pair: usize,
) -> Result<Duration, BenchError> {
    let allocator = Arc::new(
        Allocator::new(AllocatorConfig::default())
            .map_err(|error| BenchError::Alloc(format!("orthotope init failed: {error}")))?,
    );
    let barrier = Arc::new(Barrier::new(pair_count * 2 + 1));
    let mut handles = Vec::with_capacity(pair_count * 2);

    for _ in 0..pair_count {
        let (sender, receiver) = sync_channel::<usize>(0);

        let producer_allocator = Arc::clone(&allocator);
        let producer_barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), BenchError> {
            let mut cache = ThreadCache::new(*producer_allocator.config());
            producer_barrier.wait();
            for _ in 0..iterations_per_pair {
                let ptr = producer_allocator
                    .allocate_with_cache(&mut cache, 256)
                    .map_err(|error| {
                        BenchError::Alloc(format!("orthotope allocation failed: {error}"))
                    })?;
                sender
                    .send(ptr.as_ptr() as usize)
                    .map_err(|error| BenchError::Thread(format!("send failed: {error}")))?;
            }
            Ok(())
        }));

        let consumer_allocator = Arc::clone(&allocator);
        let consumer_barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), BenchError> {
            let mut cache = ThreadCache::new(*consumer_allocator.config());
            consumer_barrier.wait();
            for _ in 0..iterations_per_pair {
                let address = receiver
                    .recv()
                    .map_err(|error| BenchError::Thread(format!("receive failed: {error}")))?;
                // SAFETY: producer sends only addresses from successful `NonNull`
                // allocations on the paired channel.
                let ptr = unsafe { NonNull::new_unchecked(address as *mut u8) };
                // SAFETY: producer/consumer pair uses the same allocator and
                // fixed allocation size for the transferred pointer.
                unsafe {
                    consumer_allocator
                        .deallocate_with_size_checked(&mut cache, ptr, 256)
                        .map_err(|error| {
                            BenchError::Alloc(format!("orthotope deallocation failed: {error}"))
                        })?;
                }
            }
            Ok(())
        }));
    }

    barrier.wait();
    let start = Instant::now();

    for handle in handles {
        handle
            .join()
            .map_err(|_| BenchError::Thread("handoff thread panicked".to_string()))??;
    }

    Ok(start.elapsed())
}

fn with_backend<T>(
    allocator: AllocatorKind,
    f: impl FnOnce(&mut dyn AllocationApi) -> Result<T, BenchError>,
) -> Result<T, BenchError> {
    match allocator {
        AllocatorKind::Orthotope => {
            let mut backend = OrthotopeBackend::new()?;
            f(&mut backend)
        }
        AllocatorKind::System | AllocatorKind::MiMalloc | AllocatorKind::Jemalloc => {
            let mut backend = raw_backend(allocator);
            f(&mut backend)
        }
    }
}

fn layout_for(size: usize) -> Result<Layout, BenchError> {
    Layout::from_size_align(size, ALIGNMENT).map_err(|error| {
        BenchError::Alloc(format!(
            "invalid layout for size {size} and alignment {ALIGNMENT}: {error}"
        ))
    })
}

fn raw_backend(allocator: AllocatorKind) -> RawBackend {
    match allocator {
        AllocatorKind::Orthotope => unreachable!("orthotope is not a raw global allocator backend"),
        AllocatorKind::System => RawBackend::new(&SYSTEM_ALLOCATOR),
        AllocatorKind::MiMalloc => RawBackend::new(&MIMALLOC_ALLOCATOR),
        AllocatorKind::Jemalloc => RawBackend::new(&JEMALLOC_ALLOCATOR),
    }
}

#[cfg(test)]
mod tests {
    use super::{TimeUnit, per_operation};
    use std::time::Duration;

    #[test]
    fn per_operation_handles_counts_larger_than_u32() {
        let operations = usize::try_from(u64::from(u32::MAX) + 1)
            .unwrap_or_else(|_| panic!("test requires usize wider than u32"));
        let value = per_operation(Duration::from_secs(1), operations, TimeUnit::Nanoseconds);

        assert!(value.is_finite());
        assert!(value > 0.0);
    }
}
