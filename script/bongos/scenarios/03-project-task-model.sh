# Scenario for feature 03-project-task-model. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only: it reads the Work panel through
# `work: dump view` (<logs>/work-view.txt) and what a chat session would be told through
# `work: dump session context` (<logs>/work-session-context.json).
#
# Setup: a git repository $HOME/src/invoicing with one commit, `"base_keymap": "Emacs"` (the C-c w
# keys exist only there) and `work.repositories` naming it. Files are the model, so most checks read
# task.toml, artifacts.toml and git; the rows of the panel come from the dump.
#   1. C-c w p n project, C-c w N group and two pr-ship leaves (C-c w n makes a one-off, not a
#      child), C-c w e stacked edge, C-c w s on the first: worktree, `git worktree list`,
#      info/exclude within ten seconds; the second's row is Blocked.
#   2. C-c w a and C-c w d b on the first: the second's task.toml is unchanged and its row Ready;
#      C-c w s on it cuts its branch from the first's branch.
#   3. Advancing the second to its ships todo is refused while the first is not Done.
#   4. A stack tree written as files shows within five seconds with no error; a second stacked on
#      is refused.
#   5. A document leaf: spec.md and notes.md, C-c w l link, C-c w h notes.md; the session context
#      lists the other three; C-c w H then C-c w U round-trips.
#   6. A group of twelve leaves written as files reads 4/12, and 4/11 after task: drop.
#   7. One-offs written with old times: two days old fades, four days old is under Stale.
#   8. Quit and relaunch: the drill path and the focused task come back.
#   9. No request through the proxy beyond the two kinds every launch makes (see Work 9).
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 03-project-task-model skipped under BONGOS_STOCK=1: the stock run has no Work view"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
VIEW_FILE="$LOGS_DIR/work-view.txt"
CONTEXT_FILE="$LOGS_DIR/work-session-context.json"
REPO="$HOME/src/invoicing"
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/invoicing"
GROUP_DIR="$PROJECT_DIR/tasks/usage-events"
FIRST_DIR="$GROUP_DIR/tasks/backfill-usage-events"
SECOND_DIR="$GROUP_DIR/tasks/bill-usage-events"
DOC_DIR="$PROJECT_DIR/tasks/write-the-usage-spec"
STACK_DIR="$PROJECT_DIR/tasks/stack-tree"
ROLLUP_DIR="$PROJECT_DIR/tasks/quarter-close"
STAGE="$RUN/stage-03"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

for tool in git python3 sha256sum; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# --- Input -----------------------------------------------------------------------------------

# C-c w <keys>: each binding of the D17 table opens a picker or a prompt, so the screen changes.
wkeys() { act keys ctrl+c w "$@"; }

# Types into the open picker or prompt and confirms. A picker keeps the first row that contains
# every word typed.
choose() {
  act type_text "$1"
  act keys Return
}

# Runs a command by its palette name, for the commands with no key.
palette() {
  act keys ctrl+shift+p
  choose "$1"
}

# Clicks the center of the window, so the workspace (and not a dock or nothing) has the focus.
focus_center() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH * 3 / 4)) $((HEIGHT / 3)) click 1
  wait_for_settled
}

# Opens the Work panel with the focus in it. `work: toggle panel` takes the focus away when the
# panel has it, so the center is focused first.
focus_panel() {
  focus_center
  keys ctrl+c w w
  wait_for_settled
}

# Launch with no arguments (the session restores the workspaces) and wait until it is drawn.
launch_drawn() {
  launch
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# --- Reading the Work view --------------------------------------------------------------------
# A dump is the breadcrumb, then one line per line of the level: a marker (`>` selected row, `~`
# faded, else a space) and the text. A title line is the goal, or the goal, two spaces and the
# state (leaf) or age (group); a detail line is indented four spaces, an edge marker two.

dump_view() {
  rm -f "$VIEW_FILE"
  palette "work: dump view"
  assert_file "$VIEW_FILE"
  cp "$VIEW_FILE" "$RUN/work-view-$1.txt"
}

breadcrumb() { head -n1 "$VIEW_FILE"; }

# The title of the row of <goal>, without its marker.
view_title() {
  awk -v goal="$1" 'NR > 1 && substr($0, 2, 1) != " " {
    t = substr($0, 2); if (t == goal || index(t, goal "  ") == 1) { print t; exit }
  }' "$VIEW_FILE"
}

