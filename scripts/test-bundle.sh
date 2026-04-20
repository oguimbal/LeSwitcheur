#!/usr/bin/env bash
# Test a production build.
#
# Rebuilds signed with the local self-signed identity (override via
# $CODESIGN_IDENTITY), prints the resulting signature, then launches the app.
# Saved settings are preserved across runs by default — pass --reset to wipe
# them and exercise the first-launch flow (onboarding wizard, default config).
#
# Pass --attach to run the executable inline instead of `open`, so logs
# stream to the terminal.
#
# Usage: ./scripts/test-bundle.sh [--reset] [--attach]

set -euo pipefail

RESET=0
ATTACH=0
for arg in "$@"; do
    case "$arg" in
        --reset) RESET=1 ;;
        --attach) ATTACH=1 ;;
        -h|--help)
            sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unknown argument '$arg' (use --reset, --attach or --help)" >&2
            exit 2
            ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

CONFIG="$HOME/Library/Application Support/fr.gmbl.LeSwitcheur/config.toml"
APP="$ROOT/dist/LeSwitcheur.app"

echo ">> Killing any running instance"
pkill -x switcheur 2>/dev/null || true

if [ "$RESET" -eq 1 ]; then
    echo ">> --reset: wiping saved settings + stale bundle"
    rm -f "$CONFIG"
fi
rm -rf "$APP"

CODESIGN_IDENTITY="${CODESIGN_IDENTITY:-LeSwitcher Code Signing}" \
    "$ROOT/bundle/bundle.sh"

echo ">> Signature:"
codesign -dv --verbose=4 "$APP" 2>&1 \
    | grep -E "Identifier|Authority|TeamIdentifier" || true

if [ "$ATTACH" -eq 1 ]; then
    exec env RUST_LOG="${RUST_LOG:-debug}" "$APP/Contents/MacOS/switcheur"
else
    open "$APP"
fi
