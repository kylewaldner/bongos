# Scenario for feature 07-kill-ring. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 08 do. Main.kt is rewritten to three short lines before launch, and
# every check reads the saved file.
#   1. C-M-k kills the three lines one after the other: the kills join into one ring entry. C-y
#      twice puts that entry in twice.
#   2. C-k at the start of a line kills the whole line (a new entry), C-SPC C-e M-w copies the line the cursor is on, C-y
#      at the end of the buffer yanks the copy and M-y swaps it for the line C-k killed.
#   3. The app quits and launches again. M-Y opens the kill ring in the minibuffer strip, with the
#      preview of the highlighted entry (kill-ring.png). Two Downs pick the entry of part 1, which
#      only a ring saved across the restart can offer; Enter inserts it at the start.
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 07-kill-ring skipped under BONGOS_STOCK=1: the stock run has no Emacs default"
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
step "Kill ring: kill, join, yank, yank-pop, browse after a restart"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-07.json"
cp "$MAIN" "$RUN/config-backup/Main-07.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-07.json" >"$GLOBAL_FILE"
printf 'aaa\nbbb\nccc\n' >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

keys ctrl+Home
sleep 1
keys ctrl+alt+k
sleep 0.3
keys ctrl+alt+k
sleep 0.3
keys ctrl+alt+k
sleep 0.5
keys ctrl+y
sleep 0.3
keys ctrl+y
sleep 0.5
save_and_expect 'aaa\nbbb\nccc\naaa\nbbb\nccc'

keys ctrl+Home
sleep 0.5
keys ctrl+k
sleep 0.5
keys ctrl+n
keys ctrl+space
keys ctrl+e
keys alt+w
sleep 0.5
keys ctrl+End
sleep 0.5
keys ctrl+y
sleep 0.5
keys alt+y
sleep 0.5
save_and_expect 'bbb\nccc\naaa\nbbb\nccc\naaa'

# The ring is saved in the background; let it land before the app quits.
sleep 2
quit_app

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

keys ctrl+Home
sleep 1
act keys alt+shift+y
sleep 0.5
keys Down
keys Down
sleep 1
shot kill-ring
say "smoke: screenshot $RUN/kill-ring.png"
keys Return
sleep 0.5
save_and_expect 'aaa\nbbb\nccc\nbbb\nccc\naaa\nbbb\nccc\naaa'

quit_app
cp "$RUN/config-backup/global-07.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-07.kt" "$MAIN"
