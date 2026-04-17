use core::alloc::{GlobalAlloc, Layout};
use core::hint::black_box;
use core::mem::size_of;
use core::ptr::NonNull;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io::{stdout, Write};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use jemallocator::Jemalloc;
use mimalloc::MiMalloc;
use orthotope::{Allocator, AllocatorConfig, ThreadCache};

const ALIGNMENT: usize = 64;
const EMBEDDING_BATCH_SIZE: usize = 8;
const EMBEDDING_BYTES: usize = 1_536 * size_of::<f32>();
const LARGE_REQUEST_BYTES: usize = 20 * 1024 * 1024;
const SAME_THREAD_SIZES: [usize; 5] = [32, 64, 65, 4_096, 70_000];
const MIXED_SIZES: [usize; 8] = [32, 64, 65, 128, 512, 4_096, 16_384, 70_000];
const WARMUP_SAMPLES: usize = 3;
const MEASURE_SAMPLES: usize = 9;

static SYSTEM_ALLOCATOR: std::alloc::System = std::alloc::System;
static MIMALLOC_ALLOCATOR: MiMalloc = MiMalloc;
static JEMALLOC_ALLOCATOR: Jemalloc = Jemalloc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AllocatorKind {
    Orthotope,
    System,
    MiMalloc,
    Jemalloc,
}

impl AllocatorKind {
    const ALL: [Self; 4] = [
        Self::Orthotope,
        Self::System,
        Self::MiMalloc,
        Self::Jemalloc,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Orthotope => "Orthotope",
            Self::System => "System",
            Self::MiMalloc => "mimalloc",
            Self::Jemalloc => "jemalloc",
        }
    }
}

#[derive(Clone, Copy)]
enum TimeUnit {
    Nanoseconds,
    Microseconds,
}

impl TimeUnit {
    const fn suffix(self) -> &'static str {
        match self {
            Self::Nanoseconds => "ns",
            Self::Microseconds => "us",
        }
    }
}

#[derive(Clone, Copy)]
struct Workload {
    name: &'static str,
    operations: usize,
    unit: TimeUnit,
    kind: WorkloadKind,
}

struct ResultRow {
    workload: &'static str,
    allocator: &'static str,
    value: f64,
    unit: TimeUnit,
}

#[derive(Debug)]
enum BenchError {
    Alloc(String),
    Thread(String),
    Io(std::io::Error),
}

#[derive(Clone, Copy)]
enum WorkloadKind {
    SameThreadSmallChurn(usize),
    EmbeddingBatch,
    MixedSizeChurn,
    LargePath,
    LongLivedHandoff,
}

impl Display for BenchError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alloc(message) | Self::Thread(message) => f.write_str(message),
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
        let ptr = unsafe { self.allocator.alloc(layout) };
        NonNull::new(ptr)
            .ok_or_else(|| BenchError::Alloc(format!("allocation failed for {size} bytes")))
    }

    unsafe fn deallocate(&mut self, ptr: NonNull<u8>, size: usize) -> Result<(), BenchError> {
        let layout = layout_for(size)?;
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
        unsafe {
            self.allocator
                .deallocate_with_size_checked(&mut self.cache, ptr, size)
        }
        .map_err(|error| BenchError::Alloc(format!("orthotope deallocation failed: {error}")))
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let workloads = workloads();
    let workload_filter = std::env::var("ORTHOTOPE_BENCH_WORKLOAD").ok();
    let allocator_filter = std::env::var("ORTHOTOPE_BENCH_ALLOCATOR").ok();

    let mut rows = Vec::new();

    for workload in workloads {
        if workload_filter
            .as_deref()
            .is_some_and(|filter| workload.name != filter)
        {
            continue;
        }

        for allocator in AllocatorKind::ALL {
            if allocator_filter
                .as_deref()
                .is_some_and(|filter| allocator.name() != filter)
            {
                continue;
            }

            let elapsed = measure(workload, allocator)?;
            let per_op = per_operation(elapsed, workload.operations, workload.unit);
            rows.push(ResultRow {
                workload: workload.name,
                allocator: allocator.name(),
                value: per_op,
                unit: workload.unit,
            });
        }
    }

    let mut out = stdout().lock();
    writeln!(out, "# allocator_harness")?;
    writeln!(out)?;
    writeln!(
        out,
        "warmup_samples={WARMUP_SAMPLES}, measure_samples={MEASURE_SAMPLES}, alignment={ALIGNMENT}, large_request={LARGE_REQUEST_BYTES}"
    )?;
    writeln!(out)?;
    writeln!(out, "| Workload | Allocator | Median |")?;
    writeln!(out, "| --- | --- | ---: |")?;

    for row in rows {
        writeln!(
            out,
            "| `{}` | {} | `{:.2} {}` |",
            row.workload,
            row.allocator,
            row.value,
            row.unit.suffix()
        )?;
    }

    Ok(())
}

