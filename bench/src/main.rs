use std::error::Error;
use std::io::{Write, stdout};

use orthotope_bench::{AllocatorKind, ResultRow, measure, per_operation, workloads};

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
        "warmup_samples=3, measure_samples=9, alignment=64, large_request=20971520"
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
