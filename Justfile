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

qa: fmt fmt-check check clippy test
