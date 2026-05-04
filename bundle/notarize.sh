#!/usr/bin/env bash
# Submit a .app or .dmg to Apple's notary service and staple the resulting
# ticket back into the artifact. Blocks until the submission completes.
#
# Usage:
#   ./bundle/notarize.sh dist/LeSwitcheur.app
#   ./bundle/notarize.sh dist/LeSwitcheur.dmg
#
# Requirements:
#   - The artifact must already be signed with a Developer ID cert under
#     hardened runtime (see bundle.sh) — Apple rejects ad-hoc / unhardened
#     binaries.
#   - Notarytool credentials, supplied one of two ways:
#       a) (local) keychain profile name in $NOTARYTOOL_PROFILE (default
#          "leswitcheur-notary"). Create once via:
#            xcrun notarytool store-credentials "leswitcheur-notary" \
#                --apple-id <email> --team-id <TEAMID> --password <app-pwd>
#       b) (CI) inline env vars: $NOTARY_APPLE_ID + $NOTARY_TEAM_ID +
#          $NOTARY_PASSWORD. Used when all three are set; takes precedence
#          over the keychain profile.

set -euo pipefail

ARTIFACT="${1:-}"
if [ -z "$ARTIFACT" ] || [ ! -e "$ARTIFACT" ]; then
    echo "usage: $0 <path-to-.app-or-.dmg>" >&2
    exit 2
fi

if [ -n "${NOTARY_APPLE_ID:-}" ] && [ -n "${NOTARY_TEAM_ID:-}" ] && [ -n "${NOTARY_PASSWORD:-}" ]; then
    AUTH_ARGS=(--apple-id "$NOTARY_APPLE_ID" --team-id "$NOTARY_TEAM_ID" --password "$NOTARY_PASSWORD")
else
    AUTH_ARGS=(--keychain-profile "${NOTARYTOOL_PROFILE:-leswitcheur-notary}")
fi

case "$ARTIFACT" in
    *.app)
        # notarytool only accepts .zip / .dmg / .pkg — ditto preserves the
        # bundle layout (resource forks, symlinks) where `zip` would mangle it.
        ZIP="${ARTIFACT%.app}.zip"
        rm -f "$ZIP"
        ditto -c -k --keepParent "$ARTIFACT" "$ZIP"
        echo ">> Submitting $ZIP to Apple..."
        xcrun notarytool submit "$ZIP" "${AUTH_ARGS[@]}" --wait
        rm -f "$ZIP"
        echo ">> Stapling $ARTIFACT"
        xcrun stapler staple "$ARTIFACT"
        xcrun stapler validate "$ARTIFACT"
        ;;
    *.dmg)
        echo ">> Submitting $ARTIFACT to Apple..."
        xcrun notarytool submit "$ARTIFACT" "${AUTH_ARGS[@]}" --wait
        echo ">> Stapling $ARTIFACT"
        xcrun stapler staple "$ARTIFACT"
        xcrun stapler validate "$ARTIFACT"
        ;;
    *)
        echo "error: unsupported artifact type: $ARTIFACT (need .app or .dmg)" >&2
        exit 2
        ;;
esac

echo ">> Notarised + stapled: $ARTIFACT"
