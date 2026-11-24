# Scenario for feature 17-navigation-pickers. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 does. The pickers open in the strip, so opening and closing show as
# changes of the bottom rows of the window; what they do shows in the window title and in the saved
# Main.kt.
#   1. C-x C-f README.md RET opens README.md (title). C-x b opens the switcher (bottom rows change,
#      switcher.png); C-x b again toggles back to Main.kt (title) and closes the strip. C-x b then
#      C-g leaves the strip as it was and the title unchanged.
#   2. C-x C-f opens go to file (go-to-file.png, bottom rows change); C-g closes it, the bottom rows
#      are as they were and the title is unchanged: no file was opened.
#   3. M-? opens find in files; the query `hello from` matches Main.kt only (find-in-files.png);
#      C-c C-o closes the strip and opens the results multibuffer (find-export.png), which changes
#      the window title away from Main.kt.
#   4. C-x C-f Main.kt RET returns to Main.kt; M-s / opens search buffer lines (search-lines.png),
#      `println` RET jumps to its line, which a typed `#` shows in the saved file: on line 2 only.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 17-navigation-pickers skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
GLOBAL_FILE="$CONFIG_DIR/global_settings.json"
MAIN="$RUN/project/Main.kt"
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

# The text of line 2 of Main.kt carries the typed marker, and no other line does.
marker_on_line_2() {
  [ "$(sed -n 2p "$MAIN" | grep -c '#')" = 1 ] && [ "$(grep -c '#' "$MAIN")" = 1 ]
}

title_is() { title_matches "$1"; }
title_is_not() { ! title_matches "$1"; }

# ---------------------------------------------------------------------------------------------
step "Navigation pickers: switch buffer, go to file, find in files, search buffer lines"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-17.json"
cp "$MAIN" "$RUN/config-backup/Main-17.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-17.json" >"$GLOBAL_FILE"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

closed=$(bottom_hash)

# 1. Switch buffer. A second buffer first: go to README.md.
act keys ctrl+x ctrl+f
act type_text README.md
keys Return
wait_for_title 'README\.md'
wait_for_settled
[ "$(bottom_hash)" = "$closed" ] || fail "go to file left the strip open after RET"

act keys ctrl+x b
[ "$(bottom_hash)" != "$closed" ] || fail "C-x b changed nothing at the bottom of the window"
shot switcher
say "smoke: screenshot $RUN/switcher.png"
act keys ctrl+x b
poll 30 "the toggle to bring back Main.kt" title_matches 'Main\.kt'
wait_for_settled
[ "$(bottom_hash)" = "$closed" ] || fail "the second C-x b did not close the strip"

act keys ctrl+x b
[ "$(bottom_hash)" != "$closed" ] || fail "C-x b did not open the switcher the second time"
act keys ctrl+g
[ "$(bottom_hash)" = "$closed" ] || fail "C-g did not leave the strip as it was"
title_is 'Main\.kt' || fail "C-g in the switcher changed the buffer: $(xdo getwindowname "$WIN")"

# 2. Go to file opens and C-g opens no file.
act keys ctrl+x ctrl+f
[ "$(bottom_hash)" != "$closed" ] || fail "C-x C-f changed nothing at the bottom of the window"
shot go-to-file
say "smoke: screenshot $RUN/go-to-file.png"
act keys ctrl+g
[ "$(bottom_hash)" = "$closed" ] || fail "C-g did not close go to file"
title_is 'Main\.kt' || fail "C-g in go to file changed the title: $(xdo getwindowname "$WIN")"

# 3. Find in files: the query matches Main.kt only; C-c C-o exports it.
act keys alt+question
[ "$(bottom_hash)" != "$closed" ] || fail "M-? changed nothing at the bottom of the window"
act type_text 'hello from'
shot find-in-files
say "smoke: screenshot $RUN/find-in-files.png"
act keys ctrl+c ctrl+o
poll 30 "the results multibuffer to replace Main.kt in the title" title_is_not 'Main\.kt'
wait_for_settled
[ "$(bottom_hash)" = "$closed" ] || fail "C-c C-o did not close the find strip"
shot find-export
say "smoke: screenshot $RUN/find-export.png"

# 4. Search buffer lines in Main.kt: RET jumps to the line.
act keys ctrl+x ctrl+f
act type_text Main.kt
keys Return
wait_for_title 'Main\.kt'
wait_for_settled
keys ctrl+Home
sleep 0.5
act keys alt+s slash
[ "$(bottom_hash)" != "$closed" ] || fail "M-s / changed nothing at the bottom of the window"
act type_text println
shot search-lines
say "smoke: screenshot $RUN/search-lines.png"
keys Return
wait_for_settled
[ "$(bottom_hash)" = "$closed" ] || fail "RET did not close search buffer lines"
type_text '#'
keys ctrl+x ctrl+s
poll 30 "the typed marker on line 2 of Main.kt" marker_on_line_2 || fail "Main.kt is $(od -c "$MAIN" | head -n8)"

quit_app
cp "$RUN/config-backup/global-17.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-17.kt" "$MAIN"
