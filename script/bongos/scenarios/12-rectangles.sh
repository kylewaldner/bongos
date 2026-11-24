# Scenario for feature 12-rectangles. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 08 do. Main.kt is rewritten before each part and the app launches
# again, and every check reads the saved file. A rectangle runs between the mark and the cursor.
#   1. C-x r o opens a blank column in a block; C-/ puts the block back.
#   2. C-x r k kills a column; C-/ brings it back; the kill is repeated, then Other.kt is opened
#      with C-x C-f and C-x r y puts the column in at its start; C-/ removes it again.
#   3. C-x r c clears a comment block to spaces; C-/ restores it.
#   4. C-x r t asks `String rectangle: ` in the minibuffer and puts a prefix on every row; C-/
#      removes it.
#   5. A kill and a yank at the end of the last row, which is shorter: the yank pads the rows it
#      adds. C-/ undoes the newline the save added, the yank, then the kill.
#   6. A tab row and a CJK row are cut at the displayed column (C-x r d); C-/ restores them.
#   7. C-x SPC starts rectangle-mark-mode and shows the rectangle (rectangle-mark.png); C-g ends it.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 12-rectangles skipped under BONGOS_STOCK=1: the stock run has no rectangle commands"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
GLOBAL_FILE="$CONFIG_DIR/global_settings.json"
MAIN="$RUN/project/Main.kt"
OTHER="$RUN/project/Other.kt"

command -v import >/dev/null || { echo "bongos: ImageMagick is not installed; the scenario needs it" >&2; exit 1; }

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# file_is <path> <expected text>: the file holds exactly the text (printf escapes allowed).
file_is() { [ "$(cat "$1")" = "$(printf "$2")" ]; }

# save_and_expect <path> <expected text>: saves the active file and waits for it to read the text.
save_and_expect() {
  keys ctrl+x ctrl+s
  poll 30 "$(basename "$1") to read $2" file_is "$1" "$2" || fail "$1 is $(od -c "$1" | head -n5)"
  # Let the save finish in the app (the file event and the clean state) before the next key.
  sleep 1
}

# start <text>: Main.kt gets the text (printf escapes allowed) and the app opens it.
start() {
  printf "$1" >"$MAIN"
  launch
  wait_for_window
  wait_for_title 'Main\.kt'
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
  keys ctrl+Home
  sleep 1
}

# quit_part: quits after the last save has settled. A quit right after a save can store the buffer
# as still modified, and the next launch would restore that text over the rewritten Main.kt.
quit_part() {
  sleep 2
  quit_app
}

# mark_and_move <mark right> <down> <right>: C-f to the mark column, C-SPC, then C-n and C-f to the
# other corner. Moves are by character, so only rows of one-column characters use it.
mark_and_move() {
  local n
  for ((n = 0; n < $1; n++)); do keys ctrl+f; done
  keys ctrl+space
  for ((n = 0; n < $2; n++)); do keys ctrl+n; done
  for ((n = 0; n < $3; n++)); do keys ctrl+f; done
  sleep 0.5
}

# ---------------------------------------------------------------------------------------------
step "Rectangles: open, kill and yank in another file, clear, string, uneven yank, wide rows, mark mode"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-12.json"
cp "$MAIN" "$RUN/config-backup/Main-12.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-12.json" >"$GLOBAL_FILE"
printf 'xxxx\nyyyy\nzzzz\n' >"$OTHER"

# 1. A blank column opened in a block: columns 1 and 2 of three rows.
start 'abcd\nefgh\nijkl\n'
mark_and_move 1 2 2
keys ctrl+x r o
sleep 0.5
save_and_expect "$MAIN" 'a  bcd\ne  fgh\ni  jkl\n'
keys ctrl+slash
save_and_expect "$MAIN" 'abcd\nefgh\nijkl\n'

