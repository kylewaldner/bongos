# Scenario for feature 48-lanes-and-dispatch. Sourced by script/bongos/smoke after the base flow
# (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only.
#
# Setup: `"base_keymap": "Emacs"`, one code repository $HOME/src/storefront in `work.repositories`,
# and a new-format project "storefront" built as a git repository (definitions, a lane table, a
# fake workflow, six tasks) and added with `project: add existing` from a local bare remote. The
# app is then quit and every stage that says "shell" drives the `bongos` command line with no
# display (DISPLAY unset), the way a shell on a machine with no screen would.
# The fake workflow "fake job" is a Python program node and the shipped `merge` node. It waits while
# the file $SCRATCH/hold names the task's goal (one goal a line) and fails when $SCRATCH/fail names
# it, so a scenario holds a job, lets it end or fails it without a model.
# The stages run in this order: setup (the project is added from a window, the six tasks are
# started with `task: start`, a lane job runs in the task's own tree), then 3, 1, 2, 4, 5, 6, 7.
#   3. Every code task has the area `editor`. `lanes dry-run` (served from the store while no process
#      holds the project) shows build-1 taking Add editor palette and build-2 nothing, the tag
#      `editor` named for each waiting task. An `[[exempt]]` entry for `editor` lands from a window
#      (a working copy and `project: land`); the next dry run shows both editor tasks picked.
#   1. `bongos lanes start` from a shell with no display starts a host; `bongos lanes status` shows
#      four lanes; `status --json` parses (schema "lanes-status 1"); both build lanes run the two
#      editor tasks together, held by the hold file.
#   2. A window opens: `host.json` still names the host (the window attached as a client), the
#      Lanes view is shown (`C-c f L L`, a screenshot), the window closes and `status` still shows
#      the lanes running.
#   4. Add editor tabs fails: build-2 halts and build-1 frees. `lanes: adopt` (`C-c f L o`: the task,
#      build-1, a reason) puts "adopted by build-1 from build-2: <reason>" in the task's history in the
#      state store, and build-1 runs the task to its end.
#   5. build-2 runs a held job and is asked to stop (`bongos lanes stop build-2`): it ends the job and
#      stops, and no other lane takes the tasks pinned to it.
#   6. The host this scenario started is killed (`kill -9` of the pid in `host.json`); `bongos lanes
#      start` starts another; the interrupted run of design-1 is the same run, waiting on you
#      (`waitingonyou`); every run record of a lane has its own id.
#   7. `bongos host stop`; the host is ended by its pid; a window opened on the project is the writer
#      in window mode; `bongos run "fake job" --task <id>` reaches it and no engine host starts;
#      `lanes: start` (summary, then the confirmation) hands the writer to a new host and the window
#      becomes a client (`host.json` names the host, the window still runs).
# Deviations from the brief, each forced by what the build offers today:
# - Stage 3 and stage 1 swap places: `lanes dry-run` is answered only while no process holds the
#   project (a running host says it is not served yet), so it runs before the lanes start.
# - Stage 2 reads the Lanes view from a screenshot (there is no dump of the view): what is asserted
#   is that `host.json` still names the host with the window open, and that the view was drawn.
# - Stage 5: the count of `build` cannot be lowered from a script (a lane table lands only from the
#   window that holds the writer, and a test `LaneProfileSource` is code), so build-2 is stopped by
#   its own command, which ends a lane at its job boundary the same way.
# - Stage 7: `bongos host stop` is refused by a running host in this build ("does not take Stop"), so
#   the line is run and its answer logged, and the host is ended with `kill` of its own pid; lanes
#   are stopped with `bongos lanes stop` at the end.
# - A program node runs under the audit hook alone: the smoke sandbox is itself a bubblewrap sandbox
#   whose /tmp/h is a symlink, so a `bwrap` that always fails is put first on PATH.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 48-lanes-and-dispatch skipped under BONGOS_STOCK=1: the stock run has no lanes"
  return 0
fi

