use std::error::Error;
use std::io::{Write, stdout};

use orthotope::size_class::SizeClass;
use orthotope::{allocate, deallocate};

fn main() -> Result<(), Box<dyn Error>> {
    let mut out = stdout().lock();

    writeln!(
        out,
        "size class for 100: {:?}",
        SizeClass::from_request(100)
    )?;
    writeln!(out, "size class for 8: {:?}", SizeClass::from_request(8))?;

    let first = allocate(100)?;
    writeln!(out, "allocated: {:p}", first.as_ptr())?;

    // SAFETY: `first` was allocated above and has not been freed yet.
    unsafe {
        deallocate(first)?;
    }

    let second = allocate(100)?;
    writeln!(out, "allocated after free: {:p}", second.as_ptr())?;
    writeln!(
        out,
        "reuse? {}",
        if first == second { "YES!" } else { "NO" }
    )?;

    // SAFETY: `second` is the currently live allocation.
    unsafe {
        deallocate(second)?;
    }

    Ok(())
}
