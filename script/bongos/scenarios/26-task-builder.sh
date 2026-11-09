# Scenario for feature 26-task-builder. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh. Debug builds only: it reads the proposal view and preview through
# `proposal: dump view` and `proposal: dump preview` (<logs>/proposal-view.txt and
# <logs>/proposal-preview.txt) and the Work panel through `work: dump view`.
#
# Setup, as scenarios 04 and 25 do: a git repository /tmp/builder-src/billing with one commit,
# `"base_keymap": "Emacs"` (the C-c w keys exist only there), `work.repositories` naming it, and
# `chats.agents.claude.command` naming a wrapper script around the fake Claude of
# crates/bongos_chat/test-fixtures. `task_builder.enabled` is true by default, so settings.json holds
# nothing about the builder. The project and its tasks are made with the Work commands. The fake CLI
# cannot plan, so the scenario writes proposal files as an agent would, with `by chat <id>` taken
# from the chat records (the id the fake logs as BONGOS_CHAT_ID).
# State is checked through files (task.toml, proposals/, context.md, $FAKE_CLI_LOG, the fake's
# byte-for-byte input log) and the three debug dumps. What no file or dump shows (the proposal view,
# the confirm question over the preview) is saved as a screenshot for the orchestrator to read.
#   1. C-c w n "fix rounding in invoice totals" (a one-off): one todo `plan` with role plans. C-c w b:
#      the fake logs a new plan session, the chat record has type plan, its context.md holds the plan
#      section, and the fake gets the opening line. Proposal 1 keeps the task a leaf; C-c w A: the
#      preview lists the three todos to add; Apply: plan (done), implement, review, ship.
#   2. A task "implement proration for billing" under the project, C-c w b, the D3 example as proposal
#      1. C-c w v: the view dump shows the five children only (proposal-view.png). C-c w V: `api` is
#      edited to `after integration` in a buffer and saved; the view dump reflects it. C-c w A shows
#      the preview over the question (proposal-confirm.png); Cancel writes nothing; again, Apply:
#      five task directories, the group has no todos, its history says plan done, proposals/1 is
#      listed as an artifact.
#   3. Proposal 2 closes a cycle and proposal 3 has a cross-level edge: C-c w A refuses both and the
#      view dump names each edge (proposal-problems.png); C-c w X rejects both, so the project row no
#      longer shows "needs you".
#   4. C-c w s on `schema` (a worktree), `integration` done, a review chat on `engine`; proposal 1
#      edited (as Kyle would) to drop `engine` and rename a todo of `api`: the preview leaves done and
#      live tasks untouched and says `engine` is kept, not removed, because it has a running chat;
#      Apply; the chat is still running and `engine` is still open.
#   5. The plan chat of the billing group, as the agent: proposal 4 adds a "rounding helper" child; it
#      applies within five seconds and the group's history says `agent applied proposal 4`. As the
#      agent in `engine`: proposal 5 adds a sibling; it waits (the project row reads "needs you"); C-c w
#      X rejects it and the file stays.
#   6. `api` gets a worktree (see the deviations) and proposal 6 splits it without `keeps work`: the
#      keeper question names the branch, the preview names the keeper, the branch and the worktree;
#      choosing `api-core` and Apply: the keeper's task.toml has the worktree and branch.
#   7. C-c w B on the shown chat (the review chat of `engine`): proposal 1 of `engine` is written with
#      the chat's id and pre-filled with the printed subtree, and the fake gets the "Edit proposal"
#      line; the untouched file does not make the project row "need you".
#   8. A proposal written while the app is quit: after the relaunch it is classified (it waits), never
#      applied, because proposals present at the first scan are not applied on their own; the Work
#      panel's top level is saved as a screenshot (work-panel-final.png).
#   9. No request through the proxy beyond the two kinds every launch makes (as in scenarios 03 and 25).
# Deviations from the EDD's scenario, each forced by the code: the D3 example creates 13 todos and 7
# edges (the EDD's "14 todos, 6 edges" is a miscount, see diff_tests.rs), the stack engine -> schema
# makes `schema` unsplittable (check_split), so step 6 splits `api`, and the group's row shows only a
# "needs you" count (the reason text is on leaf rows).
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 26-task-builder skipped under BONGOS_STOCK=1: the stock run has no task builder"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
VIEW_FILE="$LOGS_DIR/work-view.txt"
PROPOSAL_VIEW="$LOGS_DIR/proposal-view.txt"
PROPOSAL_PREVIEW="$LOGS_DIR/proposal-preview.txt"
REPO=/tmp/builder-src/billing
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/billing"
FIXTURES="$BONGOS_ROOT/crates/bongos_chat/test-fixtures"
CHAT_BIN="$RUN/chat-bin"
FAKE_CLAUDE_INPUT="$RUN/fake-claude-input.txt"
export FAKE_CLI_LOG="$RUN/fake-cli.log"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

FIX_GOAL="fix rounding in invoice totals"
ROOT_GOAL="implement proration for billing"