for tool in git python3 find awk; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
PROJECTS="$DATA_DIR/projects"
PROJ="$PROJECTS/storefront"
REPO="$PROJ/repository"
CODE="$HOME/src/storefront"
SEED="$RUN/seed-48"
BARE="$RUN/remote-48/storefront.git"
SCRATCH="$RUN/lanes-48"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

# --- Input -----------------------------------------------------------------------------------

wkeys() { act keys ctrl+c w "$@"; }
lkeys() { act keys ctrl+c f shift+l "$@"; }

choose() {
  act type_text "$1"
  act keys Return
}

palette() {
  act keys ctrl+shift+p
  choose "$1"
}

focus_center() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH * 3 / 4)) $((HEIGHT / 3)) click 1
  wait_for_settled
}

launch_drawn() {
  launch "$@"
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# --- The command line, with no display -------------------------------------------------------------

# The command line is the `cli` binary next to the app; it finds the app by itself.
BONGOS_CLI=${BONGOS_CLI:-$(dirname "$BONGOS_BIN")/cli}
CLI_OUT="$RUN/cli-48.out"
CLI_ERR="$RUN/cli-48.err"
CLI_CODE=0

# cli <args>: runs `bongos <args>` (the cli binary) with no display; the output goes to $CLI_OUT and $CLI_ERR and the
# exit code to $CLI_CODE.
cli() {
  CLI_CODE=0
  env -u DISPLAY -u WAYLAND_DISPLAY "$BONGOS_CLI" "$@" >"$CLI_OUT" 2>"$CLI_ERR" || CLI_CODE=$?
}

cli_ok() {
  cli "$@"
  [ "$CLI_CODE" = 0 ] || fail "bongos $* exited $CLI_CODE: $(cat "$CLI_ERR") $(cat "$CLI_OUT")"
}

# status_json: `lanes status --json` in $CLI_OUT.
status_json() { cli lanes status --json; [ "$CLI_CODE" = 0 ]; }

# lane_field <lane id> <field>: the field of a lane in the last `status --json`, empty for null.
lane_field() {
  python3 -I - "$CLI_OUT" "$1" "$2" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
for lane in doc["lanes"]:
    if lane["id"] == sys.argv[2]:
        value = lane[sys.argv[3]]
        print("" if value is None else value)
        break
PY
}

# lane_is <lane id> <state>
lane_is() { status_json && [ "$(lane_field "$1" state)" = "$2" ]; }

# The host.json of the project: its mode ("host" or "window") and pid.
HOST_FILE_GLOB="$PROJ/state/host.json"
host_json() {
  python3 -I - "$HOST_FILE_GLOB" "$1" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
print(doc.get(sys.argv[2], ""))
PY
}
host_mode_is() { [ -f "$HOST_FILE_GLOB" ] && [ "$(host_json mode)" = "$1" ]; }


# --- The Work view and the state store (see scenarios 03 and 47) -----------------------------------

VIEW_FILE="$LOGS_DIR/work-view.txt"
STATE_DB="$PROJ/state/state.db"

dump_view() {
  rm -f "$VIEW_FILE"
  palette "work: dump view"
  assert_file "$VIEW_FILE"
}

breadcrumb() { head -n1 "$VIEW_FILE"; }

view_title() {
  awk -v goal="$1" 'NR > 1 && substr($0, 2, 1) != " " {
    t = substr($0, 2); if (t == goal || index(t, goal "  ") == 1) { print t; exit }
  }' "$VIEW_FILE"
}

go_top() {
  local i
  for i in 1 2 3 4 5; do
    dump_view
    [ "$(breadcrumb)" = Work ] && return 0
    palette "work: up"
  done
  fail "work: up did not reach the top level: $(breadcrumb)"
}

view_lists() { dump_view; [ -n "$(view_title "$1")" ]; }
wait_listed() {
  go_top
  poll 40 "$1 in the Work view" view_lists "$1" || fail "$1 is not in the Work view: $(cat "$VIEW_FILE")"
}

# The worktree a started task records in the state store, on stdout.
worktree_in_state() {
  python3 -I - "$STATE_DB" "$1" <<'PY'
import sqlite3, sys, tomllib
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
row = db.execute("select state_toml from tasks where id = ?", (sys.argv[2],)).fetchone()
path = tomllib.loads(row[0]).get("worktree", "") if row else ""
if not path:
    sys.exit(1)
print(path)
PY
}

