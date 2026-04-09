fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

check:
    cargo check --all-targets

clippy:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets

test-release:
    cargo test --release --all-targets

bench:
    cargo run --release --manifest-path bench/Cargo.toml

qa: fmt fmt-check check clippy test

qa-release: fmt fmt-check check clippy test test-release
