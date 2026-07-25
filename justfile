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

# Open one of the pages in tests/pages. `just test-page borders` opens borders.html.
#
# One recipe rather than one per page: a page is a file, and remembering its name
# is remembering the file's name.
test-page name:
    cargo run -- --file tests/pages/{{name}}.html

# Render that page to a PNG instead of opening a window, without our own interface
# so that the page starts at the top of the picture.
test-page-shot name width="820" height="2000":
    @mkdir -p "{{screenshot_dir}}"
    cargo run --quiet -- --file tests/pages/{{name}}.html --no-interface \
        --screenshot "{{screenshot_dir}}/{{name}}.png" \
        --width {{width}} --height {{height}} --scale-factor 1
    @echo "wrote {{screenshot_dir}}/{{name}}.png"

# The same page against the reference browsers.
test-page-reference name width="820" height="900":
    @just reference tests/pages/{{name}}.html {{width}} {{height}}

# Somewhere for a form on a test page to be sent, which prints what arrived.
#
# `tests/pages/try.html` posts a file to it. Run it in a second terminal; it is a
# hand-checking tool and nothing in the browser or the tests needs it.
echo-server:
    @python3 tools/echo-server.py

# What pages there are to open.
test-pages:
    @ls tests/pages/*.html | xargs -n1 basename | sed 's/\.html$//'

# Render a page twice — through us, and through whatever browser
# $OTLYRA_REFERENCE points at — so the two can be put side by side.
#
# The comparison is the point: several real bugs were invisible in a dump and
# obvious the moment the same page was rendered by something that gets it right.
#
# Widths under about five hundred are not worth asking for: the reference lays out
# wider than the picture it then writes, and every comparison comes back as a page
# that does not fit.
reference page width="820" height="900":
    #!/usr/bin/env bash
    set -euo pipefail
    out="{{screenshot_dir}}/reference"
    mkdir -p "$out"
    name="$(basename {{page}} .html)"
    url="file://$(cd "$(dirname {{page}})" && pwd)/$(basename {{page}})"
    # Without our own interface: the page has to start at the top of the picture,
    # or every comparison is a comparison of two toolbars.
    cargo run --quiet -- --file {{page}} --no-interface --screenshot "$out/$name.ours.png" \
        --width {{width}} --height {{height}} --scale-factor 1
    if [ -z "${OTLYRA_REFERENCE:-}" ] && [ -z "${OTLYRA_REFERENCE_ALT:-}" ]; then
        echo "set OTLYRA_REFERENCE (and OTLYRA_REFERENCE_ALT) to browser binaries for the other half"
        exit 0
    fi
    if [ -n "${OTLYRA_REFERENCE:-}" ]; then
        # One device pixel to one CSS pixel, said out loud: on a dense screen the
        # reference answers a page's questions about density with the screen's
        # while writing the picture at one, so a page that chooses by density
        # chooses differently in each half of the comparison.
        "$OTLYRA_REFERENCE" --headless --disable-gpu --hide-scrollbars \
            --force-device-scale-factor=1 \
            --window-size={{width}},{{height}} \
            --screenshot="$out/$name.reference.png" "$url" >/dev/null 2>&1
        printf 'chrome  '
        cargo run --quiet -p otlyra-gfx --example compare -- \
            "$out/$name.ours.png" "$out/$name.reference.png" "$out/$name.difference.png" || true
    fi
    # The second reference is not a second opinion to average with the first. Where
    # the two disagree, neither is the answer and the specification is; where they
    # agree and we do not, the page is ours to fix.
    if [ -n "${OTLYRA_REFERENCE_ALT:-}" ]; then
        # An absolute path: this one resolves a relative one against somewhere of
        # its own choosing and writes the picture where nobody is looking.
        "$OTLYRA_REFERENCE_ALT" --headless --window-size={{width}},{{height}} \
            --screenshot "$(pwd)/$out/$name.alternate.png" "$url" >/dev/null 2>&1
        printf 'firefox '
        cargo run --quiet -p otlyra-gfx --example compare -- \
            "$out/$name.ours.png" "$out/$name.alternate.png" "$out/$name.difference-alt.png" || true
        if [ -n "${OTLYRA_REFERENCE:-}" ]; then
            printf 'between '
            cargo run --quiet -p otlyra-gfx --example compare -- \
                "$out/$name.reference.png" "$out/$name.alternate.png" \
                "$out/$name.difference-between.png" || true
        fi
    fi
    echo "wrote $out/$name.*.png"

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

    # From the thousand-and-twenty-four source, which carries the margin the
    # platform's own icon grid leaves: the artwork is eight hundred and
    # twenty-four of it, so this icon sits the same size in the Dock as every
    # other one rather than a quarter larger than its neighbours.
    iconset="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
      sips -z $size $size assets/logo/icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
      double=$((size * 2))
      sips -z $double $double assets/logo/icon-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
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

# Record a launch distribution without turning current misses into a local error.
startup-benchmark samples="20":
    cargo build --locked --release -p otlyra-app
    python3 tools/startup-benchmark.py --samples {{samples}}

# The dedicated reference runner uses this strict form.
startup-check samples="30":
    cargo build --locked --release -p otlyra-app
    python3 tools/startup-benchmark.py --samples {{samples}} --check
