#!/usr/bin/env bash
# Assemble the launcher, Unity player, and installed-version metadata into one AppImage.
set -euo pipefail

if [ "$#" -ne 8 ]; then
  echo "Usage: $0 <launcher.AppImage> <update-helper> <game-dir> <launcher-manifest> <launcher-version> <game-manifest> <game-version> <output.AppImage>" >&2
  exit 2
fi

LAUNCHER_APPIMAGE="$1"
UPDATE_HELPER="$2"
GAME_DIR="$3"
LAUNCHER_MANIFEST="$4"
LAUNCHER_VERSION="$5"
GAME_MANIFEST="$6"
GAME_VERSION="$7"
OUTPUT="$8"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APPIMAGETOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"

require_file() {
  if [ ! -f "$1" ]; then
    echo "Required file not found: $1" >&2
    exit 1
  fi
}

require_file "$LAUNCHER_APPIMAGE"
require_file "$UPDATE_HELPER"
require_file "$LAUNCHER_MANIFEST"
require_file "$GAME_MANIFEST"
require_file "$GAME_DIR/Rebellion2.x86_64"
require_file "$SCRIPT_DIR/AppRun"
if [ -z "$APPIMAGETOOL" ] || [ ! -f "$APPIMAGETOOL" ]; then
  echo "Set APPIMAGETOOL to the appimagetool executable." >&2
  exit 1
fi

case "$OUTPUT" in
  /*) ;;
  *) OUTPUT="$PWD/$OUTPUT" ;;
esac

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
LAUNCHER_COPY="$WORK/Rebellion2-launcher-Linux.AppImage"
cp "$LAUNCHER_APPIMAGE" "$LAUNCHER_COPY"
chmod +x "$LAUNCHER_COPY" "$APPIMAGETOOL"

RUNTIME_OFFSET="$($LAUNCHER_COPY --appimage-offset)"
if ! [[ "$RUNTIME_OFFSET" =~ ^[0-9]+$ ]] || [ "$RUNTIME_OFFSET" -le 0 ]; then
  echo "The launcher did not report a valid AppImage runtime offset." >&2
  exit 1
fi
head -c "$RUNTIME_OFFSET" "$LAUNCHER_COPY" > "$WORK/runtime-x86_64"

(
  cd "$WORK"
  "$LAUNCHER_COPY" --appimage-extract >/dev/null
)
APP_DIR="$WORK/squashfs-root"
require_file "$APP_DIR/AppRun"
mv "$APP_DIR/AppRun" "$APP_DIR/AppRun.launcher"
install -m 0755 "$SCRIPT_DIR/AppRun" "$APP_DIR/AppRun"

BOOTSTRAP="$APP_DIR/usr/share/rebellion2/bootstrap"
mkdir -p "$BOOTSTRAP/game"
cp -a "$GAME_DIR"/. "$BOOTSTRAP/game/"
install -m 0755 "$UPDATE_HELPER" "$BOOTSTRAP/rebellion2-update-helper"
install -m 0644 "$LAUNCHER_MANIFEST" "$BOOTSTRAP/.launcher-manifest.json"
install -m 0644 "$GAME_MANIFEST" "$BOOTSTRAP/.game-manifest.json"
printf '%s' "$LAUNCHER_VERSION" > "$BOOTSTRAP/.launcher-version"
printf '%s' "$GAME_VERSION" > "$BOOTSTRAP/.game-version"
chmod +x "$BOOTSTRAP/game/Rebellion2.x86_64"
if [ -f "$BOOTSTRAP/game/UnityCrashHandler64" ]; then
  chmod +x "$BOOTSTRAP/game/UnityCrashHandler64"
fi

mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"
ARCH=x86_64 \
VERSION="$GAME_VERSION" \
APPIMAGETOOL_RUNTIME_FILE="$WORK/runtime-x86_64" \
APPIMAGE_EXTRACT_AND_RUN=1 \
  "$APPIMAGETOOL" "$APP_DIR" "$OUTPUT"
chmod +x "$OUTPUT"
echo "Built Rebellion 2 $GAME_VERSION at $OUTPUT"
