#!/usr/bin/env bash
# Package a Unity StandaloneOSX build into a distributable zip.
# A zip (not a .dmg) is used deliberately: while the app is unsigned, a proper .dmg
# would need macOS' hdiutil, which is unavailable on the Linux runner. Switch to a
# signed/notarized .dmg on a macOS runner once code signing is in place.
# Usage: build-zip.sh <build-dir> <version> <output.zip>
set -euo pipefail

SRCDIR="$1"
VERSION="$2"
OUTFILE="$3"
OUTFILE="$(readlink -m "$OUTFILE")"

APP="$(find "$SRCDIR" -maxdepth 1 -name '*.app' | head -n1)"
if [ -z "$APP" ]; then
  echo "No .app bundle found in $SRCDIR" >&2
  exit 1
fi

# The main executable ships without +x when the bundle is produced off-Mac.
if [ -d "$APP/Contents/MacOS" ]; then
  find "$APP/Contents/MacOS" -maxdepth 1 -type f -exec chmod +x {} \;
fi

( cd "$(dirname "$APP")" && zip -r -y -X "$OUTFILE" "$(basename "$APP")" >/dev/null )
echo "Built $OUTFILE"