# start_task <goal text> <task id>: `task: start`, then waits for the worktree in the state store.
start_task() {
  focus_center
  wkeys s
  choose "$1"
  poll 40 "the worktree of $1 in the state store" worktree_in_state "$2" ||
    fail "$1 was not started: no worktree in the state store"
  poll 20 "the worktree directory of $1" test -d "$(worktree_in_state "$2")"
}


# --- Working copies and landing (see scenario 47) --------------------------------------------------

copy_names() { ls -1 "$PROJ/working-copies" 2>/dev/null || true; }
copy_is_new() {
  local now
  now=$(copy_names)
  [ "$now" != "$1" ]
}

make_working_copy() {
  local before after
  before=$(copy_names)
  wkeys g e
  act keys Return
  poll 40 "a new working copy" copy_is_new "$before"
  after=$(copy_names)
  comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort) | head -n1
}

land_copy() {
  sleep 4
  wait_for_settled
  focus_center
  wkeys g l
  sleep 2
  wait_for_settled
  choose "$1"
  wait_for_settled
  act keys Return
  wait_for_settled
}

# land_table <name of the working copy's new lane table text file>: makes a working copy whose
# lane table is the file's text and lands it; waits for master to move.
land_table() {
  local before copy
  before=$(git -C "$REPO" rev-parse main)
  focus_center
  copy=$(make_working_copy)
  [ -n "$copy" ] || fail "no working copy was made"
  poll 20 "the lane table of $copy" test -f "$PROJ/working-copies/$copy/definitions/lanes/lanes.toml"
  chmod u+w "$PROJ/working-copies/$copy/definitions/lanes/lanes.toml"
  cp "$1" "$PROJ/working-copies/$copy/definitions/lanes/lanes.toml"
  land_copy "$copy"
  main_moved() { [ "$(git -C "$REPO" rev-parse main)" != "$before" ]; }
  poll 90 "master to move after landing $copy" main_moved || fail "the landing of $copy did not move master"
}

# --- The project as files ------------------------------------------------------------------------

new_id() { cat /proc/sys/kernel/random/uuid; }
iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# write_task <folder under definitions/tasks> <goal> <work kind> <areas toml list> <extra line>; the
# id goes to $LAST_ID.
write_task() {
  local dir="$SEED/definitions/tasks/$1"
  LAST_ID=$(new_id)
  mkdir -p "$dir"
  cat >"$dir/task.toml" <<TOML
schema = 1
id = "$LAST_ID"
goal = "$2"
kind = "pr-ship"
work_kind = "$3"
areas = $4
created = $(iso_now)
$5

[[todos]]
name = "implement"
TOML
}

# The lane table. Every code task has the area `editor`. Three pins send the last code tasks to
# build-2 and one keeps the second editor task off the design lane (whose `ahead` takes no area rule),
# so while build-1 holds the first editor task build-2 has nothing it may take.
lane_table() {
  printf '%s' "$LANE_TABLE"
  local id
  for id in "$CART_ID" "$SEARCH_ID" "$BILLING_ID"; do
    printf '\n[[pin]]\ntask = "%s"\nlane = "build-2"\n' "$id"
  done
  printf '\n[[pin]]\ntask = "%s"\nkind = "build"\n' "$TABS_ID"
}

LANE_TABLE='schema = 1
owner = "top"

[[kind]]
name = "design"
count = 1
ahead = { cap = 1 }
areas = false
rules = ["prepare"]

[[kind]]
name = "build"
count = 2
rules = ["build"]

[[kind]]
name = "docs"
count = 1
rules = ["docs"]

[rules.prepare]
workflow = "fake job"

[rules.build]
work_kind = "code"
state = ["ready", "in-progress"]
workflow = "fake job"

[rules.docs]
work_kind = "docs"
state = ["ready", "in-progress"]
workflow = "fake job"
'

