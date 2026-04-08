use std::process::Command;

#[test]
fn global_allocator_binary_smoke_runs_successfully() {
    let binary = env!("CARGO_BIN_EXE_global_alloc_smoke");
    let output = Command::new(binary)
        .output()
        .unwrap_or_else(|error| panic!("expected smoke binary to run: {error}"));

    assert!(
        output.status.success(),
        "expected smoke binary to exit successfully, status: {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
