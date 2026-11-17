#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD=1
INSTALL=1
for arg in "$@"; do
    case "$arg" in
        --skip-build) BUILD=0 ;;
        --no-install) INSTALL=0 ;;
        *) printf 'Unknown option: %s\n' "$arg" >&2; exit 2 ;;
    esac
done
if [[ "$(uname -s)" != Darwin ]]; then
    printf 'This package requires macOS.\n' >&2
    exit 1
fi
if [[ "$BUILD" == 1 ]]; then
    (cd "$ROOT" && cargo build --release --locked)
fi

BINARY="$ROOT/target/release/visualize-ssd"
BUNDLE="$ROOT/target/release/bundle/Visualize SSD.app"
[[ -x "$BINARY" ]] || { printf 'Release executable is missing: %s\n' "$BINARY" >&2; exit 1; }
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "$BINARY" "$BUNDLE/Contents/MacOS/visualize-ssd"
cp "$ROOT/assets/Info.plist" "$BUNDLE/Contents/Info.plist"
cp "$ROOT/assets/AppIcon.icns" "$BUNDLE/Contents/Resources/AppIcon.icns"
printf 'APPL????' > "$BUNDLE/Contents/PkgInfo"
# LaunchServices uses the bundle modification date to invalidate icon metadata.
# The reusable build directory otherwise retains its first-build timestamp.
touch "$BUNDLE"
/usr/bin/plutil -lint "$BUNDLE/Contents/Info.plist"
# A certificate-backed designated requirement preserves Full Disk Access across
# rebuilds. Ad-hoc signatures are tied to a changing executable hash.
SIGNING_IDENTITY="${CODESIGN_IDENTITY:-}"
if [[ -z "$SIGNING_IDENTITY" ]]; then
    SIGNING_IDENTITY="$(/usr/bin/security find-identity -v -p codesigning | /usr/bin/awk '/"Apple Development:/ { print $2; exit }')"
    SIGNING_IDENTITY="${SIGNING_IDENTITY:--}"
fi
/usr/bin/codesign --force --sign "$SIGNING_IDENTITY" "$BUNDLE"
/usr/bin/codesign --verify --deep --strict "$BUNDLE"

if [[ "$INSTALL" == 1 ]]; then
    STAGE="$(mktemp -d /Applications/.visualize-ssd.XXXXXX)"
    DEST='/Applications/Visualize SSD.app'
    PREVIOUS="$STAGE/previous.app"
    NEW_PLACED=0
    cleanup() {
        status=$?
        trap - EXIT
        if [[ "$status" != 0 ]]; then
            if [[ "$NEW_PLACED" == 1 ]] && ! rm -rf -- "$DEST"; then
                printf 'Installation failed; recovery files remain at: %s\n' "$STAGE" >&2
                exit "$status"
            fi
            if [[ -e "$PREVIOUS" ]] && ! mv "$PREVIOUS" "$DEST"; then
                printf 'Rollback requires recovery from: %s\n' "$PREVIOUS" >&2
                exit "$status"
            fi
        fi
        if ! rm -rf -- "$STAGE"; then
            printf 'Could not remove installation staging: %s\n' "$STAGE" >&2
            [[ "$status" != 0 ]] || status=1
        fi
        exit "$status"
    }
    trap cleanup EXIT
    /usr/bin/ditto "$BUNDLE" "$STAGE/Visualize SSD.app"
    /usr/bin/codesign --verify --deep --strict "$STAGE/Visualize SSD.app"
    if [[ -e "$DEST" || -L "$DEST" ]]; then
        [[ ! -L "$DEST" && "$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - "$DEST/Contents/Info.plist")" == 'com.visualizessd.app' ]] || {
            printf 'Refusing to replace an unrelated application: %s\n' "$DEST" >&2
            exit 1
        }
        mv "$DEST" "$PREVIOUS"
    fi
    NEW_PLACED=1
    mv "$STAGE/Visualize SSD.app" "$DEST"
    /usr/bin/codesign --verify --deep --strict "$DEST"
    /usr/bin/plutil -lint "$DEST/Contents/Info.plist"
    # Remove only our former visible upgrade backups after the new app passes
    # verification. The current rollback bundle remains hidden until cleanup.
    shopt -s nullglob
    LSREGISTER='/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'
    REMOVED=0
    # Iterate the glob directly: Bash 3.2 treats an empty array as unset under -u.
    for legacy in /Applications/Visualize\ SSD.previous.*.app; do
        if [[ -d "$legacy" && ! -L "$legacy" ]] &&
            [[ "$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - "$legacy/Contents/Info.plist" 2>/dev/null || true)" == 'com.visualizessd.app' ]]; then
            if [[ -x "$LSREGISTER" ]]; then
                "$LSREGISTER" -u "$legacy" >/dev/null 2>&1 ||
                    printf 'Could not unregister old app entry: %s\n' "$legacy" >&2
            fi
            rm -rf -- "$legacy"
            REMOVED=$((REMOVED + 1))
        fi
    done
    if [[ -x "$LSREGISTER" ]]; then
        "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 ||
            printf 'Could not refresh app registration: %s\n' "$DEST" >&2
    fi
    printf 'Installed: %s\n' "$DEST"
    printf 'Removed old app versions: %s\n' "$REMOVED"
else
    printf 'Packaged: %s\n' "$BUNDLE"
fi
