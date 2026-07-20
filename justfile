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

# Open the default-rendering test page: every element the UA stylesheet has an
# opinion about, unstyled.
defaults:
    cargo run -- --file tests/pages/defaults.html

# Render that page to a PNG instead of opening a window.
defaults-shot path=(screenshot_dir / "defaults.png"):
    @mkdir -p "$(dirname {{path}})"
    cargo run --quiet -- --file tests/pages/defaults.html --screenshot {{path}} --width 820 --height 3000 --scale-factor 1
    @echo "wrote {{path}}"

# Open the CSS test page: which selectors match, and what the cascade will do
# with them once it exists.
css:
    cargo run -- --file tests/pages/css.html

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

# Build a macOS .app bundle. `cargo run` already sets the Dock icon at runtime;
# this is for a bundle you can drag to /Applications, which also gets the Finder
# icon, the real app name in the menu bar and file-type associations.
bundle: release
    #!/usr/bin/env bash
    set -euo pipefail
    app="target/Otlyra.app"
    rm -rf "$app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp target/release/otlyra "$app/Contents/MacOS/otlyra"
    cp assets/macos/Info.plist "$app/Contents/Info.plist"

    iconset="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
      sips -z $size $size assets/logo/icon-512.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
      double=$((size * 2))
      sips -z $double $double assets/logo/icon-512.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

    echo "built $app"
    echo "run it with: open $app"

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
