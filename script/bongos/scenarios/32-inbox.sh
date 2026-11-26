# Scenario for feature 32-inbox. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh. Debug builds only (`inbox: dump view` is a debug action).
#
# settings.json holds `"base_keymap": "Emacs"` and a theme; it never mentions `inbox`, so the stages
# also show that the inbox is on by default. The inbox directory is $HOME/.bongos/inbox in the
# throwaway home. Every item is written by this script from outside the app, the way an agent or a
# shell does it: written under tmp/, then moved into new/ (or, for read items, cur/).
#   1. An item is written into tmp/ and moved into new/ while the app runs and no inbox view is
#      open. `inbox: dump view` then reads `unread: 1` and shows the row. A second item left in tmp/
#      never shows.
#   2. Nine `decision-default` items (`actions: [accept, overturn]`, one `group`) and one file that
#      does not parse arrive: one group row of nine, one unreadable row, `unread: 11`.
#   3. `C-c i` opens the view. RET on the single item renames it to cur/<u>:2,S, the count is 10 and
#      the item pane shows it (inbox-item.png); `u` renames it to cur/<u>:2, and the count is 11.
#   4. `d` on the group row moves its nine files to archive/, byte for byte as they were written
#      (`cmp`); the count is 2.
#   5. `o` opens the item's target in an editor tab and reads the item (the count is 1); `/` filters the list by kind (typed into the
#      `Filter: ` prompt) and `all` clears it; `g` scans again.
#   6. Nothing interrupted: the log has no inbox warning or error and none of the item text
#      (inbox.png shows the window with the count in the status bar and no toast).
#   7. Design captures (`design-<name>.png`, the whole window, no crop). On a fresh inbox directory
#      twelve items are written with ages from 4 minutes to 4 days; nine are read (cur/<u>:2,S) and
#      the three unread are the first three rows. The view is shown at 1280x720 (16:9, full screen)
#      and at 640x720 (8:9, half screen), each in One Dark and One Light, in two states: the list with
#      the first row under the cursor and the list focused, and RET on the first row (focus in the
#      item pane). `u` then puts the first row back to unread for the next capture.
#   8. The sidebar captures (see Deviations) on the same twelve items, in One Dark and One Light: the
#      list with the first row under the cursor, and RET on it (the modal).
# Deviations:
# - The sidebar captures (design-sidebar-list-* and design-sidebar-modal-*): nothing in the app yet
#   hosts the list as a sidebar, so the debug action `inbox: open as sidebar` puts an inbox view in
#   the sidebar layout into a right dock panel (320 px) beside an ordinary editor (a Kotlin file
#   opened from the command line) in place of the design's custom screen; RET on the first row opens
#   the item modal and Esc closes it. The window is 1280x720.
# - `open-run` is not covered: a `run:` target is live only while a workflow engine run of that id
#   exists, and this scenario starts no workflow.
# - The first row of the design's list is the item aged 1h, above the one aged 4m, which a list
#   ordered newest first cannot show; here the first row is aged 3m (the second 4m), so the rows keep
#   the design's order. Ages are read from the clock when each capture is taken, so the minute rows
#   can read a few minutes more than the design's.
# - Stage 1 does not enforce five seconds itself: it polls `inbox: dump view` (each dump takes a few
#   seconds) and fails when the item has not shown in 15 s, under the 30 s of the inbox's periodic
#   rescan, so a dead watcher fails it unless a rescan happens to fall in those 15 s; it prints how
#   long the item took to show. The other stages use a 90 s guard.
# - RET in the item-focus captures reads the first row (the design's picture keeps it unread); `u`
#   restores it before the next capture.
# The scenario leaves the app quit and restores settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 32-inbox skipped under BONGOS_STOCK=1: the stock run has no inbox"
  return 0
fi

command -v import >/dev/null || { echo "bongos: ImageMagick is not installed; the scenario needs it" >&2; exit 1; }

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
APP_LOG="$HOME/$BONGOS_LOG_PATH"
DUMP_FILE="$LOGS_DIR/inbox-view.txt"
INBOX="$HOME/.bongos/inbox"
SENT="$RUN/inbox-sent"
printf 'the target of the nightly item\n' >"$RUN/inbox-target.txt"

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# Write in place: a rename over the file would end the app's watch on it.
write_settings() {
  cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "theme": "$1"
}
JSON
}

# --- Delivery, from outside the app ----------------------------------------------------------------

