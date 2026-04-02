use core::ptr::NonNull;
use std::sync::LazyLock;
use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use orthotope::allocator::Allocator;
use orthotope::config::AllocatorConfig;
use orthotope::error::FreeError;
use orthotope::thread_cache::ThreadCache;
use orthotope::{allocate, deallocate};

static GLOBAL_API_TEST_LOCK: LazyLock<std::sync::Mutex<()>> =
    LazyLock::new(|| std::sync::Mutex::new(()));

const fn concurrency_config() -> AllocatorConfig {
    AllocatorConfig {
        arena_size: 1 << 24,
        alignment: 64,
        refill_target_bytes: 1,
        local_cache_target_bytes: 1,
    }
}

fn allocator() -> Arc<Allocator> {
    let allocator = match Allocator::new(concurrency_config()) {
        Ok(allocator) => allocator,
        Err(error) => panic!("expected allocator to initialize: {error}"),
    };

    Arc::new(allocator)
}

fn pointer_from_addr(addr: usize) -> NonNull<u8> {
    NonNull::new(addr as *mut u8).unwrap_or_else(|| panic!("expected non-null pointer address"))
}

#[test]
fn cross_thread_free_routes_block_into_freeing_thread_cache() {
    let allocator = allocator();
    let request = 32;
    let (allocated_tx, allocated_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();

    let allocating_allocator = Arc::clone(&allocator);
    let allocating_thread = thread::spawn(move || {
        let mut cache = ThreadCache::new(*allocating_allocator.config());
        let ptr = match allocating_allocator.allocate_with_cache(&mut cache, request) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected allocating thread allocation to succeed: {error}"),
        };

        match allocated_tx.send(ptr.as_ptr().addr()) {
            Ok(()) => {}
            Err(error) => panic!("expected pointer handoff to succeed: {error}"),
        }
    });

    let freeing_allocator = Arc::clone(&allocator);
    let freeing_thread = thread::spawn(move || {
        let addr = match allocated_rx.recv() {
            Ok(addr) => addr,
            Err(error) => panic!("expected to receive allocated pointer: {error}"),
        };
        let ptr = pointer_from_addr(addr);
        let mut cache = ThreadCache::new(*freeing_allocator.config());

        // SAFETY: `ptr` is a live allocation produced by the paired allocating thread.
        match unsafe { freeing_allocator.deallocate_with_cache(&mut cache, ptr) } {
            Ok(()) => {}
            Err(error) => panic!("expected cross-thread free to succeed: {error}"),
        }

        let reused = match freeing_allocator.allocate_with_cache(&mut cache, request) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected freeing thread reuse allocation to succeed: {error}"),
        };

        match result_tx.send((addr, reused.as_ptr().addr())) {
            Ok(()) => {}
            Err(error) => panic!("expected reuse result handoff to succeed: {error}"),
        }

        // SAFETY: `reused` is the currently live allocation in this thread.
        match unsafe { freeing_allocator.deallocate_with_cache(&mut cache, reused) } {
            Ok(()) => {}
            Err(error) => panic!("expected freeing thread cleanup free to succeed: {error}"),
        }
    });

    if let Err(error) = allocating_thread.join() {
        panic!("expected allocating thread to complete successfully: {error:?}");
    }
    if let Err(error) = freeing_thread.join() {
        panic!("expected freeing thread to complete successfully: {error:?}");
    }

    let (original_addr, reused_addr) = match result_rx.recv() {
        Ok(addrs) => addrs,
        Err(error) => panic!("expected reused pointer result: {error}"),
    };

    assert_eq!(reused_addr, original_addr);
}

