set default-list := true

build:
    cargo build

fmt:
    cargo fmt --all

clippy:
    cargo clippy --all-targets -- -D warnings

# List outdated dependencies
outdated:
    cargo outdated --root-deps-only
