# Scenario for feature 47-task-checks. Sourced by script/bongos/smoke after the base flow (the app
# is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY,
# the proxy and the helpers of lib.sh. Debug builds only: it reads the Work view through
# `work: dump view` (<logs>/work-view.txt) and a task's details, Checks section included, through
# `task: dump details` (<logs>/task-details.txt); the rest is read from disk (the state store, the
# check output files, the files the fake check scripts write).
#
# Setup: `"base_keymap": "Emacs"` (the C-c w keys exist only there), one code repository
# $HOME/src/storefront in `work.repositories`, and a new-format project "storefront" that is built
# as a git repository (definitions, fake shell check scripts, a check set) and added with
# `project: add existing` from a local bare remote, so master is the verified commit with no landing.
# The check scripts are plain sh; no network is used. Tasks are started with `task: start`, which
# gives each a worktree in the code repository, and the checks run in those trees.
#   1. Group "Checkout release" with two steps, `use`ing the set `standard` (`format` and a scoped
#      `tests`) and a closing `tests-full` (scope "all", eight parts). `C-c w i` on step 1: the dump
#      shows the goal, the todos and a Checks section with two inherited rows "not run" under the
#      header "not done: 2 checks missing".
#   2. `C-c w x x` on step 1: the scope script answers 2 items, the rows read `pass` with
#      `scope 2 items`, the header says all required checks pass; `C-c w c` makes the step Done and
#      a done record is in the state store. On step 2, `C-c w x x` passes all three checks; a file
#      edited in step 2's tree afterwards makes the pass stale, so `C-c w c` on step 2 is refused
#      (the step is not Done and has no done record).
#   3. Step 2 is the last open child: its section lists `tests-full` as "closing check of ...".
#      `C-c w x x` starts it; once four of the eight parts ran, `C-c w x k` cancels; `C-c w x x` again
#      shows "running part 5 of 8, 4 kept" (parts 1 to 4 ran once), then passes; `C-c w c` makes
#      step 2 Done and the group Done.
#   4. A task whose `tests` fails one item and is `or_accepted` by `flake-verdict`: the verdict
#      reads the definitions file `checks/registry/flaky-tests.md` from the pin, not the same-named
#      file of the task's tree; the row reads "accepted by flake-verdict".
#   5. A check that exits 3 (declared an environment code) is run again twice (the interval is 0
#      minutes in the scenario settings: the setting is whole minutes, 0 means no pause) and ends
#      `no verdict: environment (exit 3)`; the script ran three times.
#   6. `project: land` of a working copy that adds a file the landing check `guard` rejects, and
#      rewrites `checks/guard` to pass: the landing is refused by `guard` with the script's own
#      output in the stored result and log, and master does not move.
# Deviations from the brief, each forced by the harness or by what the app does today:
# - The Checks section does not turn a row `stale` when a file of the tree is edited (the state it
#   compares with is taken when a check ends, and the tree is not watched), so no `stale` text is
#   asserted; the effect of the edit is read where it is decided, at `task: complete`.
# - Toasts have no log and no dump, so the refusal of `C-c w c` on step 2 is read from its effect
#   (not Done, no done record), and the landing view (`project: check`) is read from the stored
#   check result and its output file.
# - Workflow runs are not connected to an agent runner, so the parts of 4, 5 and 6 that need a run
#   are out of reach: `rerun-alone` twice at tip and twice at base, the run Waiting on you and
#   no fix node running are not driven. Part 4 drives `or_accepted` and the pinned read; part 5 the
#   reruns and the verdict text.
# - The rerun interval is `task_checks.no_verdict_rerun_minutes`, whole minutes; 0 means no pause.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 47-task-checks skipped under BONGOS_STOCK=1: the stock run has no Work view"
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
VIEW_FILE="$LOGS_DIR/work-view.txt"
DETAILS_FILE="$LOGS_DIR/task-details.txt"
PROJECTS="$DATA_DIR/projects"
PROJ="$PROJECTS/storefront"
REPO="$PROJ/repository"
STATE_DB="$PROJ/state/state.db"
CODE="$HOME/src/storefront"
SEED="$RUN/seed-47"
BARE="$RUN/remote-47/storefront.git"
SCRATCH="$RUN/checks-47"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

# --- Input -----------------------------------------------------------------------------------

wkeys() { act keys ctrl+c w "$@"; }

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