# The detail line of the row of <goal>, without its marker and indent.
view_detail() {
  awk -v goal="$1" 'NR > 1 {
    t = substr($0, 2)
    if (hit) { sub(/^ +/, "", t); print t; exit }
    if (substr(t, 1, 1) != " " && (t == goal || index(t, goal "  ") == 1)) hit = 1
  }' "$VIEW_FILE"
}

# The marker of the detail line of the row of <goal>: `~` when the row is faded.
view_detail_marker() {
  awk -v goal="$1" 'NR > 1 {
    if (hit) { print substr($0, 1, 1); exit }
    t = substr($0, 2)
    if (substr(t, 1, 1) != " " && (t == goal || index(t, goal "  ") == 1)) hit = 1
  }' "$VIEW_FILE"
}

expect_title() {
  local got
  got=$(view_title "$1")
  [ "$got" = "$2" ] || fail "the Work view row of '$1' reads '${got:-no such row}', expected '$2': $(cat "$VIEW_FILE")"
}

expect_detail() {
  local got
  got=$(view_detail "$1")
  [ "$got" = "$2" ] || fail "the Work view detail of '$1' reads '${got:-no such row}', expected '$2': $(cat "$VIEW_FILE")"
}

# Moves the panel's selection to the row of <goal>. With `reuse`, the last dump is still what the
# panel shows (nothing has run since it), so no new one is taken.
select_row() {
  local pos sel tgt
  if [ "${2:-}" != reuse ]; then dump_view select; fi
  pos=$(awk -v goal="$1" 'NR > 1 && substr($0, 2, 1) != " " {
    n++; t = substr($0, 2)
    if (substr($0, 1, 1) == ">") sel = n
    if (t == goal || index(t, goal "  ") == 1) tgt = n
  } END { print sel + 0, tgt + 0 }' "$VIEW_FILE")
  sel=${pos% *}
  tgt=${pos#* }
  [ "$tgt" -gt 0 ] || fail "no row '$1' in the Work view: $(cat "$VIEW_FILE")"
  focus_panel
  if [ "$sel" = 0 ]; then
    keys Down
    sel=1
  fi
  while [ "$sel" -lt "$tgt" ]; do keys Down; sel=$((sel + 1)); done
  while [ "$sel" -gt "$tgt" ]; do keys Up; sel=$((sel - 1)); done
  wait_for_settled
}

# RET on the row of the group <goal>: the panel shows its tasks. `reuse` as for select_row.
drill() {
  select_row "$1" "${2:-}"
  act keys Return
  dump_view drill
  [ "$(breadcrumb | sed 's/.* > //')" = "$1" ] || fail "drilling into '$1' shows '$(breadcrumb)'"
}

# `work: up` until the panel shows the top level.
go_top() {
  local i
  for i in 1 2 3 4 5; do
    dump_view top
    [ "$(breadcrumb)" = Work ] && return 0
    palette "work: up"
  done
  fail "work: up did not reach the top level: $(breadcrumb)"
}

project_level() {
  go_top
  drill Invoicing reuse
}

# --- Reading files ----------------------------------------------------------------------------

# toml <file> <python expression over d, the parsed file>
toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 10 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

# One line per artifact in <dir>/artifacts.toml: path=visibility.
visibilities() {
  toml "$1/artifacts.toml" '"\n".join(sorted(a["path"] + "=" + a.get("visibility", "default") for a in d.get("artifact", [])))'
}

# The goal and the file names of the shared artifacts in the dumped session context.
context_summary() {
  python3 -I -c 'import json, sys
c = json.load(open(sys.argv[1]))
print(c["goal"] + ": " + " ".join(sorted(a["path"].rsplit("/", 1)[-1] for a in c["artifacts"])))' "$CONTEXT_FILE"
}

dump_context() {
  rm -f "$CONTEXT_FILE"
  palette "work: dump session context"
  assert_file "$CONTEXT_FILE"
  cp "$CONTEXT_FILE" "$RUN/work-session-context-$1.json"
}

expect_context() {
  local got
  dump_context "$1"
  got=$(context_summary)
  [ "$got" = "$2" ] || fail "the session context lists '$got', expected '$2'"
}

# --- Writing tasks by hand, as an agent would ----------------------------------------------------

iso_ago() { date -u -d "$1" +%Y-%m-%dT%H:%M:%SZ; }

new_id() { cat /proc/sys/kernel/random/uuid; }

PR_TODOS='[[todos]]
name = "implement"
role = "produces-branch"
state = "open"

[[todos]]
name = "review"
state = "open"

[[todos]]
name = "ship"
role = "ships"
state = "open"'

# write_task <dir> <goal> <kind> <status> <created> [<more toml>]
write_task() {
  mkdir -p "$1"
  {
    printf 'schema = 1\nid = "%s"\ngoal = "%s"\nkind = "%s"\nstatus = "%s"\ncreated = %s\n' \
      "$(new_id)" "$2" "$3" "$4" "$5"
    printf 'default_visibility = "shared"\n'
    if [ -n "${6:-}" ]; then printf '%s\n' "$6"; fi
  } >"$1/task.toml"
}

# A stacked pr-ship leaf of the stack tree: <slug> <goal> [<slug it is stacked on>]
write_stacked() {
  local more=""
  if [ -n "${3:-}" ]; then more="stacked_on = { task = \"$3\", unblock = \"review\" }"$'\n'; fi
  write_task "$STAGE/stack-tree/tasks/$1" "$2" pr-ship open "$NOW" "$more$PR_TODOS"
}

# A one-off of <activity>, all of whose times are <age> ago: created, history.at and file mtimes.
write_oneoff() {
  local activity=$1 goal=$2 slug=$3 age=$4 at dir
  at=$(iso_ago "$age")
  dir="$STAGE/oneoffs/$activity/$(date -u -d "$age" +%F)-$slug"
  write_task "$dir" "$goal" spike open "$at" "activity = \"$activity\"

[[todos]]
name = \"explore\"
state = \"open\"

[[todos]]
name = \"write up\"
state = \"open\"

[[history]]
at = $at
by = \"kyle\"
event = \"created\"
detail = \"$goal\""
  printf 'notes on %s\n' "$goal" >"$dir/notes.md"
  : >"$dir/prompts.txt"
  find "$dir" -exec touch -d "$age" {} +
}

# Moves a staged directory into the tree in one rename, then waits until the panel shows it.
move_in_and_wait() {
  LAST_SCREEN=$(screen_hash)
  mv "$1" "$2"
  poll 5 "the Work panel to show $2 (an external edit) within five seconds" screen_changed
  wait_for_settled
}

# ---------------------------------------------------------------------------------------------
step "Work setup: a repository with one commit, base_keymap Emacs, work.repositories"
# Work 9 looks only at the requests made from here on.
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-03.json"
git init -q -b main "$REPO"
printf '# invoicing\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" "${GIT_ID[@]}" commit -qm "first commit"
MAIN_SHA=$(git -C "$REPO" rev-parse main)
printf '%s\n' '{
  "base_keymap": "Emacs",
  "work": {
    "repositories": [
      { "name": "invoicing", "path": "~/src/invoicing", "deployments": true, "merge_is_ship": true, "branch_template": "{slug}" }
    ]
  }
}' >"$SETTINGS_FILE"
# Made before the app starts, so the store watches it from the first load.
mkdir -p "$WORK_ROOT/oneoffs"
rm -rf "$STAGE"
mkdir -p "$STAGE"
launch_drawn
dump_view start
[ "$(breadcrumb)" = Work ] || fail "the Work view did not start at the top level: $(cat "$VIEW_FILE")"

