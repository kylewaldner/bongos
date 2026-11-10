# Scenario for feature 25-prompt-editor. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh. Debug builds only: it reads the prompts view through
# `prompts: dump view` (<logs>/prompts-view.txt) and the Chats panel through `chat: dump view`.
#
# Setup, as scenario 04 does: a git repository /tmp/prompt-src/billing with one commit,
# `"base_keymap": "Emacs"` (the C-c keys exist only there), `work.repositories` naming it, and
# `chats.agents.claude.command` naming a wrapper script around the fake Claude of
# crates/bongos_chat/test-fixtures. The project and its task are made with the Work commands.
# `prompts.enabled` is true by default, so settings.json holds nothing about the prompts.
# State is checked through files (prompts.txt, the chat record, $FAKE_CLI_LOG, the fake's
# byte-for-byte input log) and `prompts: dump view`. What no file or dump shows (the rules) is saved
# as a screenshot for the orchestrator to read.
#   1. The task's prompts.txt holds three markers (one sent). C-c c n, claude, implement: the dump
#      shows three rules, no marker text in the display, "prompt 3/3"; prompts-dual.png shows them.
#   2. A four-paragraph prompt with a bulleted list, C-x C-s, C-c C-c: the fake logs one paste equal to
#      the block and one Enter; the block's marker has `| sent`; point is in a new block.
#   3. C-c C-p twice, C-c C-w, C-c C-n, C-y, an edit, C-c C-c: the earlier block still reads sent
#      with its own stamps, the new one is sent with the edit.
#   4. Two selected lines, C-c C-r: the fake logs exactly them; no new sent stamp.
#   5. C-c C-a, then C-c C-c on a new block: the record has auto_submit = false; the fake logs a paste
#      and no Enter; the mode line reads "auto-submit off".
#   6. C-c C-u to the project's file: the record has prompts_file; quit, relaunch, open the chat: the
#      pane shows the project's file. C-c C-d: the field is gone.
#   7. The terminal pane focused, `hello`, C-c C-r and C-c C-w: the fake logs all as typed input and the
#      task's prompts.txt is unchanged (C-c C-n is not usable there, see the comment in the step).
#   8. No request through the proxy beyond the two kinds every launch makes (as in scenario 03).
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 25-prompt-editor skipped under BONGOS_STOCK=1: the stock run has no prompt editor"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
PROMPTS_VIEW="$LOGS_DIR/prompts-view.txt"
REPO=/tmp/prompt-src/billing
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/billing"
TASK_DIR="$PROJECT_DIR/tasks/prorate-invoices"
TASK_PROMPTS="$TASK_DIR/prompts.txt"
PROJECT_PROMPTS="$PROJECT_DIR/prompts.txt"
WORKTREE="$REPO/.bongos-worktrees/prorate-invoices"
FIXTURES="$BONGOS_ROOT/crates/bongos_chat/test-fixtures"
CHAT_BIN="$RUN/chat-bin"
FAKE_CLAUDE_INPUT="$RUN/fake-claude-input.txt"
export FAKE_CLI_LOG="$RUN/fake-cli.log"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

SEED_PENDING_ONE="Seed one."
SEED_SENT="Seed two body."
SEED_STAMP_SENT="2026-11-01 09:30"
# The four paragraphs; the second holds a bulleted list. Each line ends in a period or a colon.
PROMPT=$'Add proration to the invoice lines.\n\nCover these cases:\n- credits.\n- refunds.\n\nKeep the public API unchanged.\n\nFinish with unit tests.'
PROMPT_NAME="Add proration to the invoice lines."
EDIT=" Then run the tests."
REGION=$'Second line.\nThird line.'
QUIET="Quiet prompt."
PROJECT_LINE="Project wide prompt."

for tool in git python3 realpath pgrep; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# --- Input -----------------------------------------------------------------------------------

wkeys() { act keys ctrl+c w "$@"; }

# C-c c <keys> for a command that opens a picker.
cpick() { act keys ctrl+c c "$@"; }

# C-c c <keys> for a command that changes no screen for certain: send, then wait until it is quiet.
ckeys() {
  keys ctrl+c c "$@"
  wait_for_settled
}

# C-c C-<letter> in the prompts editor, then wait until the screen is quiet.
pkeys() {
  keys ctrl+c "ctrl+$1"
  wait_for_settled
}

