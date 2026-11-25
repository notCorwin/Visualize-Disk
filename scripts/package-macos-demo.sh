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

BINARY="$ROOT/target/release/visualize-disk"
BUNDLE_DIR="$ROOT/target/release/bundle"
BUNDLE_NAME='Visualize Disk Demo.app'
BUNDLE="$BUNDLE_DIR/$BUNDLE_NAME"
IDENTIFIER='com.visualizedisk.demo'
[[ -x "$BINARY" ]] || { printf 'Release executable is missing: %s\n' "$BINARY" >&2; exit 1; }
mkdir -p "$BUNDLE_DIR"
WORK="$(mktemp -d "$BUNDLE_DIR/.visualize-disk-demo.XXXXXX")"
STAGED="$WORK/$BUNDLE_NAME"
cleanup_work() {
    status=$?
    trap - EXIT
    if [[ -d "$WORK" ]] && ! /usr/bin/trash "$WORK"; then
        printf 'Could not move build staging to Trash: %s\n' "$WORK" >&2
        [[ "$status" != 0 ]] || status=1
    fi
    exit "$status"
}
trap cleanup_work EXIT
mkdir -p "$STAGED/Contents/MacOS" "$STAGED/Contents/Resources"
cp "$BINARY" "$STAGED/Contents/MacOS/visualize-disk-demo"
cp "$ROOT/assets/Info.plist" "$STAGED/Contents/Info.plist"
cp "$ROOT/assets/AppIcon.icns" "$STAGED/Contents/Resources/AppIcon.icns"
printf 'APPL????' > "$STAGED/Contents/PkgInfo"
PLIST="$STAGED/Contents/Info.plist"
/usr/libexec/PlistBuddy -c 'Set :CFBundleExecutable visualize-disk-demo' "$PLIST"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $IDENTIFIER" "$PLIST"
/usr/libexec/PlistBuddy -c 'Set :CFBundleDisplayName Visualize Disk Demo' "$PLIST"
/usr/libexec/PlistBuddy -c 'Set :CFBundleName Visualize Disk Demo' "$PLIST"
/usr/bin/plutil -lint "$PLIST"
SIGNING_IDENTITY="${CODESIGN_IDENTITY:-}"
if [[ -z "$SIGNING_IDENTITY" ]]; then
    SIGNING_IDENTITY="$(/usr/bin/security find-identity -v -p codesigning | /usr/bin/awk '/"Apple Development:/ { print $2; exit }')"
    SIGNING_IDENTITY="${SIGNING_IDENTITY:--}"
fi
/usr/bin/codesign --force --sign "$SIGNING_IDENTITY" "$STAGED"
/usr/bin/codesign --verify --deep --strict "$STAGED"
if [[ -e "$BUNDLE" || -L "$BUNDLE" ]]; then
    [[ ! -L "$BUNDLE" && "$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - "$BUNDLE/Contents/Info.plist" 2>/dev/null || true)" == "$IDENTIFIER" ]] || {
        printf 'Refusing to replace an unrelated bundle: %s\n' "$BUNDLE" >&2
        exit 1
    }
    /usr/bin/trash "$BUNDLE"
fi
mv "$STAGED" "$BUNDLE"
touch "$BUNDLE"
/usr/bin/trash "$WORK"
trap - EXIT

if [[ "$INSTALL" == 1 ]]; then
    STAGE="$(mktemp -d /Applications/.visualize-disk-demo.XXXXXX)"
    DEST="/Applications/$BUNDLE_NAME"
    STAGED="$STAGE/$BUNDLE_NAME"
    PREVIOUS="$STAGE/previous.app"
    NEW_PLACED=0
    cleanup_install() {
        status=$?
        trap - EXIT
        if [[ "$status" != 0 ]]; then
            if [[ "$NEW_PLACED" == 1 ]] && ! /usr/bin/trash "$DEST"; then
                printf 'Installation failed; recovery files remain at: %s\n' "$STAGE" >&2
                exit "$status"
            fi
            if [[ -e "$PREVIOUS" ]] && ! mv "$PREVIOUS" "$DEST"; then
                printf 'Rollback requires recovery from: %s\n' "$PREVIOUS" >&2
                exit "$status"
            fi
        fi
        if [[ -d "$STAGE" ]] && ! /usr/bin/trash "$STAGE"; then
            printf 'Could not move installation staging to Trash: %s\n' "$STAGE" >&2
            [[ "$status" != 0 ]] || status=1
        fi
        exit "$status"
    }
    trap cleanup_install EXIT
    /usr/bin/ditto "$BUNDLE" "$STAGED"
    /usr/bin/codesign --verify --deep --strict "$STAGED"
    if [[ -e "$DEST" || -L "$DEST" ]]; then
        [[ ! -L "$DEST" && "$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - "$DEST/Contents/Info.plist" 2>/dev/null || true)" == "$IDENTIFIER" ]] || {
            printf 'Refusing to replace an unrelated application: %s\n' "$DEST" >&2
            exit 1
        }
        mv "$DEST" "$PREVIOUS"
    fi
    mv "$STAGED" "$DEST"
    NEW_PLACED=1
    /usr/bin/codesign --verify --deep --strict "$DEST"
    /usr/bin/plutil -lint "$DEST/Contents/Info.plist"
    LSREGISTER='/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'
    LEGACY='/Applications/Visualize SSD Demo.app'
    if [[ -d "$LEGACY" && ! -L "$LEGACY" ]] &&
        /usr/bin/codesign --verify --deep --strict -R='identifier "com.visualizessd.demo"' "$LEGACY" 2>/dev/null; then
        if [[ -x "$LSREGISTER" ]]; then
            "$LSREGISTER" -u "$LEGACY" >/dev/null 2>&1 ||
                printf 'Could not unregister old app entry: %s\n' "$LEGACY" >&2
        fi
        /usr/bin/trash "$LEGACY" || printf 'Could not move old app to Trash: %s\n' "$LEGACY" >&2
    fi
    if [[ -x "$LSREGISTER" ]]; then
        "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 ||
            printf 'Could not refresh app registration: %s\n' "$DEST" >&2
    fi
    printf 'Installed: %s\n' "$DEST"
else
    printf 'Packaged: %s\n' "$BUNDLE"
fi
