#!/usr/bin/env bash
# Test a production build from scratch.
#
# Wipes saved settings + any stale bundle, rebuilds signed with the local
# self-signed identity (override via $CODESIGN_IDENTITY), prints the resulting
# signature, then launches the app.
#
# Usage: ./scripts/test-bundle.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

CONFIG="$HOME/Library/Application Support/fr.gmbl.LeSwitcheur/config.toml"
APP="$ROOT/dist/LeSwitcheur.app"

rm -f "$CONFIG"
rm -rf "$APP"

CODESIGN_IDENTITY="${CODESIGN_IDENTITY:-LeSwitcheur Code Signing}" \
    "$ROOT/bundle/bundle.sh"

echo ">> Signature:"
codesign -dv --verbose=4 "$APP" 2>&1 \
    | grep -E "Identifier|Authority|TeamIdentifier" || true

open "$APP"