for tool in git python3 realpath pgrep sha256sum; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# --- Input -----------------------------------------------------------------------------------

# C-c w <keys>: the Work keys. Each opens a picker, a prompt or a view, so the screen changes.
wkeys() { act keys ctrl+c w "$@"; }

# C-c w <keys> for a command that may change no screen for certain: send, then wait until it is quiet.
wquiet() {
  keys ctrl+c w "$@"
  wait_for_settled
}

# C-c c <keys> for a command that opens a picker.
cpick() { act keys ctrl+c c "$@"; }

# Types into the open picker or prompt and confirms. A picker keeps the first row that contains every
# word typed.
choose() {
  act type_text "$1"
  act keys Return
}

# Runs a command by its palette name, for the commands with no key.
palette() {
  act keys ctrl+shift+p
  choose "$1"
}

# Clicks the editor area left of the Chats dock, so the workspace has the keyboard focus (a chat
# terminal sends C-c w keys to the CLI).
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

# Quits with chats running: ctrl+q asks, the first button is Quit and Return takes it.
quit_with_prompt() {
  local pid=$APP_PID
  focus_editor
  keys ctrl+q
  wait_for_settled
  keys Return
  APP_PID=
  if ! poll 30 "the app to quit after the prompt" pid_gone "$pid"; then
    kill -TERM "$pid"
    return 1
  fi
  wait "$pid"
}

# Quits with or without a prompt (a restored chat that is not running asks nothing).
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
  wait "$pid"
}

# Focuses the task whose goal has the words <$1> (C-c w f), then the editor. The picker opens on the
# focused task and keeps it selected while it still matches what is typed, and the rows of a task's
# children match whatever its own row matches. So a word that matches nothing is typed and erased
# first: with no match left, the selection starts over at the first row that matches the words.
focus_task() {
  wkeys f
  act type_text zzz
  act keys BackSpace BackSpace BackSpace
  choose "$1"
  focus_editor
}

# --- Reading files ----------------------------------------------------------------------------

toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 15 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

# The directory of the task whose goal is <$1>, anywhere under the Work root; empty when there is none.
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

have_task() { [ -n "$(task_dir_of "$1")" ]; }

# The record in <task dir>/chats of the first chat whose <field> is <value>.
record_with() {
  python3 -I -c 'import glob, sys, tomllib
for path in sorted(glob.glob(sys.argv[1] + "/chats/*.toml")):
    if tomllib.load(open(path, "rb")).get(sys.argv[2]) == sys.argv[3]:
        print(path)
        break' "$1" "$2" "$3"
}

have_record() { [ -n "$(record_with "$1" "$2" "$3")" ]; }

log_has() { [ -f "$FAKE_CLI_LOG" ] && grep -qxF -- "$1" "$FAKE_CLI_LOG"; }

log_has_text() { [ -f "$FAKE_CLI_LOG" ] && grep -qF -- "$1" "$FAKE_CLI_LOG"; }

fake_claude_up() { pgrep -f "test-fixtures/fake-claude" >/dev/null; }

# 0 when the bytes the fake claude got hold the text <$1>.
input_has_text() {
  [ -f "$FAKE_CLAUDE_INPUT" ] || return 1
  python3 -I -c 'import sys
data = bytes(int(token, 8) for token in open(sys.argv[1]).read().split())
sys.exit(0 if sys.argv[2].encode() in data else 1)' "$FAKE_CLAUDE_INPUT" "$1"
}

# The line of the dump <file> that follows the first line whose text, after its marker character, is <$2>.
line_after() {
  awk -v want="$2" 'hit { print; exit } substr($0, 2) == want { hit = 1 }' "$1"
}

# --- Writing proposals as an agent (or Kyle) would -----------------------------------------------

# "task <slug>: <goal> | id <id><extra>" and the edges of the task in <task dir> $1, indented by $2
# spaces: the line the printer of the builder writes for an existing task.
tline() {
  python3 -I -c 'import os, sys, tomllib
d = tomllib.load(open(os.path.join(sys.argv[1], "task.toml"), "rb"))
pad = " " * int(sys.argv[2])
print(pad + "task " + os.path.basename(sys.argv[1]) + ": " + d["goal"] + " | id " + d["id"] + sys.argv[3])
for edge in d.get("after", []):
    line = pad + "  after " + edge["task"]
    if edge["unblock"] != "merged":
        line += " | unblock " + edge["unblock"]
    print(line)
stacked = d.get("stacked_on")
if stacked:
    line = pad + "  stacked on " + stacked["task"]
    if stacked["unblock"] != "review":
        line += " | unblock " + stacked["unblock"]
    print(line)' "$1" "$2" "${3:-}"
}

# header <number> <task id> <by> <why>
header() { printf 'proposal %s\nfor %s\nby %s\nwhy %s\n\n' "$1" "$2" "$3" "$4"; }

