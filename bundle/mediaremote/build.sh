#!/usr/bin/env bash
#
# Build MediaRemoteAdapter.framework from vendored sources.
#
# Output: <OUT_DIR>/MediaRemoteAdapter.framework — a universal (arm64+x86_64)
# Mach-O dynamic library wrapped in a standard macOS framework directory
# layout. Loaded at runtime by /usr/bin/perl via DynaLoader (see
# bin/mediaremote-adapter.pl).
#
# Why we don't use the upstream CMakeLists: a single `clang -dynamiclib` does
# the whole job in ~2s with zero build-system surface. CMake is overkill for
# one library + zero subprojects.
#
# Idempotent: skip rebuild if the framework binary is newer than every input
# source. The bundle script calls this on every `just bundle`, so the
# fast-path matters.
#
# Usage:
#   bundle/mediaremote/build.sh <OUT_DIR>
# where <OUT_DIR> is typically "$APP/Contents/Frameworks".

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <OUT_DIR>" >&2
    exit 1
fi

OUT_DIR=$1
SRC_DIR=$(cd "$(dirname "$0")" && pwd)
FRAMEWORK_NAME=MediaRemoteAdapter
BUNDLE_ID=fr.gmbl.MediaRemoteAdapter
SHORT_VER=0.1
LONG_VER=0.1.0
MIN_OS=13.0

FRAMEWORK_DIR="$OUT_DIR/$FRAMEWORK_NAME.framework"
VERSIONS_DIR="$FRAMEWORK_DIR/Versions/A"
BIN_PATH="$VERSIONS_DIR/$FRAMEWORK_NAME"

# Collect source files. Trailing newline matters — keep one entry per line.
SOURCES=(
    "$SRC_DIR/src/adapter/env.m"
    "$SRC_DIR/src/adapter/get.m"
    "$SRC_DIR/src/adapter/globals.m"
    "$SRC_DIR/src/adapter/keys.m"
    "$SRC_DIR/src/adapter/now_playing.m"
    "$SRC_DIR/src/adapter/repeat.m"
    "$SRC_DIR/src/adapter/seek.m"
    "$SRC_DIR/src/adapter/send.m"
    "$SRC_DIR/src/adapter/shuffle.m"
    "$SRC_DIR/src/adapter/speed.m"
    "$SRC_DIR/src/adapter/stream.m"
    "$SRC_DIR/src/private/MediaRemote.m"
    "$SRC_DIR/src/utility/Debounce.m"
    "$SRC_DIR/src/utility/helpers.m"
)

# Fast path: skip rebuild if the binary is newer than every source + this
# script itself. Saves ~2s on every `just bundle` after the first one.
if [[ -f "$BIN_PATH" ]]; then
    needs_rebuild=0
    for src in "${SOURCES[@]}" "$0"; do
        if [[ "$src" -nt "$BIN_PATH" ]]; then
            needs_rebuild=1
            break
        fi
    done
    if [[ $needs_rebuild -eq 0 ]]; then
        echo ">> $FRAMEWORK_NAME.framework up to date — skipping build"
        exit 0
    fi
fi

echo ">> Building $FRAMEWORK_NAME.framework (universal arm64+x86_64)"

# Wipe any previous build so symlinks / Info.plist stay consistent.
rm -rf "$FRAMEWORK_DIR"
mkdir -p "$VERSIONS_DIR/Resources"

# UniformTypeIdentifiers is only present on macOS 11+; we target 13.0 so it's
# always available. JavaScriptCore is needed by upstream's helpers (date
# parsing). Foundation + AppKit cover the rest.
clang -dynamiclib \
    -fobjc-arc \
    -fvisibility=default \
    -mmacosx-version-min="$MIN_OS" \
    -arch arm64 -arch x86_64 \
    -framework Foundation \
    -framework AppKit \
    -framework JavaScriptCore \
    -framework UniformTypeIdentifiers \
    -I"$SRC_DIR/include" \
    -I"$SRC_DIR/src" \
    -install_name "@rpath/$FRAMEWORK_NAME.framework/Versions/A/$FRAMEWORK_NAME" \
    -compatibility_version "$SHORT_VER" \
    -current_version "$LONG_VER" \
    "${SOURCES[@]}" \
    -o "$BIN_PATH"

# Minimal framework Info.plist. Keys cribbed from a typical
# `xcodebuild -target Framework` output — anything fewer and `codesign` /
# `notarytool` complain about a missing identifier.
cat > "$VERSIONS_DIR/Resources/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>$FRAMEWORK_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$FRAMEWORK_NAME</string>
    <key>CFBundlePackageType</key>
    <string>FMWK</string>
    <key>CFBundleShortVersionString</key>
    <string>$SHORT_VER</string>
    <key>CFBundleVersion</key>
    <string>$LONG_VER</string>
</dict>
</plist>
PLIST

# Standard framework symlink layout. macOS dyld expects exactly these.
ln -sf A "$FRAMEWORK_DIR/Versions/Current"
ln -sf "Versions/Current/$FRAMEWORK_NAME" "$FRAMEWORK_DIR/$FRAMEWORK_NAME"
ln -sf "Versions/Current/Resources" "$FRAMEWORK_DIR/Resources"

echo ">> Built $FRAMEWORK_DIR"