#[test]
fn thread_exit_drain_makes_cached_blocks_visible_to_other_threads() {
    let _guard = GLOBAL_API_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| panic!("expected global API test lock to be available: {error}"));
    let request = 70_000;
    let (drained_tx, drained_rx) = mpsc::channel();

    let draining_thread = thread::spawn(move || {
        let ptr = match allocate(request) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected worker allocation to succeed: {error}"),
        };

        // SAFETY: `ptr` is still live and belongs to this allocator.
        match unsafe { deallocate(ptr) } {
            Ok(()) => {}
            Err(error) => panic!("expected worker free to succeed: {error}"),
        }

        match drained_tx.send(ptr.as_ptr().addr()) {
            Ok(()) => {}
            Err(error) => panic!("expected drained pointer handoff to succeed: {error}"),
        }
    });

    if let Err(error) = draining_thread.join() {
        panic!("expected draining thread to complete successfully: {error:?}");
    }

    let drained_addr = match drained_rx.recv() {
        Ok(addr) => addr,
        Err(error) => panic!("expected drained pointer address: {error}"),
    };

    let receiving_thread = thread::spawn(move || {
        let ptr = match allocate(request) {
            Ok(ptr) => ptr,
            Err(error) => panic!("expected receiving thread allocation to succeed: {error}"),
        };
        let received_addr = ptr.as_ptr().addr();

        // SAFETY: `ptr` is the currently live allocation in this thread.
        match unsafe { deallocate(ptr) } {
            Ok(()) => {}
            Err(error) => panic!("expected receiving thread free to succeed: {error}"),
        }

        received_addr
    });

    let received_addr = receiving_thread.join().unwrap_or_else(|error| {
        panic!("expected receiving thread to complete successfully: {error:?}")
    });

    assert_eq!(received_addr, drained_addr);
}

#[test]
fn many_threads_allocate_and_free_without_errors() {
    let allocator = allocator();
    let thread_count = 8;
    let iterations = 256;
    let requests = [1, 64, 65, 256, 257, 4096];
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut handles = Vec::new();

    for thread_index in 0..thread_count {
        let allocator = Arc::clone(&allocator);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut cache = ThreadCache::new(*allocator.config());
            barrier.wait();

            for iteration in 0..iterations {
                let request = requests[(thread_index + iteration) % requests.len()];
                let ptr = match allocator.allocate_with_cache(&mut cache, request) {
                    Ok(ptr) => ptr,
                    Err(error) => panic!(
                        "expected stress allocation to succeed for request {request}: {error}"
                    ),
                };
                let byte = u8::try_from(thread_index ^ iteration).unwrap_or_else(|error| {
                    panic!("expected stress byte conversion to fit: {error}")
                });

                // SAFETY: `ptr` points to a live allocation with at least one usable byte.
                unsafe {
                    ptr.as_ptr().write(byte);
                }

                // SAFETY: `ptr` is still the live allocation returned above.
                match unsafe { allocator.deallocate_with_size_checked(&mut cache, ptr, request) } {
                    Ok(()) => {}
                    Err(error) => panic!(
                        "expected stress deallocation to succeed for request {request}: {error}"
                    ),
                }
            }
        }));
    }

    for handle in handles {
        if let Err(error) = handle.join() {
            panic!("expected stress thread to complete successfully: {error:?}");
        }
    }
}

#[test]
fn cross_allocator_small_free_is_rejected_as_foreign_pointer() {
    let allocating_allocator = allocator();
    let freeing_allocator = allocator();
    let mut allocating_cache = ThreadCache::new(*allocating_allocator.config());
    let mut freeing_cache = ThreadCache::new(*freeing_allocator.config());

    let ptr = match allocating_allocator.allocate_with_cache(&mut allocating_cache, 32) {
        Ok(ptr) => ptr,
        Err(error) => panic!("expected source allocator allocation to succeed: {error}"),
    };

    // SAFETY: `ptr` is live but belongs to a different allocator instance, which this
    // test expects the destination allocator to reject via its arena-range ownership
    // check on the decoded small block start.
    let result = unsafe { freeing_allocator.deallocate_with_cache(&mut freeing_cache, ptr) };
    assert_eq!(result, Err(FreeError::ForeignPointer));

    // SAFETY: `ptr` is still the original live allocation from the source allocator.
    match unsafe { allocating_allocator.deallocate_with_cache(&mut allocating_cache, ptr) } {
        Ok(()) => {}
        Err(error) => panic!("expected source allocator cleanup free to succeed: {error}"),
    }
}
