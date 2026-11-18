# Scenario for feature 40-workflow-engine-hardening. Sourced by script/bongos/smoke after the base
# flow (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN,
# HOME, DISPLAY, the proxy and the helpers of lib.sh.
#
# Real runs, in a throwaway HOME, with the fake headless Claude of crates/bongos_workflow/test-fixtures
# behind a wrapper script (`chats.agents.claude.command`). The wrapper adds the scenario the fake
# plays (read from a file, so the scenario can switch it) and, for a successful node, commits a file
# in the run's worktree the way an agent would. The profile workflow "fake run" has one agent node
# and the shipped `merge` node (merge-commit style). The project is made with the Work commands, on
# a temporary git repository.
#   1. A leaf task, `C-c f r` from the Emacs keymap, the definition picked from the picker and the
#      review confirmed: the run record (events.jsonl of the task's run) ends succeeded, the task
#      branch holds the commit of the fake agent under a merge commit of the run, and the todo of
#      the task is done.
#   2. A second leaf task, run while the fake sleeps mid-node: the app is quit while the node
#      runs, no fake process is left behind, the run reads interrupted, the relaunch starts
#      nothing by itself, and `C-c f c` resumes the run, which ends succeeded with its work node
#      run once more.
#   3. The proxy got no request beyond the two kinds every launch makes.
# Reading a toast is not possible, so a failed start is read from its effect: no run record.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 40-workflow-engine-hardening skipped under BONGOS_STOCK=1: the stock run has no workflows"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/billing"
PROFILE_DIR="$WORK_ROOT/workflows"
FIXTURES="$BONGOS_ROOT/crates/bongos_workflow/test-fixtures"
BIN_DIR="$RUN/hardening-bin"
SCENARIO_FILE="$RUN/hardening-fake-scenario"
REPO=/tmp/hardening-src/billing
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

for tool in git python3 pgrep realpath; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-40.json"
scenario_exit() {
  local rc=$?
  set +e
  cp "$RUN/config-backup/settings-40.json" "$SETTINGS_FILE"
  pkill -f "fake-claude-headless" 2>/dev/null
  (exit "$rc")
  cleanup
}
trap scenario_exit EXIT

# --- Input -----------------------------------------------------------------------------------

wkeys() { act keys ctrl+c w "$@"; }

choose() {
  act type_text "$1"
  act keys Return
}

focus_editor() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH / 5)) $((HEIGHT / 3)) click 1
  wait_for_settled
}

launch_drawn() {
  launch
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# Quits with a node running: ctrl+q may ask, the first button is Quit and Return takes it.
quit_any() {
  local pid=$APP_PID
  focus_editor
  keys ctrl+q
  APP_PID=
  if ! poll 4 "the app to quit" pid_gone "$pid"; then
    keys Return
  fi
  if ! poll 30 "the app to quit after the prompt" pid_gone "$pid"; then
    kill -TERM "$pid"
    return 1
  fi
  wait "$pid" || true
}

# --- Reading files ----------------------------------------------------------------------------

toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 15 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

# The directory of the task whose goal is <$1>.
task_dir_of() {
  python3 -I -c 'import os, sys, tomllib
for base, dirs, files in os.walk(sys.argv[1]):
    dirs[:] = [d for d in dirs if d not in (".git", "chats", "runs", "proposals")]
    if "task.toml" not in files:
        continue
    try:
        with open(os.path.join(base, "task.toml"), "rb") as f:
            d = tomllib.load(f)
    except Exception:
        continue
    if d.get("goal") == sys.argv[2]:
        print(base)
        break' "$WORK_ROOT" "$1"
}

# The events.jsonl files of the runs of the task in <$1> (runs/<id>/attempts/<n>/events.jsonl).
events_of() {
  local file
  for file in "$1"/runs/*/attempts/*/events.jsonl; do
    [ -f "$file" ] && echo "$file"
  done
  return 0
}

events_has() {
  local file
  while IFS= read -r file; do
    grep -qE "$2" "$file" && return 0
  done < <(events_of "$1")
  return 1
}

# 0 when no fake agent process (or the sleep it started) is alive.
no_fake() { ! pgrep -f 'fake-claude-headless|sleep 600' >/dev/null; }

fake_sleeping() { pgrep -f 'sleep 600' >/dev/null; }

# 0 when a task with the goal <$1> exists; read again at every poll.
task_exists() { [ -n "$(task_dir_of "$1")" ]; }

# The starts of the `work` node in the run records of the task in <$1>.
work_starts() {
  local file n count=0
  while IFS= read -r file; do
    n=$(grep -c '"event":"NodeStarted".*"node":"work"' "$file" || true)
    count=$((count + n))
  done < <(events_of "$1")
  echo "$count"
}

# make_task <goal>: a task in the billing project with a worktree and its task branch started; sets
# TASK_DIR and WORKTREE.
make_task() {
  wkeys shift+n
  choose billing
  choose "$1"
  poll 10 "the task '$1'" task_exists "$1"
  TASK_DIR=$(task_dir_of "$1")
  wkeys s
  choose "${1%% *}"
  poll 10 "the worktree line in $TASK_DIR/task.toml" worktree_of "$TASK_DIR" >/dev/null
  WORKTREE=$(worktree_of "$TASK_DIR")
  poll 10 "the worktree $WORKTREE" test -d "$WORKTREE"
}

# Focuses the task whose goal has the word <$1> (C-c w f), then the editor.
focus_task() {
  wkeys f
  act type_text zzz
  act keys BackSpace BackSpace BackSpace
  choose "$1"
  focus_editor
}

# Runs the "fake run" workflow on the focused task: C-c f r, the definition, the review's first row.
start_run() {
  act keys ctrl+c f r
  choose "fake run"
  # The sandbox probe and the MCP check run before the summary and the review list open; the
  # screen may settle on the picker's closing before the list is there.
  sleep 4
  wait_for_settled
  # The summary buffer takes the keyboard focus from the review list: a click on the list's input
  # line gives it back.
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH / 2)) $((HEIGHT - 12)) click 1
  wait_for_settled
  act keys Return
}