build_seed() {
  rm -rf "$SEED"
  mkdir -p "$SEED/definitions" "$SCRATCH"
  git init -q -b main "$SEED"
  printf '* text=auto\n' >"$SEED/.gitattributes"
  : >"$SEED/definitions/plan.md"
  PROJECT_ID=$(new_id)
  printf 'schema = 1\nid = "%s"\ntitle = "Storefront"\nrepositories = ["storefront"]\n' "$PROJECT_ID" >"$SEED/definitions/project.toml"
  cat >"$SEED/definitions/task.toml" <<TOML
schema = 1
id = "$PROJECT_ID"
goal = "Storefront"
kind = "other"
created = $(iso_now)
TOML
  write_task add-editor-palette "Add editor palette" code '["editor"]' ""
  PALETTE_ID=$LAST_ID
  write_task add-editor-tabs "Add editor tabs" code '["editor"]' ""
  TABS_ID=$LAST_ID
  write_task build-cart-totals "Build cart totals" code '["editor"]' ""
  CART_ID=$LAST_ID
  write_task build-search-index "Build search index" code '["editor"]' ""
  SEARCH_ID=$LAST_ID
  write_task draft-billing-page "Draft billing page" code '["editor"]' ""
  BILLING_ID=$LAST_ID
  write_task write-docs-guide "Write docs guide" docs '["docs"]' ""
  DOCS_ID=$LAST_ID
  mkdir -p "$SEED/definitions/lanes" "$SEED/definitions/workflows/fake-job"
  lane_table >"$SEED/definitions/lanes/lanes.toml"
  cat >"$SEED/definitions/workflows/fake-job/workflow.toml" <<'TOML'
schema = 1
name = "fake job"
description = "A job that waits while its goal is in the hold file and fails when it is in the fail file"
start = "work"

[inputs]
goal = { type = "text", from = "task.goal" }

[nodes.work]
kind = "program"
purpose = "Hold, then pass or fail"
script = "job.py"
language = "python"
permissions = { commands = [], network = [], mcp = [] }
output = { result = { type = "choice", values = ["ok"] } }
routes = [{ to = "merge" }]

[nodes.merge]
kind = "program"
purpose = "Bring the run branch into the task branch"
use = "merge"
with = { style = "merge-commit" }
output = { result = { type = "choice", values = ["merged", "conflict"] } }
routes = [
  { when = { result = "merged" }, to = "done" },
  { when = { result = "conflict" }, to = "stuck" },
]

[ends.done]
kind = "success"

[ends.stuck]
kind = "failure"
reason = "merge conflict needs a hand"
TOML
  cat >"$SEED/definitions/workflows/fake-job/job.py" <<PY
import json
import os
import sys
import time

control = "$SCRATCH"


def listed(name):
    try:
        return open(control + "/" + name).read().splitlines()
    except OSError:
        return []


with open(os.environ["BONGOS_STATE"]) as state_file:
    goal = json.load(state_file)["inputs"].get("goal", "")
while goal in listed("hold"):
    time.sleep(0.2)
if goal in listed("fail"):
    sys.exit("fake job: told to fail " + goal)
with open(os.environ["BONGOS_RESULT"], "w") as out:
    json.dump({"result": "ok"}, out)
PY
  git -C "$SEED" add -A
  git -C "$SEED" "${GIT_ID[@]}" commit -qm "the storefront project"
  mkdir -p "$(dirname "$BARE")"
  git init -q --bare -b main "$BARE"
  git -C "$SEED" push -q "$BARE" main
}

