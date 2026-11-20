# List available tasks.
default:
    @just --list

# Install the Git pre-commit hook.
install-hooks:
    prek install --allow-missing-config

# Build the release binary.
build:
    cargo build --locked --release

# Run the example server on 127.0.0.1:8080.
run:
    mkdir -p var
    cargo run --locked -- --config examples/ragios.yaml

# Run the test suite.
test:
    cargo test --locked

# Run formatting, type, lint, and test checks.
check:
    cargo fmt --check
    cargo check --locked --all-targets
    cargo clippy --locked --all-targets -- -D warnings
    just test
