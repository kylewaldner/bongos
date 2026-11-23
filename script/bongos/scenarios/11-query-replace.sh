# Scenario for feature 11-query-replace. Sourced by script/bongos/smoke after the base flow (the app
# is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY,
# the proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 10 do. Main.kt is rewritten before launch and every check reads the
# saved file. Each replace starts from a line that holds no word, so the first prompt seeds from the
# last string and the scenario clears each prompt before it types.
#   1. M-% foo RET bar RET, then y n q: the first match is replaced, the second is skipped and q
#      ends. The decision view shows in the strip (the bottom rows change); query-replace.png.
#   2. C-/ undoes the whole replace in one step.
#   3. M-% foo RET bar RET, then !: every match is replaced; one C-/ puts them all back.
#   4. A region over the first two `foo` lines: ! replaces only inside it.
#   5. C-s two M-% 2 RET: the search hands over its string and only the replacement is asked; y.
#   6. C-M-% with a regexp and a group in the replacement, then !.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 11-query-replace skipped under BONGOS_STOCK=1: the stock run has no query replace"
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

# save_expects <expected text>: save and expect the file.
save_expects() {
  keys ctrl+x ctrl+s
  poll 30 "Main.kt to read $1" file_is "$1" || fail "Main.kt is $(od -c "$MAIN" | head -n8)"
}

# prompt_text <text>: replace the content of the focused prompt with the text and press Enter.
prompt_text() {
  keys ctrl+a ctrl+k
  type_text "$1"
  sleep 0.5
  keys Return
  sleep 0.5
}

# ask <chord> <from> <to>: start a replace with the chord and answer both prompts.
ask() {
  keys "$1"
  sleep 1
  prompt_text "$2"
  prompt_text "$3"
  sleep 1
}

# ---------------------------------------------------------------------------------------------
step "Query replace: answers, undo, batch, region, hand-off from isearch and regexp"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-11.json"
cp "$MAIN" "$RUN/config-backup/Main-11.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-11.json" >"$GLOBAL_FILE"
ORIGINAL='\nfoo one\nfoo two\nfoo three\nfoo four\n'
printf "$ORIGINAL" >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# 1. y replaces, n skips, q ends.
keys ctrl+Home
sleep 1
before=$(bottom_hash)
ask alt+percent foo bar
[ "$(bottom_hash)" != "$before" ] || fail "M-% left the bottom of the window unchanged"
shot query-replace
say "smoke: screenshot $RUN/query-replace.png"
type_text y
sleep 0.5
type_text n
sleep 0.5
type_text q
sleep 1
save_expects '\nbar one\nfoo two\nfoo three\nfoo four\n'

# 2. The whole replace is one undo step.
keys ctrl+slash
sleep 1
save_expects "$ORIGINAL"

# 3. ! replaces every match; one undo puts them all back.
keys ctrl+Home
sleep 1
ask alt+percent foo bar
type_text '!'
sleep 2
save_expects '\nbar one\nbar two\nbar three\nbar four\n'
keys ctrl+slash
sleep 1
save_expects "$ORIGINAL"

# 4. With a region, only the region is searched: lines 2 and 3 are inside it.
keys ctrl+Home
keys ctrl+n
sleep 0.5
keys ctrl+space
keys ctrl+n ctrl+n
sleep 0.5
ask alt+percent foo bar
type_text '!'
sleep 2
save_expects '\nbar one\nbar two\nfoo three\nfoo four\n'
keys ctrl+slash
sleep 1
save_expects "$ORIGINAL"

# 5. C-s two, then M-% hands the search over: only the replacement is asked.
keys ctrl+Home
sleep 1
keys ctrl+s
type_text two
sleep 1
keys alt+percent
sleep 1
prompt_text 2
sleep 1
type_text y
sleep 1
save_expects '\nfoo one\nfoo 2\nfoo three\nfoo four\n'
keys ctrl+slash
sleep 1
save_expects "$ORIGINAL"

# 6. C-M-% with a regexp: the group of the replacement is the word after `foo `.
keys ctrl+Home
sleep 1
ask ctrl+alt+percent 'foo (\w+)' '\1 foo'
type_text '!'
sleep 2
save_expects '\none foo\ntwo foo\nthree foo\nfour foo\n'

quit_app
cp "$RUN/config-backup/global-11.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-11.kt" "$MAIN"
