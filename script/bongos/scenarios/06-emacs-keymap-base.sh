# Scenario for feature 06-emacs-keymap-base. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so nothing here writes `base_keymap`: the base flow pinned it to
# VSCode (its keys are VS Code keys) in global_settings.json, and the scenario drops that pin.
#   1. Defaults: M-x opens the command palette in the strip (the bottom rows change) and C-g closes
#      it; in the editor C-k kills the first line's text and C-x C-s saves it, which the file shows;
#      emacs-default.png shows the block cursor.
#   2. Off: settings.json says `"base_keymap": "VSCode"`; the running app reloads, M-x changes nothing
#      at the bottom of the window; vscode-default.png shows the bar cursor.
# The scenario leaves the app quit and restores settings.json and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 06-emacs-keymap-base skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
# Write in place: a rename over the file would end the app's watch on it after the first write.
atomic_write() {
  printf '%s\n' "$2" >"$1"
}
GLOBAL_FILE="$CONFIG_DIR/global_settings.json"
STRIP_ROWS=40

command -v import >/dev/null || { echo "bongos: ImageMagick is not installed; the scenario needs it" >&2; exit 1; }

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# The md5 of the bottom STRIP_ROWS rows of the app window, where the strip and the status bar are.
bottom_hash() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  DISPLAY="$BONGOS_DISPLAY" import -window root \
    -crop "${WIDTH}x${STRIP_ROWS}+${X}+$((Y + HEIGHT - STRIP_ROWS))" rgb:- | md5sum
}

line_killed() { ! grep -q "fun main" "$RUN/project/Main.kt" && grep -q smoke "$RUN/project/Main.kt"; }

# ---------------------------------------------------------------------------------------------
step "Emacs base: the default, then switched off"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-06.json"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-06.json"
if grep -q '"base_keymap"' "$SETTINGS_FILE"; then
  fail "settings.json already names a base keymap, so the default is not tested: $(cat "$SETTINGS_FILE")"
fi
grep -v '"base_keymap"' "$RUN/config-backup/global-06.json" >"$GLOBAL_FILE"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

closed=$(bottom_hash)
act keys alt+x
act type_text "toggle"
open=$(bottom_hash)
[ "$open" != "$closed" ] || fail "M-x changed nothing at the bottom of the window with the default keymap"
act keys ctrl+g
[ "$(bottom_hash)" != "$open" ] || fail "C-g did not close the prompt"

keys ctrl+Home
sleep 1
act keys ctrl+k
keys ctrl+x ctrl+s
poll 30 "C-k and C-x C-s to change the file" line_killed
shot emacs-default
say "smoke: screenshot $RUN/emacs-default.png"

atomic_write "$SETTINGS_FILE" '{ "base_keymap": "VSCode" }'
sleep 3
wait_for_settled
closed=$(bottom_hash)
keys alt+x
sleep 2
[ "$(bottom_hash)" = "$closed" ] || fail "M-x still opens a prompt after base_keymap was set to VSCode"
shot vscode-default
say "smoke: screenshot $RUN/vscode-default.png"

quit_app
cp "$RUN/config-backup/settings-06.json" "$SETTINGS_FILE"
cp "$RUN/config-backup/global-06.json" "$GLOBAL_FILE"