# ---------------------------------------------------------------------------------------------
step "Work 1: project, group, two pr-ship leaves, stacked edge, task: start makes the worktree"
focus_center
wkeys p n
choose Invoicing
assert_file "$PROJECT_DIR/project.toml"
assert_file "$PROJECT_DIR/task.toml"
poll 10 "git init in $PROJECT_DIR" test -d "$PROJECT_DIR/.git"
# No command names a project's repository; edit project.toml in place, as Kyle or an agent would.
python3 -I -c 'import re, sys
p = sys.argv[1]
t = open(p).read()
line = "repositories = [\"invoicing\"]"
if re.search(r"^repositories = .*$", t, re.M):
    t = re.sub(r"^repositories = .*$", line, t, count=1, flags=re.M)
else:
    t = re.sub(r"^(title = .*)$", lambda m: m.group(1) + "\n" + line, t, count=1, flags=re.M)
open(p, "w").write(t)' "$PROJECT_DIR/project.toml"
wait_toml "$PROJECT_DIR/project.toml" 'd["repositories"]' "['invoicing']"
# The picker lists the project's top group first: every other task's label starts with its title.
wkeys shift+n
choose invoicing
choose "Usage events"
assert_file "$GROUP_DIR/task.toml"
wkeys shift+n
choose "usage events"
choose "Backfill usage events"
assert_file "$FIRST_DIR/task.toml"
wkeys shift+n
choose "usage events"
choose "Bill usage events"
assert_file "$SECOND_DIR/task.toml"
for dir in "$FIRST_DIR" "$SECOND_DIR"; do
  [ "$(toml "$dir/task.toml" 'd["kind"]')" = pr-ship ] || fail "$dir is not a pr-ship leaf: $(cat "$dir/task.toml")"
