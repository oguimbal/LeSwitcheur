#!/usr/bin/env bash
# Assert Cargo.toml workspace version == Info.plist CFBundleShortVersionString,
# and (when run on a tagged CI build) == the git tag (stripped of leading "v").
#
# Invoked by bundle.sh on every bundle, and by the release workflow on every
# tag push. Fail-fast keeps the three sources of truth in lockstep.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/version *= *"([^"]+)".*/\1/')"
PLIST_VERSION="$(awk '
    /<key>CFBundleShortVersionString<\/key>/ { getline; gsub(/.*<string>|<\/string>.*/, ""); print; exit }
' "$ROOT/bundle/Info.plist")"

if [ -z "$CARGO_VERSION" ]; then
    echo "FATAL: could not parse workspace version from Cargo.toml" >&2
    exit 2
fi
if [ -z "$PLIST_VERSION" ]; then
    echo "FATAL: could not parse CFBundleShortVersionString from bundle/Info.plist" >&2
    exit 2
fi

if [ "$CARGO_VERSION" != "$PLIST_VERSION" ]; then
    echo "FATAL: version mismatch — Cargo.toml=$CARGO_VERSION  Info.plist=$PLIST_VERSION" >&2
    exit 1
fi

# Optional: verify against git tag when GITHUB_REF_NAME is set (CI tag push).
if [ "${GITHUB_REF_TYPE:-}" = "tag" ] && [ -n "${GITHUB_REF_NAME:-}" ]; then
    TAG_VERSION="${GITHUB_REF_NAME#v}"
    if [ "$TAG_VERSION" != "$CARGO_VERSION" ]; then
        echo "FATAL: tag $GITHUB_REF_NAME does not match Cargo.toml version $CARGO_VERSION" >&2
        exit 1
    fi
fi

echo "version ok: $CARGO_VERSION"