fn workloads() -> Vec<Workload> {
    let mut workloads = Vec::new();

    for size in SAME_THREAD_SIZES {
        workloads.push(Workload {
            name: workload_name("same_thread_small_churn", size),
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
        name: "long_lived_handoff",
        operations: 80_000,
        unit: TimeUnit::Microseconds,
        kind: WorkloadKind::LongLivedHandoff,
    });

    workloads
}

fn workload_name(prefix: &'static str, size: usize) -> &'static str {
    Box::leak(format!("{prefix}/{size}").into_boxed_str())
}

const fn same_thread_iterations(size: usize) -> usize {
    match size {
        0..=64 => 6_000_000,
        65..=4_096 => 3_000_000,
        _ => 600_000,
    }
}

fn measure(workload: Workload, allocator: AllocatorKind) -> Result<Duration, BenchError> {
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

fn run_workload(kind: WorkloadKind, allocator: AllocatorKind) -> Result<Duration, BenchError> {
    match kind {
        WorkloadKind::SameThreadSmallChurn(size) => run_same_thread_small_churn(allocator, size),
        WorkloadKind::EmbeddingBatch => run_embedding_batch(allocator),
        WorkloadKind::MixedSizeChurn => run_mixed_size_churn(allocator),
        WorkloadKind::LargePath => run_large_path(allocator),
        WorkloadKind::LongLivedHandoff => run_long_lived_handoff(allocator),
    }
}

fn per_operation(duration: Duration, operations: usize, unit: TimeUnit) -> f64 {
    let divisor = u32::try_from(operations).map_or_else(
        |_| unreachable!("benchmark operation count exceeded u32"),
        f64::from,
    );
    match unit {
        TimeUnit::Nanoseconds => duration.as_secs_f64() * 1_000_000_000.0 / divisor,
        TimeUnit::Microseconds => duration.as_secs_f64() * 1_000_000.0 / divisor,
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
            unsafe {
                backend.deallocate(ptr, LARGE_REQUEST_BYTES)?;
            }
        }

        Ok(start.elapsed())
    })
}

fn run_long_lived_handoff(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    match allocator {
        AllocatorKind::Orthotope => run_long_lived_handoff_orthotope(),
        AllocatorKind::System | AllocatorKind::MiMalloc | AllocatorKind::Jemalloc => {
            run_long_lived_handoff_raw(allocator)
        }
    }
}

fn run_long_lived_handoff_raw(allocator: AllocatorKind) -> Result<Duration, BenchError> {
    let (sender, receiver) = sync_channel::<usize>(0);
    let barrier = Arc::new(Barrier::new(3));

    let producer_barrier = Arc::clone(&barrier);
    let producer = thread::spawn(move || -> Result<(), BenchError> {
        let mut backend = raw_backend(allocator);
        producer_barrier.wait();
        for _ in 0..80_000 {
            let ptr = backend.allocate(256)?;
            sender
                .send(ptr.as_ptr() as usize)
                .map_err(|error| BenchError::Thread(format!("send failed: {error}")))?;
        }
        Ok(())
    });

    let consumer_barrier = Arc::clone(&barrier);
    let consumer = thread::spawn(move || -> Result<(), BenchError> {
        let mut backend = raw_backend(allocator);
        consumer_barrier.wait();
        for _ in 0..80_000 {
            let address = receiver
                .recv()
                .map_err(|error| BenchError::Thread(format!("receive failed: {error}")))?;
            let ptr = NonNull::new(address as *mut u8).ok_or_else(|| {
                BenchError::Thread("received null pointer during handoff".to_string())
            })?;
            unsafe {
                backend.deallocate(ptr, 256)?;
            }
        }
        Ok(())
    });

    barrier.wait();
    let start = Instant::now();

    producer
        .join()
        .map_err(|_| BenchError::Thread("producer thread panicked".to_string()))??;
    consumer
        .join()
        .map_err(|_| BenchError::Thread("consumer thread panicked".to_string()))??;

    Ok(start.elapsed())
}

fn run_long_lived_handoff_orthotope() -> Result<Duration, BenchError> {
    let allocator = Arc::new(
        Allocator::new(AllocatorConfig::default())
            .map_err(|error| BenchError::Alloc(format!("orthotope init failed: {error}")))?,
    );
    let (sender, receiver) = sync_channel::<usize>(0);
    let barrier = Arc::new(Barrier::new(3));

    let producer_allocator = Arc::clone(&allocator);
    let producer_barrier = Arc::clone(&barrier);
    let producer = thread::spawn(move || -> Result<(), BenchError> {
        let mut cache = ThreadCache::new(*producer_allocator.config());
        producer_barrier.wait();
        for _ in 0..80_000 {
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
    });

    let consumer_allocator = Arc::clone(&allocator);
    let consumer_barrier = Arc::clone(&barrier);
    let consumer = thread::spawn(move || -> Result<(), BenchError> {
        let mut cache = ThreadCache::new(*consumer_allocator.config());
        consumer_barrier.wait();
        for _ in 0..80_000 {
            let address = receiver
                .recv()
                .map_err(|error| BenchError::Thread(format!("receive failed: {error}")))?;
            let ptr = NonNull::new(address as *mut u8).ok_or_else(|| {
                BenchError::Thread("received null pointer during handoff".to_string())
            })?;
            unsafe {
                consumer_allocator
                    .deallocate_with_size_checked(&mut cache, ptr, 256)
                    .map_err(|error| {
                        BenchError::Alloc(format!("orthotope deallocation failed: {error}"))
                    })?;
            }
        }
        Ok(())
    });

    barrier.wait();
    let start = Instant::now();

    producer
        .join()
        .map_err(|_| BenchError::Thread("producer thread panicked".to_string()))??;
    consumer
        .join()
        .map_err(|_| BenchError::Thread("consumer thread panicked".to_string()))??;

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
