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
    BACKUP=''
    cleanup() {
        status=$?
        if [[ "$status" != 0 && -n "$BACKUP" && ! -e "$DEST" ]]; then
            mv "$BACKUP" "$DEST"
        fi
        rm -rf "$STAGE"
        exit "$status"
    }
    trap cleanup EXIT
    /usr/bin/ditto "$BUNDLE" "$STAGE/Visualize SSD.app"
    /usr/bin/codesign --verify --deep --strict "$STAGE/Visualize SSD.app"
    if [[ -e "$DEST" ]]; then
        BACKUP="/Applications/Visualize SSD.previous.$(date +%Y%m%d-%H%M%S).$$.app"
        mv "$DEST" "$BACKUP"
    fi
    mv "$STAGE/Visualize SSD.app" "$DEST"
    /usr/bin/codesign --verify --deep --strict "$DEST"
    printf 'Installed: %s\n' "$DEST"
    [[ -z "$BACKUP" ]] || printf 'Previous version preserved: %s\n' "$BACKUP"
else
    printf 'Packaged: %s\n' "$BUNDLE"
fi
