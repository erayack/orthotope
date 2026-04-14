use std::error::Error;
use std::io::{Write, stdout};
use std::ptr::NonNull;
use std::sync::Arc;
use std::thread;

use orthotope::{AllocError, Allocator, AllocatorConfig, FreeError, ThreadCache};

#[derive(Debug)]
enum WorkerError {
    Alloc(AllocError),
    Free(FreeError),
    Io(std::io::Error),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alloc(error) => write!(f, "{error}"),
            Self::Free(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl Error for WorkerError {}

impl From<AllocError> for WorkerError {
    fn from(error: AllocError) -> Self {
        Self::Alloc(error)
    }
}

impl From<FreeError> for WorkerError {
    fn from(error: FreeError) -> Self {
        Self::Free(error)
    }
}

impl From<std::io::Error> for WorkerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct AllocationBatch<'a> {
    allocator: &'a Allocator,
    cache: &'a mut ThreadCache,
    pointers: Vec<NonNull<u8>>,
}

impl Drop for AllocationBatch<'_> {
    fn drop(&mut self) {
        for ptr in self.pointers.drain(..) {
            // SAFETY: the batch owns each live pointer exactly once and uses the
            // matching allocator instance plus thread cache for cleanup.
            unsafe {
                if let Err(error) = self.allocator.deallocate_with_cache(self.cache, ptr) {
                    panic!("threaded example cleanup should succeed: {error}");
                }
            }
        }
    }
}

fn worker_main(worker_id: usize, allocator: &Allocator) -> Result<(), WorkerError> {
    let mut cache = ThreadCache::new(*allocator.config());
    let mut out = stdout().lock();
    let mut pointers = Vec::new();

    for _ in 0..32 {
        let ptr = allocator.allocate_with_cache(&mut cache, 512)?;
        pointers.push(ptr);
    }

    let local_before_free = cache.stats().total_local_blocks();
    let batch = AllocationBatch {
        allocator,
        cache: &mut cache,
        pointers,
    };
    drop(batch);
    let local_after_free = cache.stats().total_local_blocks();

    writeln!(
        out,
        "worker {worker_id}: local cached blocks before free = {local_before_free}"
    )?;
    writeln!(
        out,
        "worker {worker_id}: local cached blocks after free = {local_after_free}"
    )?;

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let allocator = Arc::new(Allocator::new(AllocatorConfig::default())?);
    let mut workers = Vec::new();

    for worker_id in 0..4 {
        let allocator = Arc::clone(&allocator);
        workers.push(thread::spawn(move || {
            worker_main(worker_id, allocator.as_ref())
        }));
    }

    for worker in workers {
        worker
            .join()
            .map_err(|_| "worker thread panicked")?
            .map_err(Box::<dyn Error>::from)?;
    }

    Ok(())
}
