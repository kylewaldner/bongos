# Scenario for feature 08-mark-ring. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 does. Main.kt is rewritten to three short lines before launch, and every
# check reads the saved file: a digit typed at the cursor shows where the mark keys left it.
#   1. C-SPC at the start, C-n (the region grows), C-x C-x (the cursor goes back to the start), then 1
#      is typed: it lands at the start and the text stays (emacs.delete_selection is false).
#      mark-region.png shows the region before the swap.
#   2. M-> pushes the mark and jumps to the end, where 2 is typed. C-x C-SPC pops back to the
#      pushed mark (after the 1), where 3 is typed; C-x C-SPC again pops to the mark C-x C-x left at
#      the start of the second line, where 4 is typed.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 08-mark-ring skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
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

# file_is <expected text>: Main.kt holds exactly the text (printf escapes allowed).
file_is() { [ "$(cat "$MAIN")" = "$(printf "$1")" ]; }

save_and_expect() {
  keys ctrl+x ctrl+s
  poll 30 "Main.kt to read $1" file_is "$1" || fail "Main.kt is $(od -c "$MAIN" | head -n5)"
}

# ---------------------------------------------------------------------------------------------
step "Mark ring: set, exchange, push and pop"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-08.json"
cp "$MAIN" "$RUN/config-backup/Main-08.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-08.json" >"$GLOBAL_FILE"
printf 'aaa\nbbb\nccc\n' >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

keys ctrl+Home
sleep 1
keys ctrl+space
act keys ctrl+n
shot mark-region
say "smoke: screenshot $RUN/mark-region.png"
keys ctrl+x ctrl+x
type_text 1
save_and_expect '1aaa\nbbb\nccc'

keys alt+greater
sleep 0.5
type_text 2
keys ctrl+x ctrl+space
sleep 0.5
type_text 3
keys ctrl+x ctrl+space
sleep 0.5
type_text 4
save_and_expect '13aaa\n4bbb\nccc\n2'
shot mark-ring
say "smoke: screenshot $RUN/mark-ring.png"

quit_app
cp "$RUN/config-backup/global-08.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-08.kt" "$MAIN"
