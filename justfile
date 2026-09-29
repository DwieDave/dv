# Run every quality gate required before a commit.
check: fmt-check lint test deny

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo nextest run

deny:
    cargo deny check