# ---------------------------------------------------------------------------------------------
step "setup: the code repository, the project as a repository, the project added in a window"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-48.json"
printf '[user]\n\tname = bongos\n\temail = bongos@example.invalid\n' >"$HOME/.gitconfig"
git init -q -b main "$CODE"
printf '# storefront\n' >"$CODE/README.md"
git -C "$CODE" add README.md
git -C "$CODE" "${GIT_ID[@]}" commit -qm "first commit"
build_seed
# The smoke sandbox is itself a bubblewrap sandbox whose /tmp/h is a symlink, so a program node's own
# sandbox cannot see the run's files. A `bwrap` that always fails makes the engine run the node under
# the audit hook alone ("not sandboxed"), which is what these jobs need.
mkdir -p "$RUN/no-bwrap"
printf '#!/bin/sh\necho "bwrap is not available in this scenario" >&2\nexit 1\n' >"$RUN/no-bwrap/bwrap"
chmod +x "$RUN/no-bwrap/bwrap"
export PATH="$RUN/no-bwrap:$PATH"
cat >"$SETTINGS_FILE" <<'JSON'
{
  "base_keymap": "Emacs",
  "task_builder": { "enabled": false },
  "project_repository": { "enabled": true },
  "work": {
    "repositories": [
      { "name": "storefront", "path": "~/src/storefront", "deployments": true, "merge_is_ship": true, "branch_template": "{slug}" }
    ]
  },
  "lanes": { "reevaluate_interval": 2, "on_interruption": "ask" }
}
JSON
launch_drawn
focus_center
wkeys g a
choose "$BARE"
poll 90 "the clone in $PROJ" test -d "$PROJ/repository/.git"
poll 30 "the project list" test -s "$HOME/.local/state/bongos/project-repositories.toml"
wait_listed Storefront
# A lane job runs in the task's own tree, so every task is started first (`task: start`).
for pair in "editor palette:$PALETTE_ID" "editor tabs:$TABS_ID" "cart totals:$CART_ID" "search index:$SEARCH_ID" "billing page:$BILLING_ID" "docs guide:$DOCS_ID"; do
  start_task "${pair%%:*}" "${pair#*:}"
done
quit_app
say "smoke: project added from $BARE; six tasks started"

# names_of: a stream with the task ids replaced by their goals.
names_of() {
  sed -e "s/$PALETTE_ID/Add editor palette/g" -e "s/$TABS_ID/Add editor tabs/g" -e "s/$CART_ID/Build cart totals/g" \
    -e "s/$SEARCH_ID/Build search index/g" -e "s/$BILLING_ID/Draft billing page/g" -e "s/$DOCS_ID/Write docs guide/g"
}
show() { names_of <"$CLI_OUT"; names_of <"$CLI_ERR"; }

# ---------------------------------------------------------------------------------------------
step "Lanes 3: two tasks share the area editor; with both build lanes free one is taken; an exemption lets both run"
# `lanes dry-run` is served from the store while no process holds the project, so it runs now.
cli_ok lanes dry-run
show
cp "$CLI_OUT" "$RUN/dry-run-before.txt"
grep -q '^build-1: would start '"$PALETTE_ID" "$CLI_OUT" || fail "build-1 would not start the first editor task: $(show)"
grep -q '^build-2: would start '"$PALETTE_ID" "$CLI_OUT" && fail "build-2 starts the same task"
if grep -q '^build-2: would start '"$TABS_ID" "$CLI_OUT"; then
  fail "both editor tasks would start with no exemption: $(show)"
fi
grep -qE "$TABS_ID waits: .*editor" "$CLI_OUT" || fail "the dry run does not name the tag editor for the second editor task: $(show)"
say "smoke: dry run before the exemption: build-1 takes Add editor palette; build-2 has nothing; the tag editor is named"

# The exemption lands in master as a change to the lane table, from a window.
git -C "$REPO" show main:definitions/lanes/lanes.toml >"$RUN/lanes-with-exemption.toml"
printf '\n[[exempt]]\ntags = ["editor"]\n' >>"$RUN/lanes-with-exemption.toml"
launch_drawn
focus_center
wait_listed Storefront
land_table "$RUN/lanes-with-exemption.toml"
git -C "$REPO" show main:definitions/lanes/lanes.toml | grep -q '^\[\[exempt\]\]' || fail "master has no [[exempt]] entry after the landing"
quit_app
cli_ok lanes dry-run
show
grep -q '^build-1: would start '"$PALETTE_ID" "$CLI_OUT" || fail "build-1 does not take the first editor task after the exemption: $(show)"
grep -q '^build-2: would start '"$TABS_ID" "$CLI_OUT" || fail "build-2 does not take the second editor task after the exemption: $(show)"
say "smoke: the exemption landed; the next dry run picks Add editor palette (build-1) and Add editor tabs (build-2)"

