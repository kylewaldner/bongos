# Scenario for feature 04-chat-sessions. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh. Debug builds only: it reads the Chats panel through
# `chat: dump view` (<logs>/chat-view.txt).
#
# Setup: a git repository /tmp/chat-src/billing with one commit (a real directory, not under $HOME,
# which is a symlink here: the fake Codex records its own `pwd` and bongos matches that string against
# the chat's cwd), `"base_keymap": "Emacs"` (the C-c w and C-c c keys exist only there),
# `work.repositories` naming it, and `chats.agents.<agent>.command` naming two wrapper scripts that
# set the knobs of the fake CLIs in crates/bongos_chat/test-fixtures and run them. The fake Codex
# wrapper hides `fork` from `--help`, so a fork is a handoff. The project and its task are made with
# the Work commands, as scenario 03 does; the task's artifacts are written as files.
# State is checked through files (records, context.md, $FAKE_CLI_LOG, the fake's byte-for-byte input
# log) and `chat: dump view`. What no file or dump shows (which pane is where, the text of the quit
# prompt, the artifacts tab) is saved as a screenshot for the orchestrator to read.
#   1. Focus the task, C-c c n, claude, implement: the fake logs --session-id, the worktree cwd and
#      BONGOS_TASK_ID; the record has layout = "dual"; chat-dual.png shows the terminal left and
#      prompts.txt right.
#   2. A three-paragraph prompt typed in the prompts pane, chat: send region: exactly one bracketed
#      paste with the text and one Enter reach the fake; the record's name is the first line.
#   3. C-c c p twice: layout goes chat-only and back in the record; a second send holds the text
#      typed (and never saved) before the toggles, so the editor kept it.
#   4. One artifact human-only (C-c w h), C-c c k, C-c c o: the fake logs --resume; the regenerated
#      context.md lacks the hidden file and every generated file and the log never name it; C-c c v.
#   5. Project focus, C-c c n, codex, plan: chat-only, in the repository; the fake writes its session
#      file and the record gets session_id. Quit (the prompt, with two running chats, is
#      quit-prompt-two.png), relaunch: both are listed; chat: open any resumes with codex resume <id>.
#   6. C-c c f on the project chat into the task: a record under the task with parent, fork =
#      "handoff", no prompts_file (so the task's prompts.txt is in the right pane), labeled handoff.
#   7. C-c c a then C-c c u: the record moves between the lists and still exists.
#   8. The fake claude rings the bell: the header and the row read waiting. The one second bound is the
#      runtime test's; here the dump takes a few seconds, and the state has to still be waiting.
#   9. No request through the proxy beyond the two kinds every launch makes (as in scenario 03).
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 04-chat-sessions skipped under BONGOS_STOCK=1: the stock run has no Chats view"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
CHAT_VIEW="$LOGS_DIR/chat-view.txt"
REPO=/tmp/chat-src/billing
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/billing"
TASK_DIR="$PROJECT_DIR/tasks/prorate-invoices"
FIXTURES="$BONGOS_ROOT/crates/bongos_chat/test-fixtures"
CHAT_BIN="$RUN/chat-bin"
FAKE_CLAUDE_INPUT="$RUN/fake-claude-input.txt"
export FAKE_CLI_LOG="$RUN/fake-cli.log"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

PROMPT=$'Prorate the invoice lines.\n\nCover credits and refunds.\n\nFinish with unit tests.'
PROMPT_NAME="Prorate the invoice lines."
# Each line ends in a period, so no word completion menu is open when Return is typed.
UNSAVED=$'\n\nUnsaved marker text.'

for tool in git python3 realpath pgrep; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# --- Input -----------------------------------------------------------------------------------

# C-c w <keys>: the Work keys of scenario 03. Each opens a picker or a prompt, so the screen changes.
wkeys() { act keys ctrl+c w "$@"; }

# C-c c <keys> for a command that opens a picker.
cpick() { act keys ctrl+c c "$@"; }

# C-c c <keys> for a command that changes no screen for certain: send, then wait until it is quiet.
ckeys() {
  keys ctrl+c c "$@"
  wait_for_settled
}

# Types text that has newlines: xdotool type drops them here, so each line is typed and Return is
# pressed between lines. Each line is typed only when it is not empty.
type_lines() {
  local text="$1" line first=1
  while IFS= read -r line || [ -n "$line" ]; do
    [ "$first" = 1 ] || keys Return
    first=0
    [ -z "$line" ] || type_text "$line"
  done <<<"$text"
}

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

