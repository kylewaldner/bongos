# Scenario for feature 09-universal-argument. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY,
# the proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 08 do. Main.kt is rewritten to a few rows before launch, and every
# check reads the saved file, one row of it per step:
#   1. C-u C-f moves four characters; the x typed after it lands there.
#   2. After two kills, C-u C-y yanks the second newest entry (four wraps around a ring of two).
#   3. C-u 1 0 shows "C-u 10" in the echo area (prefix-echo.png) and C-n then goes down ten rows.
#   4. M-3 C-k kills three lines as one entry and C-y brings all three back.
#   5. C-u C-u M-f goes sixteen words forward and C-u - M-f four words back, each shown by a typed
#      marker.
#   6. C-u 3 C-u 7 inserts 777 (the second C-u ends the number, the 7 is a typed character); a 7
#      alone inserts one; C-u 7 C-g inserts nothing and clears the argument.
#   7. C-u C-x C-x swaps the ends of a region once, not four times.
#   8. Three marks are set, and C-u C-SPC pops one entry each time, not four.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 09-universal-argument skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
GLOBAL_FILE="$CONFIG_DIR/global_settings.json"
MAIN="$RUN/project/Main.kt"

command -v import >/dev/null || { echo "bongos: ImageMagick is not installed; the scenario needs it" >&2; exit 1; }

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# rows_are <row>...: Main.kt holds exactly these rows, one per line.
rows_are() {
  local IFS=$'\n'
  [ "$(cat "$MAIN")" = "$*" ]
}

# save_and_expect <row>...: save the buffer and wait for the file to hold exactly these rows.
save_and_expect() {
  keys ctrl+x ctrl+s
  poll 30 "Main.kt to hold the expected rows" rows_are "$@" || fail "Main.kt is $(od -c "$MAIN" | head -n8)"
}

# down <n>: n presses of C-n, with no argument.
down() { keys $(yes ctrl+n | head -n "$1"); }

# ---------------------------------------------------------------------------------------------
step "Universal argument: C-u, digits, minus, kills, marks"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-09.json"
cp "$MAIN" "$RUN/config-backup/Main-09.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-09.json" >"$GLOBAL_FILE"

WORDS="a b c d e f g h i j k l m n o p q r s t"
want=(abcdefghij kill-one kill-two r03 r04 r05 r06 r07 r08 r09 r10 r11 r12 r13 r14 "$WORDS" "" "one two" 0123456789)
printf '%s\n' "${want[@]}" >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# 1. C-u C-f: four characters.
keys ctrl+Home
sleep 1
keys ctrl+u ctrl+f
sleep 0.5
type_text x
want[0]=abcdxefghij
save_and_expect "${want[@]}"

# 2. Two kills of the rest of a line each (C-k at the start of a line takes the newline too, so the
#    caret is after the first letter), then C-u C-y: the second newest entry.
down 1
keys ctrl+a ctrl+f ctrl+k
down 1
keys ctrl+k
sleep 0.5
keys ctrl+u ctrl+y
sleep 0.5
want[1]=k
want[2]=kill-one
save_and_expect "${want[@]}"

# 3. C-u 1 0 shows the argument in the echo area; C-n then goes down ten rows (row 2 to 12).
keys ctrl+u 1 0
sleep 1
shot prefix-echo
say "smoke: screenshot $RUN/prefix-echo.png"
keys ctrl+n
sleep 0.5
type_text N
want[12]=r12N
save_and_expect "${want[@]}"

# 4. M-3 C-k kills three lines as one entry; C-y brings them back.
keys ctrl+a alt+3 ctrl+k
sleep 0.5
save_and_expect "${want[@]:0:12}" "${want[@]:15}"
keys ctrl+y
sleep 0.5
save_and_expect "${want[@]}"

# 5. C-u C-u M-f: sixteen words forward. C-u - M-f: four words back from there. The caret is at
#    the start of the row of words after the yank.
keys ctrl+u ctrl+u alt+f
sleep 0.5
type_text '|'
keys ctrl+u minus alt+f
sleep 0.5
type_text '^'
want[15]="a b c d e f g h i j k l ^m n o p| q r s t"
save_and_expect "${want[@]}"

# 6. Typed characters: C-u 3 C-u 7 is 777, a bare 7 is one 7, and C-u 7 C-g inserts nothing.
down 1
keys ctrl+u 3 ctrl+u
sleep 0.5
type_text 7
type_text -
type_text 7
keys ctrl+u 7 ctrl+g
sleep 0.5
type_text z
want[16]=777-7z
save_and_expect "${want[@]}"

# 7. C-u C-x C-x swaps the ends of the region once: the caret is at the start when X is typed.
down 1
keys ctrl+a ctrl+space ctrl+f ctrl+f ctrl+f
sleep 0.5
keys ctrl+u ctrl+x ctrl+x
sleep 0.5
type_text X
keys ctrl+g
sleep 0.5
want[17]="Xone two"
save_and_expect "${want[@]}"

# 8. Three marks at 0, 3 and 6 (the second C-SPC of each pair turns the region off), the caret at
#    9. Each C-u C-SPC jumps to the newest mark and pops one entry: 6, then 3, then 0.
down 1
keys ctrl+a ctrl+space ctrl+space ctrl+f ctrl+f ctrl+f
keys ctrl+space ctrl+space ctrl+f ctrl+f ctrl+f
keys ctrl+space ctrl+space ctrl+f ctrl+f ctrl+f
sleep 0.5
keys ctrl+u ctrl+space
sleep 0.5
type_text a
keys ctrl+u ctrl+space
sleep 0.5
type_text b
keys ctrl+u ctrl+space
sleep 0.5
type_text c
want[18]=c012b345a6789
save_and_expect "${want[@]}"

quit_app
cp "$RUN/config-backup/global-09.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-09.kt" "$MAIN"
