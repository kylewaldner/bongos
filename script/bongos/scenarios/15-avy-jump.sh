# Scenario for feature 15-avy-jump. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 does. Other.kt is written before launch with a unique word. The app
# restores the base flow's layout (two editor panes of Main.kt side by side), and Other.kt opens in
# the right pane. Every check reads a saved file.
#   1. M-; the word, RET (not the timer), `a`: the label jumps to the word. A marker typed next lands
#      right before the word in Other.kt, and Main.kt is unchanged.
#   2. M-; `q` C-g: the cancel keeps point and replays nothing, so a typed `Z` lands right after the
#      marker.
#   3. C-M-; shows the line labels (avy-line.png); Escape cancels them.
# The scenario leaves the app quit and restores global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 15-avy-jump skipped under BONGOS_STOCK=1: the stock run has no Avy jump"
  return 0
fi

GLOBAL_FILE="$HOME/$BONGOS_CONFIG_PATH/global_settings.json"
MAIN="$RUN/project/Main.kt"
OTHER="$RUN/project/Other.kt"
WORD=zebraquux

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# other_is <expected text>: Other.kt holds exactly the text (printf escapes allowed).
other_is() { [ "$(cat "$OTHER")" = "$(printf "$1")" ]; }

# save_and_expect <expected text>: save the focused file and expect Other.kt to hold the text.
save_and_expect() {
  keys ctrl+x ctrl+s
  poll 30 "Other.kt to read $1" other_is "$1" || fail "Other.kt is $(od -c "$OTHER" | head -n8)"
}

# ---------------------------------------------------------------------------------------------
step "Avy jump: char timer with labels, cancel keeps point, line labels"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-15.json"
cp "$MAIN" "$RUN/config-backup/Main-15.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-15.json" >"$GLOBAL_FILE"
printf 'fun %s() {}\n' "$WORD" >"$OTHER"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# Open Other.kt in the right pane (C-x o and the letter s pick it).
act keys ctrl+x o
act keys s
act keys ctrl+x ctrl+f
act type_text Other.kt
keys Return
wait_for_title 'Other\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled
keys ctrl+Home
sleep 1

# 1. M-; the word, RET shows the labels at once, and `a` jumps to the word.
act keys alt+semicolon
type_text "$WORD"
sleep 1
act keys Return
act keys a
type_text MARK
save_and_expect "fun MARK${WORD}() {}"
cmp -s "$MAIN" "$RUN/config-backup/Main-15.kt" || fail "Main.kt changed: $(cat "$MAIN")"

# 2. M-; `q` C-g: point stays after the marker and nothing is replayed.
act keys alt+semicolon
type_text q
sleep 1
act keys ctrl+g
type_text Z
save_and_expect "fun MARKZ${WORD}() {}"
cmp -s "$MAIN" "$RUN/config-backup/Main-15.kt" || fail "Main.kt changed: $(cat "$MAIN")"

# 3. C-M-; shows the line labels; Escape cancels them and leaves the file alone.
act keys ctrl+alt+semicolon
shot avy-line
say "smoke: screenshot $RUN/avy-line.png"
act keys Escape
save_and_expect "fun MARKZ${WORD}() {}"

quit_app
cp "$RUN/config-backup/global-15.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-15.kt" "$MAIN"
rm -f "$OTHER"