# C-c C-n.
new_block() {
  pkeys n
}

# Types text that has newlines: xdotool type drops them here, so each line is typed and Return is
# pressed between lines. Each line is typed only when it is not empty.
type_lines() {
  local text="$1" line first=1
  while IFS= read -r line || [ -n "$line" ]; do
    [ "$first" = 1 ] || keys Return
    first=0
    # `--` because a bulleted line starts with a dash, which xdotool would take for an option.
    [ -z "$line" ] || xdo type --delay 40 -- "$line"
  done <<<"$text"
}

choose() {
  act type_text "$1"
  act keys Return
}

# Runs a command by its palette name, for the commands with no key.
palette() {
  act keys ctrl+shift+p
  choose "$1"
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

# Quits with a chat running: ctrl+q asks, the first button is Quit and Return takes it.
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

# --- Reading files ----------------------------------------------------------------------------

toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 15 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

record_of_agent() {
  python3 -I -c 'import glob, sys, tomllib
for path in sorted(glob.glob(sys.argv[1] + "/chats/*.toml")):
    if tomllib.load(open(path, "rb")).get("agent") == sys.argv[2]:
        print(path)
        break' "$1" "$2"
}

have_agent_record() { [ -n "$(record_of_agent "$1" "$2")" ]; }

# The blocks of a prompts file, one line each: "<created>|<sent stamp or ->|<body as JSON>". A marker
# is "=== prompt <stamp> ===" or "=== prompt <stamp> | sent <stamp> ===" on a row of its own; the body
# is the rows up to the next marker without blank rows at its edges (what a send takes).
blocks_of() {
  python3 -I -c 'import json, re, sys
marker = re.compile(r"^=== prompt (\d{4}-\d\d-\d\d \d\d:\d\d)(?: \| sent (\d{4}-\d\d-\d\d \d\d:\d\d))? ===$")
blocks, current = [], None
for line in open(sys.argv[1]).read().split("\n"):
    found = marker.match(line)
    if found:
        current = [found.group(1), found.group(2) or "-", []]
        blocks.append(current)
    elif current is not None:
        current[2].append(line)
for created, sent, rows in blocks:
    while rows and not rows[0].strip():
        rows.pop(0)
    while rows and not rows[-1].strip():
        rows.pop()
    print(created + "|" + sent + "|" + json.dumps("\n".join(rows)))' "$1"
}

# The line of blocks_of for the block whose body is <text>; empty when there is none.
block_with() {
  python3 -I -c 'import json, sys
for line in sys.stdin.read().splitlines():
    created, sent, body = line.split("|", 2)
    if json.loads(body) == sys.argv[1]:
        print(created + "|" + sent)' "$2" <<<"$(blocks_of "$1")"
}

sent_count() { blocks_of "$1" | awk -F'|' '$2 != "-"' | wc -l; }

block_count() { blocks_of "$1" | wc -l; }

block_is_sent() { local line; line=$(block_with "$1" "$2"); [ -n "$line" ] && [ "${line#*|}" != "-" ]; }

wait_sent() { poll 15 "the block '${2%%$'\n'*}' to be sent in $1" block_is_sent "$1" "$2"; }

# --- Reading what the fake CLI did -------------------------------------------------------------

log_has() { [ -f "$FAKE_CLI_LOG" ] && grep -qxF -- "$1" "$FAKE_CLI_LOG"; }

logged_cwd() { grep "^$1 cwd " "$FAKE_CLI_LOG" | tail -n1 | cut -d' ' -f3-; }

same_dir() { [ "$(realpath "$1")" = "$(realpath "$2")" ]; }

# pastes_are <expectation>...: the fake claude's input holds exactly these bracketed pastes, in order,
# and nothing else. An expectation is "E:<text>" (the paste, then one Enter) or "N:<text>" (the paste,
# and no Enter).
pastes_are() {
  [ -f "$FAKE_CLAUDE_INPUT" ] || return 1
  python3 -I -c 'import sys
data = bytes(int(token, 8) for token in open(sys.argv[1]).read().split())
start, end = b"\x1b[200~", b"\x1b[201~"
got, position = [], 0
while True:
    first = data.find(start, position)
    if first < 0:
        break
    last = data.find(end, first)
    if last < 0:
        sys.exit(1)
    following = data.find(start, last)
    tail = data[last + len(end):following if following >= 0 else len(data)]
    pasted = data[first + len(start):last].decode().replace("\r\n", "\n").replace("\r", "\n")
    got.append(("E:" if tail == b"\r" else "N:" if tail == b"" else "?:") + pasted)
    position = last
sys.exit(0 if got == sys.argv[2:] else 1)' "$FAKE_CLAUDE_INPUT" "$@"
}

# 0 when the fake claude's input ends with the bytes of <text> (a string with \\ooo octal escapes allowed).
input_ends_with() {
  [ -f "$FAKE_CLAUDE_INPUT" ] || return 1
  python3 -I -c 'import sys
data = bytes(int(token, 8) for token in open(sys.argv[1]).read().split())
sys.exit(0 if data.endswith(sys.argv[2].encode().decode("unicode_escape").encode("latin-1")) else 1)' "$FAKE_CLAUDE_INPUT" "$1"
}

# --- Reading the prompts view -----------------------------------------------------------------

dump_prompts() {
  rm -f "$PROMPTS_VIEW"
  palette "prompts: dump view"
  assert_file "$PROMPTS_VIEW"
  cp "$PROMPTS_VIEW" "$RUN/prompts-view-$1.txt"
}

mode_line_of_dump() { sed -n 's/^mode line: //p' "$PROMPTS_VIEW"; }

expect_mode_line() {
  local line
  line=$(mode_line_of_dump)
  grep -qE -- "$1" <<<"$line" || fail "the mode line reads '$line', expected a match for $1: $(cat "$PROMPTS_VIEW")"
}

# ---------------------------------------------------------------------------------------------
step "Prompts setup: a repository, the fake claude, base_keymap Emacs, work settings"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$RUN/config-backup" "$CHAT_BIN"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-25.json"
: >"$FAKE_CLI_LOG"
cat >"$CHAT_BIN/claude" <<SH
#!/bin/sh
FAKE_CLI_LOG='$FAKE_CLI_LOG'
FAKE_CLI_INPUT='$FAKE_CLAUDE_INPUT'
export FAKE_CLI_LOG FAKE_CLI_INPUT
exec '$FIXTURES/fake-claude' "\$@"
SH
chmod +x "$CHAT_BIN/claude"
mkdir -p /tmp/prompt-src
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

wkeys p n
choose Billing
assert_file "$PROJECT_DIR/task.toml"
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
wkeys shift+n
choose billing
choose "Prorate invoices"
assert_file "$TASK_DIR/task.toml"
TASK_ID=$(toml "$TASK_DIR/task.toml" 'd["id"]')
wkeys s
choose prorate
poll 10 "the worktree $WORKTREE within ten seconds" test -d "$WORKTREE"
wkeys f
choose prorate
focus_editor

# ---------------------------------------------------------------------------------------------
step "Prompts 1: three markers, one sent, show as three rules with no marker text"
# The last block is empty, and C-c C-f twice puts point in it, so the typing of check 2 lands there.
cat >"$TASK_PROMPTS" <<SEED
=== prompt 2026-11-01 09:00 ===
$SEED_PENDING_ONE
=== prompt $SEED_STAMP_SENT | sent 2026-11-01 09:45 ===
$SEED_SENT
=== prompt 2026-11-02 08:00 ===
SEED
cpick n
choose claude
choose "implement:"
wait_for_settled
poll 15 "a claude record in $TASK_DIR/chats" have_agent_record "$TASK_DIR" claude
CLAUDE_REC=$(record_of_agent "$TASK_DIR" claude)
poll 2 "the fake claude to start within two seconds" log_has "claude start"
poll 2 "BONGOS_TASK_ID in the log" log_has "claude env BONGOS_TASK_ID=$TASK_ID"
same_dir "$(logged_cwd claude)" "$WORKTREE" || fail "the fake claude ran in '$(logged_cwd claude)', not in $WORKTREE"
wait_toml "$CLAUDE_REC" 'd["layout"]' dual
[ "$(block_count "$TASK_PROMPTS")" = 3 ] || fail "the chat start changed the seeded prompts.txt: $(cat "$TASK_PROMPTS")"
ckeys e
# The file opens with point in its first block; C-c C-f twice goes to the last, the empty one.
pkeys f
pkeys f
dump_prompts seeded
grep -qx 'rules: 3' "$PROMPTS_VIEW" || fail "the dump does not show three rules: $(cat "$PROMPTS_VIEW")"
if grep -q '=== prompt' "$PROMPTS_VIEW"; then fail "the display still holds marker text: $(cat "$PROMPTS_VIEW")"; fi
grep -qF "$SEED_PENDING_ONE" "$PROMPTS_VIEW" || fail "the display lost a body: $(cat "$PROMPTS_VIEW")"
expect_mode_line '^prompt 3/3 · pending · auto-submit on$'
shot prompts-dual
frame_has_ui prompts-dual
say "smoke: three rules, no marker text, '$(mode_line_of_dump)' (the rules: $RUN/prompts-dual.png)"

# ---------------------------------------------------------------------------------------------
step "Prompts 2: a four-paragraph prompt, C-x C-s, C-c C-c: one paste, one Enter, marked sent"
type_lines "$PROMPT"
keys ctrl+x ctrl+s
wait_for_settled
poll 15 "the typed prompt saved in $TASK_PROMPTS" block_with "$TASK_PROMPTS" "$PROMPT"
pkeys c
poll 15 "one bracketed paste of the prompt and one Enter" pastes_are "E:$PROMPT"
wait_sent "$TASK_PROMPTS" "$PROMPT"
[ "$(sent_count "$TASK_PROMPTS")" = 2 ] || fail "expected two sent blocks: $(cat "$TASK_PROMPTS")"
wait_toml "$CLAUDE_REC" 'd["name"]' "$PROMPT_NAME"
# The send saved the file by itself (prompts.save_on_send): the marker is on disk without a second save.
dump_prompts after-send
grep -qx 'rules: 4' "$PROMPTS_VIEW" || fail "point did not get a new block (expected four rules): $(cat "$PROMPTS_VIEW")"
expect_mode_line '^prompt 4/4 · pending · auto-submit on$'
say "smoke: one paste equal to the block and one Enter; its marker reads sent; point is in block 4 of 4"

# ---------------------------------------------------------------------------------------------
step "Prompts 3: previous twice, copy, new, yank, edit, send: the earlier block is untouched"
SENT_BEFORE=$(block_with "$TASK_PROMPTS" "$SEED_SENT")
pkeys p
pkeys p
pkeys w
pkeys n
keys ctrl+y
wait_for_settled
type_text "$EDIT"
wait_for_settled
pkeys c
poll 15 "the second paste and its Enter" pastes_are "E:$PROMPT" "E:$SEED_SENT$EDIT"
wait_sent "$TASK_PROMPTS" "$SEED_SENT$EDIT"
[ "$(block_with "$TASK_PROMPTS" "$SEED_SENT")" = "$SENT_BEFORE" ] || fail "the earlier block changed: was '$SENT_BEFORE', now '$(block_with "$TASK_PROMPTS" "$SEED_SENT")'"
block_is_sent "$TASK_PROMPTS" "$SEED_SENT" || fail "the earlier block does not read sent: $(cat "$TASK_PROMPTS")"
[ "$(sent_count "$TASK_PROMPTS")" = 3 ] || fail "expected three sent blocks: $(cat "$TASK_PROMPTS")"
say "smoke: the earlier block still reads sent ($SENT_BEFORE); the copy with the edit was sent as a new block"

# ---------------------------------------------------------------------------------------------
step "Prompts 4: C-c C-r sends the two selected lines and stamps nothing"
new_block
type_lines $'First line.\nSecond line.\nThird line.'
# From the end of the third line: up, to the start of the second, then down one and to the end.
keys Up Home shift+Down shift+End
wait_for_settled
pkeys r
poll 15 "the region paste and its Enter" pastes_are "E:$PROMPT" "E:$SEED_SENT$EDIT" "E:$REGION"
[ "$(sent_count "$TASK_PROMPTS")" = 3 ] || fail "a region send stamped a block sent: $(cat "$TASK_PROMPTS")"
say "smoke: the fake got exactly the two selected lines; no new sent stamp"

# ---------------------------------------------------------------------------------------------
step "Prompts 5: C-c C-a turns auto-submit off for the chat; a send pastes and does not press Enter"
pkeys a
wait_toml "$CLAUDE_REC" 'd.get("auto_submit")' False
new_block
type_text "$QUIET"
wait_for_settled
pkeys c
poll 15 "a paste with no Enter" pastes_are "E:$PROMPT" "E:$SEED_SENT$EDIT" "E:$REGION" "N:$QUIET"
wait_sent "$TASK_PROMPTS" "$QUIET"
dump_prompts auto-off
expect_mode_line 'auto-submit off$'
say "smoke: record auto_submit = false; the fake got a paste and no Enter; the mode line reads '$(mode_line_of_dump)'"

# ---------------------------------------------------------------------------------------------
step "Prompts 6: C-c C-u to the project's file survives a restart; C-c C-d goes back"
printf '%s\n' "$PROJECT_LINE" >"$PROJECT_PROMPTS"
keys ctrl+x ctrl+s
wait_for_settled
pkeys u
act type_text project
act keys Return
wait_toml "$CLAUDE_REC" '"prompts_file" in d' True
same_dir "$(toml "$CLAUDE_REC" 'd["prompts_file"]')" "$PROJECT_PROMPTS" || fail "the record's prompts_file is not the project's file: $(cat "$CLAUDE_REC")"
CHAT_NAME=$(toml "$CLAUDE_REC" 'd["name"]')
quit_with_prompt
launch_drawn
palette "chat: open any"
choose "$CHAT_NAME"
poll 10 "claude --resume in the log" log_has "claude arg --resume"
wait_for_settled
ckeys e
dump_prompts project-file
grep -qF "$PROJECT_LINE" "$PROMPTS_VIEW" || fail "the pane does not show the project's file after the restart: $(cat "$PROMPTS_VIEW")"
shot prompts-project-file
frame_has_ui prompts-project-file
pkeys d
wait_toml "$CLAUDE_REC" '"prompts_file" in d' False
dump_prompts default-file
if grep -qF "$PROJECT_LINE" "$PROMPTS_VIEW"; then fail "the pane still shows the project's file after use default: $(cat "$PROMPTS_VIEW")"; fi
grep -qF "$QUIET" "$PROMPTS_VIEW" || fail "the pane does not show the task's file after use default: $(cat "$PROMPTS_VIEW")"
say "smoke: the record had prompts_file, the project's file showed after the restart ($RUN/prompts-project-file.png), use default removed the field"

# ---------------------------------------------------------------------------------------------
step "Prompts 7: in the terminal pane, the prompts keys are typed input"
BLOCKS_BEFORE=$(block_count "$TASK_PROMPTS")
ckeys t
type_text hello
# C-c C-r (send region) and C-c C-w (copy) are prompts keys too. C-c C-n is not used here: the Emacs
# keymap leaves the Workspace binding of ctrl-n (new file) in the terminal, so it never reaches the CLI.
keys ctrl+c ctrl+r
keys ctrl+c ctrl+w
wait_for_settled
poll 10 "hello, C-c C-r and C-c C-w as typed input" input_ends_with 'hello\003\022\003\027'
[ "$(block_count "$TASK_PROMPTS")" = "$BLOCKS_BEFORE" ] || fail "a prompts key in the terminal changed prompts.txt: $(cat "$TASK_PROMPTS")"
[ "$(sent_count "$TASK_PROMPTS")" = 4 ] || fail "a prompts key in the terminal stamped a block: $(cat "$TASK_PROMPTS")"
shot terminal-keys
frame_has_ui terminal-keys
say "smoke: the fake got hello, C-c C-r and C-c C-w as typed bytes; prompts.txt is unchanged"
quit_with_prompt

# ---------------------------------------------------------------------------------------------
step "Prompts 8: no network"
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
prompt_requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
stray_requests=$(printf '%s\n' "$prompt_requests" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
if [ -n "$stray_requests" ]; then
  echo "bongos: the prompt editor scenario reached the proxy for other hosts:" >&2
  printf '%s\n' "$stray_requests" | sort | uniq -c >&2
  exit 1
fi
say "smoke: no request beyond the app's startup ones ($(printf '%s' "$prompt_requests" | grep -c . || true) requests, all to the ACP registry or localhost)"

cp "$RUN/config-backup/settings-25.json" "$SETTINGS_FILE"