# write_proposal <task dir> <number>, the text on stdin: written beside the file and moved in, so the
# watcher sees the whole file at once.
write_proposal() {
  local dir="$1/proposals"
  mkdir -p "$dir"
  cat >"$dir/.$2.tmp"
  mv "$dir/.$2.tmp" "$dir/$2.proposal.txt"
}

# --- Reading the Work view, the proposal view and the preview ----------------------------------------

go_top() {
  local i
  for i in 1 2 3 4 5; do
    rm -f "$VIEW_FILE"
    palette "work: dump view"
    assert_file "$VIEW_FILE"
    [ "$(head -n1 "$VIEW_FILE")" = Work ] && return 0
    palette "work: up"
  done
  fail "work: up did not reach the top level: $(cat "$VIEW_FILE")"
}

# The detail line of the row of <goal> in the last Work dump, without its marker and indent.
view_detail() {
  awk -v goal="$1" 'NR > 1 {
    t = substr($0, 2)
    if (hit) { sub(/^ +/, "", t); print t; exit }
    if (substr(t, 1, 1) != " " && (t == goal || index(t, goal "  ") == 1)) hit = 1
  }' "$VIEW_FILE"
}

# Whether the Billing row of the top level reads "needs you": yes or no. Looked at up to four times,
# two seconds apart, since the builder scans in the background.
expect_needs_you() {
  local want=$1 got i detail
  for i in 1 2 3 4; do
    go_top
    detail=$(view_detail Billing)
    if grep -q 'needs you' <<<"$detail"; then got=yes; else got=no; fi
    [ "$got" = "$want" ] && { cp "$VIEW_FILE" "$RUN/work-view-$2.txt"; return 0; }
    sleep 2
  done
  fail "the Billing row reads '$detail' (needs you: $got), expected needs you: $want: $(cat "$VIEW_FILE")"
}

dump_proposal_view() {
  rm -f "$PROPOSAL_VIEW"
  palette "proposal: dump view"
  assert_file "$PROPOSAL_VIEW"
  cp "$PROPOSAL_VIEW" "$RUN/proposal-view-$1.txt"
}

dump_preview() {
  rm -f "$PROPOSAL_PREVIEW"
  palette "proposal: dump preview"
  assert_file "$PROPOSAL_PREVIEW"
  cp "$PROPOSAL_PREVIEW" "$RUN/proposal-preview-$1.txt"
}

preview_has() {
  grep -qF -- "$1" "$PROPOSAL_PREVIEW" || fail "the preview lacks '$1': $(cat "$PROPOSAL_PREVIEW")"
}

view_has() {
  grep -qF -- "$1" "$PROPOSAL_VIEW" || fail "the proposal view lacks '$1': $(cat "$PROPOSAL_VIEW")"
}

# ---------------------------------------------------------------------------------------------
step "Builder setup: a repository, the fake claude, base_keymap Emacs, work settings"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$RUN/config-backup" "$CHAT_BIN"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-26.json"
: >"$FAKE_CLI_LOG"
cat >"$CHAT_BIN/claude" <<SH
#!/bin/sh
FAKE_CLI_LOG='$FAKE_CLI_LOG'
FAKE_CLI_INPUT='$FAKE_CLAUDE_INPUT'
export FAKE_CLI_LOG FAKE_CLI_INPUT
exec '$FIXTURES/fake-claude' "\$@"
SH
chmod +x "$CHAT_BIN/claude"
mkdir -p /tmp/builder-src
git init -q -b main "$REPO"
printf '# billing\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" "${GIT_ID[@]}" commit -qm "first commit"
cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "chats": {
    "enabled": true,
    "agents": {
      "claude": { "command": "$CHAT_BIN/claude" }
    }
  },
  "work": {
    "repositories": [
      { "name": "billing", "path": "$REPO", "deployments": true, "merge_is_ship": true, "branch_template": "{slug}" }
    ]
  }
}
JSON
mkdir -p "$WORK_ROOT/oneoffs"
launch_drawn
focus_editor
assert_file "$WORK_ROOT/task-builder/format.txt" 'proposal'

wkeys p n
choose Billing
assert_file "$PROJECT_DIR/task.toml"
poll 10 "git init in $PROJECT_DIR" test -d "$PROJECT_DIR/.git"
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
step "Builder 1: a new task has a plan todo; C-c w b starts a plan chat; a proposal keeps a leaf a leaf"
wkeys n
choose "$FIX_GOAL"
poll 10 "the task '$FIX_GOAL'" have_task "$FIX_GOAL"
FIX_DIR=$(task_dir_of "$FIX_GOAL")
FIX_SLUG=$(basename "$FIX_DIR")
FIX_ID=$(toml "$FIX_DIR/task.toml" 'd["id"]')
[ "$(toml "$FIX_DIR/task.toml" '[(t["name"], t.get("role")) for t in d["todos"]]')" = "[('plan', 'plans')]" ] ||
  fail "a new task does not hold exactly the plan todo: $(cat "$FIX_DIR/task.toml")"
