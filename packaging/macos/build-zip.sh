#!/usr/bin/env bash
# Assemble the native launcher and Unity player into one draggable macOS app.
# Usage: build-zip.sh <launcher.app> <player-dir> <version> <output.zip>
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "Usage: $0 <launcher.app> <player-dir> <version> <output.zip>" >&2
  exit 2
fi

LAUNCHER_APP="$1"
PLAYER_DIR="$2"
VERSION="$3"
OUTFILE="$4"

if [ ! -d "$LAUNCHER_APP/Contents/MacOS" ]; then
  echo "Launcher app bundle not found at $LAUNCHER_APP" >&2
  exit 1
fi

GAME_APP="$(find "$PLAYER_DIR" -maxdepth 1 -type d -name '*.app' -print -quit)"
if [ -z "$GAME_APP" ]; then
  echo "No Unity .app bundle found in $PLAYER_DIR" >&2
  exit 1
fi
if [ ! -d "$GAME_APP/Contents/MacOS" ]; then
  echo "Unity app bundle has no Contents/MacOS directory: $GAME_APP" >&2
  exit 1
fi

case "$OUTFILE" in
  /*) ;;
  *) OUTFILE="$PWD/$OUTFILE" ;;
esac

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

APP="$WORK/Rebellion2.app"
GAME_DESTINATION="$APP/Contents/Resources/Rebellion2 Game.app"

copy_bundle() {
  local source="$1"
  local destination="$2"
  if command -v ditto >/dev/null 2>&1; then
    ditto "$source" "$destination"
  else
    cp -a "$source" "$destination"
  fi
}

copy_bundle "$LAUNCHER_APP" "$APP"
mkdir -p "$(dirname "$GAME_DESTINATION")"
copy_bundle "$GAME_APP" "$GAME_DESTINATION"

# Artifact downloads do not preserve executable bits. Restore the launcher and
# Unity entry points before creating the distributable archive.
find "$APP/Contents/MacOS" -maxdepth 1 -type f -exec chmod +x {} \;
find "$GAME_DESTINATION/Contents/MacOS" -maxdepth 1 -type f -exec chmod +x {} \;

# The player was produced on Linux and the launcher bundle has just been changed,
# so give both bundles valid ad-hoc signatures. Apple notarization can replace
# these when release signing is introduced.
if command -v codesign >/dev/null 2>&1; then
  xattr -cr "$APP"
  codesign --force --deep --sign - "$GAME_DESTINATION"
  codesign --force --deep --sign - "$APP"
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
