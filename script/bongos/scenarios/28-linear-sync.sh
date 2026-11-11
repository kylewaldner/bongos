# Scenario for feature 28-linear-sync. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh. Debug builds only: it reads the Work panel through `work: dump
# view` (<logs>/work-view.txt), the proposal preview through `proposal: dump preview` and the sync
# through `linear: dump state` (<logs>/linear-state.txt).
#
# Setup, as scenarios 04, 25 and 26 do: a git repository /tmp/linear-src/billing with one commit,
# `"base_keymap": "Emacs"` (the C-c w keys exist only there), `work.repositories` naming it and
# `chats.agents.claude.command` naming a wrapper around the fake Claude of crates/bongos_chat/
# test-fixtures. script/bongos/fixtures/fake_linear.py is a GraphQL server on 127.0.0.1 (canned
# ticket SEQ-120 with subtickets SEQ-121 to SEQ-124, one team with workflow states). It applies
# issueCreate and issueUpdate to its state and logs `<operation> <ids> auth=<present|absent>` to
# $RUN/fake-linear.log, never the header value. The app is started with BONGOS_LINEAR_API_URL at it
# and NO_PROXY=127.0.0.1, so the fake is reached directly and anything else still hits the logging
# proxy. The scenario launches the app once, with ZED_RELEASE_CHANNEL=dev (see the deviations).
#   1. linear.enabled false: Work flows run, the Linear commands are hidden from the palette and say
#      so. Then true with no token: Work flows run, `C-c w L L` says there is no token. The fake's
#      log and the proxy log have no request; `linear: dump state` reads token present: false.
#   2. `linear: set token`, typed into the masked modal: one `viewer` call (auth present). The token
#      is then found nowhere under the throwaway home and run directory (the credential store file
#      aside), nor in the app's arguments or environment, nor in the fake's log.
#   3. `C-c w L L` SEQ-120 into Billing: the preview (a screenshot) lists one group and four
#      leaves; Import: five task directories with `linear` set, and `work: dump view` shows the
#      identifiers of the four leaves.
#   4. A proposal by kyle adds three leaves under the group; C-c w A; the offer (preview, checklist
#      and question, screenshots) lists three; y: three issueCreate with the parent SEQ-120, and
#      the leaves read SEQ-140 to SEQ-142.
#   5. The fake adds SEQ-127; C-c w L R; C-c w L s lists it (screenshot), enter: a linked leaf. The
#      fake adds SEQ-128; refresh, C-c w L s, d, y: it is dismissed and does not come back after
#      another refresh (C-c w L s says there is none).
#   6. C-c w d b on the plan todo of SEQ-121: one issueUpdate to In Progress (and the group, the
#      first leaf having moved); a refresh writes no other. C-c w s: the branch is seq-121-<slug>.
#      A plan chat on SEQ-121 has `SEQ-121 Add proration columns` in its context.md.
#   7. task: drop on the leaf SEQ-122: the question (screenshot); c: one issueUpdate to Canceled.
#   8. The fake is stopped; C-c w L R: the rows read `linear unreachable`; C-c w n still works.
#   9. The proxy log has nothing beyond the requests every launch makes, and never api.linear.app.
# Deviations from the design, each forced by the code or the harness:
# - A group row of the Work view shows neither its identifier nor the chips (`1 suggestion`, `linear
#   unreachable`): only leaf rows do. So the dump shows the identifiers and `linear unreachable` on
#   the leaves, and the suggestion is shown by the picker `C-c w L s` opens.
# - The preview of the import and the offer are read-only buffers that have no dump, so their text is
#   checked through the files they lead to and saved as screenshots for the orchestrator.
# - The debug build is the `stable` channel, whose credential store is the system keychain (no
#   secret service in the sandbox). ZED_RELEASE_CHANNEL=dev (honored by debug builds only) makes it
#   the development credential file, so the token store works; the app then starts with the project
#   directory as its argument, as the dev channel keeps its own session database.
# - Created tickets are numbered from SEQ-140, so the tickets the fake adds later keep the names
#   SEQ-127 and SEQ-128.
# - A todo that has started is no longer offered by C-c w d b, so "again: none" is shown by an
#   explicit refresh that writes no second issueUpdate.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 28-linear-sync skipped under BONGOS_STOCK=1: the stock run has no Linear sync"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
VIEW_FILE="$LOGS_DIR/work-view.txt"
STATE_FILE="$LOGS_DIR/linear-state.txt"
PROPOSAL_PREVIEW="$LOGS_DIR/proposal-preview.txt"
REPO=/tmp/linear-src/billing
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/billing"
FIXTURES="$BONGOS_ROOT/crates/bongos_chat/test-fixtures"
CHAT_BIN="$RUN/chat-bin"
FAKE_LOG="$RUN/fake-linear.log"
FAKE_PORT_FILE="$RUN/fake-linear.port"
FAKE_PID=""
export FAKE_CLI_LOG="$RUN/fake-cli.log"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)
TOKEN="lin_api_SmokeToken0123456789abcdefghijkl"
GROUP_GOAL="Proration for billing"

