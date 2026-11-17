# Scenario for feature 42-keybinding-profiles. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh.
#
# The base flow pinned `base_keymap` to VSCode in global_settings.json; the scenario drops that pin
# and switches the profile through settings.json while the app runs. keymap.json holds one user
# binding of the scenario and must be byte for byte the same at the end.
#   1. Vim: `d d` then `:w` Enter removes the first line of Main.kt; vim-normal.png shows the block
#      cursor and the mode indicator, `i` gives vim-insert.png with the bar cursor; `:` opens the
#      command line (the bottom rows change) and Escape closes it.
#   2. VS Code: ctrl+p and ctrl+shift+p each open a prompt and Escape closes it; vscode.png.
#   3. JetBrains is retired: the app keeps running, retired.png shows the notice, and settings.json
#      still says "JetBrains".
#   4. Emacs: alt+x opens the command prompt.
# The scenario leaves the app quit and restores the three config files.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 42-keybinding-profiles skipped under BONGOS_STOCK=1: the stock run has no keybinding profiles"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
GLOBAL_FILE="$CONFIG_DIR/global_settings.json"
KEYMAP_FILE="$CONFIG_DIR/keymap.json"
# Write in place: a rename over the file would end the app's watch on it after the first write.
atomic_write() {
  printf '%s\n' "$2" >"$1"
}
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

# switch_profile <json>: write settings.json and let the running app reload it.
switch_profile() {
  atomic_write "$SETTINGS_FILE" "$1"
  sleep 3
  wait_for_settled
}

# opens_and_closes <label> <keys...>: the keys change the bottom rows, Escape restores them.
opens_and_closes() {
  local label=$1 before open
  shift
  before=$(bottom_hash)
  act keys "$@"
  open=$(bottom_hash)
  [ "$open" != "$before" ] || fail "$label changed nothing at the bottom of the window"
  act keys Escape
  [ "$(bottom_hash)" = "$before" ] || fail "Escape did not restore the bottom rows after $label"
}

# ---------------------------------------------------------------------------------------------
step "keybinding profiles: Vim, VS Code, retired JetBrains, Emacs"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-42.json"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-42.json"
cp "$KEYMAP_FILE" "$RUN/config-backup/keymap-42.json"
grep -v '"base_keymap"' "$RUN/config-backup/global-42.json" >"$GLOBAL_FILE"
atomic_write "$KEYMAP_FILE" '[{ "context": "Editor", "bindings": { "ctrl-alt-f9": "editor::MoveToEnd" } }]'
keymap_md5=$(md5sum <"$KEYMAP_FILE")
atomic_write "$SETTINGS_FILE" '{ "base_keymap": "Vim" }'

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# Vim
keys ctrl+Home
sleep 1
keys d d
keys colon w Return
poll 30 "dd and :w to remove the first line and save" line_killed
shot vim-normal
say "smoke: screenshot $RUN/vim-normal.png"
act keys i
shot vim-insert
say "smoke: screenshot $RUN/vim-insert.png"
act keys Escape
opens_and_closes "Vim ':'" colon

# VS Code
switch_profile '{ "base_keymap": "VSCode" }'
opens_and_closes "VS Code ctrl+p" ctrl+p
opens_and_closes "VS Code ctrl+shift+p" ctrl+shift+p
shot vscode
say "smoke: screenshot $RUN/vscode.png"

# JetBrains is retired
switch_profile '{ "base_keymap": "JetBrains" }'
sleep 2
kill -0 "$APP_PID" || fail "the app stopped after base_keymap was set to the retired JetBrains"
shot retired
say "smoke: screenshot $RUN/retired.png"
grep -q '"JetBrains"' "$SETTINGS_FILE" || fail "settings.json lost the retired JetBrains value: $(cat "$SETTINGS_FILE")"

# Emacs
switch_profile '{ "base_keymap": "Emacs" }'
opens_and_closes "Emacs alt+x" alt+x

[ "$(md5sum <"$KEYMAP_FILE")" = "$keymap_md5" ] || fail "keymap.json changed during the scenario"

quit_app
cp "$RUN/config-backup/settings-42.json" "$SETTINGS_FILE"
cp "$RUN/config-backup/global-42.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/keymap-42.json" "$KEYMAP_FILE"
