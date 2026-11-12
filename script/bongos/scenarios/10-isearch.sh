# Scenario for feature 10-isearch. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 does. Main.kt is rewritten to four lines before launch, and every check
# but the last reads the saved file: a `#` typed at the cursor shows where a search left it. A
# session's prompt is in the strip, so opening, a toggle and closing show as changes of the bottom
# rows of the window.
#   1. C-s fun RET: the prompt opens (the bottom rows change), the match is found, RET ends the
#      session with the cursor after it. isearch-prompt.png shows the open session.
#   2. C-s C-w C-s RET: C-w yanks the word at the cursor into the search, the second C-s repeats it
#      and finds the second `fun`.
#   3. M-c: `FUN` (smart case: case-sensitive) fails; M-c makes it insensitive and finds `fun`; the
#      prompt changes both times.
#   4. M-r: `f.n` as a literal string fails; M-r makes it a regexp and finds `fun`.
#   5. C-r backward from the end finds the last `fun` and ends at its start.
#   6. C-g aborts a search that moved the cursor, and the cursor is back where it was.
#   7. C-c C-o turns the search into an Occur tab (isearch-occur.png shows it); Alt-Enter on its last result
#      jumps to that line in Main.kt, which a typed `#` shows.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 10-isearch skipped under BONGOS_STOCK=1: the stock run has no isearch"
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

# file_is <expected text>: Main.kt holds exactly the text (printf escapes allowed).
file_is() { [ "$(cat "$MAIN")" = "$(printf "$1")" ]; }

# mark_and_save <expected text>: type a `#` at the cursor, save, and expect the file.
mark_and_save() {
  type_text '#'
  keys ctrl+x ctrl+s
  poll 30 "Main.kt to read $1" file_is "$1" || fail "Main.kt is $(od -c "$MAIN" | head -n8)"
}

# changes_bottom <what> <command...>: run the command and expect the bottom rows to change.
changes_bottom() {
  local what="$1" before
  shift
  before=$(bottom_hash)
  "$@"
  sleep 1
  [ "$(bottom_hash)" != "$before" ] || fail "$what changed nothing at the bottom of the window"
}

# ---------------------------------------------------------------------------------------------
step "Incremental search: start, repeat, yank, toggles, backward, abort and Occur"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-10.json"
cp "$MAIN" "$RUN/config-backup/Main-10.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-10.json" >"$GLOBAL_FILE"
printf 'fun main() {\n    println("hello")\n}\nfun other() {}\n' >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# 1. C-s opens a session; the match is found; RET leaves the cursor after it.
keys ctrl+Home
sleep 1
act keys ctrl+s
type_text fun
sleep 1
shot isearch-prompt
say "smoke: screenshot $RUN/isearch-prompt.png"
changes_bottom "RET" keys Return
mark_and_save 'fun# main() {\n    println("hello")\n}\nfun other() {}'

# 2. C-w yanks the word at the cursor; the second C-s finds the second match.
keys ctrl+Home
sleep 1
act keys ctrl+s
keys ctrl+w
sleep 0.5
keys ctrl+s
sleep 0.5
keys Return
mark_and_save 'fun# main() {\n    println("hello")\n}\nfun# other() {}'

# 3. M-c: an uppercase string is case-sensitive and fails; M-c makes it insensitive.
keys ctrl+Home
sleep 1
act keys ctrl+s
type_text FUN
sleep 1
changes_bottom "M-c" keys alt+c
keys Return
mark_and_save 'fun## main() {\n    println("hello")\n}\nfun# other() {}'

# 4. M-r: `f.n` is literal and fails; M-r makes it a regexp and it finds `fun`.
keys ctrl+Home
sleep 1
act keys ctrl+s
type_text f.n
sleep 1
changes_bottom "M-r" keys alt+r
keys Return
mark_and_save 'fun### main() {\n    println("hello")\n}\nfun# other() {}'

# 5. C-r from the end finds the last `fun`; the cursor ends at its start.
keys ctrl+End
sleep 1
act keys ctrl+r
type_text fun
sleep 1
keys Return
mark_and_save 'fun### main() {\n    println("hello")\n}\n#fun# other() {}'

# 6. C-g puts the cursor back: the search moved it to the last line, `other` was found there.
keys ctrl+Home
sleep 1
act keys ctrl+s
type_text other
sleep 1
changes_bottom "C-g" keys ctrl+g
mark_and_save '#fun### main() {\n    println("hello")\n}\n#fun# other() {}'

# 7. C-c C-o turns the search into an Occur tab.
keys ctrl+Home
sleep 1
act keys ctrl+s
type_text fun
sleep 1
before=$(bottom_hash)
LAST_SCREEN=$(screen_hash)
keys ctrl+c ctrl+o
poll 30 "the screen to change after C-c C-o" screen_changed
wait_for_settled
[ "$(bottom_hash)" != "$before" ] || fail "C-c C-o left the session open"
shot isearch-occur
say "smoke: screenshot $RUN/isearch-occur.png"
# The tab is the focused editor with the result lines; Alt-Enter on the last line jumps to that line
# in Main.kt, where a `#` typed lands at the end of the line.
keys ctrl+End
sleep 0.5
keys alt+Return
sleep 1
mark_and_save '#fun### main() {\n    println("hello")\n}\n#fun# other() {}#'

quit_app
cp "$RUN/config-backup/global-10.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-10.kt" "$MAIN"
