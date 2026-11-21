# Scenario for feature 13-symbol-overlay-xref-history. Sourced by script/bongos/smoke after the base
# flow (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN,
# HOME, DISPLAY, the proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 09 do. Main.kt is rewritten to three Kotlin functions before launch,
# and every check reads the saved file, because the character typed after a move lands where the
# caret is:
#   1. M-n on the `fun` keyword moves to the next `fun`; a typed 1 shows where the caret landed.
#   2. M-i puts the overlay on the symbol at point (overlay.png).
#   3. `up` pushes the current place onto the goto stack, C-n C-n moves away, `left` goes back to the
#      pushed place (a typed 2 shows it) and `right` goes forward again (a typed 3 shows it).
#   4. C-c x h opens the xref history list (xref-list.png) and RET goes to the entry at the top.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 13-symbol-overlay-xref-history skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
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
  poll 30 "Main.kt to hold the expected rows" rows_are "$@" || fail "Main.kt is $(cat "$MAIN")"
}

# ---------------------------------------------------------------------------------------------
step "Symbol jump and goto stack: M-n, M-i, up, left, right, C-c x h"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-13.json"
cp "$MAIN" "$RUN/config-backup/Main-13.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-13.json" >"$GLOBAL_FILE"

want=("fun alpha(n: Int) {" "    println(n)" "}" "fun beta(n: Int) {" "    println(n)" "}" "fun gamma(n: Int) {" "    println(n)" "}")
printf '%s\n' "${want[@]}" >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# 1. M-n on `fun` at the top: the next `fun` is the one of beta.
keys ctrl+Home
sleep 1
keys alt+n
sleep 0.5
type_text 1
want[3]="1fun beta(n: Int) {"
save_and_expect "${want[@]}"

# 2. M-i puts the overlay on the symbol at point.
act keys alt+i
shot overlay
say "smoke: screenshot $RUN/overlay.png"

# 3. Push this place, move two rows down, go back, then forward.
keys Up
sleep 0.5
keys ctrl+n ctrl+n
sleep 0.5
keys Left
sleep 0.5
type_text 2
want[3]="12fun beta(n: Int) {"
save_and_expect "${want[@]}"
keys Right
sleep 0.5
type_text 3
want[5]="}3"
save_and_expect "${want[@]}"

# 4. The history list shows the stack, newest first; RET goes to the entry.
act keys ctrl+c x h
shot xref-list
say "smoke: screenshot $RUN/xref-list.png"
keys Return
sleep 1
wait_for_settled
type_text 4
want[3]="124fun beta(n: Int) {"
save_and_expect "${want[@]}"

quit_app
cp "$RUN/config-backup/global-13.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-13.kt" "$MAIN"