done
[ "$(toml "$GROUP_DIR/task.toml" 'len(d.get("todos", []))')" = 0 ] || fail "Usage events kept its todos after gaining children"
# Bill usage events is stacked on Backfill usage events, unblocked at review (the default).
wkeys e
choose "bill usage"
choose backfill
choose stacked
act keys Return
wait_toml "$SECOND_DIR/task.toml" 'd["stacked_on"]["task"] + " " + d["stacked_on"]["unblock"]' "backfill-usage-events review"
FIRST_WT="$REPO/.bongos-worktrees/backfill-usage-events"
wkeys s
choose backfill
poll 10 "the worktree $FIRST_WT within ten seconds" test -d "$FIRST_WT"
git -C "$REPO" worktree list --porcelain | grep -qE '^worktree .*/\.bongos-worktrees/backfill-usage-events$' ||
  fail "git worktree list does not show $FIRST_WT: $(git -C "$REPO" worktree list)"
grep -qxF '/.bongos-worktrees/' "$REPO/.git/info/exclude" || fail "info/exclude lacks /.bongos-worktrees/: $(cat "$REPO/.git/info/exclude")"
[ "$(grep -cxF '/.bongos-worktrees/' "$REPO/.git/info/exclude")" = 1 ] || fail "info/exclude has the line more than once"
wait_toml "$FIRST_DIR/task.toml" 'd.get("branch")' backfill-usage-events
[ "$(toml "$FIRST_DIR/task.toml" 'd["base"]')" = main ] || fail "the first task's base is not main: $(cat "$FIRST_DIR/task.toml")"
[ -z "$(git -C "$REPO" status --porcelain)" ] || fail "the repository is not clean with the worktree in it: $(git -C "$REPO" status --porcelain)"
drill Invoicing
drill "Usage events" reuse
expect_title "Bill usage events" "Bill usage events  Blocked"
expect_title "Backfill usage events" "Backfill usage events  In Progress"
shot work-blocked
frame_has_ui work-blocked
say "smoke: worktree, git worktree list and info/exclude; the stacked task reads Blocked: $RUN/work-blocked.png"

# ---------------------------------------------------------------------------------------------
step "Work 2: implement done and review started unblock the stacked task; its branch starts on the first's"
second_before=$(sha256sum <"$SECOND_DIR/task.toml")
wkeys a
choose backfill
wait_toml "$FIRST_DIR/task.toml" 'd["todos"][0]["state"]' done
wkeys d b
choose backfill
choose review
wait_toml "$FIRST_DIR/task.toml" '"started" in d["todos"][1]' True
[ "$(sha256sum <"$SECOND_DIR/task.toml")" = "$second_before" ] || fail "the second task's task.toml changed: $(cat "$SECOND_DIR/task.toml")"
dump_view unblocked
expect_title "Backfill usage events" "Backfill usage events  Review"
expect_title "Bill usage events" "Bill usage events  Ready"
# A commit on the first's branch, so a branch cut from main and one cut from it differ.
printf 'backfill\n' >"$FIRST_WT/backfill.txt"
git -C "$FIRST_WT" add backfill.txt
git -C "$FIRST_WT" "${GIT_ID[@]}" commit -qm "backfill usage events"
FIRST_SHA=$(git -C "$REPO" rev-parse backfill-usage-events)
[ "$FIRST_SHA" != "$MAIN_SHA" ] || fail "the commit on backfill-usage-events did not land"
SECOND_WT="$REPO/.bongos-worktrees/bill-usage-events"
wkeys s
choose "bill usage"
poll 10 "the worktree $SECOND_WT within ten seconds" test -d "$SECOND_WT"
wait_toml "$SECOND_DIR/task.toml" 'd.get("base")' backfill-usage-events
[ "$(git -C "$REPO" merge-base bill-usage-events backfill-usage-events)" = "$FIRST_SHA" ] ||
  fail "bill-usage-events does not start on backfill-usage-events (merge-base $(git -C "$REPO" merge-base bill-usage-events backfill-usage-events), want $FIRST_SHA)"