# ago "<n> minutes": the time that long ago, as an item's `created`.
ago() { date -u -d "$1 ago" +%Y-%m-%dT%H:%M:%SZ; }

# post <name> <state> <kind> <created> <source> <summary> <actions> [group] [target] [body] [extra header]
# Writes the item under tmp/ and moves it into new/ (state `new`) or cur/ as read (state `read`), or
# leaves it there (state `tmp`).
# A copy of the bytes goes to $SENT, for comparing with what the app archives.
post() {
  local name=$1 state=$2 kind=$3 created=$4 source=$5 summary=$6 actions=$7
  local group=${8:-} target=${9:-} body=${10:-} extra=${11:-} staged="$INBOX/tmp/$1"
  {
    printf '%s\n' '---' 'inbox-version: 1'
    printf 'kind: %s\n' "$kind"
    printf 'created: %s\n' "$created"
    printf 'source: %s\n' "$source"
    [ -z "$group" ] || printf 'group: %s\n' "$group"
    [ -z "$target" ] || printf 'target: %s\n' "$target"
    [ -z "$extra" ] || printf '%s\n' "$extra"
    printf 'summary: %s\n' "$summary"
    printf 'actions: [%s]\n' "$actions"
    printf '%s\n' '---'
    printf '%s\n' "${body:-$summary}"
  } >"$staged"
  mkdir -p "$SENT"
  cp "$staged" "$SENT/$name"
  case "$state" in
    tmp) ;;
    new) mv "$staged" "$INBOX/new/$name" ;;
    read) mv "$staged" "$INBOX/cur/${name}:2,S" ;;
    *) fail "post: unknown state $state" ;;
  esac
}

file_count() { find "$INBOX/$1" -type f | wc -l; }

# The single file of a directory that starts with <name>.
file_of() { find "$INBOX/$1" -type f -name "$2*" | head -n1; }

# --- Reading the app --------------------------------------------------------------------------------

palette() {
  act keys alt+x
  act type_text "$1"
  keys Return
}

# dump: `inbox: dump view` from the window of the app, written afresh to $DUMP_FILE.
dump() {
  rm -f "$DUMP_FILE"
  palette "inbox: dump view"
  poll 30 "the inbox dump" test -s "$DUMP_FILE"
  wait_for_settled
}

# dump_until_within <seconds> <what> <command...>: dumps until the command (which reads $DUMP_FILE)
# succeeds, and fails when it has not by then (checked after each dump, so a little later).
dump_until_within() {
  local limit=$1 what=$2 deadline=$((SECONDS + $1))
  shift 2
  while :; do
    dump
    if "$@"; then
      return 0
    fi
    [ "$SECONDS" -lt "$deadline" ] || fail "$what within ${limit}s; the dump reads: $(cat "$DUMP_FILE")"
    sleep 1
  done
}

# dump_until <what> <command...>: the same with a generous guard.
dump_until() { dump_until_within 90 "$@"; }

dump_unread() { [ "$(head -n1 "$DUMP_FILE")" = "unread: $1" ]; }

# Dump predicates, for dump_until.
unread_is() { dump_unread "$1"; }
unread_is_and_has() { dump_unread "$1" && grep -qF -- "$2" "$DUMP_FILE"; }
unread_is_and_lacks() { dump_unread "$1" && ! grep -qF -- "$2" "$DUMP_FILE"; }

# The cursor row of the last dump: the line that starts with `>`.
cursor_row() { grep -m1 '^>' "$DUMP_FILE" || true; }

# --- The window --------------------------------------------------------------------------------------

launch_drawn() {
  launch "$@"
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
  MAIN_WIN=$WIN
}

# resize <width> <height>: the app window at that size, with the screen settled.
resize() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  if [ "$WIDTH" != "$1" ] || [ "$HEIGHT" != "$2" ]; then
    act xdo windowsize "$WIN" "$1" "$2"
  fi
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  [ "$WIDTH" = "$1" ] && [ "$HEIGHT" = "$2" ] ||
    fail "the window is ${WIDTH}x${HEIGHT}, not $1x$2: the harness cannot size it"
  # The pointer would tint the row under it.
  xdo mousemove 1279 799
  wait_for_settled
}

title_has() { title_matches "$1"; }

# use_theme <name>: the theme of the running app, with the screen settled.
THEME="One Dark"
use_theme() {
  if [ "$THEME" != "$1" ]; then
    THEME=$1
    act write_settings "$1"
  fi
}

