#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APP_DIR="$WORK/AppDir"
BOOTSTRAP="$APP_DIR/usr/share/rebellion2/bootstrap"
DATA_HOME="$WORK/data"
OUTPUT="$WORK/launcher-arguments"

mkdir -p "$BOOTSTRAP/game"
cp "$SCRIPT_DIR/AppRun" "$APP_DIR/AppRun"
cat > "$APP_DIR/AppRun.launcher" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$TEST_OUTPUT"
EOF
printf 'game-one' > "$BOOTSTRAP/game/Rebellion2.x86_64"
printf 'crash-one' > "$BOOTSTRAP/game/UnityCrashHandler64"
printf 'helper-one' > "$BOOTSTRAP/rebellion2-update-helper"
printf 'launcher-manifest-one' > "$BOOTSTRAP/.launcher-manifest.json"
printf 'launcher-one' > "$BOOTSTRAP/.launcher-version"
printf 'game-manifest-one' > "$BOOTSTRAP/.game-manifest.json"
printf 'game-one' > "$BOOTSTRAP/.game-version"
chmod +x "$APP_DIR/AppRun" "$APP_DIR/AppRun.launcher"

HOME="$WORK/home" \
XDG_DATA_HOME="$DATA_HOME" \
APPDIR="$APP_DIR" \
TEST_OUTPUT="$OUTPUT" \
  "$APP_DIR/AppRun" first second

INSTALL_DIR="$DATA_HOME/rebellion2"
test "$(cat "$INSTALL_DIR/Rebellion2.x86_64")" = game-one
test "$(cat "$INSTALL_DIR/.game-version")" = game-one
test "$(cat "$INSTALL_DIR/.launcher-version")" = launcher-one
if [[ "$(uname -s)" != MINGW* ]]; then
  test -x "$INSTALL_DIR/Rebellion2.x86_64"
  test -x "$INSTALL_DIR/UnityCrashHandler64"
  test -x "$INSTALL_DIR/rebellion2-update-helper"
fi
test "$(sed -n '1p' "$OUTPUT")" = first
test "$(sed -n '2p' "$OUTPUT")" = second

printf 'game-two' > "$BOOTSTRAP/game/Rebellion2.x86_64"
printf 'helper-two' > "$BOOTSTRAP/rebellion2-update-helper"
printf 'launcher-two' > "$BOOTSTRAP/.launcher-version"
printf 'game-two' > "$BOOTSTRAP/.game-version"
rm "$INSTALL_DIR/rebellion2-update-helper"

HOME="$WORK/home" \
XDG_DATA_HOME="$DATA_HOME" \
APPDIR="$APP_DIR" \
TEST_OUTPUT="$OUTPUT" \
  "$APP_DIR/AppRun" again

test "$(cat "$INSTALL_DIR/Rebellion2.x86_64")" = game-one
test "$(cat "$INSTALL_DIR/.game-version")" = game-one
test "$(cat "$INSTALL_DIR/.launcher-version")" = launcher-one
test "$(cat "$INSTALL_DIR/rebellion2-update-helper")" = helper-two
test "$(cat "$OUTPUT")" = again