# 2. A column killed, undone, killed again and yanked into another file.
keys ctrl+Home
sleep 0.5
mark_and_move 1 2 2
keys ctrl+x r k
sleep 0.5
save_and_expect "$MAIN" 'ad\neh\nil\n'
keys ctrl+slash
save_and_expect "$MAIN" 'abcd\nefgh\nijkl\n'
keys ctrl+Home
sleep 0.5
mark_and_move 1 2 2
keys ctrl+x r k
sleep 0.5
# Save the killed Main.kt so that the next launch does not restore it unsaved.
save_and_expect "$MAIN" 'ad\neh\nil\n'
act keys ctrl+x ctrl+f
type_text 'Other.kt'
sleep 1
keys Return
wait_for_title 'Other\.kt'
wait_for_settled
keys ctrl+Home
sleep 0.5
keys ctrl+x r y
sleep 0.5
save_and_expect "$OTHER" 'bcxxxx\nfgyyyy\njkzzzz\n'
keys ctrl+slash
save_and_expect "$OTHER" 'xxxx\nyyyy\nzzzz\n'
# Close Other.kt so the window restores on Main.kt in the next part.
act keys ctrl+x k
wait_for_title 'Main\.kt'
quit_part

# 3. A comment block cleared to spaces.
start '// one\n// two\n// three\n'
mark_and_move 0 2 3
keys ctrl+x r c
sleep 0.5
save_and_expect "$MAIN" '   one\n   two\n   three\n'
keys ctrl+slash
save_and_expect "$MAIN" '// one\n// two\n// three\n'
quit_part

# 4. A prefix added to every row: the rectangle has no width and the minibuffer asks the string.
start 'one\ntwo\nthree\n'
mark_and_move 0 2 0
act keys ctrl+x r t
shot string-rectangle
say "smoke: screenshot $RUN/string-rectangle.png"
type_text '# '
keys Return
sleep 0.5
save_and_expect "$MAIN" '# one\n# two\n# three\n'
keys ctrl+slash
save_and_expect "$MAIN" 'one\ntwo\nthree\n'
quit_part

# 5. A yank at the end of the last row where the rows are uneven: the added row is padded out. Saving
# ends the file with a newline, so the text carries one from the start.
start 'abcd\nefgh\nij\n'
mark_and_move 0 1 2
keys ctrl+x r k
sleep 0.5
save_and_expect "$MAIN" 'cd\ngh\nij\n'
keys ctrl+End ctrl+p ctrl+e
sleep 0.5
keys ctrl+x r y
sleep 0.5
save_and_expect "$MAIN" 'cd\ngh\nijab\n  ef\n'
# The added row has no newline, so the save added one as its own undo step: the first C-/ takes
# that back and the second the yank.
keys ctrl+slash
sleep 0.3
keys ctrl+slash
save_and_expect "$MAIN" 'cd\ngh\nij\n'
keys ctrl+slash
save_and_expect "$MAIN" 'abcd\nefgh\nij\n'
quit_part

# 6. A tab row and a CJK row are cut at the displayed column: columns 2 to 5 of four rows.
start 'abcdefgh\na\tbcd\n日本語x\nabcdefgh\n'
keys ctrl+f ctrl+f ctrl+space ctrl+n ctrl+n ctrl+n ctrl+a ctrl+f ctrl+f ctrl+f ctrl+f ctrl+f
sleep 0.5
keys ctrl+x r d
sleep 0.5
save_and_expect "$MAIN" 'abfgh\na cd\n日x\nabfgh\n'
keys ctrl+slash
save_and_expect "$MAIN" 'abcdefgh\na\tbcd\n日本語x\nabcdefgh\n'
quit_part

# 7. C-x SPC starts rectangle-mark-mode; the rectangle shows on the screen.
start 'aaaa\nbbbb\ncccc\n'
keys ctrl+f
keys ctrl+x space
sleep 0.5
keys ctrl+n ctrl+n ctrl+f ctrl+f
sleep 1
shot rectangle-mark
say "smoke: screenshot $RUN/rectangle-mark.png"
keys ctrl+g
sleep 0.5
save_and_expect "$MAIN" 'aaaa\nbbbb\ncccc\n'
quit_part

cp "$RUN/config-backup/global-12.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-12.kt" "$MAIN"
rm -f "$OTHER"
