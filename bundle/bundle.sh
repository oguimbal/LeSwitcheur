#!/usr/bin/env bash
# Assemble LeSwitcheur.app from a release build.
# Usage: ./bundle/bundle.sh
# Output: dist/LeSwitcheur.app

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="LeSwitcheur"
BIN_NAME="switcheur"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN_PATH="$TARGET_DIR/release/$BIN_NAME"

# Keep Cargo.toml + Info.plist in lockstep before producing an .app. The
# update manifest advertises the Cargo.toml version; shipping a bundle whose
# Info.plist disagrees would mislead the drift-watcher in older installs.
"$ROOT/bundle/verify-version.sh"

# Always invoke cargo — it's incremental, so this is a no-op when nothing
# changed but correctly rebuilds when sources moved since the last bundle.
# Skipping based on `-x $BIN_PATH` would ship a stale binary.
echo ">> Building release..."
(cd "$ROOT" && cargo build --release -p "$BIN_NAME")

APP_DIR="$ROOT/dist/$APP_NAME.app"
CONTENTS="$APP_DIR/Contents"
MACOS="$CONTENTS/MacOS"
RES="$CONTENTS/Resources"

rm -rf "$APP_DIR"
mkdir -p "$MACOS" "$RES"

cp "$BIN_PATH" "$MACOS/$BIN_NAME"
cp "$ROOT/bundle/Info.plist" "$CONTENTS/Info.plist"

if [ -f "$ROOT/bundle/AppIcon.icns" ]; then
    cp "$ROOT/bundle/AppIcon.icns" "$RES/AppIcon.icns"
fi

# Sign the bundle. Identity comes from `$CODESIGN_IDENTITY`:
#   - unset / empty / "-" → ad-hoc (dev default, no stable identity → TCC
#     grants are lost on every rebuild because the designated requirement
#     falls back to the raw cdhash).
#   - otherwise → name of a code-signing identity present in the current
#     keychain search path. A self-signed cert is enough to stabilise the
#     designated requirement across rebuilds, which is what TCC keys off
#     for Accessibility / Screen Recording persistence.
# No notarisation here — that requires Apple Developer ID, not a self-signed
# cert. Users of a self-signed build must right-click → Open on first launch.
IDENTITY="${CODESIGN_IDENTITY:--}"
codesign --force --deep --sign "$IDENTITY" "$APP_DIR"
echo ">> Signed with identity: $IDENTITY"

echo ">> Built $APP_DIR"