# ---------------------------------------------------------------------------------------------
step "Lanes 1: lanes start from a shell with no display starts a host; status shows four lanes"
printf '%s\n' "Add editor palette" "Add editor tabs" "Build cart totals" "Build search index" "Draft billing page" "Write docs guide" >"$SCRATCH/hold"
: >"$SCRATCH/fail"
lanes_started() { cli lanes start; [ "$CLI_CODE" = 0 ]; }
# The first call starts the host; while the host is still loading the project the start may be
# refused, so it is asked again until it is taken.
poll 60 "lanes start to be taken" lanes_started || fail "lanes start: $(cat "$CLI_ERR") $(cat "$CLI_OUT")"
say "smoke: lanes start: $(cat "$CLI_OUT")"
host_mode_is host || fail "host.json does not say host: $(cat "$HOST_FILE_GLOB")"
HOST_PID=$(host_json pid)
cli_ok lanes status
show
[ "$(grep -cE '^(design|build|docs)-[0-9]+ ' "$CLI_OUT")" = 4 ] || fail "status does not list four lanes: $(show)"
cli_ok lanes status --json
python3 -I - "$CLI_OUT" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
assert doc["schema"] == "lanes-status 1", doc["schema"]
ids = [lane["id"] for lane in doc["lanes"]]
assert ids == ["build-1", "build-2", "design-1", "docs-1"], ids
assert doc["header"]["lanes"] == 4
PY
lanes_busy() { status_json && [ "$(lane_field build-1 state)" = running ] && [ "$(lane_field build-2 state)" = running ]; }
poll 60 "both build lanes to run" lanes_busy || fail "the build lanes do not run: $(cat "$CLI_OUT")"
[ "$(lane_field build-1 task)" = "$PALETTE_ID" ] || fail "build-1 does not work on Add editor palette: $(show)"
[ "$(lane_field build-2 task)" = "$TABS_ID" ] || fail "build-2 does not work on Add editor tabs: $(show)"
say "smoke: host pid $HOST_PID; four lanes (build-1, build-2, design-1, docs-1); status --json parses; build-1 and build-2 run the two editor tasks together"

# ---------------------------------------------------------------------------------------------
step "Lanes 2: a window attaches as a client and shows the lanes; closing it leaves the lanes running"
launch_drawn
host_mode_is host || fail "host.json no longer says host with a window open: $(cat "$HOST_FILE_GLOB")"
[ "$(host_json pid)" = "$HOST_PID" ] || fail "the host is not the writer any more: $(cat "$HOST_FILE_GLOB")"
lkeys L
sleep 2
wait_for_settled
shot lanes-view
frame_has_ui lanes-view
cli_ok host status
grep -q "^mode: host" "$CLI_OUT" || fail "host status: $(cat "$CLI_OUT")"
quit_app
status_json || fail "status after the window closed: $(cat "$CLI_ERR")"
[ "$(lane_field build-1 state)" = running ] && [ "$(lane_field build-2 state)" = running ] || fail "the lanes stopped when the window closed: $(show)"
host_mode_is host && [ "$(host_json pid)" = "$HOST_PID" ] || fail "the host went away with the window: $(cat "$HOST_FILE_GLOB")"
say "smoke: the window attached to host $HOST_PID as a client (host.json unchanged), the Lanes view was shown; after it closed both build lanes still run"

