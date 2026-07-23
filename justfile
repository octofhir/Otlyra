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

# Open the image test page: intrinsic sizes, ratios, and pictures in a line.
images:
    cargo run -- --file tests/pages/images.html

# Render that page to a PNG instead of opening a window.
images-shot path=(screenshot_dir / "images.png"):
    @mkdir -p "$(dirname {{path}})"
    cargo run --quiet -- --file tests/pages/images.html --screenshot {{path}} --width 820 --height 2000 --scale-factor 1
    @echo "wrote {{path}}"

# Open the table test page: columns sized from their contents, and cells that
# reach across columns and down rows.
tables:
    cargo run -- --file tests/pages/tables.html

# Open the font test page: a family the page brings with it, one that never
# arrives, and the platform's own beside them.
fonts:
    cargo run -- --file tests/pages/fonts.html

# Open the stacking test page: which box paints over which, and where a
# `z-index` is compared against its siblings rather than against the page.
stacking:
    cargo run -- --file tests/pages/stacking.html

# Open the opacity test page: what a group is composited as, and what is in one.
opacity:
    cargo run -- --file tests/pages/opacity.html

# Open the transform test page: what a box is drawn through once it is moved,
# turned or scaled, and what goes with it.
transform:
    cargo run -- --file tests/pages/transform.html

# Open the inline-block test page: boxes that take their place in a line.
inline-block:
    cargo run -- --file tests/pages/inline-block.html

# Open the picture-choosing test page: which of the files an element offers is
# the one fetched, and what size it is then drawn at.
srcset:
    cargo run -- --file tests/pages/srcset.html

# Open the object-fit test page: a picture in a box that is not its shape.
object-fit:
    cargo run -- --file tests/pages/object-fit.html

# Open the line-height test page: how tall a line is, and what on it decides.
line-height:
    cargo run -- --file tests/pages/line-height.html

# Open the flex test page: where the lines of a wrapped container go, which
# sibling an item is laid out among, and a container that sits in a line.
flex:
    cargo run -- --file tests/pages/flex.html

# Open the background test page: several layers behind one box, and the shadow a
# box casts on the inside of itself.
backgrounds:
    cargo run -- --file tests/pages/backgrounds.html

# Open the border test page: the lines a border style draws, and how two sides
# that disagree meet at a corner.
borders:
    cargo run -- --file tests/pages/borders.html

# Open the replaced-element test page: a picture's own background and border, and
# the room they take.
replaced:
    cargo run -- --file tests/pages/replaced.html

# Open the white-space test page: what happens to the spaces between things.
white-space:
    cargo run -- --file tests/pages/white-space.html

# Open the colour scheme test page: what a page draws when the reader has asked
# for the dark palette, and what does not move either way.
color-scheme:
    cargo run -- --file tests/pages/color-scheme.html

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
