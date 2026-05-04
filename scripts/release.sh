#!/usr/bin/env bash
# Cut a new release: bump version, commit, tag, push.
#
# Build + sign + notarise + GitHub Release publication run on CI when the
# tag lands (.github/workflows/release.yml). This script's only job is to
# move the version forward and trigger the workflow.
#
# Usage:
#   ./scripts/release.sh                # patch bump (0.1.9 -> 0.1.10)
#   ./scripts/release.sh patch
#   ./scripts/release.sh minor          # 0.1.9 -> 0.2.0
#   ./scripts/release.sh major          # 0.1.9 -> 1.0.0
#   ./scripts/release.sh 0.3.1          # explicit version
#   ./scripts/release.sh patch --no-push  # commit + tag locally only
#
# What it does:
#   1. Refuse to run with a dirty tree or off master.
#   2. Bump Cargo.toml workspace version + bundle/Info.plist.
#   3. Refresh Cargo.lock so workspace crates pick up the new version.
#   4. Verify the version sources are in lockstep.
#   5. Commit "release vX.Y.Z" and create the matching annotated tag.
#   6. Push the commit + tag (skip with --no-push). CI takes over from here.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

bump_kind="patch"
push=1
for arg in "$@"; do
    case "$arg" in
        --no-push) push=0 ;;
        --help|-h)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) bump_kind="$arg" ;;
    esac
done

if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "error: working tree is dirty — commit or stash first" >&2
    exit 1
fi

current_branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$current_branch" != "master" ] && [ "$current_branch" != "main" ]; then
    echo "error: must release from master/main, currently on '$current_branch'" >&2
    exit 1
fi

current_version="$(grep -m1 '^version' Cargo.toml | sed -E 's/version *= *"([^"]+)".*/\1/')"
if [ -z "$current_version" ]; then
    echo "error: could not parse current version from Cargo.toml" >&2
    exit 1
fi

# Compute next version. Explicit "X.Y.Z" wins over keywords.
case "$bump_kind" in
    patch|minor|major)
        IFS='.' read -r maj min pat <<< "$current_version"
        case "$bump_kind" in
            patch) pat=$((pat + 1)) ;;
            minor) min=$((min + 1)); pat=0 ;;
            major) maj=$((maj + 1)); min=0; pat=0 ;;
        esac
        new_version="$maj.$min.$pat"
        ;;
    [0-9]*.[0-9]*.[0-9]*)
        new_version="$bump_kind"
        ;;
    *)
        echo "error: bump must be patch|minor|major or X.Y.Z, got '$bump_kind'" >&2
        exit 2
        ;;
esac

echo ">> Releasing $current_version -> $new_version"

# Cargo.toml — only the workspace.package version line, not stray deps.
sed -i '' -E "s/^(version *= *)\"$current_version\"/\1\"$new_version\"/" Cargo.toml

# Info.plist — CFBundleShortVersionString sits on the line after its <key>.
python3 - "$ROOT/bundle/Info.plist" "$new_version" <<'PY'
import sys, re
path, new_version = sys.argv[1], sys.argv[2]
with open(path) as f:
    content = f.read()
content = re.sub(
    r'(<key>CFBundleShortVersionString</key>\s*<string>)[^<]+(</string>)',
    rf'\g<1>{new_version}\g<2>',
    content,
    count=1,
)
with open(path, 'w') as f:
    f.write(content)
PY

# Refresh Cargo.lock for workspace members.
cargo update --workspace >/dev/null

# Sanity-check the lockstep before tagging — saves a doomed CI run if
# something drifted.
./bundle/verify-version.sh

git add Cargo.toml Cargo.lock bundle/Info.plist
git commit -m "release v$new_version"
git tag -a "v$new_version" -m "v$new_version"

if [ "$push" -eq 1 ]; then
    git push origin "$current_branch"
    git push origin "v$new_version"
    cat <<EOF

>> Pushed v$new_version. CI will build, sign, notarise and publish.
   Track it: gh run watch
   Release page: gh release view "v$new_version" --web
EOF
else
    cat <<EOF

>> Local commit + tag for v$new_version ready (not pushed).
   When ready:
     git push origin $current_branch
     git push origin v$new_version
EOF
fi