# ---------------------------------------------------------------------------------------------
step "Lanes 4: a failing job halts build-2; lanes: adopt to the free build-1 records the reason in history"
# The first editor task ends, the second fails: build-2 halts, build-1 frees and waits (the last code
# tasks are pinned to build-2).
printf '%s\n' "Add editor tabs" >"$SCRATCH/fail"
printf '%s\n' "Build cart totals" "Build search index" "Draft billing page" "Write docs guide" >"$SCRATCH/hold"
lanes_halted() { status_json && [ "$(lane_field build-2 state)" = halted ] && [ "$(lane_field build-1 state)" = idle ]; }
poll 90 "build-2 to halt and build-1 to free" lanes_halted || fail "build-2 did not halt with build-1 free: $(show)"
[ "$(lane_field build-2 task)" = "$TABS_ID" ] || fail "build-2 halted on another task: $(show)"
say "smoke: build-2 halted on Add editor tabs ($(lane_field build-2 reason)); build-1 is idle"
# The failure is fixed, then the task goes to build-1.
: >"$SCRATCH/fail"
launch_drawn
focus_center
lkeys o
choose "$TABS_ID"
choose build-1
choose "build-2 halted on a failing fake job"
adopted_in_history() {
  python3 -I - "$STATE_DB" "$TABS_ID" <<'PY'
import sqlite3, sys
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
for (entry,) in db.execute("select entry_toml from history where task_id = ?", (sys.argv[2],)):
    if "adopted by build-1 from build-2: build-2 halted on a failing fake job" in entry:
        sys.exit(0)
sys.exit(1)
PY
}
poll 60 "the adopt in the task's history" adopted_in_history || fail "no adopt entry with its reason in the history of Add editor tabs"
tabs_done() { status_json && [ "$(lane_field build-1 jobs_done)" -ge 2 ] && [ "$(lane_field build-1 state)" = idle ]; }
poll 90 "build-1 to finish the adopted task" tabs_done || fail "build-1 did not finish Add editor tabs: $(show)"
say "smoke: lanes: adopt moved Add editor tabs to build-1 with the reason in its history; build-1 ran it to the end and is idle again"

# ---------------------------------------------------------------------------------------------
step "Lanes 5: build-2 is asked to stop while it runs a job and stops after the job"
# Deviation: the count of build cannot be lowered from a script (the table lands only from the window
# that holds the writer, and a test profile source is code), so the lane is stopped by its own
# command, which ends a lane at its job boundary the same way.
quit_app
status_json
[ "$(lane_field build-2 state)" = running ] || fail "build-2 is not running a job: $(show)"
cli_ok lanes stop build-2
show
printf '%s\n' "Build search index" "Draft billing page" "Write docs guide" >"$SCRATCH/hold"
build_two_stopped() { status_json && [ "$(lane_field build-2 state)" = stopped ]; }
poll 90 "build-2 to stop after its job" build_two_stopped || fail "build-2 did not stop after its job: $(show)"
[ "$(lane_field build-2 jobs_done)" -ge 1 ] || fail "build-2 stopped before its job ended: $(show)"
[ "$(lane_field build-1 state)" = idle ] || fail "build-1 took work that is pinned to build-2: $(show)"
say "smoke: build-2 was asked to stop during Build cart totals, ended the job and stopped; the pinned tasks were not taken"

# ---------------------------------------------------------------------------------------------
step "Lanes 6: the host is killed; lanes start again; the interrupted run waits on you; no run id twice"
status_json
[ "$(lane_field design-1 state)" = running ] || fail "design-1 does not run a job before the kill: $(show)"
INTERRUPTED_RUN=$(lane_field design-1 run)
[ -n "$INTERRUPTED_RUN" ] || fail "design-1 has no run before the kill: $(show)"
# Only the host this scenario started is killed.
[ "$(host_json pid)" = "$HOST_PID" ] || fail "host.json does not name the host this scenario started: $(cat "$HOST_FILE_GLOB")"
kill -9 "$HOST_PID"
host_gone() { ! kill -0 "$HOST_PID" 2>/dev/null; }
host_gone_2() { host_gone; }
poll 30 "the killed host to be gone" host_gone
poll 60 "lanes start to be taken after the kill" lanes_started || fail "lanes start after the kill: $(cat "$CLI_ERR") $(cat "$CLI_OUT")"
HOST_PID=$(host_json pid)
interrupted_waits() { status_json && [ "$(lane_field design-1 run_lifecycle)" = waitingonyou ]; }
poll 60 "the interrupted run to wait on you" interrupted_waits || fail "the interrupted run does not wait on you: $(show)"
[ "$(lane_field design-1 run)" = "$INTERRUPTED_RUN" ] || fail "design-1 runs another run after the restart: $(show)"
[ "$(lane_field design-1 state)" = running ] || fail "design-1 lost its job: $(show)"
# No run id appears twice in any lane's history: every run record names its lane.
python3 -I - "$PROJ/files" "$INTERRUPTED_RUN" <<'PY'
import collections, pathlib, sys, tomllib
by_lane = collections.defaultdict(list)
for record in pathlib.Path(sys.argv[1]).glob("*/runs/*/run.toml"):
    run = tomllib.loads(record.read_text())
    lane = run.get("lane", {}).get("id")
    if lane:
        by_lane[lane].append(run["id"])
