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

# Live edge cases against docker (crash loop, slow job, orphans, 5xx, stale credentials, name conflict); about 4 min.
edge: build
    tests/edge.sh
