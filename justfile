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

data_dir := "target/bench-data"

# Generate the standard benchmark fixtures.
data:
    cargo build --release --example gen
    mkdir -p {{data_dir}}
    for shape in api dense small-objects wide escapes; do \
        target/release/examples/gen $shape 15M {{data_dir}}/$shape-15M.json; done
    target/release/examples/gen deep 600K {{data_dir}}/deep-100k.json
    target/release/examples/gen ndjson 15M {{data_dir}}/ndjson-15M.ndjson
    target/release/examples/gen yaml 15M {{data_dir}}/yaml-15M.yaml
    target/release/examples/gen api 100M {{data_dir}}/api-100M.json
    target/release/examples/gen api 256M {{data_dir}}/api-256M.json

bench:
    cargo bench

# Print the peak RSS of a command in MB.
rss +cmd:
    @/usr/bin/time -l {{cmd}} 2>&1 >/dev/null | awk '/maximum resident set size/ {printf "peak RSS: %.1f MB\n", $1 / 1000000}'

# End-to-end load time of `dv --index-only` over the JSON fixtures (NFR-1).
bench-load:
    cargo build --release
    hyperfine --warmup 2 -N -L file api-15M.json,dense-15M.json,small-objects-15M.json,wide-15M.json,escapes-15M.json,deep-100k.json,api-100M.json 'target/release/dv --index-only {{data_dir}}/{file}'

# The 10 GB streaming suite over `file`, with spill files in `dir` (T5.14); needs tmux.
suite dir file:
    cargo build --release
    scripts/suite-10g.sh {{dir}} {{file}}