# ---------------------------------------------------------------------------------------------
step "Hardening setup: a repository, the fake agent behind a wrapper, a profile workflow, Emacs keymap"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$BIN_DIR" /tmp/hardening-src
# The merge node commits as the user of the throwaway home.
printf '[user]\n\tname = bongos\n\temail = bongos@example.invalid\n' >"$HOME/.gitconfig"
echo success >"$SCENARIO_FILE"
cat >"$BIN_DIR/claude" <<SH
#!/bin/sh
# The probes of the app (--version, --help, mcp list) pass straight through.
case "\$1" in
  -p)
    scenario=\$(cat '$SCENARIO_FILE')
    if [ "\$scenario" = success ]; then
      printf 'written by the fake agent\n' >fake-agent.txt
      git add fake-agent.txt
      git -c user.name=fake -c user.email=fake@example.invalid commit -qm 'fake agent work'
    fi
    exec '$FIXTURES/fake-claude-headless' "\$@" --fake-scenario "\$scenario"
    ;;
esac
exec '$FIXTURES/fake-claude-headless' "\$@"
SH
chmod +x "$BIN_DIR/claude"
git init -q -b main "$REPO"
printf '# billing\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" "${GIT_ID[@]}" commit -qm "first commit"
cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "project_repository": { "enabled": false },
  "chats": {
    "enabled": true,
    "agents": {
      "claude": { "command": "$BIN_DIR/claude" }
    }
  },
  "work": {
    "repositories": [
      { "name": "billing", "path": "$REPO", "deployments": true, "merge_is_ship": true, "branch_template": "{slug}" }
    ]
  }
}
JSON
mkdir -p "$WORK_ROOT/oneoffs" "$PROFILE_DIR/fake-run"
cat >"$PROFILE_DIR/fake-run/workflow.toml" <<'TOML'
schema = 1
name = "fake run"
description = "One agent node, then the merge"
start = "work"

[inputs]
goal = { type = "text", from = "task.goal" }

[defaults]
agent = "claude"
model = "haiku"
permission_mode = "acceptEdits"
allowed_tools = ["Read"]
max_turns = 3
max_cost_usd = 0.5
max_minutes = 2

[nodes.work]
kind = "agent"
purpose = "Do the work"
prompt = "work.md"
output = { mode = { type = "choice", values = ["one-shot", "planned"] }, why = { type = "text" } }
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
printf '{{inputs.goal}}\n' >"$PROFILE_DIR/fake-run/work.md"
launch_drawn
focus_editor

wkeys p n
choose Billing
poll 15 "$PROJECT_DIR/project.toml" test -f "$PROJECT_DIR/project.toml"
python3 -I -c 'import re, sys
p = sys.argv[1]
t = open(p).read()
line = "repositories = [\"billing\"]"
if re.search(r"^repositories = .*$", t, re.M):
    t = re.sub(r"^repositories = .*$", line, t, count=1, flags=re.M)
else:
    t = re.sub(r"^(title = .*)$", lambda m: m.group(1) + "\n" + line, t, count=1, flags=re.M)