# quiet_shot <name>: waits for the echo area message of the last dump to go (it lasts a few seconds),
# then saves the screenshot.
quiet_shot() {
  LAST_SCREEN=$(screen_hash)
  poll 20 "the echo area message to go" screen_changed || true
  wait_for_settled
  shot "$1"
}

# ---------------------------------------------------------------------------------------------------
step "inbox: setup, the app is launched with the Emacs base and the default settings for the inbox"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-32.json"
write_settings "One Dark"
rm -rf "$INBOX"
launch_drawn "$RUN/project"
poll 30 "the inbox directories" test -d "$INBOX/new" -a -d "$INBOX/cur" -a -d "$INBOX/tmp" -a -d "$INBOX/archive"
[ "$(stat -c %a "$INBOX/new")" = 700 ] || fail "new/ is not mode 0700: $(stat -c %a "$INBOX/new")"

# --- 1. One item from outside, no view open -------------------------------------------------------
step "inbox 1: an item moved into new/ shows with no view open; a file left in tmp/ never does"
SINGLE=20261126T000001Z-bongos-nightly-0001.md
LEFT_IN_TMP=20261126T000002Z-bongos-partial-0002.md
post "$LEFT_IN_TMP" tmp run-finished "$(ago '2 minutes')" bongos/partial "Left in tmp and never moved" "open"
post "$SINGLE" new run-finished "$(ago '2 minutes')" bongos/nightly "Nightly index finished" "open" "" \
  "$RUN/inbox-target.txt:1" "The nightly index finished without problems."
STAGE1_LIMIT=15
STARTED=$SECONDS
# The watcher delivers it in a fraction of a second; the periodic rescan, the safety net, runs every
# 30 s. Showing within STAGE1_LIMIT (under that) means the watcher did it, not the rescan.
dump_until_within "$STAGE1_LIMIT" "the delivered item does not show with no view open" unread_is_and_has 1 "Nightly index finished"
say "smoke: inbox 1: the item showed $((SECONDS - STARTED)) seconds after it was moved into new/ (limit ${STAGE1_LIMIT}s)"
grep -qF "Left in tmp and never moved" "$DUMP_FILE" && fail "an item left in tmp/ shows: $(cat "$DUMP_FILE")"
grep -qF "bongos · run finished · nightly" "$DUMP_FILE" || fail "the row has no project, type and source: $(cat "$DUMP_FILE")"
[ -f "$INBOX/new/$SINGLE" ] || fail "the item left new/ by itself"
[ -f "$INBOX/tmp/$LEFT_IN_TMP" ] || fail "the file in tmp/ was touched"

# --- 2. Nine decisions in one group and a file that does not parse -----------------------------------
step "inbox 2: nine grouped decisions and an unparseable file: one group row, one unreadable row, unread 11"
for i in 1 2 3 4 5 6 7 8 9; do
  post "20261126T0100${i}0Z-bongos-decision-000$i.md" new decision-default "$(ago "$((30 + i)) minutes")" \
    bongos/overnight-refactor "Took the default for choice $i" "accept, overturn" overnight-refactor
done
BROKEN=20261126T020000Z-garbage-0010.md
printf 'this is not an inbox item\n' >"$INBOX/tmp/$BROKEN"
touch -d '3 days ago' "$INBOX/tmp/$BROKEN"
mv "$INBOX/tmp/$BROKEN" "$INBOX/new/$BROKEN"
dump_until "the group and the unreadable row do not show with unread 11" unread_is_and_has 11 "9 items (9 unread)"
grep -qF "$BROKEN" "$DUMP_FILE" || fail "no row for the unparseable file: $(cat "$DUMP_FILE")"
grep -F "$BROKEN" "$DUMP_FILE" | grep -qF "unreadable" || fail "the unparseable file's row is not unreadable: $(cat "$DUMP_FILE")"
[ "$(grep -c '^[> ][* ] ' "$DUMP_FILE")" = 3 ] || fail "expected three rows (single, group, unreadable): $(cat "$DUMP_FILE")"
grep -qF "Left in tmp and never moved" "$DUMP_FILE" && fail "an item left in tmp/ shows: $(cat "$DUMP_FILE")"
cp "$DUMP_FILE" "$RUN/inbox-dump-delivered.txt"
quiet_shot inbox-delivered
say "smoke: screenshot $RUN/inbox-delivered.png"

