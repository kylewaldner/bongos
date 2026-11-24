# Scenario for feature 16-undo-tree. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# Emacs is the default base keymap, so the scenario drops the base flow's VSCode pin from
# global_settings.json, as 06 and 12 do. Main.kt holds one line and is rewritten before launch.
# The buffer's file is read after a save (C-x s saves every modified buffer), and Main.kt ends with
# a newline so that a save adds no edit of its own. While the undo tree is open the buffer is not
# touched: the tree moves a preview of it in the pane beside the buffer, so the preview's text line is
# compared by its pixels (the text of each state is rendered once, so equal pixels are equal text).
#   1. Three edits (" one", " two", " three") are separate undo steps, because C-e between them
#      is a command. C-/ twice goes back to " one"; a new edit " new" makes a branch.
#   2. C-x u opens the undo tree (undo-tree-open.png). `p` goes to the old branch (" two") and `f`
#      to its child (" three"); the preview shows each state.
#   3. `m` marks the state, `b` goes back and `d` shows the diff (undo-tree-diff.png).
#   4. `q` closes the tree and leaves the buffer as it was before C-x u (" new"), with the caret at the end of the line.
#   5. C-x u, `p` and RET apply the old branch (" two"); C-/ undoes that and returns " new".
# The scenario leaves the app quit and restores Main.kt and global_settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 16-undo-tree skipped under BONGOS_STOCK=1: the stock run has no undo tree"
  return 0
fi

GLOBAL_FILE="$HOME/$BONGOS_CONFIG_PATH/global_settings.json"
MAIN="$RUN/project/Main.kt"

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# main_is <expected text>: Main.kt holds exactly the text (printf escapes allowed).
main_is() { [ "$(cat "$MAIN")" = "$(printf "$1")" ]; }

# save_and_expect <expected text>: saves the modified buffers and waits for Main.kt to read the text.
save_and_expect() {
  keys ctrl+x s
  poll 30 "Main.kt to read $1" main_is "$1" || fail "Main.kt is $(od -c "$MAIN" | head -n5)"
}

# preview_sig: a hash of the text line of the preview pane (the pane right of the buffer's).
preview_sig() { DISPLAY="$BONGOS_DISPLAY" import -window root -crop 240x22+440+114 +repage rgb:- | md5sum; }

# preview_is <hash>: the preview line is the one with the hash.
preview_is() { [ "$(preview_sig)" = "$1" ]; }

# preview_not <hash>...: the preview line is none of the hashes.
preview_not() {
  local now ref
  now=$(preview_sig)
  for ref in "$@"; do [ "$now" != "$ref" ] && continue; return 1; done
}

# ---------------------------------------------------------------------------------------------
step "Undo tree: branches, switching states, diff, quit and confirm"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_FILE" "$RUN/config-backup/global-16.json"
cp "$MAIN" "$RUN/config-backup/Main-16.kt"
grep -v '"base_keymap"' "$RUN/config-backup/global-16.json" >"$GLOBAL_FILE"
printf 'base\n' >"$MAIN"

launch
wait_for_window
wait_for_title 'Main\.kt'
poll 60 "a rendered frame" ui_rendered
wait_for_settled

# 1. Three edits, two undos and a new edit: a branch below " one".
keys ctrl+Home ctrl+e
type_text ' one'
keys ctrl+e
type_text ' two'
keys ctrl+e
type_text ' three'
save_and_expect 'base one two three\n'
keys ctrl+slash
keys ctrl+slash
save_and_expect 'base one\n'
keys ctrl+e
type_text ' new'
save_and_expect 'base one new\n'

# 2. The tree opens beside the buffer; `p` shows the old branch and `f` its child.
act keys ctrl+x u
shot undo-tree-open
say "smoke: screenshot $RUN/undo-tree-open.png"
SIG_NEW=$(preview_sig)
act keys p
poll 30 "the preview to leave the new text" preview_not "$SIG_NEW"
SIG_TWO=$(preview_sig)
act keys f
poll 30 "the preview to show the child state" preview_not "$SIG_NEW" "$SIG_TWO"

# 3. Mark this state, go back and show the diff against the mark.
keys m
act keys b
poll 30 "the preview to show the old branch again" preview_is "$SIG_TWO"
act keys d
shot undo-tree-diff
say "smoke: screenshot $RUN/undo-tree-diff.png"

# 4. Quit: the file is as it was before C-x u, and the caret is back at the end of the line, so a
# typed marker lands after " new".
act keys q
save_and_expect 'base one new\n'
type_text '!'
save_and_expect 'base one new!\n'
keys ctrl+slash
save_and_expect 'base one new\n'

# 5. Open again, pick the old branch and confirm; one C-/ returns the newer text.
act keys ctrl+x u
act keys p
poll 30 "the preview to show the old branch" preview_is "$SIG_TWO"
keys Return
save_and_expect 'base one two\n'
keys ctrl+slash
save_and_expect 'base one new\n'

quit_app

cp "$RUN/config-backup/global-16.json" "$GLOBAL_FILE"
cp "$RUN/config-backup/Main-16.kt" "$MAIN"