for tool in git python3 curl realpath; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# The smoke trap waits for every child, so the fake must go first, with the exit status kept.
scenario_exit() {
  local rc=$?
  set +e
  [ -z "${FAKE_PID:-}" ] || kill "$FAKE_PID" 2>/dev/null
  (exit "$rc")
  cleanup
}
trap scenario_exit EXIT

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

# Clicks the editor area between the docks (the Work panel is on the left, the chats on the right),
# so the workspace has the keyboard focus (a chat terminal sends C-c w keys to the CLI).
focus_editor() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH * 3 / 5)) $((HEIGHT / 3)) click 1
  wait_for_settled
}

# Launches with the project directory (the dev channel keeps its own session database) and waits
# until the first frame is drawn.
launch_drawn() {
  launch "$RUN/project"
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# Quits with or without a prompt (running chats ask, the first button is Quit).
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

# Focuses the task whose picker row has the words <$1> (C-c w f), then the editor. The picker opens
# on the focused task and keeps it selected while it still matches what is typed, so a word that
# matches nothing is typed and erased first: the selection then starts over at the first match.
focus_task() {
  wkeys f
  act type_text zzz
  act keys BackSpace BackSpace BackSpace
  choose "$1"
  focus_editor
}

# --- The fake Linear server ---------------------------------------------------------------------

start_fake() {
  rm -f "$FAKE_PORT_FILE"
  python3 -I -u "$BONGOS_ROOT/script/bongos/fixtures/fake_linear.py" "$FAKE_PORT_FILE" "$FAKE_LOG" \
    >>"$RUN/fake-linear.out" 2>&1 &
  FAKE_PID=$!
  poll 15 "the fake Linear server" test -s "$FAKE_PORT_FILE"
  FAKE_PORT=$(cat "$FAKE_PORT_FILE")
}

stop_fake() {
  kill "$FAKE_PID"
  wait "$FAKE_PID" 2>/dev/null || true
  FAKE_PID=""
}

# fake_control <json>: changes the fake's state; the request is not logged.
fake_control() {
  curl -s -f --noproxy '*' -X POST --data "$1" "http://127.0.0.1:$FAKE_PORT/control" >/dev/null ||
    fail "the fake Linear server refused $1"
}

# The number of lines of the fake's log that match the extended pattern <$1>.
fake_count() { grep -c -E -- "$1" "$FAKE_LOG" || true; }

fake_has_count() { [ "$(fake_count "$1")" = "$2" ]; }

fake_at_least() { [ "$(fake_count "$1")" -ge "$2" ]; }

# --- Reading files ----------------------------------------------------------------------------

toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 20 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

# One line per task with a `linear` identifier under the Work root: `<identifier><TAB><task dir>`.
linked_tasks() {
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
    if d.get("linear") and d.get("linear_state"):
        print(d["linear"] + "\t" + base)' "$WORK_ROOT"
}

linked_count() { linked_tasks | wc -l; }

linked_count_is() { [ "$(linked_count)" = "$1" ]; }

# The directory of the task linked to the identifier <$1>; empty when there is none.
linear_dir() { linked_tasks | awk -F '\t' -v id="$1" '$1 == id { print $2; exit }'; }

children_count_is() { [ "$(ls "$1/tasks" 2>/dev/null | wc -l)" = "$2" ]; }

have_linear_dir() { [ -n "$(linear_dir "$1")" ]; }

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

# --- Writing proposals as Kyle would -------------------------------------------------------------

# "task <slug>: <goal> | id <id>" of the task in <task dir> $1, indented by $2 spaces: the line the
# printer of the builder writes for an existing task.
tline() {
  python3 -I -c 'import os, sys, tomllib
d = tomllib.load(open(os.path.join(sys.argv[1], "task.toml"), "rb"))
print(" " * int(sys.argv[2]) + "task " + os.path.basename(sys.argv[1]) + ": " + d["goal"] + " | id " + d["id"])' "$1" "$2"
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

# --- Reading the Work view -------------------------------------------------------------------------
# A dump is the breadcrumb, then one line per line of the level: a marker (`>` selected row, `~`
# faded, else a space) and the text. A title line is the goal, or the goal, two spaces and the state
# or age; a detail line is indented four spaces.

dump_view() {
  rm -f "$VIEW_FILE"
  palette "work: dump view"
  assert_file "$VIEW_FILE"
  cp "$VIEW_FILE" "$RUN/work-view-$1.txt"
}

breadcrumb() { head -n1 "$VIEW_FILE"; }

# The detail line of the row of <goal> in the last dump, without its marker and indent.
view_detail() {
  awk -v goal="$1" 'NR > 1 {
    t = substr($0, 2)
    if (hit) { sub(/^ +/, "", t); print t; exit }
    if (substr(t, 1, 1) != " " && (t == goal || index(t, goal "  ") == 1)) hit = 1
  }' "$VIEW_FILE"
}

# Opens the Work panel with the focus in it. `work: toggle panel` takes the focus away when the panel
# has it, so the editor is focused first.
focus_panel() {
  focus_editor
  keys ctrl+c w w
  wait_for_settled
}

# Moves the panel's selection to the row of <goal>; the last dump is what the panel shows.
select_row() {
  local pos sel tgt
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

# RET on the row of the group <goal>: the panel shows its tasks.
drill() {
  select_row "$1"
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

# The panel shows the level of the project Billing, or of the group under it.
project_level() {
  go_top
  drill Billing
}

group_level() {
  project_level
  drill "$GROUP_GOAL"
}

# Dumps the level the panel shows again, up to five times two seconds apart, until the detail line of
# <goal> has (or, with `lacks`, has not) the words <want>: the sync reads in the background.
wait_detail() {
  local mode=$1 goal=$2 want=$3 name=$4 i detail=""
  for i in 1 2 3 4 5; do
    dump_view "$name"
    detail=$(view_detail "$goal")
    if grep -qF -- "$want" <<<"$detail"; then
      [ "$mode" = has ] && return 0
    else
      [ "$mode" = lacks ] && [ -n "$detail" ] && return 0
    fi
    sleep 2
  done
  fail "the detail of '$goal' reads '$detail', expected it to $mode '$want': $(cat "$VIEW_FILE")"
}

dump_state() {
  local i
  for i in 1 2 3 4 5; do
    rm -f "$STATE_FILE"
    palette "linear: dump state"
    if poll 3 "the Linear state dump" test -f "$STATE_FILE" 2>/dev/null; then
      cp "$STATE_FILE" "$RUN/linear-state-$1.txt"
      return 0
    fi
  done
  fail "linear: dump state wrote nothing"
}

state_has() { grep -qxF -- "$1" "$STATE_FILE" || fail "the Linear state lacks '$1': $(cat "$STATE_FILE")"; }

write_settings() {
  cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "linear": { "enabled": $1 },
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
}

# Waits until the fake has logged <$2> or more lines matching <$1>, then lets the screen settle.
settle_after_fake() {
  poll 30 "the fake to log $1 ($2 times)" fake_at_least "$1" "$2"
  sleep 1
  wait_for_settled
}

# Fails when the proxy got a request since the scenario began other than the two kinds every launch
# makes (the ACP registry and localhost, as in scenarios 03, 25 and 26), or any request naming Linear.
proxy_strays() {
  local registry_host requests strays
  registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
    "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
  [ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
  requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
  strays=$(printf '%s\n' "$requests" |
    awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
  if [ -n "$strays" ]; then
    echo "bongos: the proxy got other requests while $1:" >&2
    printf '%s\n' "$strays" | sort | uniq -c >&2
    exit 1
  fi
}

# ---------------------------------------------------------------------------------------------
step "Linear 1: disabled, then enabled with no token: Work flows run and nothing is requested"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$RUN/config-backup" "$CHAT_BIN" /tmp/linear-src "$WORK_ROOT/oneoffs"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-28.json"
: >"$FAKE_CLI_LOG"
cat >"$CHAT_BIN/claude" <<SH
#!/bin/sh
FAKE_CLI_LOG='$FAKE_CLI_LOG'
export FAKE_CLI_LOG
exec '$FIXTURES/fake-claude' "\$@"
SH
chmod +x "$CHAT_BIN/claude"
git init -q -b main "$REPO"
printf '# billing\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" "${GIT_ID[@]}" commit -qm "first commit"
write_settings false

start_fake
[ ! -s "$FAKE_LOG" ] || fail "the fake has a request before the app started: $(cat "$FAKE_LOG")"
export BONGOS_LINEAR_API_URL="http://127.0.0.1:$FAKE_PORT/graphql"
export NO_PROXY=127.0.0.1
export ZED_RELEASE_CHANNEL=dev
launch_drawn
focus_editor

wkeys p n
choose Billing
assert_file "$PROJECT_DIR/task.toml"
assert_file "$PROJECT_DIR/project.toml"
[ ! -e "$PROJECT_DIR/.git" ] || fail "project: new made $PROJECT_DIR/.git"
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
wkeys n
choose "write the intro"
poll 10 "the task 'write the intro'" have_task "write the intro"
# Off: the commands are not in the palette and the key only says so.
act keys ctrl+shift+p
act type_text "new from linear"
shot linear-palette-off
frame_has_ui linear-palette-off
act keys Escape
wquiet shift+l shift+l
shot linear-off-toast
frame_has_ui linear-off-toast
focus_editor

# On with no token: the command says there is no token and nothing is sent.
write_settings true
sleep 2
wait_for_settled
act keys ctrl+shift+p
act type_text "new from linear"
shot linear-palette-on
frame_has_ui linear-palette-on
act keys Escape
wkeys n
choose "review the tariff"
poll 10 "the task 'review the tariff'" have_task "review the tariff"
wquiet shift+l shift+l
shot linear-no-token-toast
frame_has_ui linear-no-token-toast
focus_editor
wquiet shift+l shift+r
dump_state no-token
state_has "enabled: true"
state_has "token present: false"
state_has "requests allowed: 0"
state_has "linked tasks: 0"
[ ! -s "$FAKE_LOG" ] || fail "the fake has a request while no token is set: $(cat "$FAKE_LOG")"
proxy_strays "no token is set"
say "smoke: off, then on with no token: tasks were made, the Linear key said so, the fake and the proxy saw nothing ($RUN/linear-no-token-toast.png)"

# ---------------------------------------------------------------------------------------------
step "Linear 2: linear: set token (masked modal): one viewer call, and the token is nowhere"
palette "linear: set token"
act type_text "$TOKEN"
shot linear-token-modal
frame_has_ui linear-token-modal
act keys Return
poll 30 "the token check" fake_has_count '^viewer auth=present$' 1
wait_for_settled
[ "$(fake_count '.')" = 1 ] || fail "the token check made other requests: $(cat "$FAKE_LOG")"
assert_file "$CONFIG_DIR/development_credentials"
dump_state token
state_has "token present: true"
leaks=$(grep -rIlF -- "$TOKEN" "$RUN" --exclude=development_credentials 2>/dev/null || true)
[ -z "$leaks" ] || fail "the token is in: $leaks"
for proc_file in cmdline environ; do
  if tr '\0' '\n' <"/proc/$APP_PID/$proc_file" | grep -qF -- "$TOKEN"; then
    fail "the token is in the app's $proc_file"
  fi
done
say "smoke: one viewer call with the Authorization header present; the token is not in $RUN (home, logs, dumps, chats runtime), the fake's log, or the app's arguments and environment"

# ---------------------------------------------------------------------------------------------
step "Linear 3: C-c w L L SEQ-120: the preview, then five linked tasks"
wquiet shift+l shift+l
settle_after_fake '^recent auth=present$' 1
act type_text "SEQ-120"
shot linear-import-picker
frame_has_ui linear-import-picker
act keys Return
settle_after_fake '^issues SEQ-120 auth=present$' 1
choose Billing
settle_after_fake '^issues SEQ-121,SEQ-122,SEQ-123,SEQ-124 auth=present$' 1
shot linear-import-preview
frame_has_ui linear-import-preview
choose Import
poll 30 "five linked tasks" linked_count_is 5
for identifier in SEQ-120 SEQ-121 SEQ-122 SEQ-123 SEQ-124; do
  have_linear_dir "$identifier" || fail "no task is linked to $identifier: $(linked_tasks)"
done
GROUP_DIR=$(linear_dir SEQ-120)
GROUP_ID=$(toml "$GROUP_DIR/task.toml" 'd["id"]')
[ "$(toml "$GROUP_DIR/task.toml" 'd["goal"]')" = "$GROUP_GOAL" ] || fail "the group's goal is not the ticket's title: $(cat "$GROUP_DIR/task.toml")"
[ "$(ls "$GROUP_DIR/tasks" | wc -l)" = 4 ] || fail "the group does not have four children: $(ls "$GROUP_DIR/tasks")"
for identifier in SEQ-121 SEQ-122 SEQ-123 SEQ-124; do
  leaf=$(linear_dir "$identifier")
  [ "$(toml "$leaf/task.toml" '[t["name"] for t in d["todos"]]')" = "['plan']" ] || fail "$identifier is not a leaf with one plan todo: $(cat "$leaf/task.toml")"
done
focus_editor
project_level
shot linear-work-project-level
frame_has_ui linear-work-project-level
drill "$GROUP_GOAL"
for identifier in SEQ-121 SEQ-122 SEQ-123 SEQ-124; do
  grep -qF "$identifier" "$VIEW_FILE" || fail "the Work view does not show $identifier: $(cat "$VIEW_FILE")"
done
shot linear-work-group-level
frame_has_ui linear-work-group-level
focus_editor
say "smoke: five task directories with linear set; the Work view shows SEQ-120 to SEQ-124 ($RUN/linear-import-preview.png, $RUN/linear-work-group-level.png)"

# ---------------------------------------------------------------------------------------------
step "Linear 4: a proposal adds three leaves under the group; the offer; three issueCreate"
{
  header 1 "$GROUP_ID" "kyle" "three more leaves for the refunds work"
  tline "$GROUP_DIR" 0
  cat <<'EOF'
  task refund-api: Refund API
    result a PR adding the refund API
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task credit-notes: Credit notes
    result a PR adding credit notes
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task proration-report: Proration report
    result a PR adding the proration report
    todo implement | role produces-branch
    todo review
    todo ship | role ships
EOF
} | write_proposal "$GROUP_DIR" 1
focus_task "SEQ-120"
wkeys shift+a
sleep 2
wait_for_settled
shot linear-proposal-confirm
frame_has_ui linear-proposal-confirm
choose Apply
wait_toml "$GROUP_DIR/task.toml" '[p["number"] for p in d["proposals"] if p["state"] == "applied"]' "[1]"
poll 20 "seven children of the group" children_count_is "$GROUP_DIR" 7
[ "$(fake_count '^issueCreate')" = 0 ] || fail "a ticket was created before the offer was confirmed: $(cat "$FAKE_LOG")"
sleep 2
wait_for_settled
shot linear-offer-checklist
frame_has_ui linear-offer-checklist
act keys Return
wait_for_settled
shot linear-offer-confirm
frame_has_ui linear-offer-confirm
[ "$(fake_count '^issueCreate')" = 0 ] || fail "a ticket was created before the question was answered: $(cat "$FAKE_LOG")"
act keys y
poll 30 "three issueCreate" fake_has_count '^issueCreate parent=SEQ-120 id=SEQ-14[0-9] auth=present$' 3
poll 30 "eight linked tasks" linked_count_is 8
for identifier in SEQ-140 SEQ-141 SEQ-142; do
  have_linear_dir "$identifier" || fail "no leaf is linked to $identifier: $(linked_tasks)"
done
new_goals=$(for identifier in SEQ-140 SEQ-141 SEQ-142; do toml "$(linear_dir "$identifier")/task.toml" 'd["goal"]'; done | sort | tr '\n' ',')
[ "$new_goals" = "Credit notes,Proration report,Refund API," ] || fail "the three tickets are not the three new leaves: $new_goals"
focus_editor
group_level
for identifier in SEQ-140 SEQ-141 SEQ-142; do
  grep -qF "$identifier" "$VIEW_FILE" || fail "the Work view does not show $identifier: $(cat "$VIEW_FILE")"
done
shot linear-work-after-offer
frame_has_ui linear-work-after-offer
focus_editor
say "smoke: the offer listed three leaves; y created SEQ-140 to SEQ-142 under SEQ-120 ($RUN/linear-offer-checklist.png, $RUN/linear-offer-confirm.png)"

# ---------------------------------------------------------------------------------------------
step "Linear 5: suggestions: accept one, dismiss one, and the dismissed one does not return"
fake_control '{"op": "add", "parent": "SEQ-120", "identifier": "SEQ-127", "title": "Handle refunds"}'
READS=$(fake_count '^issues ')
focus_task "SEQ-120"
wquiet shift+l shift+r
poll 30 "a refresh read" fake_at_least '^issues ' $((READS + 1))
sleep 2
wait_for_settled
focus_task "SEQ-120"
wquiet shift+l s
shot linear-suggestion-picker
frame_has_ui linear-suggestion-picker
act keys Return
poll 30 "SEQ-127 linked" have_linear_dir SEQ-127
[ "$(toml "$(linear_dir SEQ-127)/task.toml" 'd["goal"]')" = "Handle refunds" ] || fail "SEQ-127 did not become a leaf named by its title"
wait_for_settled
fake_control '{"op": "add", "parent": "SEQ-120", "identifier": "SEQ-128", "title": "Rounding rules"}'
READS=$(fake_count '^issues ')
focus_task "SEQ-120"
wquiet shift+l shift+r
poll 30 "a refresh read" fake_at_least '^issues ' $((READS + 1))
focus_task "SEQ-120"
wquiet shift+l s
act keys d
shot linear-dismiss-question
frame_has_ui linear-dismiss-question
act keys y
wait_toml "$GROUP_DIR/task.toml" 'd["linear_state"]["dismissed"]' "['SEQ-128']"
if have_linear_dir SEQ-128; then fail "the dismissed ticket became a task: $(linked_tasks)"; fi
READS=$(fake_count '^issues ')
focus_task "SEQ-120"
wquiet shift+l shift+r
poll 30 "a refresh read" fake_at_least '^issues ' $((READS + 1))
sleep 2
wait_for_settled
focus_task "SEQ-120"
wquiet shift+l s
shot linear-no-suggestions
frame_has_ui linear-no-suggestions
focus_editor
say "smoke: SEQ-127 accepted as a linked leaf; SEQ-128 dismissed and still not suggested after another refresh ($RUN/linear-suggestion-picker.png, $RUN/linear-no-suggestions.png)"

# ---------------------------------------------------------------------------------------------
step "Linear 6: the plan todo starts the ticket once; the branch and the chat carry the identifier"
[ "$(fake_count '^issueUpdate')" = 0 ] || fail "a status was written before any plan began: $(cat "$FAKE_LOG")"
wkeys d b
choose "SEQ-121"
choose plan
poll 30 "SEQ-121 started" fake_has_count '^issueUpdate SEQ-121 state=In Progress auth=present$' 1
poll 30 "SEQ-120 started, the group following its first leaf" fake_has_count '^issueUpdate SEQ-120 state=In Progress auth=present$' 1
wait_for_settled
START_DIR=$(linear_dir SEQ-121)
poll 20 "started_sent on SEQ-121" toml_is "$START_DIR/task.toml" '"started_sent" in d["linear_state"]' True
READS=$(fake_count '^issues ')
focus_task "SEQ-120"
wquiet shift+l shift+r
poll 30 "a refresh read" fake_at_least '^issues ' $((READS + 1))
sleep 2
[ "$(fake_count '^issueUpdate')" = 2 ] || fail "a second status was written: $(cat "$FAKE_LOG")"
focus_task "SEQ-121"
wkeys s
choose "SEQ-121"
START_SLUG=$(basename "$START_DIR")
poll 20 "the branch of SEQ-121" toml_is "$START_DIR/task.toml" 'd.get("branch")' "seq-121-$START_SLUG"
wait_worktree_line "$START_DIR" || fail "SEQ-121 has no worktree line: $(cat "$START_DIR/task.toml")"
poll 10 "the worktree of SEQ-121" test -d "$(worktree_of "$START_DIR")"
focus_task "SEQ-121"
wkeys b
choose claude
focus_editor
poll 15 "a plan chat record in $START_DIR/chats" have_record "$START_DIR" type plan
PLAN_CHAT=$(toml "$(record_with "$START_DIR" type plan)" 'd["id"]')
CONTEXT="$DATA_DIR/chats/$PLAN_CHAT/context.md"
assert_file "$CONTEXT" '^## Linear$'
assert_file "$CONTEXT" 'SEQ-121 Add proration columns'
[ "$(fake_count '^issueUpdate')" = 2 ] || fail "starting the plan chat wrote a status: $(cat "$FAKE_LOG")"
shot linear-started
frame_has_ui linear-started
say "smoke: one issueUpdate to In Progress for SEQ-121 and one for the group; a refresh wrote none; branch seq-121-$START_SLUG; the chat context has the SEQ-121 title line"

# ---------------------------------------------------------------------------------------------
step "Linear 7: dropping a linked leaf asks, and c cancels the ticket"
DROP_DIR=$(linear_dir SEQ-122)
palette "task: drop"
choose "SEQ-122"
wait_toml "$DROP_DIR/task.toml" 'd["status"]' cancelled
sleep 2
wait_for_settled
shot linear-cancel-question
frame_has_ui linear-cancel-question
[ "$(fake_count '^issueUpdate SEQ-122')" = 0 ] || fail "the ticket was cancelled before the answer: $(cat "$FAKE_LOG")"
act keys c
poll 30 "SEQ-122 canceled" fake_has_count '^issueUpdate SEQ-122 state=Canceled auth=present$' 1
poll 20 "the cancel record" toml_is "$DROP_DIR/task.toml" 'd["linear_state"].get("cancel")' cancelled
sleep 2
[ "$(fake_count '^issueUpdate')" = 3 ] || fail "another status was written: $(cat "$FAKE_LOG")"
say "smoke: the question named SEQ-122; c wrote one issueUpdate to Canceled ($RUN/linear-cancel-question.png)"

# ---------------------------------------------------------------------------------------------
step "Linear 8: the fake is stopped: rows read linear unreachable, and Work goes on"
stop_fake
focus_task "SEQ-120"
wquiet shift+l shift+r
group_level
wait_detail has "Proration endpoint" "linear unreachable" unreachable-group
wait_detail has "Proration engine" "linear unreachable" unreachable-group
shot linear-unreachable
frame_has_ui linear-unreachable
focus_editor
wkeys n
choose "offline note"
poll 10 "the task 'offline note'" have_task "offline note"
say "smoke: the leaves read linear unreachable; C-c w n made a task ($RUN/linear-unreachable.png)"

# ---------------------------------------------------------------------------------------------
step "Linear 9: no request through the proxy beyond the app's own"
proxy_strays "the scenario ran"
linear_requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
if grep -qF 'linear' "$RUN/proxy.log"; then fail "a Linear host reached the proxy: $(cat "$RUN/proxy.log")"; fi
if grep -q 'auth=absent' "$FAKE_LOG"; then fail "a request had no Authorization header: $(cat "$FAKE_LOG")"; fi
if grep -qF -- "$TOKEN" "$FAKE_LOG"; then fail "the fake's log has the token"; fi
cp "$FAKE_LOG" "$RUN/fake-linear-final.log"
say "smoke: the fake logged $(grep -c . "$FAKE_LOG") requests, all with the header present and none with its value; the proxy saw $(printf '%s' "$linear_requests" | grep -c . || true) requests, all to the ACP registry or localhost ($RUN/fake-linear-final.log)"
quit_any

cp "$RUN/config-backup/settings-28.json" "$SETTINGS_FILE"
unset BONGOS_LINEAR_API_URL NO_PROXY ZED_RELEASE_CHANNEL
trap cleanup EXIT