[ "$(git -C "$REPO" rev-parse bill-usage-events)" = "$FIRST_SHA" ] || fail "bill-usage-events was not cut at the head of backfill-usage-events"
say "smoke: the stacked task read Ready with its task.toml unchanged; its branch starts at $FIRST_SHA"

# ---------------------------------------------------------------------------------------------
step "Work 3: the stacked task cannot ship while the task under it is not Done"
wkeys a
choose "bill usage"
wait_toml "$SECOND_DIR/task.toml" 'd["todos"][0]["state"]' done
wkeys a
choose "bill usage"
wait_toml "$SECOND_DIR/task.toml" 'd["todos"][1]["state"]' done
second_before=$(sha256sum <"$SECOND_DIR/task.toml")
wkeys a
choose "bill usage"
# The refusal is a toast; give the writer its turn, then nothing may have changed.
wait_for_settled
shot work-ship-refused
frame_has_ui work-ship-refused
[ "$(sha256sum <"$SECOND_DIR/task.toml")" = "$second_before" ] || fail "advancing to the ships todo changed task.toml: $(cat "$SECOND_DIR/task.toml")"
[ "$(toml "$SECOND_DIR/task.toml" 'd["todos"][2]["state"] + " " + str("ship_overridden" in d)')" = "open False" ] ||
  fail "the ships todo was completed before the first task was Done: $(cat "$SECOND_DIR/task.toml")"
say "smoke: the ship was refused; the toast (bill-usage-events cannot ship before backfill-usage-events is Done (it is Review); use task: advance override) is in $RUN/work-ship-refused.png"

# ---------------------------------------------------------------------------------------------
step "Work 4: a stack tree written as files is read within five seconds; a second stacked on is refused"
NOW=$(iso_ago now)
write_task "$STAGE/stack-tree" "Stack tree" other open "$NOW"
write_stacked base Base
write_stacked left Left base
write_stacked right Right base
write_stacked left-top "Left top" left
write_stacked right-top "Right top" right
palette "work: up"
dump_view project
[ "$(breadcrumb)" = "Work > Invoicing" ] || fail "work: up did not show the project: $(breadcrumb)"
move_in_and_wait "$STAGE/stack-tree" "$STACK_DIR"
dump_view stack-project
[ -n "$(view_title "Stack tree")" ] || fail "the stack tree is not in the Work view: $(cat "$VIEW_FILE")"
case $(view_detail "Stack tree") in *error*) fail "the stack tree has an error: $(view_detail "Stack tree")" ;; esac
drill "Stack tree" reuse
for goal in Base Left Right "Left top" "Right top"; do
  [ -n "$(view_title "$goal")" ] || fail "no row '$goal' in the stack tree: $(cat "$VIEW_FILE")"
  case $(view_detail "$goal") in *error*) fail "'$goal' has an error: $(view_detail "$goal")" ;; esac
done
[ "$(grep -cxE '.  stacked on Base' "$VIEW_FILE")" = 2 ] || fail "the stack tree does not show two tasks stacked on Base: $(cat "$VIEW_FILE")"
grep -qxE '.  stacked on Left' "$VIEW_FILE" && grep -qxE '.  stacked on Right' "$VIEW_FILE" ||
  fail "the stack tree does not show the tops stacked on Left and Right: $(cat "$VIEW_FILE")"