focus_task "fix rounding"
wkeys b
choose claude
focus_editor
poll 15 "a plan chat record in $FIX_DIR/chats" have_record "$FIX_DIR" type plan
FIX_REC=$(record_with "$FIX_DIR" type plan)
FIX_CHAT=$(toml "$FIX_REC" 'd["id"]')
poll 5 "the fake claude to start within five seconds" log_has "claude start"
poll 5 "BONGOS_CHAT_ID in the log" log_has "claude env BONGOS_CHAT_ID=$FIX_CHAT"
poll 5 "BONGOS_TASK_ID in the log" log_has "claude env BONGOS_TASK_ID=$FIX_ID"
# The chat is told the real path of the directory ($HOME may be a symlink).
poll 30 "the opening line in the fake's input" log_has_text "Plan this task and write proposal 1 at $(realpath "$FIX_DIR")/proposals/1.proposal.txt."
CONTEXT="$DATA_DIR/chats/$FIX_CHAT/context.md"
assert_file "$CONTEXT" '^## Task builder$'
assert_file "$CONTEXT" 'You are planning this task'
assert_file "$CONTEXT" 'Splitting test'
say "smoke: one todo plan (role plans); the plan chat ($FIX_CHAT) started, was told the splitting test and got its opening line"

{
  header 1 "$FIX_ID" "chat $FIX_CHAT" "a leaf is enough"
  printf 'task %s: %s | id %s\n' "$FIX_SLUG" "$FIX_GOAL" "$FIX_ID"
  cat <<'EOF'
  result a PR fixing the rounding of invoice totals
  todo implement | role produces-branch
  todo review
  todo ship | role ships
EOF
} | write_proposal "$FIX_DIR" 1
wkeys shift+a
wait_for_settled
choose Apply
wait_toml "$FIX_DIR/task.toml" '[(t["name"], t["state"]) for t in d["todos"]]' \
  "[('plan', 'done'), ('implement', 'open'), ('review', 'open'), ('ship', 'open')]"
dump_preview fix
preview_has 'add todo "implement"'
preview_has 'add todo "review"'
preview_has 'add todo "ship"'
wait_toml "$FIX_DIR/task.toml" '[p["number"] for p in d["proposals"] if p["state"] == "applied"]' "[1]"
say "smoke: the preview listed the three todos; the todos read plan (done), implement, review, ship"

# ---------------------------------------------------------------------------------------------
step "Builder 2: the D3 example as a proposal: the view shows five children, an edit shows, Apply makes the tree"
wkeys shift+n
choose billing
choose "$ROOT_GOAL"
poll 10 "the task '$ROOT_GOAL'" have_task "$ROOT_GOAL"
ROOT_DIR=$(task_dir_of "$ROOT_GOAL")
ROOT_SLUG=$(basename "$ROOT_DIR")
ROOT_ID=$(toml "$ROOT_DIR/task.toml" 'd["id"]')
focus_task "implement proration"
wkeys b
choose claude
focus_editor
poll 15 "a plan chat record in $ROOT_DIR/chats" have_record "$ROOT_DIR" type plan
PLAN_CHAT=$(toml "$(record_with "$ROOT_DIR" type plan)" 'd["id"]')
poll 30 "the opening line for the billing task" log_has_text "Plan this task and write proposal 1 at $(realpath "$ROOT_DIR")/proposals/1.proposal.txt."
# A second C-c w b shows the chat it started and starts none.
wquiet b
focus_editor
[ "$(find "$ROOT_DIR/chats" -name '*.toml' | wc -l)" = 1 ] || fail "a second task: plan started another chat: $(ls "$ROOT_DIR/chats")"
[ "$(grep -c '^claude start$' "$FAKE_CLI_LOG")" = 2 ] || fail "a second task: plan started another fake claude: $(cat "$FAKE_CLI_LOG")"
{
  header 1 "$ROOT_ID" "chat $PLAN_CHAT" "billing needs proration; one PR per layer"
  printf 'task %s: %s | id %s\n' "$ROOT_SLUG" "$ROOT_GOAL" "$ROOT_ID"
  cat <<'EOF'
  task schema: add proration columns to plans
    result a PR adding proration columns
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task engine: proration engine
    stacked on schema
    result a PR with the proration engine
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task integration: use proration in invoices
    after engine
    result a PR wiring proration into invoice totals
  task api: proration endpoint
    after engine
    result a PR adding the endpoint
  task deploy: deploy proration | kind other
    after schema | unblock merged
    after engine
    after integration
    after api
    result proration live in production
    todo deploy
EOF
} | write_proposal "$ROOT_DIR" 1
PROPOSAL_ONE="$ROOT_DIR/proposals/1.proposal.txt"
wkeys v
wait_for_settled
dump_proposal_view five
view_has 'breadcrumb: '
view_has 'problems: none'
[ "$(grep -cE '^[ >][a-z][a-z-]*: .*\[new, ' "$PROPOSAL_VIEW")" = 5 ] || fail "the view does not show exactly the five children as new: $(cat "$PROPOSAL_VIEW")"
[ "$(line_after "$PROPOSAL_VIEW" 'api: proration endpoint  [new, pr-ship]' | sed 's/^ *//')" = "after engine" ] ||
  fail "the view does not show api after engine: $(cat "$PROPOSAL_VIEW")"
