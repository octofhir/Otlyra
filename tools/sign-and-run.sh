#!/usr/bin/env bash
#
# Sign the browser with the stable identity, then run it. Cargo's `runner`, so
# that `cargo run` gets what `just run` gets.
#
# Why this exists: macOS decides whether a program may read a keychain item by
# its *code signature*, and `cargo build` leaves an ad-hoc one whose identifier
# is derived from the build's own hash — a different application every time, and
# so a keychain prompt every time. `just sign` makes an identity that does not
# change when the bytes do; this puts it back on the binary after each build,
# which is the half `just sign` alone cannot do because cargo rebuilds after it.
#
# It never *makes* an identity. That writes a private key into somebody's login
# keychain and asks them about it, which is a decision for `just sign` to put in
# front of them rather than something a build step does on the way past.
set -euo pipefail

binary="$1"
shift

identity="Otlyra Development"

# Cargo runs every executable it builds through this — tests and examples
# included. Only the browser reads the keychain, and signing the rest would be a
# cost with nothing on the other side of it.
if [ "$(basename "$binary")" = "otlyra" ]; then
  if security find-identity -v -p codesigning 2>/dev/null | grep -q "$identity"; then
    # Already ours: a binary that was not rebuilt keeps the signature from last
    # time, and re-signing a quarter-gigabyte debug build for nothing is a
    # second or two of every run.
    if ! codesign --display --verbose=2 "$binary" 2>&1 | grep -q "Authority=$identity"; then
      codesign --force --sign "$identity" --identifier io.octofhir.otlyra "$binary" >/dev/null
    fi
  elif [ -t 2 ]; then
    # Only when somebody is there to read it: this is advice, and a screenshot
    # run piped into a file does not need it on every line of its log.
    echo "otlyra: no \"$identity\" identity yet — run \`just sign\` to stop the keychain asking on every build" >&2
  fi
fi

exec "$binary" "$@"