left_top_before=$(sha256sum <"$STACK_DIR/tasks/left-top/task.toml")
wkeys e
choose "left top"
choose "(right)"
choose stacked
act keys Return
wait_for_settled
shot work-second-stack-refused
[ "$(sha256sum <"$STACK_DIR/tasks/left-top/task.toml")" = "$left_top_before" ] ||
  fail "a second stacked on was written: $(cat "$STACK_DIR/tasks/left-top/task.toml")"
palette "work: up"
say "smoke: two stacked children each with a child accepted; a second stacked on refused ($RUN/work-second-stack-refused.png)"

# ---------------------------------------------------------------------------------------------
step "Work 5: a document leaf's artifacts, the lock, and what a chat session is told"
focus_center
wkeys shift+n
choose invoicing
choose "Write the usage spec"
assert_file "$DOC_DIR/task.toml"
palette "task: edit"
choose "usage spec"
act keys Return
choose document
wait_toml "$DOC_DIR/task.toml" 'd["kind"]' document
printf '# Usage spec\n' >"$DOC_DIR/spec.md"
printf 'private notes\n' >"$DOC_DIR/notes.md"
wkeys l
choose "usage spec"
choose "https://www.notion.so/acme/usage-spec"
choose "Usage spec in Notion"
assert_file "$DOC_DIR/usage-spec-in-notion.link.toml" 'notion\.so/acme/usage-spec'
wkeys h
choose "usage spec"
choose notes
assert_file "$DOC_DIR/artifacts.toml" 'human-only'
[ "$(visibilities "$DOC_DIR" | xargs)" = "notes.md=human-only prompts.txt=shared spec.md=shared usage-spec-in-notion.link.toml=shared" ] ||
  fail "artifacts.toml after hiding notes.md: $(cat "$DOC_DIR/artifacts.toml")"
select_row "Write the usage spec"
act keys Tab
dump_view artifacts
grep -qE '^.      notes\.md  locked$' "$VIEW_FILE" || fail "the expanded row does not show notes.md locked: $(cat "$VIEW_FILE")"
case $(view_detail "Write the usage spec") in *"4 artifacts, 1 locked"*) ;; *) fail "the document row does not count the lock: $(view_detail "Write the usage spec")" ;; esac
shot artifact-locked
frame_has_ui artifact-locked
# Focus the task (its directory: a document has no worktree) so the context can be dumped.
wkeys f
choose "usage spec"
focus_center
expect_context shared "Write the usage spec: prompts.txt spec.md usage-spec-in-notion.link.toml"
if grep -q 'notes' "$CONTEXT_FILE"; then fail "the session context mentions the hidden notes.md: $(cat "$CONTEXT_FILE")"; fi
wkeys shift+h
choose "usage spec"
poll 10 "every artifact human-only" sh -c '! grep -q "visibility = \"shared\"" "$1"' _ "$DOC_DIR/artifacts.toml"
[ "$(visibilities "$DOC_DIR" | xargs)" = "notes.md=human-only prompts.txt=human-only spec.md=human-only usage-spec-in-notion.link.toml=human-only" ] ||
  fail "artifacts.toml after hide all: $(cat "$DOC_DIR/artifacts.toml")"
expect_context hidden "Write the usage spec: "
wkeys shift+u
choose "usage spec"
poll 10 "every artifact shared" sh -c '! grep -q "visibility = \"human-only\"" "$1"' _ "$DOC_DIR/artifacts.toml"
[ "$(visibilities "$DOC_DIR" | xargs)" = "notes.md=shared prompts.txt=shared spec.md=shared usage-spec-in-notion.link.toml=shared" ] ||
  fail "artifacts.toml after share all: $(cat "$DOC_DIR/artifacts.toml")"
expect_context all-shared "Write the usage spec: notes.md prompts.txt spec.md usage-spec-in-notion.link.toml"
say "smoke: lock shown ($RUN/artifact-locked.png); the context left out the hidden file; hide all and share all round-trip"

# ---------------------------------------------------------------------------------------------
step "Work 6: a group of twelve reads 4/12, then 4/11 after a drop"
NOW=$(iso_ago now)
write_task "$STAGE/quarter-close" "Quarter close" other open "$NOW"
for n in 01 02 03 04 05 06 07 08 09 10 11 12; do
  if [ "${n#0}" -le 4 ]; then
    write_task "$STAGE/quarter-close/tasks/step-$n" "Step $n" other done "$NOW" "closed = $NOW

