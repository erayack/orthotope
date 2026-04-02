use core::mem::size_of;
use std::error::Error;
use std::io::{Write, stdout};

use orthotope::{allocate, deallocate};

const BATCH_SIZE: usize = 8;
const EMBEDDING_DIMENSIONS: usize = 1_536;

fn main() -> Result<(), Box<dyn Error>> {
    let mut out = stdout().lock();
    let embedding_bytes = EMBEDDING_DIMENSIONS * size_of::<f32>();
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    writeln!(
        out,
        "allocating {BATCH_SIZE} embedding buffers of {embedding_bytes} bytes each"
    )?;

    for index in 0..BATCH_SIZE {
        let buffer = allocate(embedding_bytes)?;
        writeln!(out, "buffer[{index}] = {:p}", buffer.as_ptr())?;
        batch.push(buffer);
    }

    for buffer in batch {
        // SAFETY: each pointer in `batch` is a live Orthotope allocation that has not
        // yet been freed.
        unsafe {
            deallocate(buffer)?;
        }
    }

    writeln!(out, "released embedding batch")?;
    Ok(())
}