shot proposal-view
frame_has_ui proposal-view
# C-c w V: the file as a buffer; api's edge line is edited to `after integration` and saved.
API_EDGE_LINE=$(($(grep -n '^  task api:' "$PROPOSAL_ONE" | cut -d: -f1) + 1))
[ "$(sed -n "${API_EDGE_LINE}p" "$PROPOSAL_ONE")" = "    after engine" ] || fail "the edge line of api is not line $API_EDGE_LINE of $PROPOSAL_ONE"
wkeys shift+v
wait_for_settled
# The long first lines wrap, so the editor goes to the line by number.
palette "go to line: toggle"
choose "$API_EDGE_LINE"
keys ctrl+e alt+BackSpace
type_text integration
wait_for_settled
keys ctrl+x ctrl+s
wait_for_settled
poll 15 "api after integration saved in proposal 1" test "$(sed -n "${API_EDGE_LINE}p" "$PROPOSAL_ONE")" = "    after integration"
dump_proposal_view edited
[ "$(line_after "$PROPOSAL_VIEW" 'api: proration endpoint  [new, pr-ship]' | sed 's/^ *//')" = "after integration" ] ||
  fail "the view does not reflect the edit: $(cat "$PROPOSAL_VIEW")"
say "smoke: the view showed the five children ($RUN/proposal-view.png); the edit of api reached the file and the view"

# C-c w A over the preview: Cancel writes nothing.
wkeys shift+a
wait_for_settled
shot proposal-confirm
frame_has_ui proposal-confirm
choose Cancel
wait_for_settled
[ ! -e "$ROOT_DIR/tasks" ] || fail "Cancel wrote tasks: $(ls "$ROOT_DIR/tasks")"
[ "$(toml "$ROOT_DIR/task.toml" 'len(d.get("proposals", []))')" = 0 ] || fail "Cancel wrote a proposal record: $(cat "$ROOT_DIR/task.toml")"
dump_preview billing
preview_has 'create 5 tasks, 13 todos, 7 edges; nothing existing is touched'
focus_editor
wkeys shift+a
wait_for_settled
choose Apply
wait_toml "$ROOT_DIR/task.toml" '[p["number"] for p in d["proposals"] if p["state"] == "applied"]' "[1]"
for slug in api deploy engine integration schema; do
  assert_file "$ROOT_DIR/tasks/$slug/task.toml"
done
[ "$(ls "$ROOT_DIR/tasks" | sort | tr '\n' ' ')" = "api deploy engine integration schema " ] || fail "the group has other children: $(ls "$ROOT_DIR/tasks")"
[ "$(toml "$ROOT_DIR/task.toml" 'len(d.get("todos", []))')" = 0 ] || fail "the group kept its todos: $(cat "$ROOT_DIR/task.toml")"
toml "$ROOT_DIR/task.toml" 'any("plan" in (h.get("event", "") + " " + h.get("detail", "")) and "done" in (h.get("event", "") + " " + h.get("detail", "")) for h in d["history"])' | grep -qx True ||
  fail "the group's history does not say the plan is done: $(cat "$ROOT_DIR/task.toml")"
[ "$(toml "$ROOT_DIR/tasks/engine/task.toml" 'd["stacked_on"]["task"]')" = schema ] || fail "engine is not stacked on schema"
[ "$(toml "$ROOT_DIR/tasks/api/task.toml" '[e["task"] for e in d["after"]]')" = "['integration']" ] || fail "api is not after integration: $(cat "$ROOT_DIR/tasks/api/task.toml")"
[ "$(toml "$ROOT_DIR/tasks/deploy/task.toml" 'sorted(e["task"] for e in d["after"])')" = "['api', 'engine', 'integration', 'schema']" ] || fail "deploy has other edges: $(cat "$ROOT_DIR/tasks/deploy/task.toml")"
CONTEXT_FILE="$LOGS_DIR/work-session-context.json"
rm -f "$CONTEXT_FILE"
focus_editor
palette "work: dump session context"
assert_file "$CONTEXT_FILE"
grep -q '1\.proposal\.txt' "$CONTEXT_FILE" || fail "proposals/1.proposal.txt is not listed as an artifact: $(cat "$CONTEXT_FILE")"
say "smoke: five task directories, the group has no todos, its history says plan done, proposals/1.proposal.txt is an artifact"