[[todos]]
name = \"do it\"
state = \"done\""
  else
    write_task "$STAGE/quarter-close/tasks/step-$n" "Step $n" other open "$NOW" '[[todos]]
name = "do it"
state = "open"'
  fi
done
project_level
move_in_and_wait "$STAGE/quarter-close" "$ROLLUP_DIR"
dump_view rollup-12
expect_detail "Quarter close" "4/12"
shot rollup-4-of-12
palette "task: drop"
choose "step 05"
wait_toml "$ROLLUP_DIR/tasks/step-05/task.toml" 'd["status"]' cancelled
dump_view rollup-11
expect_detail "Quarter close" "4/11"
shot rollup-4-of-11
frame_has_ui rollup-4-of-11
say "smoke: Quarter close read 4/12, then 4/11 after the drop: $RUN/rollup-4-of-12.png $RUN/rollup-4-of-11.png"

# ---------------------------------------------------------------------------------------------
step "Work 7: idle one-offs: two days old fades, four days old is Stale"
write_oneoff support "Check the refund queue" check-the-refund-queue "2 days ago"
write_oneoff research "Compare ledger vendors" compare-ledger-vendors "4 days ago"
go_top
move_in_and_wait "$STAGE/oneoffs/support" "$WORK_ROOT/oneoffs/support"
move_in_and_wait "$STAGE/oneoffs/research" "$WORK_ROOT/oneoffs/research"
dump_view idle
[ -n "$(view_title support)" ] || fail "the support one-offs are not at the top level: $(cat "$VIEW_FILE")"
[ "$(view_detail_marker support)" = "~" ] || fail "the two days old one-off does not fade: $(cat "$VIEW_FILE")"
[ -n "$(view_title Invoicing)" ] && [ "$(view_detail_marker Invoicing)" != "~" ] || fail "the project fades: $(cat "$VIEW_FILE")"
expect_title "Stale (1)" "Stale (1)  folded"
[ -z "$(view_title research)" ] || fail "the four days old one-off shows outside Stale: $(cat "$VIEW_FILE")"
palette "work: show stale"
dump_view stale
awk 'NR > 1 { t = substr($0, 2) } t ~ /^Stale \(1\)/ { s = NR } t ~ /^research  / && s { r = NR } END { exit !r }' "$VIEW_FILE" ||
  fail "research is not listed under Stale: $(cat "$VIEW_FILE")"
shot work-stale
frame_has_ui work-stale
say "smoke: support fades and research is Stale: $RUN/work-stale.png"

# ---------------------------------------------------------------------------------------------
step "Work 8: quit and relaunch restore the drill path and the focused task"
drill Invoicing reuse
drill "Usage events" reuse
quit_app
launch_drawn
focus_center
restored() {
  dump_view restored
  [ "$(breadcrumb)" = "Work > Invoicing > Usage events" ]
}
poll 30 "the drill path Work > Invoicing > Usage events after a relaunch" restored
expect_context restored "Write the usage spec: notes.md prompts.txt spec.md usage-spec-in-notion.link.toml"
focus_panel
shot work-restored
frame_has_ui work-restored
quit_app
say "smoke: drill path and focus restored: $RUN/work-restored.png"

# ---------------------------------------------------------------------------------------------
step "Work 9: no network"
# Every launch of the app, with or without the Work view, makes two kinds of request that the base
# flow shows too: a CONNECT to the host of REGISTRY_URL (the ACP agent registry fetch when its cache
# is empty, AgentRegistryStore::init_global; the host is read from that source file rather than
# named here, so check-hosts keeps one allow entry for it) and GET localhost (the local
# model providers, such as Ollama, asking a server on this machine for its models). Neither comes
# from the Work view. Anything else the scenario sent through the proxy is a failure.
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
work_requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
stray_requests=$(printf '%s\n' "$work_requests" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
if [ -n "$stray_requests" ]; then
  echo "bongos: the Work scenario reached the proxy for other hosts:" >&2
  printf '%s\n' "$stray_requests" | sort | uniq -c >&2
  exit 1
fi
say "smoke: no request beyond the app's startup ones ($(printf '%s' "$work_requests" | grep -c . || true) requests, all to the ACP registry or localhost)"

cp "$RUN/config-backup/settings-03.json" "$SETTINGS_FILE"
