default:
    @just --list

build:
    cargo build --locked

test:
    cargo test --locked

# The CI gate: format, clippy with warnings as errors, tests, dependency licences and advisories.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    cargo deny check

# Live edge cases against docker: the failure catalog, one small stack per cause; 4 at once.
edge: build
    tests/edge.sh