# --- 3. C-c i, RET on the single item, u ---------------------------------------------------------------
step "inbox 3: C-c i opens the view; RET reads the single item, u makes it unread again"
title_has 'inbox-target\.txt' && fail "the window title names the target before it is opened: $(xdo getwindowname "$WIN")"
act keys ctrl+c i
dump_until "the view dump does not show the cursor on the single item" unread_is_and_has 11 ">* Nightly index finished"
[ -f "$INBOX/new/$SINGLE" ] || fail "opening the view renamed the item"
act keys Return
poll 30 "cur/$SINGLE:2,S" test -f "$INBOX/cur/${SINGLE}:2,S"
[ ! -e "$INBOX/new/$SINGLE" ] || fail "the item is still in new/ after RET"
dump_until "the count did not drop to 10" unread_is 10
cursor_row | grep -qF "Nightly index finished" || fail "the cursor left the single item: $(cat "$DUMP_FILE")"
cmp "$SENT/$SINGLE" "$INBOX/cur/${SINGLE}:2,S" || fail "reading the item changed its bytes"
quiet_shot inbox-item
say "smoke: screenshot $RUN/inbox-item.png (the item pane shows the item)"
act keys u
poll 30 "cur/$SINGLE:2," test -f "$INBOX/cur/${SINGLE}:2,"
[ ! -e "$INBOX/cur/${SINGLE}:2,S" ] || fail "the item is still read after u"
dump_until "the count did not come back to 11" unread_is 11
cmp "$SENT/$SINGLE" "$INBOX/cur/${SINGLE}:2," || fail "marking the item unread changed its bytes"

# --- 4. d on the group -----------------------------------------------------------------------------------
step "inbox 4: d on the group archives its nine files byte for byte; unread 2"
act keys Left
act keys n
dump_until "the cursor is not on the group row" unread_is_and_has 11 ">* Took the default for choice 1"
cursor_row | grep -qF "9 items (9 unread)" || fail "the cursor row is not the group: $(cat "$DUMP_FILE")"
act keys d
poll 30 "nine files in archive/" test "$(file_count archive)" = 9
dump_until "the unread count did not drop to 2" unread_is 2
[ "$(file_count archive)" = 9 ] || fail "archive/ holds $(file_count archive) files, not nine"
for i in 1 2 3 4 5 6 7 8 9; do
  name="20261126T0100${i}0Z-bongos-decision-000$i.md"
  moved=$(file_of archive "$name")
  [ -n "$moved" ] || fail "$name is not in archive/"
  cmp "$SENT/$name" "$moved" || fail "$name was changed on its way to archive/"
  [ ! -e "$INBOX/new/$name" ] || fail "$name is still in new/"
done
grep -qF "9 items" "$DUMP_FILE" && fail "the group row is still listed: $(cat "$DUMP_FILE")"
[ "$(file_count new)" = 1 ] || fail "new/ should hold only the unparseable file: $(find "$INBOX/new" -type f)"
[ "$(file_count cur)" = 1 ] || fail "cur/ should hold only the single item: $(find "$INBOX/cur" -type f)"

# --- 5. o, /, g ----------------------------------------------------------------------------------------------
step "inbox 5: o opens the target, / filters by kind, g scans"
dump
cursor_row | grep -qF "Nightly index finished" || act keys p
dump
cursor_row | grep -qF "Nightly index finished" || fail "the cursor is not on the single item: $(cat "$DUMP_FILE")"
act keys o
poll 60 "the title of the opened target" title_has 'inbox-target\.txt'
# Opening the target reads the item.
poll 30 "cur/$SINGLE:2,S" test -f "$INBOX/cur/${SINGLE}:2,S"
cmp "$SENT/$SINGLE" "$INBOX/cur/${SINGLE}:2,S" || fail "opening the target changed the item's bytes"
act keys ctrl+c i
dump_until "the view did not come back with unread 1" unread_is 1
act keys slash
act type_text "kind: run-finished"
act keys Return
dump_until "the filter did not leave only the run-finished row" unread_is_and_lacks 1 "unreadable"
grep -qF "Nightly index finished" "$DUMP_FILE" || fail "the filter hid the run-finished row: $(cat "$DUMP_FILE")"
cp "$DUMP_FILE" "$RUN/inbox-dump-filtered.txt"
keys g
dump_until "g changed the filtered list" unread_is_and_lacks 1 "unreadable"
act keys slash
act type_text "all"
act keys Return
dump_until "the filter was not cleared by all" unread_is_and_has 1 "unreadable"