# Clicks the editor area left of the Chats dock (the dock takes the right 720 px), so the workspace has
# the keyboard focus.
focus_editor() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH / 5)) $((HEIGHT / 3)) click 1
  wait_for_settled
}

# Launch with no arguments (the session restores the workspaces) and wait until it is drawn.
launch_drawn() {
  launch
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# Quits with chats running: ctrl+q asks, the first button is Quit and Return takes it. The prompt is
# kept as a screenshot, since its text is in no file.
quit_with_prompt() {
  local pid=$APP_PID
  focus_editor
  keys ctrl+q
  wait_for_settled
  shot "$1"
  frame_has_ui "$1"
  keys Return
  APP_PID=
  if ! poll 30 "the app to quit after the prompt" pid_gone "$pid"; then
    kill -TERM "$pid"
    return 1
  fi
  wait "$pid"
}

# --- Reading files ----------------------------------------------------------------------------

# toml <file> <python expression over d, the parsed file>
toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 15 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

# One line per artifact in <dir>/artifacts.toml: path=visibility.
visibilities() {
  toml "$1/artifacts.toml" '"\n".join(sorted(a["path"] + "=" + a.get("visibility", "default") for a in d.get("artifact", [])))'
}

# The record in <scope dir>/chats of the first chat that has <agent>.
record_of_agent() {
  python3 -I -c 'import glob, sys, tomllib
for path in sorted(glob.glob(sys.argv[1] + "/chats/*.toml")):
    if tomllib.load(open(path, "rb")).get("agent") == sys.argv[2]:
        print(path)
        break' "$1" "$2"
}

# The record in <scope dir>/chats whose parent is <chat id>.
record_with_parent() {
  python3 -I -c 'import glob, sys, tomllib
for path in sorted(glob.glob(sys.argv[1] + "/chats/*.toml")):
    if tomllib.load(open(path, "rb")).get("parent") == sys.argv[2]:
        print(path)
        break' "$1" "$2"
}

have_agent_record() { [ -n "$(record_of_agent "$1" "$2")" ]; }

have_child_record() { [ -n "$(record_with_parent "$1" "$2")" ]; }

has_session() { [ -n "$(toml "$1" 'd.get("session_id", "")' 2>/dev/null)" ]; }

# --- Reading what the fake CLIs did -----------------------------------------------------------

# 0 when the log has the line <line>, whole.
log_has() { [ -f "$FAKE_CLI_LOG" ] && grep -qxF -- "$1" "$FAKE_CLI_LOG"; }

# The last cwd the fake <name> logged.
logged_cwd() { grep "^$1 cwd " "$FAKE_CLI_LOG" | tail -n1 | cut -d' ' -f3-; }

same_dir() { [ "$(realpath "$1")" = "$(realpath "$2")" ]; }

fake_claude_up() { pgrep -f "test-fixtures/fake-claude" >/dev/null; }

fake_claude_gone() { ! fake_claude_up; }

# pastes_ok <count> <text>...: the fake claude's input holds exactly <count> bracketed pastes, whose
# text is each <text> in turn, and one Enter (and nothing else) after each.
pastes_ok() {
  [ -f "$FAKE_CLAUDE_INPUT" ] || return 1
  python3 -I -c 'import sys
data = bytes(int(token, 8) for token in open(sys.argv[1]).read().split())
start, end = b"\x1b[200~", b"\x1b[201~"
want = [text for text in sys.argv[3:]]
if data.count(start) != int(sys.argv[2]) or data.count(end) != int(sys.argv[2]):
    sys.exit(1)
position = 0
for text in want:
    first = data.find(start, position)
    last = data.find(end, first)
    if first < 0 or last < 0:
        sys.exit(1)
    pasted = data[first + len(start):last].decode().replace("\r\n", "\n").replace("\r", "\n")
    following = data.find(start, last)
    tail = data[last + len(end):following if following >= 0 else len(data)]
    if pasted != text or tail != b"\r":
        sys.exit(1)
    position = last
' "$FAKE_CLAUDE_INPUT" "$@"
}

# --- Reading the Chats view -------------------------------------------------------------------
# The dump of the list is "Chats", the CLI health lines, "Scope: ...", "Open (n)" with its rows,
# "Archived (n)" with its rows. A row is a title line ("> " when selected, else two spaces, then the
# name) and detail lines indented six spaces. The dump of a shown chat is "Chat" and one line:
# "<name> - <agent> - <type> - <state>".

dump_chat_view() {
  rm -f "$CHAT_VIEW"
  palette "chat: dump view"
  assert_file "$CHAT_VIEW"
  cp "$CHAT_VIEW" "$RUN/chat-view-$1.txt"
}

# The Chats list with every task's chats and the archive open.
dump_list() {
  ckeys l
  dump_chat_view "$1"
  if ! grep -q '^Scope: all tasks$' "$CHAT_VIEW"; then
    palette "chat: toggle all tasks"
    dump_chat_view "$1-all"
  fi
  grep -q '^Chats$' "$CHAT_VIEW" || fail "the Chats panel does not show the list: $(cat "$CHAT_VIEW")"
}

# The detail lines of the row named <name>, without their indent.
row_lines() {
  awk -v name="$1" '
    /^[A-Z]/ || /^$/ { hit = 0; next }
    substr($0, 1, 2) ~ /^[> ] $/ && substr($0, 3, 1) != " " { hit = (substr($0, 3) == name); next }
    hit { sub(/^ +/, ""); print }' "$CHAT_VIEW"
}

# open or archived: the section the row named <name> is in. Empty when there is no such row.
row_section() {
  awk -v name="$1" '
    /^Open \(/ { section = "open" }
    /^Archived \(/ { section = "archived" }
    substr($0, 1, 2) ~ /^[> ] $/ && substr($0, 3, 1) != " " && substr($0, 3) == name { print section; exit }' "$CHAT_VIEW"
}

expect_section() {
  local got
  got=$(row_section "$1")
  [ "$got" = "$2" ] || fail "the row '$1' is in '${got:-no section}', expected '$2': $(cat "$CHAT_VIEW")"
}

row_has() {
  local lines
  lines=$(row_lines "$1")
  grep -qE -- "$2" <<<"$lines"
}

# ---------------------------------------------------------------------------------------------
step "Chats setup: a repository, the fake CLIs, base_keymap Emacs, chats and work settings"
# Chats 9 looks only at the requests made from here on.
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$RUN/config-backup" "$CHAT_BIN"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-04.json"
: >"$FAKE_CLI_LOG"
cat >"$CHAT_BIN/claude" <<SH
#!/bin/sh
FAKE_CLI_LOG='$FAKE_CLI_LOG'
FAKE_CLI_INPUT='$FAKE_CLAUDE_INPUT'
export FAKE_CLI_LOG FAKE_CLI_INPUT
exec '$FIXTURES/fake-claude' "\$@"
SH
# The fake Codex has a fork command in its help; this one hides it, as a Codex without one would.
cat >"$CHAT_BIN/codex" <<SH
#!/bin/sh
FAKE_CLI_LOG='$FAKE_CLI_LOG'
FAKE_CLI_HOME='$HOME'
export FAKE_CLI_LOG FAKE_CLI_HOME
case "\${1:-}" in
  --help | -h)
    '$FIXTURES/fake-codex' "\$@" | grep -v '^  fork '
    exit 0
    ;;
esac
exec '$FIXTURES/fake-codex' "\$@"
SH
chmod +x "$CHAT_BIN/claude" "$CHAT_BIN/codex"
mkdir -p /tmp/chat-src
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
      "claude": { "command": "$CHAT_BIN/claude" },
      "codex": { "command": "$CHAT_BIN/codex" }
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

# ---------------------------------------------------------------------------------------------
step "Chats 1: an implement chat on the task starts in its worktree with the task's id"
wkeys p n
choose Billing
assert_file "$PROJECT_DIR/task.toml"
[ ! -e "$PROJECT_DIR/.git" ] || fail "project: new made $PROJECT_DIR/.git"
# No command names a project's repository; edit project.toml in place, as Kyle or an agent would.
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
wkeys shift+n
choose billing
choose "Prorate invoices"
assert_file "$TASK_DIR/task.toml"
TASK_ID=$(toml "$TASK_DIR/task.toml" 'd["id"]')
# Two artifacts, one to hide later. Their names appear nowhere else.
printf '# ledger draft\n' >"$TASK_DIR/ledger-draft.md"
printf '# design notes\n' >"$TASK_DIR/design-notes.md"
wkeys s
choose prorate
wait_worktree_line "$TASK_DIR" || fail "no worktree line: $(cat "$TASK_DIR/task.toml")"
WORKTREE=$(worktree_of "$TASK_DIR")
poll 10 "the worktree $WORKTREE within ten seconds" test -d "$WORKTREE"
wkeys f
choose prorate
focus_editor
cpick n
choose claude
choose "implement:"
wait_for_settled
poll 2 "the fake claude to start within two seconds" log_has "claude start"
poll 2 "claude --session-id in the log" log_has "claude arg --session-id"
poll 2 "BONGOS_TASK_ID in the log" log_has "claude env BONGOS_TASK_ID=$TASK_ID"
same_dir "$(logged_cwd claude)" "$WORKTREE" || fail "the fake claude ran in '$(logged_cwd claude)', not in $WORKTREE"
poll 15 "a claude record in $TASK_DIR/chats" have_agent_record "$TASK_DIR" claude
CLAUDE_REC=$(record_of_agent "$TASK_DIR" claude)
CLAUDE_ID=$(toml "$CLAUDE_REC" 'd["id"]')
wait_toml "$CLAUDE_REC" 'd["layout"]' dual
wait_toml "$CLAUDE_REC" 'd["type"]' implement
same_dir "$(toml "$CLAUDE_REC" 'd["cwd"]')" "$WORKTREE" || fail "the record's cwd is not the worktree: $(cat "$CLAUDE_REC")"
CLAUDE_SESSION=$(toml "$CLAUDE_REC" 'd["session_id"]')
log_has "claude arg $CLAUDE_SESSION" || fail "the session id of the record ($CLAUDE_SESSION) is not the one the fake got: $(cat "$FAKE_CLI_LOG")"
RUNTIME_DIR="$DATA_DIR/chats/$CLAUDE_ID"
assert_file "$RUNTIME_DIR/context.md" 'ledger-draft\.md'
assert_file "$RUNTIME_DIR/context.md" 'design-notes\.md'
assert_file "$TASK_DIR/prompts.txt"
dump_chat_view started
grep -qE '^.* - claude - implement - ' <<<"$(sed -n 2p "$CHAT_VIEW")" || fail "the shown chat's header is wrong: $(cat "$CHAT_VIEW")"
shot chat-dual
frame_has_ui chat-dual
say "smoke: implement chat in the worktree with the task's id; layout dual (terminal left, prompts.txt right: $RUN/chat-dual.png)"

# ---------------------------------------------------------------------------------------------
step "Chats 2: chat: send region pastes the three paragraphs once and presses Enter once"
ckeys e
type_lines "$PROMPT"
palette "chat: send region"
poll 15 "one bracketed paste of the prompt and one Enter" pastes_ok 1 "$PROMPT"
wait_toml "$CLAUDE_REC" 'd["name"]' "$PROMPT_NAME"
wait_toml "$CLAUDE_REC" 'd["prompted"]' True
say "smoke: one paste and one Enter; the chat is named '$PROMPT_NAME'"

# ---------------------------------------------------------------------------------------------
step "Chats 3: C-c c p twice toggles the layout in the record and keeps unsaved prompt text"
# Typed after the send and never saved: if the toggle rebuilt the editor from prompts.txt it is gone.
ckeys e
type_lines "$UNSAVED"
ckeys p
wait_toml "$CLAUDE_REC" 'd["layout"]' chat-only
ckeys p
wait_toml "$CLAUDE_REC" 'd["layout"]' dual
ckeys e
palette "chat: send region"
poll 15 "a second paste holding the unsaved text" pastes_ok 2 "$PROMPT" "$PROMPT$UNSAVED"
shot chat-toggled
frame_has_ui chat-toggled
say "smoke: layout went chat-only and back to dual; the second send held the unsaved text"

# ---------------------------------------------------------------------------------------------
step "Chats 4: a human-only artifact, close, open: --resume, and context.md without it"
wkeys h
choose prorate
choose ledger-draft
assert_file "$TASK_DIR/artifacts.toml" 'human-only'
[ "$(visibilities "$TASK_DIR" | grep -c 'ledger-draft.md=human-only' || true)" = 1 ] || fail "ledger-draft.md is not human-only: $(cat "$TASK_DIR/artifacts.toml")"
# chat: close asks first while the CLI is mid-turn (output in the last two seconds); wait that out.
sleep 3
ckeys k
poll 10 "the fake claude to end after chat: close" fake_claude_gone
[ -f "$CLAUDE_REC" ] || fail "chat: close removed the record"
cpick o
choose "invoice lines"
poll 10 "claude --resume in the log" log_has "claude arg --resume"
log_has "claude arg $CLAUDE_SESSION" || fail "the resume did not carry the session id $CLAUDE_SESSION"
poll 10 "the fake claude to run again" fake_claude_up
assert_file "$RUNTIME_DIR/context.md" 'design-notes\.md'
if grep -q 'ledger-draft' "$RUNTIME_DIR/context.md"; then fail "the regenerated context.md names the hidden file: $(cat "$RUNTIME_DIR/context.md")"; fi
# Nothing generated, no argument and no variable the fake saw names it either.
if grep -rq 'ledger-draft' "$RUNTIME_DIR"; then fail "a launch file of the chat names the hidden file"; fi
if grep -q 'ledger-draft' "$FAKE_CLI_LOG"; then fail "the fake CLI's log names the hidden file: $(grep ledger-draft "$FAKE_CLI_LOG")"; fi
cpick v
shot chat-artifacts
frame_has_ui chat-artifacts
say "smoke: resumed with --resume $CLAUDE_SESSION; context.md lacks the hidden file; the artifacts tab: $RUN/chat-artifacts.png"

# ---------------------------------------------------------------------------------------------
step "Chats 5: a chat-only codex plan chat on the project, quit, relaunch, resume"
wkeys shift+p
choose billing
cpick n
choose codex
choose "plan:"
wait_for_settled
poll 5 "the fake codex to start" log_has "codex start"
poll 15 "a codex record in $PROJECT_DIR/chats" have_agent_record "$PROJECT_DIR" codex
CODEX_REC=$(record_of_agent "$PROJECT_DIR" codex)
CODEX_ID=$(toml "$CODEX_REC" 'd["id"]')
CODEX_NAME=$(toml "$CODEX_REC" 'd["name"]')
wait_toml "$CODEX_REC" 'd["layout"]' chat-only
wait_toml "$CODEX_REC" 'd["type"]' plan
same_dir "$(logged_cwd codex)" "$REPO" || fail "the fake codex ran in '$(logged_cwd codex)', not in the repository $REPO"
poll 30 "session_id in the codex record" has_session "$CODEX_REC"
CODEX_SESSION=$(toml "$CODEX_REC" 'd["session_id"]')
log_has "codex session $CODEX_SESSION" || fail "the record's session id $CODEX_SESSION is not the one the fake codex wrote: $(grep '^codex session' "$FAKE_CLI_LOG")"
fake_claude_up || fail "the fake claude is not running before the quit"
quit_with_prompt quit-prompt-two
launch_drawn
dump_list relaunched
expect_section "$PROMPT_NAME" open
expect_section "$CODEX_NAME" open
row_has "$PROMPT_NAME" 'state: closed' || fail "the claude chat is not closed after the relaunch: $(cat "$CHAT_VIEW")"
row_has "$CODEX_NAME" 'state: closed' || fail "the codex chat is not closed after the relaunch: $(cat "$CHAT_VIEW")"
row_has "$CODEX_NAME" 'layout: chat-only' || fail "the codex row does not read chat-only: $(cat "$CHAT_VIEW")"
palette "chat: open any"
choose "$CODEX_NAME"
poll 10 "codex resume $CODEX_SESSION in the log" grep -qE "^codex resume $CODEX_SESSION( |\$)" "$FAKE_CLI_LOG"
say "smoke: both chats listed after the relaunch; the codex chat resumed with codex resume $CODEX_SESSION (the quit prompt: $RUN/quit-prompt-two.png)"

# ---------------------------------------------------------------------------------------------
step "Chats 6: C-c c f forks the project chat into the task as a labeled handoff"
cpick f
choose prorate
poll 20 "a record with parent $CODEX_ID in $TASK_DIR/chats" have_child_record "$TASK_DIR" "$CODEX_ID"
FORK_REC=$(record_with_parent "$TASK_DIR" "$CODEX_ID")
FORK_NAME=$(toml "$FORK_REC" 'd["name"]')
wait_toml "$FORK_REC" 'd["fork"]' handoff
wait_toml "$FORK_REC" 'd["type"]' implement
wait_toml "$FORK_REC" 'd["agent"]' codex
# No prompts_file: the chat points at the task's own prompts.txt, which is what its right pane shows.
wait_toml "$FORK_REC" '"prompts_file" in d' False
same_dir "$(toml "$FORK_REC" 'd["cwd"]')" "$WORKTREE" || fail "the fork's cwd is not the task's worktree: $(cat "$FORK_REC")"
assert_file "$TASK_DIR/prompts.txt"
if grep -q '^codex arg fork$' "$FAKE_CLI_LOG"; then fail "the fake codex was asked to fork: $(cat "$FAKE_CLI_LOG")"; fi
dump_list forked
expect_section "$FORK_NAME" open
row_has "$FORK_NAME" 'handoff' || fail "the fork's row is not labeled handoff: $(cat "$CHAT_VIEW")"
row_has "$FORK_NAME" "forked as new chat with handoff from $CODEX_NAME" || fail "the fork's row does not name its parent: $(cat "$CHAT_VIEW")"
row_has "$CODEX_NAME" 'forks: 1' || fail "the parent's row does not count the fork: $(cat "$CHAT_VIEW")"
shot chat-handoff
frame_has_ui chat-handoff
say "smoke: handoff fork under the task, parent $CODEX_ID, no prompts_file ($RUN/chat-handoff.png)"

# ---------------------------------------------------------------------------------------------
step "Chats 7: archive and restore move the record between the lists and keep it"
# The fork is the chat the panel shows; the list dump above left the list showing, so open it again.
palette "chat: open any"
choose "$FORK_NAME"
ckeys a
wait_toml "$FORK_REC" 'd["archived"]' True
[ -f "$FORK_REC" ] || fail "chat: archive removed the record"
dump_list archived
expect_section "$FORK_NAME" archived
# chat: restore lists the archived chats of the focused task, and the project is focused since step 5.
wkeys f
choose prorate
focus_editor
cpick u
choose "$FORK_NAME"
wait_toml "$FORK_REC" 'd["archived"]' False
[ -f "$FORK_REC" ] || fail "chat: restore lost the record"
dump_list restored
expect_section "$FORK_NAME" open
say "smoke: the fork went to the archive and came back; its record never left $FORK_REC"

# ---------------------------------------------------------------------------------------------
step "Chats 8: the fake claude rings the bell and the chat reads waiting"
palette "chat: open any"
choose "invoice lines"
poll 10 "the fake claude to run again" fake_claude_up
ckeys t
type_text bell
keys Return
dump_chat_view bell
sed -n 2p "$CHAT_VIEW" | grep -qE ' - claude - implement - waiting$' || fail "the shown chat does not read waiting after the bell: $(cat "$CHAT_VIEW")"
dump_list bell-list
row_has "$PROMPT_NAME" 'state: waiting' || fail "the claude row does not read waiting: $(cat "$CHAT_VIEW")"
shot chat-waiting
frame_has_ui chat-waiting
say "smoke: the bell shows as waiting in the header and the row ($RUN/chat-waiting.png)"
quit_with_prompt quit-prompt-final

# ---------------------------------------------------------------------------------------------
step "Chats 9: no network"
# Every launch of the app makes two kinds of request that the base flow shows too: a CONNECT to the host
# of REGISTRY_URL (the ACP agent registry fetch; the host is read from that source file rather than
# named here, so check-hosts keeps one allow entry for it) and GET localhost (the local model
# providers asking a server on this machine for its models). Neither comes from chats. Anything else
# the scenario sent through the proxy is a failure: the fake CLIs never reach it, and chats call no host.
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
chat_requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
stray_requests=$(printf '%s\n' "$chat_requests" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
if [ -n "$stray_requests" ]; then
  echo "bongos: the chats scenario reached the proxy for other hosts:" >&2
  printf '%s\n' "$stray_requests" | sort | uniq -c >&2
  exit 1
fi
say "smoke: no request beyond the app's startup ones ($(printf '%s' "$chat_requests" | grep -c . || true) requests, all to the ACP registry or localhost)"

cp "$RUN/config-backup/settings-04.json" "$SETTINGS_FILE"