# ---------------------------------------------------------------------------------------------
step "Builder 3: a cycle and a cross-level edge are refused and named; X rejects both"
{
  header 2 "$ROOT_ID" "chat $PLAN_CHAT" "integration waits for the api"
  tline "$ROOT_DIR" 0
  tline "$ROOT_DIR/tasks/integration" 2
  printf '    after api\n'
} | write_proposal "$ROOT_DIR" 2
{
  header 3 "$ROOT_ID" "chat $PLAN_CHAT" "api needs the schema directly"
  tline "$ROOT_DIR" 0
  tline "$ROOT_DIR/tasks/api" 2
  printf '    after billing/schema\n'
} | write_proposal "$ROOT_DIR" 3
focus_editor
# Both are pending: the picker lists the newest first.
wkeys shift+a
choose "proposal 3"
wait_for_settled
dump_proposal_view cross-level
view_has "api after billing/schema"
shot proposal-problems
frame_has_ui proposal-problems
focus_editor
wkeys shift+a
choose "proposal 2"
wait_for_settled
dump_proposal_view cycle
view_has "integration after api"
view_has "cycle"
[ "$(toml "$ROOT_DIR/task.toml" '[p["number"] for p in d["proposals"]]')" = "[1]" ] || fail "a refused apply wrote a record: $(cat "$ROOT_DIR/task.toml")"
[ "$(ls "$ROOT_DIR/tasks" | wc -l)" = 5 ] || fail "a refused apply changed the tree: $(ls "$ROOT_DIR/tasks")"
expect_needs_you yes problems
focus_editor
wkeys shift+x
choose "proposal 3"
wait_toml "$ROOT_DIR/task.toml" '[(p["number"], p["state"]) for p in d["proposals"] if p["number"] == 3]' "[(3, 'rejected')]"
focus_editor
wquiet shift+x
wait_toml "$ROOT_DIR/task.toml" '[(p["number"], p["state"]) for p in d["proposals"] if p["number"] == 2]' "[(2, 'rejected')]"
[ -f "$ROOT_DIR/proposals/2.proposal.txt" ] && [ -f "$ROOT_DIR/proposals/3.proposal.txt" ] || fail "a reject removed a file: $(ls "$ROOT_DIR/proposals")"
expect_needs_you no rejected
say "smoke: both proposals refused with their edges named ($RUN/proposal-problems.png); rejected, the files stay, the Billing row no longer needs you"

# ---------------------------------------------------------------------------------------------
step "Builder 4: a worktree, a done leaf and a running chat are left alone by an edited proposal 1"
focus_editor
wkeys s
choose "add proration columns"
SCHEMA_WT="$REPO/.bongos-worktrees/schema"
poll 10 "the worktree $SCHEMA_WT within ten seconds" test -d "$SCHEMA_WT"
wait_toml "$ROOT_DIR/tasks/schema/task.toml" 'd.get("branch")' schema
# Another leaf done, written as a file: integration.
python3 -I -c 'import sys
p = sys.argv[1]
t = open(p).read()
assert "status = \"open\"" in t, t
open(p, "w").write(t.replace("status = \"open\"", "status = \"done\"", 1))' "$ROOT_DIR/tasks/integration/task.toml"
wait_toml "$ROOT_DIR/tasks/integration/task.toml" 'd["status"]' done
# A review chat on engine, a type that needs no worktree.
focus_task "proration engine"
cpick n
choose claude
choose "review:"
focus_editor
poll 15 "a review chat record in $ROOT_DIR/tasks/engine/chats" have_record "$ROOT_DIR/tasks/engine" type review
ENGINE_CHAT=$(toml "$(record_with "$ROOT_DIR/tasks/engine" type review)" 'd["id"]')
ENGINE_CONTEXT="$DATA_DIR/chats/$ENGINE_CHAT/context.md"
assert_file "$ENGINE_CONTEXT" '^## Task builder$'
assert_file "$ENGINE_CONTEXT" 'task-builder/format\.txt'
if grep -q 'You are planning this task' "$ENGINE_CONTEXT"; then fail "a review chat got the plan section: $(cat "$ENGINE_CONTEXT")"; fi
INTEGRATION_HASH=$(sha256sum <"$ROOT_DIR/tasks/integration/task.toml")
SCHEMA_HASH=$(sha256sum <"$ROOT_DIR/tasks/schema/task.toml")
# Kyle edits proposal 1: drop engine, rename the review todo of api. The file lists every task with
# its id and edges, as the printer of the builder writes it.
{
  header 1 "$ROOT_ID" "kyle" "drop the engine, rename a todo"
  tline "$ROOT_DIR" 0
  tline "$ROOT_DIR/tasks/schema" 2
  tline "$ROOT_DIR/tasks/engine" 2 " | drop"
  tline "$ROOT_DIR/tasks/integration" 2
  tline "$ROOT_DIR/tasks/api" 2
  printf '    todo implement | role produces-branch\n    todo code review\n    todo ship | role ships\n'
  tline "$ROOT_DIR/tasks/deploy" 2
} | write_proposal "$ROOT_DIR" 1
# The review chat made engine the focused task; the proposal is the group's.
focus_task "implement proration"
wkeys shift+a
wait_for_settled
choose Apply
wait_toml "$ROOT_DIR/tasks/api/task.toml" '[t["name"] for t in d["todos"]]' "['implement', 'code review', 'ship']"
dump_preview kept
preview_has 'engine: kept, not removed'
preview_has 'has a running chat'
preview_has 'rename todo 2 "review" -> "code review"'
grep -A1 '^Untouched' "$PROPOSAL_PREVIEW" | grep -q 'integration (done)' || fail "the preview does not leave integration (done) untouched: $(cat "$PROPOSAL_PREVIEW")"
grep -A1 '^Untouched' "$PROPOSAL_PREVIEW" | grep -q 'schema' || fail "the preview does not leave schema untouched: $(cat "$PROPOSAL_PREVIEW")"
[ "$(sha256sum <"$ROOT_DIR/tasks/integration/task.toml")" = "$INTEGRATION_HASH" ] || fail "the done task's task.toml changed"
[ "$(sha256sum <"$ROOT_DIR/tasks/schema/task.toml")" = "$SCHEMA_HASH" ] || fail "the task with a worktree changed"
[ "$(toml "$ROOT_DIR/tasks/engine/task.toml" 'd["status"]')" = open ] || fail "engine was cancelled: $(cat "$ROOT_DIR/tasks/engine/task.toml")"
fake_claude_up || fail "no fake claude is running after the apply"
if grep -q '^claude exit' "$FAKE_CLI_LOG"; then fail "a chat exited: $(cat "$FAKE_CLI_LOG")"; fi
say "smoke: done and live tasks untouched, engine kept (has a running chat), the todo renamed; the chat still runs"