open(p, "w").write(t)' "$PROJECT_DIR/project.toml"
wait_toml "$PROJECT_DIR/project.toml" 'd["repositories"]' "['billing']"

# ---------------------------------------------------------------------------------------------
step "Run 1: C-c f r runs the agent node and the merge; the run succeeds, the branch has the merge, the todo is done"
make_task "Prorate invoices"
FIRST_DIR=$TASK_DIR
FIRST_TREE=$WORKTREE
FIRST_BRANCH=$(git -C "$FIRST_TREE" rev-parse --abbrev-ref HEAD)
focus_task Prorate
start_run
poll 120 "the first run to end succeeded" events_has "$FIRST_DIR" '"event":"Ended".*"state":"succeeded"'
shot hardening-run-1
frame_has_ui hardening-run-1
git -C "$REPO" "${GIT_ID[@]}" show "$FIRST_BRANCH:fake-agent.txt" >/dev/null 2>&1 ||
  fail "the task branch $FIRST_BRANCH has no fake-agent.txt: $(git -C "$REPO" log --oneline --all)"
git -C "$REPO" log --merges --format=%s "$FIRST_BRANCH" | grep -q '^Merge run ' ||
  fail "the task branch $FIRST_BRANCH has no merge of the run: $(git -C "$REPO" log --oneline --graph "$FIRST_BRANCH")"
wait_toml "$FIRST_DIR/task.toml" '[t["state"] for t in d["todos"]]' "['done']"
no_fake || fail "a fake agent is still running after the first run: $(pgrep -af 'fake-claude-headless|sleep 600')"
say "smoke: run 1 succeeded; $FIRST_BRANCH holds the fake agent's commit under a merge of the run; the todo is done"

# ---------------------------------------------------------------------------------------------
step "Run 2: quit mid-node; no fake is left behind, the run reads interrupted and resumes to its end after the relaunch"
echo sleep >"$SCENARIO_FILE"
make_task "Refund customers"
SECOND_DIR=$TASK_DIR
focus_task Refund
start_run
poll 60 "the fake agent to be mid-node" fake_sleeping
poll 30 "the node start in the second run's record" events_has "$SECOND_DIR" '"event":"NodeStarted"'
shot hardening-run-2
frame_has_ui hardening-run-2
quit_any
poll 15 "no fake process after the quit" no_fake ||
  fail "a fake agent outlived the app: $(pgrep -af 'fake-claude-headless|sleep 600')"
poll 15 "the quit to record the second run interrupted" events_has "$SECOND_DIR" '"event":"Interrupted"'
STARTS_BEFORE=$(work_starts "$SECOND_DIR")
launch_drawn
! events_has "$SECOND_DIR" '"event":"Ended"' || fail "the interrupted run ended: $(cat $(events_of "$SECOND_DIR"))"
no_fake || fail "a fake agent runs after the relaunch: $(pgrep -af 'fake-claude-headless|sleep 600')"
[ "$(work_starts "$SECOND_DIR")" -eq "$STARTS_BEFORE" ] ||
  fail "the work node restarted by itself after the relaunch: $(cat $(events_of "$SECOND_DIR"))"
shot hardening-run-2-relaunched
frame_has_ui hardening-run-2-relaunched
# The relaunch loaded the run interrupted and started nothing by itself: `C-c f c` resumes it, the
# work node runs exactly once more, and the run ends succeeded with the merge. The fake still
# plays `sleep` until here, so a node the app had restarted on its own would never end and the
# resume would be refused: the run would not end.
echo success >"$SCENARIO_FILE"
focus_task Refund
act keys ctrl+c f c
poll 120 "the resumed run to end succeeded" events_has "$SECOND_DIR" '"event":"Ended".*"state":"succeeded"'
[ "$(work_starts "$SECOND_DIR")" -eq $((STARTS_BEFORE + 1)) ] ||
  fail "the work node started $(work_starts "$SECOND_DIR") times, not $((STARTS_BEFORE + 1)): $(cat $(events_of "$SECOND_DIR"))"
no_fake || fail "a fake agent is still running after the resumed run: $(pgrep -af 'fake-claude-headless|sleep 600')"
say "smoke: the run quit mid-node came back interrupted, started nothing by itself and finished on resume"

# ---------------------------------------------------------------------------------------------
step "Privacy: no request beyond the two kinds every launch makes"
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
strays=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
[ -z "$strays" ] || fail "the proxy got other requests during the scenario: $strays"
quit_any

trap - EXIT
cp "$RUN/config-backup/settings-40.json" "$SETTINGS_FILE"
trap cleanup EXIT
