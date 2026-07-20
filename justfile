set shell := ["bash", "-uc"]

screenshot_dir := "target/screenshots"

# List the available recipes.
default:
    @just --list

# Everything CI runs, in the order CI runs it.
ci: fmt-check lint test deny audit screenshot

# Open the browser window.
run *ARGS:
    cargo run -- {{ARGS}}

# Render one frame to a PNG and exit. Needs no display server.
screenshot path=(screenshot_dir / "otlyra.png") width="1024" height="768" scale="2.0":
    @mkdir -p "$(dirname {{path}})"
    cargo run --quiet -- --screenshot {{path}} --width {{width}} --height {{height}} --scale-factor {{scale}}
    @echo "wrote {{path}}"

build:
    cargo build --workspace

release:
    cargo build --workspace --release

test:
    cargo test --workspace

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Licence and source policy. Requires `cargo install cargo-deny`.
deny:
    cargo deny check

# Advisory database. Requires `cargo install cargo-audit`.
audit:
    cargo audit

# Regenerate NOTICE. Requires `cargo install cargo-about`.
notice:
    cargo about generate about.hbs -o NOTICE

# Install the tools the supply-chain recipes need.
install-tools:
    cargo install cargo-deny cargo-audit cargo-about

doc:
    cargo doc --workspace --no-deps --open

clean:
    cargo clean

# The numbers we hold ourselves to: cold release build, binary size, package count.
metrics:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo clean
    start=$(date +%s)
    cargo build --workspace --release --quiet
    echo "cold release build: $(( $(date +%s) - start ))s"
    ls -lh target/release/otlyra | awk '{print "binary size: " $5}'
    echo "packages: $(cargo tree --edges normal | sed 's/[^a-zA-Z0-9_-]* //' | sort -u | wc -l | tr -d ' ')"

# Time an incremental rebuild after touching one file.
metrics-incremental:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --workspace --release --quiet
    touch crates/otlyra-gfx/src/lib.rs
    start=$(date +%s)
    cargo build --workspace --release --quiet
    echo "incremental release build: $(( $(date +%s) - start ))s"
