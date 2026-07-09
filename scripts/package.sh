#!/usr/bin/env bash
# Assemble a release tarball: binary + SDK bridge + installer + service templates.
#
#   scripts/package.sh <target-triple> <version> [binary-path]
#   e.g. scripts/package.sh x86_64-unknown-linux-gnu 0.1.0
#
# binary-path defaults to server-rs/target/<target>/release/agentic-dev-server, falling back to
# server-rs/target/release/agentic-dev-server (host build). Output: dist/agentic-dev-<version>-<platform>.tar.gz
set -euo pipefail

TARGET="${1:?usage: package.sh <target-triple> <version> [binary-path]}"
VERSION="${2:?usage: package.sh <target-triple> <version> [binary-path]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${3:-}"
if [ -z "$BIN" ]; then
    for candidate in "$ROOT/server-rs/target/$TARGET/release/agentic-dev-server" \
                     "$ROOT/server-rs/target/release/agentic-dev-server"; do
        [ -f "$candidate" ] && BIN="$candidate" && break
    done
fi
[ -n "$BIN" ] && [ -f "$BIN" ] || { echo "error: built binary not found (looked for target $TARGET)" >&2; exit 1; }

# Friendly platform name: x86_64-unknown-linux-gnu -> linux-x86_64, aarch64-apple-darwin -> macos-aarch64
case "$TARGET" in
    *-linux-*)       PLATFORM="linux-${TARGET%%-*}" ;;
    *-apple-darwin)  PLATFORM="macos-${TARGET%%-*}" ;;
    *)               PLATFORM="$TARGET" ;;
esac

NAME="agentic-dev-$VERSION-$PLATFORM"
STAGE="$(mktemp -d)/$NAME"
mkdir -p "$STAGE" "$ROOT/dist"

install -m 755 "$BIN"                                "$STAGE/agentic-dev-server"
install -m 644 "$ROOT/server-rs/sdk-bridge.mjs"      "$STAGE/sdk-bridge.mjs"
install -m 644 "$ROOT/server-rs/package.json"        "$STAGE/package.json"
install -m 644 "$ROOT/server-rs/package-lock.json"   "$STAGE/package-lock.json"
install -m 755 "$ROOT/deploy/install.sh"             "$STAGE/install.sh"
install -m 644 "$ROOT/deploy/agentic-dev.service"    "$STAGE/agentic-dev.service"
install -m 644 "$ROOT/deploy/agentic-dev.launchd.plist" "$STAGE/agentic-dev.launchd.plist"
install -m 644 "$ROOT/deploy/README.md"              "$STAGE/INSTALL.md"

tar -C "$(dirname "$STAGE")" -czf "$ROOT/dist/$NAME.tar.gz" "$NAME"
rm -rf "$(dirname "$STAGE")"
echo "dist/$NAME.tar.gz"
