#!/usr/bin/env bash
# Build dist/LeSwitcheur.dmg with the familiar drag-to-Applications layout.
# Requires `create-dmg` (brew install create-dmg). Expects dist/LeSwitcheur.app
# to already exist — run bundle/bundle.sh first.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="LeSwitcheur"
APP_PATH="$ROOT/dist/$APP_NAME.app"
DMG_PATH="$ROOT/dist/$APP_NAME.dmg"

if [ ! -d "$APP_PATH" ]; then
    echo "error: $APP_PATH missing — run bundle/bundle.sh first" >&2
    exit 1
fi

if ! command -v create-dmg >/dev/null 2>&1; then
    echo "error: create-dmg not installed — run: brew install create-dmg" >&2
    exit 1
fi

rm -f "$DMG_PATH"

# create-dmg returns 2 when codesigning the DMG itself fails but the DMG is
# otherwise fine; we don't sign the DMG, so treat 2 as success too.
set +e
create-dmg \
    --volname "$APP_NAME" \
    --window-pos 200 120 \
    --window-size 660 400 \
    --icon-size 128 \
    --icon "$APP_NAME.app" 165 180 \
    --app-drop-link 495 180 \
    --hide-extension "$APP_NAME.app" \
    --hdiutil-quiet \
    "$DMG_PATH" \
    "$APP_PATH"
rc=$?
set -e
if [ $rc -ne 0 ] && [ $rc -ne 2 ]; then
    exit $rc
fi

echo ">> Built $DMG_PATH"
