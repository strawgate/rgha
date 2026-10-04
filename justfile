default: ci

fmt:
    cargo fmt --all

ci:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Live Modal smoke test (uses ~/.modal.toml; costs a fraction of a cent).
smoke-modal:
    cargo run -p rgha-modal --example smoke

run config="rgha.toml":
    cargo run --release -p rgha -- run --config {{config}}
