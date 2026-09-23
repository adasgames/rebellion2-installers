#!/usr/bin/env bash
# Package the complete, signed Tauri + Unity application as a draggable zip.
# Usage: build-zip.sh <application.app> <version> <output.zip>
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "Usage: $0 <application.app> <version> <output.zip>" >&2
  exit 2
fi

SOURCE_APP="$1"
VERSION="$2"
OUTFILE="$3"

if [ ! -d "$SOURCE_APP/Contents/MacOS" ]; then
  echo "Application bundle not found at $SOURCE_APP" >&2
  exit 1
fi

GAME_APP="$SOURCE_APP/Contents/Resources/Rebellion2 Game.app"
if [ ! -d "$GAME_APP/Contents/MacOS" ]; then
  echo "Complete application does not contain the Unity player: $GAME_APP" >&2
  exit 1
fi

case "$OUTFILE" in
  /*) ;;
  *) OUTFILE="$PWD/$OUTFILE" ;;
esac

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

APP="$WORK/Rebellion2.app"

copy_bundle() {
  local source="$1"
  local destination="$2"
  if command -v ditto >/dev/null 2>&1; then
    ditto "$source" "$destination"
  else
    cp -a "$source" "$destination"
  fi
}

copy_bundle "$SOURCE_APP" "$APP"

# The updater archive and direct-download zip must contain the exact same signed
# bundle. Do not modify it after Tauri creates the updater artifact.
if command -v codesign >/dev/null 2>&1; then
  codesign --verify --deep --strict "$APP"
fi

mkdir -p "$(dirname "$OUTFILE")"
rm -f "$OUTFILE"
if command -v ditto >/dev/null 2>&1; then
  ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUTFILE"
else
  (cd "$WORK" && zip -r -y -X "$OUTFILE" "$(basename "$APP")" >/dev/null)
fi

echo "Built Rebellion 2 $VERSION at $OUTFILE"
