set shell := ["bash", "-uc"]

screenshot_dir := "target/screenshots"

# List the available recipes.
default:
    @just --list

# Everything CI runs, in the order CI runs it.
ci: fmt-check lint test deny audit screenshot

# Open the browser window.
#
# Built, signed and then run, rather than through `cargo run`. The signing is
# what stops the keychain asking about the cookie key on every rebuild — see
# `just sign` for why an unsigned binary is a new application every time.
run *ARGS:
    cargo build
    @just sign
    ./target/debug/otlyra {{ARGS}}

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

# Copy a live page and its assets into `target/mirrors/<name>`, ready to compare.
#
# A live page is not the same page tomorrow, and a headless reference pointed at
# one renders whatever the network gave *it*. The mirror freezes one page for
# both halves of the comparison. The scripts are stripped, so the number is about
# boxes and text rather than about which engine ran what — `tools/mirror.py`
# says the rest.
mirror url name:
    @python3 tools/mirror.py {{url}} target/mirrors/{{name}}
    @echo "compare it with: just reference target/mirrors/{{name}}/index.html 1280 900"

# A mirrored page against both references, at the widths the plan records.
#
# One width proves one width: a layout that switches to a column somewhere is
# right on either side of the switch and wrong at it, and a single number would
# never say so.
mirror-sweep name *WIDTHS:
    #!/usr/bin/env bash
    set -euo pipefail
    widths="{{WIDTHS}}"
    for width in ${widths:-1440 1280 1100 1000 900 800 700}; do
      printf '%5s  ' "$width"
      just reference "target/mirrors/{{name}}/index.html" "$width" 900 2>/dev/null \
        | grep -E '^(chrome|firefox|between)' | tr '\n' ' '
      echo
    done

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

# Stop the keychain asking about the cookie key on every rebuild.
#
# The prompt is not a bug and it is not the keychain being awkward. macOS decides
# whether a program may read a keychain item by its *code signature*, and an
# unsigned binary has none — so every `cargo build` produces something the
# keychain has never seen, and it asks again. A shipping browser is asked about
# once because it is signed once, with an identity that does not change when the
# bytes inside it do.
#
# This makes such an identity, once, and signs the built binaries with it. The
# next run asks one last time; answer *Always Allow* and it is answered for good,
# because the identity stays the same across every rebuild after it.
#
# `just run` does this for you, and `tools/sign-and-run.sh` — wired in as cargo's
# `runner` — does it for every `cargo run`, because the build cargo does first
# replaces the signature this recipe put on. Cookies keep working throughout: the
# point is to be asked once, not to stop keeping them.
sign:
    #!/usr/bin/env bash
    set -euo pipefail
    identity="Otlyra Development"

    # An *identity*, not a certificate: a certificate with no private key beside
    # it looks like success and then fails at `codesign` with nothing to sign
    # with. That is also exactly what a half-finished import leaves behind, so
    # asking the narrower question is what makes this recipe safe to run twice.
    if ! security find-identity -v -p codesigning | grep -q "$identity"; then
      echo "making a code-signing identity called \"$identity\""
      # Anything left from an import that did not finish. Left behind, the
      # certificate would answer for an identity that cannot sign.
      while security find-certificate -c "$identity" >/dev/null 2>&1; do
        security delete-certificate -c "$identity" >/dev/null 2>&1 || break
      done
      # Self-signed, in the login keychain. Nothing trusts it but this machine,
      # which is the whole of what is wanted here: an identity that is *stable*,
      # not one that is authoritative.
      work="$(mktemp -d)"
      {
        printf '[req]\ndistinguished_name=name\nprompt=no\nx509_extensions=codesign\n'
        printf '[name]\nCN=%s\n' "$identity"
        printf '[codesign]\nbasicConstraints=critical,CA:false\n'
        printf 'keyUsage=critical,digitalSignature\n'
        printf 'extendedKeyUsage=critical,codeSigning\n'
      } > "$work/openssl.conf"
      openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -keyout "$work/key.pem" -out "$work/cert.pem" -config "$work/openssl.conf" 2>/dev/null
      # The three `-*pbe`/`-macalg` flags are not decoration. OpenSSL 3 writes a
      # PKCS#12 with AES and a SHA-256 MAC by default, and Apple's Security
      # framework cannot read either — the import fails with *MAC verification
      # failed*, which reads as a wrong password and is not one. And the password
      # is a real one rather than empty, because an empty one fails the same way.
      openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" \
        -out "$work/identity.p12" -passout pass:otlyra -name "$identity" \
        -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg SHA1
      security import "$work/identity.p12" -k ~/Library/Keychains/login.keychain-db \
        -P otlyra -T /usr/bin/codesign
      rm -rf "$work"
      echo "made it. The next run asks once more; choose Always Allow."
    fi

    signed=0
    for binary in target/debug/otlyra target/release/otlyra; do
      if [ -f "$binary" ]; then
        # One identifier, whatever the binary: it is what the keychain's access
        # list is written against, and a debug and a release build that disagreed
        # about it would be two applications again.
        codesign --force --sign "$identity" --identifier io.octofhir.otlyra "$binary"
        echo "signed $binary"
        signed=$((signed + 1))
      fi
    done
    [ "$signed" -gt 0 ] || echo "nothing built to sign yet"

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
