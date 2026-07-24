#!/usr/bin/env bash
# Package a Unity StandaloneLinux64 build into a portable AppImage.
# Usage: build-appimage.sh <build-dir> <version> <output.AppImage>
# Requires: appimagetool on PATH, imagemagick (only when no real icon is present).
set -euo pipefail

SRCDIR="$1"
VERSION="$2"
OUTFILE="$3"
SCRIPTDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTFILE="$(readlink -m "$OUTFILE")"

EXE="$(find "$SRCDIR" -maxdepth 1 -type f -name '*.x86_64' | head -n1)"
if [ -z "$EXE" ]; then
  echo "No *.x86_64 executable found in $SRCDIR" >&2
  exit 1
fi
EXE_NAME="$(basename "$EXE")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/Rebellion2.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp -a "$SRCDIR"/. "$APPDIR/usr/bin/"
chmod +x "$APPDIR/usr/bin/$EXE_NAME"

# .desktop must sit at the AppDir root (and is mirrored under usr/share for desktop integration).
sed "s/^Exec=.*/Exec=$EXE_NAME/" "$SCRIPTDIR/rebellion2.desktop" > "$APPDIR/rebellion2.desktop"
cp "$APPDIR/rebellion2.desktop" "$APPDIR/usr/share/applications/rebellion2.desktop"

# Real icon if the repo ships one; otherwise a solid placeholder so appimagetool is satisfied.
ICON="$SCRIPTDIR/rebellion2.png"
if [ ! -f "$ICON" ]; then
  ICON="$WORK/rebellion2.png"
  convert -size 256x256 xc:'#141428' "$ICON"
fi
cp "$ICON" "$APPDIR/rebellion2.png"
cp "$ICON" "$APPDIR/usr/share/icons/hicolor/256x256/apps/rebellion2.png"

cat > "$APPDIR/AppRun" <<EOF
#!/bin/bash
HERE="\$(dirname "\$(readlink -f "\${0}")")"
exec "\${HERE}/usr/bin/$EXE_NAME" "\$@"
EOF
chmod +x "$APPDIR/AppRun"

export ARCH=x86_64
export VERSION
appimagetool "$APPDIR" "$OUTFILE"
echo "Built $OUTFILE"