focus_panel() {
  focus_center
  keys ctrl+c w w
  wait_for_settled
}

launch_drawn() {
  launch
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# --- The Work view (see scenario 03 for the format) ----------------------------------------------

dump_view() {
  rm -f "$VIEW_FILE"
  palette "work: dump view"
  assert_file "$VIEW_FILE"
  cp "$VIEW_FILE" "$RUN/work-view-47-$1.txt"
}

breadcrumb() { head -n1 "$VIEW_FILE"; }

view_title() {
  awk -v goal="$1" 'NR > 1 && substr($0, 2, 1) != " " {
    t = substr($0, 2); if (t == goal || index(t, goal "  ") == 1) { print t; exit }
  }' "$VIEW_FILE"
}

select_row() {
  local pos sel tgt
  dump_view select
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

go_top() {
  local i
  for i in 1 2 3 4 5; do
    dump_view top
    [ "$(breadcrumb)" = Work ] && return 0
    palette "work: up"
  done
  fail "work: up did not reach the top level: $(breadcrumb)"
}

view_lists() { dump_view listed; [ -n "$(view_title "$1")" ]; }
wait_listed() {
  go_top
  poll 40 "$1 in the Work view" view_lists "$1" || fail "$1 is not in the Work view: $(cat "$VIEW_FILE")"
}

# --- Task details --------------------------------------------------------------------------------

# Writes the details of the task whose details tab has the focus to $DETAILS_FILE.
dump_details() {
  local attempt
  for attempt in 1 2 3; do
    rm -f "$DETAILS_FILE"
    focus_center
    if palette "task: dump details" && poll 10 "$DETAILS_FILE" file_has "$DETAILS_FILE"; then
      cp "$DETAILS_FILE" "$RUN/details-47-$1.txt"
      return 0
    fi
    # The palette took the text badly (a keystroke lost): leave it and the picker it may have opened.
    keys Escape Escape Escape
    wait_for_settled
  done
  fail "task: dump details wrote nothing in three tries ($1)"
}

details_have() {
  dump_details "$2"
  grep -qE -- "$1" "$DETAILS_FILE"
}

# wait_details <regex> <name>: dumps again until the details match.
wait_details() {
  poll 90 "the details of $2 to match $1" details_have "$1" "$2" ||
    fail "the details never matched $1 ($2): $(cat "$DETAILS_FILE")"
}

# Opens the details of the task named in the picker by <text> (a part of its goal) in a fresh tab.
open_details() {
  local i
  focus_center
  # Every earlier details tab goes, or the focused one would be the task of the command.
  for i in 1 2 3 4 5 6; do keys ctrl+x 0; done
  wait_for_settled
  wkeys i
  choose "$1"
  dump_details "open-$(printf '%s' "$1" | tr -c 'a-z0-9\n' '-')"
}

# The row of check <name> in the last details dump.
row_of() { grep -E -- "^  $1 \| " "$DETAILS_FILE" | head -n1; }

# The line under the "Checks" title.
header_of() { awk '$0 == "Checks" { getline; sub(/^ +/, ""); print; exit }' "$DETAILS_FILE"; }

# The "output: <path>" line under the row of check <name>.
output_of() {
  awk -v name="$1" '
    $0 ~ "^  " name " \\| " { on = 1; next }
    on && /^  [a-z]/ { on = 0 }
    on && /^    output: / { sub(/^    output: /, ""); print; exit }' "$DETAILS_FILE"
}

# --- The state store -----------------------------------------------------------------------------

# kv_has <namespace> <key>
kv_has() {
  python3 -I - "$STATE_DB" "$1" "$2" <<'PY'
import sqlite3, sys
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
n = db.execute("select count(*) from kv where namespace = ? and key = ?", (sys.argv[2], sys.argv[3])).fetchone()[0]
sys.exit(0 if n else 1)
PY
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

# The text of every failed result of the working copy <name>'s change.
failed_checks() {
  python3 -I - "$STATE_DB" "work/$1" <<'PY'
import sqlite3, sys
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
for check, result in db.execute(
    "select r.check_name, r.result_toml from check_results r join changes c on c.change_id = r.change_id"
    " where c.branch = ? and r.result_toml like '%outcome = \"fail\"%'", (sys.argv[2],)):
    print(check + ": " + result)
PY
}
has_failed_checks() { [ -n "$(failed_checks "$1")" ]; }

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

# --- The project as files ------------------------------------------------------------------------

new_id() { cat /proc/sys/kernel/random/uuid; }
iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

PR_TODOS='[[todos]]
name = "implement"
role = "produces-branch"

[[todos]]
name = "review"

[[todos]]
name = "ship"
role = "ships"'

# write_task <folder under definitions/> <goal> <kind> <extra toml: keys, then tables>; the id goes to
# $LAST_ID. The extra text starts with the plain keys, the todos and the tables follow.
write_task() {
  local dir="$SEED/definitions/$1"
  LAST_ID=$(new_id)
  mkdir -p "$dir"
  {
    printf 'schema = 1\nid = "%s"\ngoal = "%s"\nkind = "%s"\ncreated = %s\n' "$LAST_ID" "$2" "$3" "$(iso_now)"
    if [ -n "${4:-}" ]; then printf '\n%s\n' "$4"; fi
  } >"$dir/task.toml"
}

# script <path under definitions/> <body>
script() {
  mkdir -p "$(dirname "$SEED/definitions/$1")"
  printf '#!/bin/sh\n%s\n' "$2" >"$SEED/definitions/$1"
  chmod +x "$SEED/definitions/$1"
}

build_seed() {
  rm -rf "$SEED"
  mkdir -p "$SEED/definitions" "$SCRATCH"
  git init -q -b main "$SEED"
  printf '* text=auto\n' >"$SEED/.gitattributes"
  : >"$SEED/definitions/plan.md"
  printf 'schema = 1\nid = "%s"\ntitle = "Storefront"\nrepositories = ["storefront"]\n\n' "$(new_id)" >"$SEED/definitions/project.toml"
  cat >>"$SEED/definitions/project.toml" <<'TOML'
[[landing.checks]]
name = "guard"
phase = "before-queue"
applies = "always"
run = { check = { name = "guard", run = "checks/guard", result_file = true } }
TOML
  write_task "" "Storefront" other
  # The group: it uses the set `standard` and carries a closing check.
  write_task tasks/checkout-release "Checkout release" other '[checks]
use = ["standard"]

[[checks.closing]]
name = "tests-full"
run = "checks/tests-full"
scope = "all"
parts = 8'
  GROUP_ID=$LAST_ID
  write_task tasks/checkout-release/tasks/cart-totals "Cart totals" pr-ship "$PR_TODOS"
  STEP1_ID=$LAST_ID
  write_task tasks/checkout-release/tasks/order-export "Order export" pr-ship "$PR_TODOS"
  STEP2_ID=$LAST_ID
  write_task tasks/flaky-export "Flaky export" pr-ship "$PR_TODOS

[checks]
inherit = \"none\"

[[checks.check]]
name = \"tests\"
run = \"checks/flaky-tests\"
result_file = true
or_accepted = \"flake-verdict\"

[[checks.check]]
name = \"flake-verdict\"
run = \"checks/flake-verdict\"
pinned = [\"checks/registry/flaky-tests.md\"]
required = false"
  FLAKY_ID=$LAST_ID
  write_task tasks/env-probe "Env probe" pr-ship "$PR_TODOS

[checks]
inherit = \"none\"

[[checks.check]]
name = \"probe\"
run = \"checks/env-probe\"
environment = [3]"
  PROBE_ID=$LAST_ID

  mkdir -p "$SEED/definitions/checks/sets" "$SEED/definitions/checks/registry"
  cat >"$SEED/definitions/checks/sets/standard.toml" <<'TOML'
[[check]]
name = "format"
run = "checks/format"

[[check]]
name = "tests"
run = "checks/run-tests"
scope = { script = "checks/scope" }
TOML
  printf 'checkout-spec\n' >"$SEED/definitions/checks/registry/flaky-tests.md"
  script checks/format 'echo "format: clean"'
  script checks/scope 'printf "{\"scope\":\"items\",\"items\":[\"cart\",\"totals\"],\"reason\":\"2 modules changed\"}" > "$BONGOS_SCOPE_RESULT"'
  script checks/run-tests 'echo "tests: $(wc -l < "$BONGOS_CHECK_ITEMS") scoped items"'
  script checks/tests-full "n=\$BONGOS_CHECK_PART
echo \"\$n\" >> $SCRATCH/parts.log
if [ \"\$n\" -ge 5 ]; then
  while [ -e $SCRATCH/hold ]; do sleep 0.2; done
fi
echo \"part \$n of \$BONGOS_CHECK_PARTS done\""
  script checks/flaky-tests 'echo "FAIL checkout-spec"
printf "{\"summary\":\"1 failing\",\"failing\":[\"checkout-spec\"]}" > "$BONGOS_CHECK_RESULT"
exit 1'
  script checks/flake-verdict 'echo "pinned registry: $(cat "$BONGOS_CHECK_PINNED/checks/registry/flaky-tests.md")"
echo "tree registry: $(cat registry/flaky-tests.md 2>/dev/null || echo none)"
grep -q "^checkout-spec$" "$BONGOS_CHECK_PINNED/checks/registry/flaky-tests.md"'
  script checks/env-probe "echo run >> $SCRATCH/env-probe.log
echo \"the toolchain is missing\"
exit 3"
  script checks/guard 'bad=$(find definitions -name "forbidden*" | head -n1)
if [ -n "$bad" ]; then
  echo "found $bad"
  printf "{\"summary\":\"forbidden file present\",\"failing\":[\"%s\"]}" "$bad" > "$BONGOS_CHECK_RESULT"
  exit 1
fi
printf "{\"summary\":\"no forbidden file\"}" > "$BONGOS_CHECK_RESULT"'
  git -C "$SEED" add -A
  git -C "$SEED" "${GIT_ID[@]}" commit -qm "the storefront project"
  mkdir -p "$(dirname "$BARE")"
  git init -q --bare -b main "$BARE"
  git -C "$SEED" push -q "$BARE" main
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

# ---------------------------------------------------------------------------------------------
step "setup: the code repository, the project as a repository, the app with checks on"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-47.json"
git init -q -b main "$CODE"
printf '# storefront\n' >"$CODE/README.md"
git -C "$CODE" add README.md
git -C "$CODE" "${GIT_ID[@]}" commit -qm "first commit"
build_seed
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
  "task_checks": {
    "no_verdict_rerun_minutes": 0,
    "tree_key_debounce_ms": 200,
    "kill_grace_seconds": 1,
    "live_write_ms": 100
  }
}
JSON
launch_drawn
focus_center
wkeys g a
choose "$BARE"
poll 90 "the clone in $PROJ" test -d "$PROJ/repository/.git"
wait_listed Storefront
start_task "cart totals" "$STEP1_ID"
start_task "order export" "$STEP2_ID"
start_task "flaky export" "$FLAKY_ID"
start_task "env probe" "$PROBE_ID"
WT1=$(worktree_in_state "$STEP1_ID")
WT2=$(worktree_in_state "$STEP2_ID")
WT_FLAKY=$(worktree_in_state "$FLAKY_ID")
say "smoke: project added from $BARE; four tasks started, trees $WT1 $WT2 $WT_FLAKY"

# ---------------------------------------------------------------------------------------------
step "Task checks 1: step 1 shows its goal, its todos and two inherited checks not run"
open_details "cart totals"
shot checks-step1
grep -qx 'Goal: Cart totals' "$DETAILS_FILE" || fail "no goal line: $(cat "$DETAILS_FILE")"
grep -qE '^Todos \(3\)' "$DETAILS_FILE" || fail "no Todos (3) heading: $(cat "$DETAILS_FILE")"
grep -qx 'Checks' "$DETAILS_FILE" || fail "no Checks section: $(cat "$DETAILS_FILE")"
for check in format tests; do
  row_of "$check" | grep -qE 'inherited from .*\| required \| not run' ||
    fail "the row of $check is not an inherited required check not run: '$(row_of "$check")'"
done
[ "$(header_of)" = "not done: 2 checks missing" ] || fail "header is '$(header_of)': $(cat "$DETAILS_FILE")"
row_of tests-full >/dev/null && [ -n "$(row_of tests-full)" ] && fail "step 1 lists the closing check: $(cat "$DETAILS_FILE")"
say "smoke: step 1: goal, 3 todos, format and tests inherited and not run; header '$(header_of)'"

# ---------------------------------------------------------------------------------------------
step "Task checks 2: run checks passes with a 2-item scope, task: complete, a pass is no longer enough after an edit"
focus_center
wkeys x x
wait_details '^  tests \| .*\| pass \| scope 2 items' "step 1 after the run"
row_of format | grep -qE '\| pass \|' || fail "format did not pass: $(cat "$DETAILS_FILE")"
[ "$(header_of)" = "all required checks pass" ] || fail "header is '$(header_of)' after the pass: $(cat "$DETAILS_FILE")"
focus_center
wkeys c
choose "cart totals"
poll 60 "the done record of step 1" kv_has checks.done "$STEP1_ID" || fail "no done record for step 1"
wait_details '^  State: Done$' "step 1 done"
say "smoke: step 1: tests pass with scope 2 items, task: complete made it Done and a done record exists"

open_details "order export"
focus_center
wkeys x x
wait_details '^  all required checks pass$' "step 2 after the run"
row_of tests-full | grep -qE '\| pass \|' || fail "tests-full did not pass on step 2: $(cat "$DETAILS_FILE")"
printf 'an edit after the pass\n' >"$WT2/edit-after-pass.txt"
focus_center
wkeys c
choose "order export"
sleep 4
wait_for_settled
dump_details step2-refused
! grep -qx '  State: Done' "$DETAILS_FILE" || fail "step 2 became Done on a tree edited after its pass: $(cat "$DETAILS_FILE")"
! kv_has checks.done "$STEP2_ID" || fail "step 2 has a done record for a tree edited after its pass"
shot checks-step2-refused
say "smoke: step 2: every check passed, a file edited in its tree afterwards, task: complete left it open with no done record"

# ---------------------------------------------------------------------------------------------
step "Task checks 3: closing check on the last open child, cancelled after four of eight parts, resumed, group Done"
dump_details step2-closing
row_of tests-full | grep -qE 'closing check of' || fail "step 2 has no closing check row: $(cat "$DETAILS_FILE")"
rm -f "$SCRATCH/parts.log"
: >"$SCRATCH/hold"
focus_center
wkeys x x
parts_started_five() { grep -qx 5 "$SCRATCH/parts.log" 2>/dev/null; }
poll 90 "part 5 of tests-full to start" parts_started_five || fail "tests-full never reached part 5: $(cat "$SCRATCH/parts.log" 2>/dev/null)"
sleep 2
focus_center
wkeys x k
wait_details '^  not done: 1 check with no verdict' "step 2 after the cancel"
for n in 1 2 3 4; do
  [ "$(grep -cx "$n" "$SCRATCH/parts.log")" = 1 ] || fail "part $n ran $(grep -cx "$n" "$SCRATCH/parts.log") times: $(cat "$SCRATCH/parts.log")"
done
focus_center
wkeys x x
wait_details 'running part 5 of 8, 4 kept' "step 2 on the rerun"
shot checks-parts-kept
say "smoke: the rerun reports '$(row_of tests-full | grep -oE 'running part 5 of 8, 4 kept')'"
rm -f "$SCRATCH/hold"
wait_details '^  all required checks pass$' "step 2 after tests-full passed"
for n in 1 2 3 4; do
  [ "$(grep -cx "$n" "$SCRATCH/parts.log")" = 1 ] || fail "part $n ran again after the cancel: $(cat "$SCRATCH/parts.log")"
done
row_of tests-full | grep -q '8 parts' || fail "tests-full is not recorded as 8 parts: '$(row_of tests-full)'"
tree_of() { row_of "$1" | grep -oE 'tree [0-9a-f]+'; }
[ "$(tree_of tests-full)" = "$(tree_of format)" ] || fail "tests-full is on another tree than format: $(cat "$DETAILS_FILE")"
focus_center
wkeys c
choose "order export"
poll 60 "the done record of step 2" kv_has checks.done "$STEP2_ID" || fail "no done record for step 2"
wait_details '^  State: Done$' "step 2 done"
project_level() {
  go_top
  select_row Storefront
  act keys Return
  dump_view "$1"
  [ "$(breadcrumb)" = "Work > Storefront" ] || fail "drilling into Storefront shows '$(breadcrumb)'"
}
project_level group
select_row "Checkout release"
wkeys i
dump_details group
grep -qx '  State: Done' "$DETAILS_FILE" || fail "the group is not Done: $(cat "$DETAILS_FILE")"
say "smoke: step 2 Done after the parts resumed (parts 1-4 ran once); the group is Done"

# ---------------------------------------------------------------------------------------------
step "Task checks 4: a failing check accepted by flake-verdict, read from the pin"
mkdir -p "$WT_FLAKY/registry"
printf 'nothing is flaky here\n' >"$WT_FLAKY/registry/flaky-tests.md"
open_details "flaky export"
focus_center
wkeys x r
choose tests
wait_details '^  tests \| .*\| fail \(1 item\)' "the flaky task after tests"
grep -qE '^    failing: checkout-spec' "$DETAILS_FILE" || fail "the failing item is not listed: $(cat "$DETAILS_FILE")"
focus_center
wkeys x r
choose flake-verdict
wait_details '^  tests \| .*accepted by flake-verdict' "the flaky task after flake-verdict"
[ "$(header_of)" = "all required checks pass" ] || fail "header is '$(header_of)': $(cat "$DETAILS_FILE")"
OUT=$(output_of flake-verdict)/part-1.log
[ -f "$OUT" ] || fail "no output file for flake-verdict ('$OUT'): $(cat "$DETAILS_FILE")"
grep -q 'pinned registry: checkout-spec' "$OUT" || fail "flake-verdict did not read the pinned registry: $(cat "$OUT")"
grep -q 'tree registry: nothing is flaky here' "$OUT" || fail "the tree's own registry was not the one in the tree: $(cat "$OUT")"
shot checks-flake-verdict
say "smoke: tests fails 1 item and reads 'accepted by flake-verdict'; the pinned registry decided, not the tree's copy"

# ---------------------------------------------------------------------------------------------
step "Task checks 5: an environment exit is run again twice, then no verdict"
rm -f "$SCRATCH/env-probe.log"
open_details "env probe"
focus_center
wkeys x r
choose probe
wait_details '^  probe \| .*no verdict: environment \(exit 3\)' "the probe after its reruns"
[ "$(wc -l <"$SCRATCH/env-probe.log")" = 3 ] || fail "the probe ran $(wc -l <"$SCRATCH/env-probe.log") times, not 3"
grep -qE 'check with no verdict' <<<"$(header_of)" || fail "header is '$(header_of)': $(cat "$DETAILS_FILE")"
shot checks-no-verdict
say "smoke: probe: 'no verdict: environment (exit 3)' after 2 reruns (3 runs); header '$(header_of)'"

# ---------------------------------------------------------------------------------------------
step "Task checks 6: a landing whose check fails is refused, and the change's own script does not decide"
MAIN_BEFORE=$(git -C "$REPO" rev-parse main)
focus_center
for i in 1 2 3 4 5 6; do keys ctrl+x 0; done
wait_for_settled
BAD=$(make_working_copy)
[ -n "$BAD" ] || fail "no working copy was made"
BAD_DEFS="$PROJ/working-copies/$BAD/definitions"
poll 20 "$BAD_DEFS" test -d "$BAD_DEFS"
mkdir -p "$BAD_DEFS/documents"
printf 'a file the guard rejects\n' >"$BAD_DEFS/documents/forbidden-note.md"
chmod u+w "$BAD_DEFS/checks/guard"
printf '#!/bin/sh\nexit 0\n' >"$BAD_DEFS/checks/guard"
land_copy "$BAD"
sleep 3
[ "$(git -C "$REPO" rev-parse main)" = "$MAIN_BEFORE" ] || fail "the change landed: $(git -C "$REPO" log --oneline | head -n3)"
poll 60 "the failed checks of $BAD" has_failed_checks "$BAD" || fail "no check of $BAD failed"
failed_checks "$BAD" | grep -q '^guard:' || fail "the refusal is not by guard: $(failed_checks "$BAD")"
failed_checks "$BAD" | grep -q 'forbidden file present' || fail "the refusal lacks the script's summary: $(failed_checks "$BAD")"
LANDING_LOG=$(find "$PROJ/files/landing/checks" -type f -name '*.log' 2>/dev/null | head -n1)
[ -n "$LANDING_LOG" ] || LANDING_LOG=$(find "$PROJ/files/landing" -type f 2>/dev/null | head -n1)
[ -n "$LANDING_LOG" ] && grep -rq 'found definitions/documents/forbidden-note.md' "$PROJ/files/landing" ||
  fail "no stored output of the landing check names the forbidden file: $(find "$PROJ/files/landing" -type f 2>/dev/null)"
[ -d "$PROJ/working-copies/$BAD" ] || fail "the working copy of the refused landing is gone"
say "smoke: landing refused by guard ('forbidden file present'); master unchanged, output stored, the edited script did not matter"

quit_app
cp "$RUN/config-backup/settings-47.json" "$CONFIG_DIR/settings.json"