# ---------------------------------------------------------------------------------------------
step "Builder 5: the group's own chat applies an addition at once; a chat of a sibling waits"
# The plan chat of the billing group is the agent of the group (a chat on a group is allowed).
{
  header 4 "$ROOT_ID" "chat $PLAN_CHAT" "the engine and the api share the rounding"
  printf 'task %s: %s | id %s\n' "$ROOT_SLUG" "$ROOT_GOAL" "$ROOT_ID"
  cat <<'EOF'
  task rounding-helper: rounding helper
    result a PR adding a rounding helper
    todo implement | role produces-branch
    todo review
    todo ship | role ships
EOF
} | write_proposal "$ROOT_DIR" 4
poll 5 "proposal 4 applied within five seconds" test -f "$ROOT_DIR/tasks/rounding-helper/task.toml"
wait_toml "$ROOT_DIR/task.toml" '[(p["number"], p["state"], p["by"]) for p in d["proposals"] if p["number"] == 4]' "[(4, 'applied', 'agent')]"
toml "$ROOT_DIR/task.toml" 'any("agent applied proposal 4" in h.get("detail", "") for h in d["history"])' | grep -qx True ||
  fail "the group's history lacks 'agent applied proposal 4': $(cat "$ROOT_DIR/task.toml")"
shot proposal-agent-applied
frame_has_ui proposal-agent-applied
expect_needs_you no applied
# The review chat of engine is an agent of a sibling: its proposal waits.
{
  header 5 "$ROOT_ID" "chat $ENGINE_CHAT" "a usage report belongs beside the engine"
  printf 'task %s: %s | id %s\n' "$ROOT_SLUG" "$ROOT_GOAL" "$ROOT_ID"
  cat <<'EOF'
  task usage-report: usage report
    result a PR adding the usage report
    todo implement | role produces-branch
    todo review
    todo ship | role ships
EOF
} | write_proposal "$ROOT_DIR" 5
expect_needs_you yes waiting
sleep 3
[ ! -e "$ROOT_DIR/tasks/usage-report" ] || fail "a proposal of a sibling's chat applied on its own"
focus_editor
wquiet shift+x
wait_toml "$ROOT_DIR/task.toml" '[(p["number"], p["state"]) for p in d["proposals"] if p["number"] == 5]' "[(5, 'rejected')]"
[ -f "$ROOT_DIR/proposals/5.proposal.txt" ] || fail "the reject removed proposal 5"
[ ! -e "$ROOT_DIR/tasks/usage-report" ] || fail "the reject created the task"
expect_needs_you no rejected-5
say "smoke: proposal 4 (the group's chat) applied by agent within five seconds and logged; proposal 5 (a sibling's chat) waited, was rejected and stays"

