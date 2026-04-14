use std::error::Error;
use std::io::{Write, stdout};
use std::ptr::NonNull;

use orthotope::{Allocator, AllocatorConfig, ThreadCache, allocate, deallocate, global_stats};

struct GlobalAllocation(NonNull<u8>);

impl Drop for GlobalAllocation {
    fn drop(&mut self) {
        // SAFETY: the guard only stores live pointers returned by the global API and
        // releases each one exactly once during cleanup.
        unsafe {
            if let Err(error) = deallocate(self.0) {
                panic!("global example cleanup should succeed: {error}");
            }
        }
    }
}

struct InstanceAllocation<'a> {
    allocator: &'a Allocator,
    cache: &'a mut ThreadCache,
    ptr: NonNull<u8>,
}

impl Drop for InstanceAllocation<'_> {
    fn drop(&mut self) {
        // SAFETY: the guard owns one live pointer from `allocator` and releases it
        // exactly once through the matching thread cache during cleanup.
        unsafe {
            if let Err(error) = self.allocator.deallocate_with_cache(self.cache, self.ptr) {
                panic!("instance example cleanup should succeed: {error}");
            }
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut out = stdout().lock();
    let global_before = global_stats()?;
    let global_ptr = GlobalAllocation(allocate(16 * 1024 * 1024 + 1)?);
    let global_after_allocate = global_stats()?;
    let global_live_before = global_before.large_live_allocations;
    let global_live_after_allocate = global_after_allocate.large_live_allocations;
    let global_live_bytes_after_allocate = global_after_allocate.large_live_bytes;
    let global_arena_before = global_before.arena_remaining;
    let global_arena_after_allocate = global_after_allocate.arena_remaining;
    drop(global_ptr);
    let global_after_free = global_stats()?;

    let allocator = Allocator::new(AllocatorConfig::default())?;
    let mut cache = ThreadCache::new(*allocator.config());
    let instance_bound_before = cache.stats().is_bound;
    let instance_ptr = allocator.allocate_with_cache(&mut cache, 512)?;
    let instance_bound_after_allocate = cache.stats().is_bound;
    let instance_local_after_allocate = cache.stats().total_local_blocks();
    let instance_central_after_allocate = allocator.stats().total_small_central_blocks();
    let instance_ptr = InstanceAllocation {
        allocator: &allocator,
        cache: &mut cache,
        ptr: instance_ptr,
    };
    drop(instance_ptr);
    let instance_local_after_free = cache.stats().total_local_blocks();
    let instance_central_after_free = allocator.stats().total_small_central_blocks();

    writeln!(
        out,
        "global large live allocations: {global_live_before} -> {global_live_after_allocate} -> {}",
        global_after_free.large_live_allocations
    )?;
    writeln!(
        out,
        "global large live bytes after allocation: {global_live_bytes_after_allocate}"
    )?;
    writeln!(
        out,
        "global arena remaining: {global_arena_before} -> {global_arena_after_allocate} -> {}",
        global_after_free.arena_remaining
    )?;
    writeln!(
        out,
        "instance cache bound: {instance_bound_before} -> {instance_bound_after_allocate}"
    )?;
    writeln!(
        out,
        "instance local cached blocks: {instance_local_after_allocate} -> {instance_local_after_free}"
    )?;
    writeln!(
        out,
        "instance central cached blocks: {instance_central_after_allocate} -> {instance_central_after_free}"
    )?;

    Ok(())
}