# --- 6. Nothing interrupted --------------------------------------------------------------------------------------
step "inbox 6: no notification, no warning in the log, no item text in the log"
if grep -iE '(warn|error)[^a-z].*inbox' "$APP_LOG"; then
  fail "the log has an inbox warning or error"
fi
for text in "Nightly index finished" "Took the default for choice" "Left in tmp and never moved"; do
  if grep -qF "$text" "$APP_LOG"; then
    fail "the log holds item text: $text"
  fi
done
quiet_shot inbox
say "smoke: screenshot $RUN/inbox.png (the count in the status bar, no toast, no modal)"
quit_app

# --- 7. The design captures -----------------------------------------------------------------------------------------
step "inbox 7: the twelve items of the design on a fresh inbox directory"
rm -rf "$INBOX" "$SENT"
mkdir -p -m 700 "$INBOX/tmp" "$INBOX/new" "$INBOX/cur" "$INBOX/archive"

# item <n> <state> <kind> <minutes ago> <source> <summary> <actions> <body> [extra header]
# Each has its own group, so no two collapse into one row.
item() {
  post "design-$1.md" "$2" "$3" "$(ago "$4 minutes")" "$5" "$6" "$7" "design-$1" "" "$8" "${9:-}"
}
item 01 new decision-default 3 bongos/overnight-refactor "Chose a SQLite cache over plain files" "accept, overturn" \
  $'The unattended run **overnight-refactor** had to decide how to cache parsed project files and took the default without asking.\n\nIt chose a SQLite cache because the plan said "files first, but UI state in SQLite" and a cache is closer to state than to a note. The record names the step and the alternatives it rejected: plain files per project, and an in-memory map.\n\nThe approval could not be verified against the run record, so it is shown as unverified until you accept it.' \
  "approval: unverified"
item 02 new workflow-healed 4 bongos/12-rectangles "ship-pr healed after the lint step failed" "promote-to-profile, open-run" \
  $'The **ship-pr** run failed at the lint step because the command name changed. The healer patched this run\'s copy of the workflow and the run finished.\n\nThe base definition is a shipped profile, so the change is not saved anywhere else yet. Promoting it writes the same edit to your profile.\n\n```diff\n step lint:\n-  run: cargo clippy --all\n+  run: cargo clippy --workspace\n```'
item 03 new manual-check 180 bongos/19-review-surface "Confirm keyboard navigation in the diff" "done, open-task" \
  $'The check **diff keyboard navigation** cannot run on its own. Open the review tab, move through three files with the keys, and mark it done when focus never leaves the diff.'
# The target of the failed run is a record that is gone, so its row is stale.
post design-04.md read run-failed "$(ago '300 minutes')" "bongos/lane2 / build-release" \
  "Failed at step 4; the run was cleared" "" design-04 "$HOME/.bongos/runs/build-release.log" \
  $'The run **build-release** failed at step 4, then its working tree was cleared. Its output is gone, so the link no longer opens anything.\n\nThe item stays so you can see it happened. Dismiss it when you are done.'
item 05 read rebase-needed 1440 "bongos/stacked on Step 1" "Step 2 is blocked until it is retargeted" "open-task" \
  $'Step 1 changed during review, so **Step 2: rectangle kill**, stacked on it, needs a rebase before it can ship.'
item 06 read run-waiting 1500 "sequence-platform-api/nightly / pool lint" "Waiting on environment, position 2" "open-run" \
  $'The nightly run is queued for the **lint** pool and spends no tokens while it waits. It is second in line.'
item 07 read pr-closed-outside 2880 "bongos/#941" "Merged outside bongos" "open-task" \
  $'PR **#941** was merged from the GitHub page, not from bongos. The task moved to done.'
item 08 read run-finished 2890 "sequence-platform-api/nightly / index-docs" "Finished in 14 minutes" "open-run" \
  $'The nightly docs index finished without problems.'
item 09 read diagram-binding-stale 4320 "bongos/22-diagram-canvas" "A binding points at a moved file" "rebind" \
  $'The binding from the lanes diagram to **lanes.rs** no longer matches after the file moved.'
item 10 read run-finished 4330 "notes-site/nightly / lint" "Finished with two warnings" "open-run" \
  $'Two warnings, no errors.'
item 11 read manual-check 5760 "bongos/47-task-checks" "Eyeball the checks badge on a long title" "done" \
  $'Open the Work panel and check the badge on a task with a very long title.'