# ---------------------------------------------------------------------------------------------
step "Builder 6: a leaf with a worktree and a branch is split; the keeper gets both"
focus_editor
wkeys s
choose "proration endpoint"
API_WT="$REPO/.bongos-worktrees/api"
poll 10 "the worktree $API_WT within ten seconds" test -d "$API_WT"
wait_toml "$ROOT_DIR/tasks/api/task.toml" 'd.get("branch")' api
{
  header 6 "$ROOT_ID" "kyle" "one PR for the endpoint, one for its docs"
  tline "$ROOT_DIR" 0
  tline "$ROOT_DIR/tasks/api" 2
  cat <<'EOF'
    task api-core: endpoint core
      result a PR with the endpoint
      todo implement | role produces-branch
      todo code review
      todo ship | role ships
    task api-docs: endpoint docs
      result a PR with the endpoint docs
      todo implement | role produces-branch
      todo review
      todo ship | role ships
EOF
} | write_proposal "$ROOT_DIR" 6
focus_editor
wkeys shift+a
wait_for_settled
shot proposal-keeper
frame_has_ui proposal-keeper
choose api-core
wait_for_settled
choose Apply
wait_toml "$ROOT_DIR/tasks/api/tasks/api-core/task.toml" 'd.get("branch")' api
dump_preview split
preview_has 'api becomes a group'
preview_has 'branch api'
preview_has 'worktree'
preview_has '"api-core" (keeps work, chosen)'
[ "$(toml "$ROOT_DIR/tasks/api/tasks/api-core/task.toml" '"worktree" in d')" = True ] || fail "the keeper has no worktree: $(cat "$ROOT_DIR/tasks/api/tasks/api-core/task.toml")"
[ "$(toml "$ROOT_DIR/tasks/api/tasks/api-docs/task.toml" '"branch" in d or "worktree" in d')" = False ] || fail "the other child took the branch or the worktree"
[ "$(toml "$ROOT_DIR/tasks/api/task.toml" '"branch" in d or "worktree" in d or len(d.get("todos", [])) > 0')" = False ] || fail "the group kept work: $(cat "$ROOT_DIR/tasks/api/task.toml")"
test -d "$API_WT" || fail "the worktree directory moved or vanished"
say "smoke: the keeper question named the branch; api-core got the worktree and branch api; the group has none ($RUN/proposal-keeper.png)"

# ---------------------------------------------------------------------------------------------
step "Builder 7: C-c w B writes the next proposal for the shown chat's task and pastes a line into the chat"
# The review chat of engine is the one shown in the Chats panel (the newest started).
focus_task "proration engine"
wquiet shift+b
ENGINE_PROPOSAL="$ROOT_DIR/tasks/engine/proposals/1.proposal.txt"
assert_file "$ENGINE_PROPOSAL" "^by chat $ENGINE_CHAT\$"
assert_file "$ENGINE_PROPOSAL" "^for $(toml "$ROOT_DIR/tasks/engine/task.toml" 'd["id"]')\$"
assert_file "$ENGINE_PROPOSAL" '^task engine: proration engine \| id '
poll 15 "the paste into the chat" input_has_text "Edit proposal 1 at $(realpath "$ROOT_DIR")/tasks/engine/proposals/1.proposal.txt"
# The pre-filled file changes nothing, so it neither applies nor waits for anyone.
expect_needs_you no untouched
say "smoke: C-c w B wrote proposal 1 of engine with its id and chat, pre-filled with the printed subtree, and pasted the line; it does not nag"

# ---------------------------------------------------------------------------------------------
step "Builder 8: a proposal written while the app is quit is classified, never applied, at the first scan"
quit_with_prompt
{
  header 7 "$ROOT_ID" "chat $PLAN_CHAT" "written while the app was quit"
  printf 'task %s: %s | id %s\n' "$ROOT_SLUG" "$ROOT_GOAL" "$ROOT_ID"
  cat <<'EOF'
  task late-helper: late helper
    result a PR adding the late helper
    todo implement | role produces-branch
    todo review
    todo ship | role ships
EOF
} | write_proposal "$ROOT_DIR" 7
launch_drawn
focus_editor
expect_needs_you yes late
sleep 5
[ ! -e "$ROOT_DIR/tasks/late-helper" ] || fail "proposal 7 applied on its own at the first scan"
[ "$(toml "$ROOT_DIR/task.toml" '[p["number"] for p in d["proposals"] if p["number"] == 7]')" = "[]" ] || fail "proposal 7 has a record: $(cat "$ROOT_DIR/task.toml")"
focus_task "implement proration"
wquiet shift+x
wait_toml "$ROOT_DIR/task.toml" '[(p["number"], p["state"]) for p in d["proposals"] if p["number"] == 7]' "[(7, 'rejected')]"
expect_needs_you no rejected-7
# The Work panel, open at its top level, shows the project row without "needs you".
focus_editor
wquiet w
shot work-panel-final
frame_has_ui work-panel-final
say "smoke: proposal 7 waited with the Billing row reading needs you; rejecting it cleared the row ($RUN/work-panel-final.png)"
quit_any

# ---------------------------------------------------------------------------------------------
step "Builder 9: no network"
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
builder_requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
stray_requests=$(printf '%s\n' "$builder_requests" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
if [ -n "$stray_requests" ]; then
  echo "bongos: the task builder scenario reached the proxy for other hosts:" >&2
  printf '%s\n' "$stray_requests" | sort | uniq -c >&2
  exit 1
fi
say "smoke: no request beyond the app's startup ones ($(printf '%s' "$builder_requests" | grep -c . || true) requests, all to the ACP registry or localhost)"

cp "$RUN/config-backup/settings-26.json" "$SETTINGS_FILE"
