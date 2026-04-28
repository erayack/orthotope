use std::error::Error;
use std::io::{Write, stdout};
use std::process::ExitCode;

use orthotope_bench::{
    AllocatorKind, BenchError, DEFAULT_FLAMEGRAPH_ALLOCATOR, DEFAULT_FLAMEGRAPH_REPETITIONS,
    DEFAULT_FLAMEGRAPH_WORKLOAD, per_operation, run_repeated_workload, workload_by_name,
    workload_names,
};

const WORKLOAD_ENV: &str = "ORTHOTOPE_FLAMEGRAPH_WORKLOAD";
const ALLOCATOR_ENV: &str = "ORTHOTOPE_FLAMEGRAPH_ALLOCATOR";
const REPETITIONS_ENV: &str = "ORTHOTOPE_FLAMEGRAPH_REPETITIONS";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    if matches!(std::env::args().nth(1).as_deref(), Some("--help" | "-h")) {
        print_help()?;
        return Ok(());
    }

    let workload_name = read_env_or_default(WORKLOAD_ENV, DEFAULT_FLAMEGRAPH_WORKLOAD)
        .map_err(Box::<dyn Error>::from)?;
    let allocator_name = read_env_or_default(ALLOCATOR_ENV, DEFAULT_FLAMEGRAPH_ALLOCATOR.name())
        .map_err(Box::<dyn Error>::from)?;
    let repetitions = read_usize_env_or_default(REPETITIONS_ENV, DEFAULT_FLAMEGRAPH_REPETITIONS)
        .map_err(Box::<dyn Error>::from)?;

    let workload = workload_by_name(&workload_name).ok_or_else(|| {
        Box::<dyn Error>::from(BenchError::Config(format!(
            "unknown {WORKLOAD_ENV} value `{workload_name}`; valid workloads: {}",
            workload_names().join(", ")
        )))
    })?;
    let allocator = AllocatorKind::parse(&allocator_name).ok_or_else(|| {
        Box::<dyn Error>::from(BenchError::Config(format!(
            "unknown {ALLOCATOR_ENV} value `{allocator_name}`; valid allocators: Orthotope, System, mimalloc, jemalloc"
        )))
    })?;

    let samples = workload.operations.checked_mul(repetitions).ok_or_else(|| {
        Box::<dyn Error>::from(BenchError::Config(format!(
            "{REPETITIONS_ENV} is too large for workload `{}`",
            workload.name
        )))
    })?;
    let elapsed = run_repeated_workload(workload, allocator, repetitions)?;
    let per_op = per_operation(elapsed, samples, workload.unit);

    let mut out = stdout().lock();
    writeln!(out, "# flamegraph_harness")?;
    writeln!(out, "workload={}", workload.name)?;
    writeln!(out, "allocator={}", allocator.name())?;
    writeln!(out, "repetitions={repetitions}")?;
    writeln!(out, "elapsed_seconds={:.6}", elapsed.as_secs_f64())?;
    writeln!(out, "per_operation={per_op:.2} {}", workload.unit.suffix())?;
    Ok(())
}

fn read_env_or_default(name: &str, default: &str) -> Result<String, BenchError> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(BenchError::Config(format!(
            "{name} must not be empty when set"
        ))),
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => Ok(default.to_string()),
        Err(error) => Err(BenchError::Config(format!(
            "failed to read {name}: {error}"
        ))),
    }
}

fn read_usize_env_or_default(name: &str, default: usize) -> Result<usize, BenchError> {
    match std::env::var(name) {
        Ok(value) => {
            let parsed = value.parse::<usize>().map_err(|error| {
                BenchError::Config(format!("{name} must be a positive integer: {error}"))
            })?;
            if parsed == 0 {
                return Err(BenchError::Config(format!(
                    "{name} must be a positive integer greater than zero"
                )));
            }
            Ok(parsed)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(BenchError::Config(format!(
            "failed to read {name}: {error}"
        ))),
    }
}

fn print_help() -> Result<(), Box<dyn Error>> {
    let mut out = stdout().lock();
    writeln!(
        out,
        "Run one allocator workload repeatedly for cargo flamegraph."
    )?;
    writeln!(out)?;
    writeln!(out, "Environment variables:")?;
    writeln!(
        out,
        "  {WORKLOAD_ENV} (default: {DEFAULT_FLAMEGRAPH_WORKLOAD})"
    )?;
    writeln!(
        out,
        "  {ALLOCATOR_ENV} (default: {})",
        DEFAULT_FLAMEGRAPH_ALLOCATOR.name()
    )?;
    writeln!(
        out,
        "  {REPETITIONS_ENV} (default: {DEFAULT_FLAMEGRAPH_REPETITIONS})"
    )?;
    writeln!(out)?;
    writeln!(out, "Valid workloads: {}", workload_names().join(", "))?;
    writeln!(
        out,
        "Valid allocators: Orthotope, System, mimalloc, jemalloc"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ALLOCATOR_ENV, REPETITIONS_ENV, WORKLOAD_ENV, read_env_or_default,
        read_usize_env_or_default,
    };
    use orthotope_bench::AllocatorKind;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        name: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let original = std::env::var(name).ok();
            // SAFETY: these tests mutate process environment in a scoped manner
            // and restore the original value before returning.
            unsafe {
                std::env::set_var(name, value);
            }
            Self { name, original }
        }

        fn unset(name: &'static str) -> Self {
            let original = std::env::var(name).ok();
            // SAFETY: these tests mutate process environment in a scoped manner
            // and restore the original value before returning.
            unsafe {
                std::env::remove_var(name);
            }
            Self { name, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => {
                    // SAFETY: restoring the original process environment value
                    // for the scoped test mutation.
                    unsafe {
                        std::env::set_var(self.name, value);
                    }
                }
                None => {
                    // SAFETY: restoring the original unset state for the scoped
                    // test mutation.
                    unsafe {
                        std::env::remove_var(self.name);
                    }
                }
            }
        }
    }

    #[test]
    fn repetitions_env_rejects_zero() {
        let _lock = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = EnvGuard::set(REPETITIONS_ENV, "0");
        let error = match read_usize_env_or_default(REPETITIONS_ENV, 400) {
            Ok(value) => panic!("expected zero repetitions to fail, got {value}"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            format!("{REPETITIONS_ENV} must be a positive integer greater than zero")
        );
    }

    #[test]
    fn workload_env_rejects_empty_string() {
        let _lock = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = EnvGuard::set(WORKLOAD_ENV, "");
        let error = match read_env_or_default(WORKLOAD_ENV, "mixed_size_churn") {
            Ok(value) => panic!("expected empty workload env to fail, got {value}"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            format!("{WORKLOAD_ENV} must not be empty when set")
        );
    }

    #[test]
    fn allocator_parser_rejects_unknown_value() {
        assert!(AllocatorKind::parse("bogus").is_none());
    }

    #[test]
    fn allocator_env_uses_default_when_unset() {
        let _lock = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = EnvGuard::unset(ALLOCATOR_ENV);
        let value = match read_env_or_default(ALLOCATOR_ENV, "Orthotope") {
            Ok(value) => value,
            Err(error) => panic!("expected default allocator value, got error: {error}"),
        };
        assert_eq!(value, "Orthotope");
    }
}