item 12 read decision-default 5770 "bongos/overnight-refactor" "Kept the old module name" "accept, overturn" \
  $'The run kept the old module name to avoid touching eleven imports.'
[ "$(file_count new)" = 3 ] || fail "three items should be unread: $(find "$INBOX/new" -type f)"
[ "$(file_count cur)" = 9 ] || fail "nine items should be read: $(find "$INBOX/cur" -type f)"

write_settings "One Dark"
# An empty project, so the window does not restore the base flow's panes: the docks are closed and
# the inbox view is all there is in the window.
mkdir -p "$RUN/inbox-project"
launch_drawn "$RUN/inbox-project"
poll 30 "the inbox directories" test -d "$INBOX/new"
palette "workspace: close all docks"
act keys ctrl+c i

# capture <size name> <width> <height> <theme name> <theme>: the list, then the item pane.
capture() {
  local size=$1 width=$2 height=$3 tone=$4 theme=$5
  resize "$width" "$height"
  use_theme "$theme"
  dump_until "the list is not the design's: three unread and the first row under the cursor" \
    unread_is_and_has 3 ">* Chose a SQLite cache over plain files"
  [ "$(grep -c '^[> ][* ] ' "$DUMP_FILE")" = 12 ] || fail "the list should have twelve rows: $(cat "$DUMP_FILE")"
  [ "$(grep -c '^.\* ' "$DUMP_FILE")" = 3 ] || fail "the three unread rows are not the first three: $(cat "$DUMP_FILE")"
  quiet_shot "design-$size-list-$tone"
  say "smoke: screenshot $RUN/design-$size-list-$tone.png"
  act keys Return
  poll 30 "the first design item read" test -n "$(file_of cur design-01.md)"
  wait_for_settled
  shot "design-$size-item-focus-$tone"
  say "smoke: screenshot $RUN/design-$size-item-focus-$tone.png"
  # Back to the list, and the first row unread again, as the next capture needs it.
  act keys Left
  act keys u
  poll 30 "the first design item unread" test -f "$INBOX/cur/design-01.md:2,"
  dump_until "the first design row is not unread again" unread_is 3
}

capture fullscreen 1280 720 dark "One Dark"
capture fullscreen 1280 720 light "One Light"
capture halfscreen 640 720 dark "One Dark"
capture halfscreen 640 720 light "One Light"
quit_app

# --- 8. The sidebar captures ---------------------------------------------------------------------------------------------
step "inbox 8: the list alone in a dock panel beside an ordinary editor, and the item in a modal"
cat >"$RUN/inbox-project/Rectangle.kt" <<'KOTLIN'
package bongos.edit

data class Rectangle(
    val top: Int, val left: Int,
    val bottom: Int,
)

// a custom screen with the inbox sidebar added
KOTLIN
THEME="One Dark"
write_settings "One Dark"
launch_drawn "$RUN/inbox-project/Rectangle.kt"
palette "workspace: close all docks"
resize 1280 720
palette "inbox: open as sidebar"

# capture_sidebar <tone> <theme>: the list in the sidebar with the first row under the cursor, then
# RET on it for the modal.
capture_sidebar() {
  local tone=$1 theme=$2
  use_theme "$theme"
  dump_until "the sidebar is not the design's: three unread and the first row under the cursor" \
    unread_is_and_has 3 ">* Chose a SQLite cache over plain files"
  [ "$(grep -c '^[> ][* ] ' "$DUMP_FILE")" = 12 ] || fail "the sidebar should list twelve rows: $(cat "$DUMP_FILE")"
  quiet_shot "design-sidebar-list-$tone"
  say "smoke: screenshot $RUN/design-sidebar-list-$tone.png"
  act keys Return
  poll 30 "the first design item read" test -n "$(file_of cur design-01.md)"
  wait_for_settled
  shot "design-sidebar-modal-$tone"
  say "smoke: screenshot $RUN/design-sidebar-modal-$tone.png"
  # Esc closes the modal and the list has the focus again; the first row goes back to unread.
  act keys Escape
  act keys u
  poll 30 "the first design item unread" test -f "$INBOX/cur/design-01.md:2,"
  dump_until "the first design row is not unread again" unread_is 3
}

capture_sidebar dark "One Dark"
capture_sidebar light "One Light"

quit_app
cp "$RUN/config-backup/settings-32.json" "$SETTINGS_FILE"