assert by_lane, "no lane run records"
for lane, ids in by_lane.items():
    assert len(ids) == len(set(ids)), f"{lane} has a run id twice: {ids}"
assert sys.argv[2] in by_lane["design-1"], by_lane
assert by_lane["design-1"].count(sys.argv[2]) == 1
PY
say "smoke: host killed (kill -9); lanes start made host $HOST_PID; run ${INTERRUPTED_RUN%%-*} waits on you; every lane's run ids are distinct"

# ---------------------------------------------------------------------------------------------
step "Lanes 7: the host stops; a window is the writer in window mode; bongos run reaches it; lanes start hands over"
# Deviation: `bongos host stop` is refused by this build's host (it does not take Stop), so the host
# this scenario started is ended with its own pid.
cli host stop
say "smoke: bongos host stop: exit $CLI_CODE: $(cat "$CLI_ERR")"
kill "$HOST_PID"
poll 60 "the host to exit" host_gone_2
# A window opened on the project is now the writer, in window mode.
printf '%s\n' "Build cart totals" "Build search index" "Write docs guide" >"$SCRATCH/hold"
launch_drawn
focus_center
poll 30 "the window to take the writer" host_mode_is window || fail "host.json does not say window: $(cat "$HOST_FILE_GLOB")"
[ "$(host_json pid)" = "$APP_PID" ] || fail "the writer is not the window ($APP_PID): $(cat "$HOST_FILE_GLOB")"
engine_hosts() { pgrep -f -- '--engine-host' || true; }
[ -z "$(engine_hosts)" ] || fail "an engine host runs while the window is the writer: $(engine_hosts)"
# `bongos run <workflow> --task <id>` reaches the window; no host starts.
cli_ok run "fake job" --task "$BILLING_ID"
grep -q '^Started the run ' "$CLI_OUT" || fail "bongos run said: $(cat "$CLI_OUT")"
host_mode_is window && [ "$(host_json pid)" = "$APP_PID" ] || fail "the writer moved after bongos run: $(cat "$HOST_FILE_GLOB")"
[ -z "$(engine_hosts)" ] || fail "bongos run started an engine host: $(engine_hosts)"
say "smoke: the window ($APP_PID) is the writer in window mode; $(cat "$CLI_OUT"); no engine host started"
# `lanes: start` in the window asks for a confirmation, then hands the writer over to a new host.
lkeys s
wait_for_settled
shot lanes-start-confirm
# The summary buffer takes the keyboard focus from the confirmation list: a click on the list's input
# line gives it back.
click_confirm() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH / 2)) $((HEIGHT - 12)) click 1
  wait_for_settled
}
click_confirm
keys Return
host_took_over() { host_mode_is host && [ "$(host_json pid)" != "$APP_PID" ]; }
poll 90 "the writer to move to an engine host" host_took_over || fail "lanes: start did not hand the writer over: $(cat "$HOST_FILE_GLOB")"
HOST_PID=$(host_json pid)
kill -0 "$APP_PID" || fail "the window went away at the handover"
started_dispatcher() { status_json && grep -q 'dispatcher running' "$CLI_OUT" || grep -q '"dispatcher": "running"' "$CLI_OUT"; }
poll 60 "the new host to run the dispatcher" started_dispatcher || fail "the new host does not run lanes: $(cat "$CLI_OUT")"
shot lanes-after-handover
say "smoke: lanes: start handed the writer to host $HOST_PID; the window ($APP_PID) is now a client"
quit_app
cli lanes stop
kill "$HOST_PID"
poll 60 "the last host to exit" host_gone_2
cp "$RUN/config-backup/settings-48.json" "$CONFIG_DIR/settings.json"
