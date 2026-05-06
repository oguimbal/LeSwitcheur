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

# Build + embed the MediaRemote bridge framework. Required so we can read
# now-playing metadata on macOS 15.4+, where Apple's daemon refuses XPC calls
# from non-Apple-signed processes. The bridge is loaded by /usr/bin/perl
# (Apple-signed) via DynaLoader; perl carries the entitlement implicitly so
# the daemon accepts. See bundle/mediaremote/UPSTREAM for source pin.
"$ROOT/bundle/mediaremote/build.sh" "$CONTENTS/Frameworks"
mkdir -p "$RES/mediaremote"
cp "$ROOT/bundle/mediaremote/bin/mediaremote-adapter.pl" "$RES/mediaremote/"
cp "$ROOT/bundle/mediaremote/LICENSE" "$RES/mediaremote/LICENSE"

# Sign the bundle. Identity comes from `$CODESIGN_IDENTITY`:
#   - unset / empty → auto-detect: pick the first "Developer ID Application"
#     identity from the keychain if present, else fall back to ad-hoc "-".
#   - "-" → ad-hoc (no stable identity → TCC grants are lost on every rebuild
#     because the designated requirement falls back to the raw cdhash).
#   - otherwise → exact name of an identity in the keychain search path.
#     A self-signed cert is enough to stabilise the designated requirement
#     across rebuilds (which is what TCC keys off for Accessibility / Screen
#     Recording persistence). A real Developer ID cert is also required for
#     notarisation — see bundle/notarize.sh.
IDENTITY="${CODESIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/Developer ID Application/ { print $2; exit }')"
    IDENTITY="${IDENTITY:--}"
fi

# Hardened runtime + secure timestamp are mandatory for notarisation but only
# enforceable with a real Developer ID cert; ad-hoc / self-signed builds skip
# them so dev rebuilds stay offline-capable and fast.
SIGN_FLAGS=(--force --deep --sign "$IDENTITY")
if [ "$IDENTITY" != "-" ] && [[ "$IDENTITY" == *"Developer ID Application"* ]]; then
    SIGN_FLAGS=(--force --deep --options runtime --timestamp \
        --entitlements "$ROOT/bundle/entitlements.plist" \
        --sign "$IDENTITY")
fi
codesign "${SIGN_FLAGS[@]}" "$APP_DIR"
echo ">> Signed with identity: $IDENTITY"

echo ">> Built $APP_DIR"
