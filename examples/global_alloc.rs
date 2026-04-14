use orthotope::OrthotopeGlobalAlloc;
use std::io::{Result, Write, stdout};

#[global_allocator]
static GLOBAL: OrthotopeGlobalAlloc = OrthotopeGlobalAlloc::new();

fn main() -> Result<()> {
    let mut out = stdout().lock();
    let mut values = Vec::with_capacity(256);
    for value in 0..256_u32 {
        values.push(value);
    }

    let text = String::from("orthotope");
    let boxed = Box::new(42_u64);

    writeln!(out, "vec len: {}", values.len())?;
    writeln!(out, "last value: {}", values[255])?;
    writeln!(out, "string: {text}")?;
    writeln!(out, "boxed: {boxed}")?;

    Ok(())
}
