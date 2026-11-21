# Scenario for feature 14-window-management. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 09 do. The app restores the base flow's layout: two editor panes
# of Main.kt side by side and the terminal below them. The pane picker labels them in reading
# order: a is the left editor, s the right one, d the terminal.
#   1. C-x o and d: the terminal gets focus, and the shell command typed next runs as typed, which
#      proves the letter never reached the shell (a stray d would turn the command into "decho").
#   2. C-x 1 zooms the focused editor (zoomed.png); a second C-x 1 restores the layout
#      (restored.png). A marker typed into each restored pane lands: the two editors share the
#      file, and the terminal runs a command that writes a marker file.
#   3. C-x _ stacks the two editors (stacked.png) and C-x | puts them side by side again
#      (side-by-side.png).
#   4. A window dedicated with C-c down keeps its file while the file finder opens another one:
#      the open goes to another pane, and the dedicated pane still shows Main.kt.
#   5. C-x 0 deletes the focused window (deleted.png) and C-x 2 splits again (split-again.png).
# The scenario leaves the app quit and restores global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 14-window-management skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
  return 0
fi

GLOBAL_FILE="$HOME/$BONGOS_CONFIG_PATH/global_settings.json"
MAIN="$RUN/project/Main.kt"

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# pick <letter>: C-x o, then the letter of a window in the picker. The picker shows its labels
# before it takes the letter, so each step waits for the screen to change.
pick() {
  act keys ctrl+x o
  act keys "$1"
}

# ---------------------------------------------------------------------------------------------
step "Window management: pick, zoom, orientation, dedication, delete and split"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-14.json"
grep -v '"base_keymap"' "$RUN/config-backup/global-14.json" >"$GLOBAL_FILE"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled
wait_for_terminal

# 1. The terminal by its letter, and the command typed there runs as typed.
pick a
pick d
type_text 'echo picked > d-proof.txt'
keys Return
assert_file "$RUN/project/d-proof.txt" '^picked$'

# 2. Zoom the left editor, restore, and type a marker into each of the three panes.
pick a
act keys ctrl+x 1
shot zoomed
say "smoke: screenshot $RUN/zoomed.png"
act keys ctrl+x 1
shot restored
say "smoke: screenshot $RUN/restored.png"
type_text LEFTMARK
pick s
type_text RIGHTMARK
pick d
type_text 'echo marker > terminal-marker.txt'
keys Return
assert_file "$RUN/project/terminal-marker.txt" '^marker$'
pick a
keys ctrl+x ctrl+s
poll 30 "Main.kt to hold both markers" grep -q 'RIGHTMARK' "$MAIN"
grep -q 'LEFTMARK' "$MAIN" || fail "Main.kt lacks LEFTMARK: $(cat "$MAIN")"

# 3. Orientation: stacked, then side by side.
act keys ctrl+x underscore
shot stacked
say "smoke: screenshot $RUN/stacked.png"
act keys ctrl+x bar
shot side-by-side
say "smoke: screenshot $RUN/side-by-side.png"

# 4. Dedicate the left window; the file finder opens README.md in the other editor pane, and the
#    left window still shows Main.kt.
act keys ctrl+c Down
act keys ctrl+x ctrl+f
act type_text README.md
keys Return
wait_for_title 'README\.md'
pick a
wait_for_title 'Main\.kt'
act keys ctrl+c Down

# 5. Delete the left window, then split again.
act keys ctrl+x 0
shot deleted
say "smoke: screenshot $RUN/deleted.png"
act keys ctrl+x 2
shot split-again
say "smoke: screenshot $RUN/split-again.png"

quit_app
cp "$RUN/config-backup/global-14.json" "$GLOBAL_FILE"
