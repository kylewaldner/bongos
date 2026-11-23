# Scenario for feature 37-chat-runtime-channels. Sourced by script/bongos/smoke after the base flow
# (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, XDG_RUNTIME_DIR, the proxy and the helpers of lib.sh. Debug builds only: it reads the Chats
# panel through `chat: dump view` (<logs>/chat-view.txt), whose shown chat has the lines
# `state: <state> (<exact|estimated: reason>)` and `channel: off|pending|live`.
#
# Setup: `"base_keymap": "Emacs"` (the C-c w and C-c c keys exist only there), `chats.channels` left
# at its default (true), and `chats.agents.claude.command` naming a wrapper made here. The wrapper
# answers probes with crates/bongos_chat/test-fixtures/fake-claude and, for a session, uses that
# fixture's hook poller (fake-session.sh: a marker file named after a hook event, written into the
# chat's event directory, runs the hooks of the `--settings` file bongos passed, with the marker's
# text in the hook input's prompt and message fields). The fixture reads its terminal one byte per
# process, about 4 ms a byte, so a 1 MiB paste would take over an hour; the wrapper reads the
# terminal with a small Python reader instead (fast-reader.py, written below) that logs the same
# byte-for-byte input format, runs the fixture's commands (bell, title, spin, exit), and, like Claude,
# fires `UserPromptSubmit` when Enter follows a bracketed paste. Each launch writes its BONGOS_*
# environment, its arguments and its own event directory to files under $RUN, keyed by chat id, so
# two chats running at once never share a marker.
#   1. A `thinking` chat (dual) on a project: after its SessionStart hook the dump reads `channel: live`
#      and `state: idle (exact)`; the report socket is mode 600 in a mode 700 directory.
#   2. chat: send region with a 1 MiB prompts.txt: one bracketed paste and one Enter reach the fake;
#      its UserPromptSubmit hook makes the state `running (exact)`, a Stop hook `idle (exact)`.
#   3. Notification with permission_prompt: `waiting (exact)`; PostToolUse: `running (exact)`.
#   4. Idle again, the fake sets a spinner title every 10 ms for at least 20 s: the record's mtime does
#      not change. The spinner is asked for through a file, not typed, since typing is activity
#      that bongos writes whatever the state.
#   5. Two Claude chats (`plan`, `free`) in the project directory, started back to back: the second
#      starts while the first is running and before any report from it, each record holds the
#      session id its own launch was given, and C-c c k then C-c c o on each resumes with that id.
#      Codex: deferred.
#   6. A wrapper whose fake reports 2.1.300, below the hooks row: the chat's state is estimated with a
#      reason naming 2.1.300, its channel is off, and its list row carries the estimated tag.
#   7. `bongos --chat-report state waiting` with chat A's token and chat B's id leaves B idle (exact);
#      the same with B's own token makes B waiting (exact), so the path itself works.
#   8. The marker text of every hook input is in neither the bongos log nor the app's output, and no
#      request went through the proxy beyond the two kinds every launch makes (as in scenario 04).
# The tight time bounds (2 s to live, 1 s per change) are the runtime tests'; here every wait
# is a condition poll with a generous hang guard, and the measured latencies are printed.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 37-chat-runtime-channels skipped under BONGOS_STOCK=1: the stock run has no Chats view"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
CHAT_VIEW="$LOGS_DIR/chat-view.txt"
WORK_ROOT="$HOME/.bongos"
PROJECT_DIR="$WORK_ROOT/projects/relay"
FIXTURES="$BONGOS_ROOT/crates/bongos_chat/test-fixtures"
CHAT_BIN="$RUN/chat-bin"
EVENTS="$RUN/chat-events"
CHAT_ENV="$RUN/chat-env"
LAUNCHES="$RUN/chat-launches"
INPUTS="$RUN/chat-input"
export FAKE_CLI_LOG="$RUN/fake-cli.log"
# The text the fake puts in every hook input's content fields; it must never reach bongos's log.
MARKER="marker-37-hook-content-text"
SEQ=0

for tool in python3 pgrep stat; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# --- Input -----------------------------------------------------------------------------------

wkeys() { act keys ctrl+c w "$@"; }

cpick() { act keys ctrl+c c "$@"; }

ckeys() {
  keys ctrl+c c "$@"
  wait_for_settled
}

choose() {
  act type_text "$1"
  act keys Return
}

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

# --- Records ----------------------------------------------------------------------------------

toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 15 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

# The first record in <scope dir>/chats for which the python expression over d (the record) holds.
record_where() {
  python3 -I -c 'import glob, sys, tomllib
for path in sorted(glob.glob(sys.argv[1] + "/chats/*.toml")):
    d = tomllib.load(open(path, "rb"))
    if eval(sys.argv[2]):
        print(path)
        break' "$1" "$2"
}

have_record() { [ -n "$(record_where "$1" "$2")" ]; }

has_session() { [ -n "$(toml "$1" 'd.get("session_id", "")' 2>/dev/null)" ]; }

same_dir() { [ "$(realpath "$1")" = "$(realpath "$2")" ]; }

mtime_ns() { python3 -I -c 'import os, sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$1"; }

# 0 when <file> has not changed for <seconds>: the 10 s debounce of last_active writes is over.
record_quiet() {
  python3 -I -c 'import os, sys, time
sys.exit(0 if time.time() - os.stat(sys.argv[1]).st_mtime >= float(sys.argv[2]) else 1)' "$1" "$2"
}

# --- What the fake did ------------------------------------------------------------------------

# The value of <name> in the environment of chat <id>'s current launch.
chat_env() { sed -n "s/^$2=//p" "$CHAT_ENV/$1" 2>/dev/null; }

# The number of launches of chat <id>, and the arguments of the last one.
launch_count() { if [ -f "$LAUNCHES/$1" ]; then wc -l <"$LAUNCHES/$1"; else echo 0; fi; }
last_launch() { tail -n1 "$LAUNCHES/$1" | cut -d' ' -f2-; }
launch_time() { sed -n "${2}p" "$LAUNCHES/$1" | cut -d' ' -f1; }

launched_times() { [ "$(launch_count "$1")" -ge "$2" ]; }

reader_up() { pgrep -f "fast-reader.py $1" >/dev/null; }
reader_gone() { ! reader_up "$1"; }

log_has() { [ -f "$FAKE_CLI_LOG" ] && grep -qxF -- "$1" "$FAKE_CLI_LOG"; }

# fire <chat id> <Event> [line...]: writes a marker for the hook <Event> into the chat's current
# event directory (written under another name, then renamed, so the poller never reads half of it).
# The lines set source= or notification_type=; the marker text fills the content fields.
fire() {
  local id=$1 event=$2 dir
  shift 2
  dir=$(chat_env "$id" FAKE_EVENT_DIR)
  [ -n "$dir" ] || fail "no event directory for chat $id"
  mkdir -p "$dir"
  SEQ=$((SEQ + 1))
  { printf '%s\n' "$@"; printf '%s\n' "$MARKER"; } >"$dir/.marker-$SEQ"
  mv "$dir/.marker-$SEQ" "$dir/$SEQ-$event"
  FIRED="$dir/$SEQ-$event"
  FIRED_AT=$(date +%s%N)
}

# Waits until the poller ran the hooks of the last marker and removed it; prints the milliseconds.
wait_fired() {
  poll 15 "the fake to run the $1 hooks" test ! -e "$FIRED"
  FIRED_MS=$((($(date +%s%N) - FIRED_AT) / 1000000))
}

# pastes_ok <input file> <count> <text file>: the input holds exactly <count> bracketed pastes, the
# last one's text is the file's text, and one Enter (and nothing else) follows each.
pastes_ok() {
  [ -f "$1" ] || return 1
  python3 -I -c 'import sys
data = bytes(int(token, 8) for token in open(sys.argv[1]).read().split())
want = open(sys.argv[3]).read().rstrip("\n")
start, end = b"\x1b[200~", b"\x1b[201~"
count = int(sys.argv[2])
if data.count(start) != count or data.count(end) != count:
    sys.exit(1)
position = 0
for index in range(count):
    first = data.find(start, position)
    last = data.find(end, first)
    following = data.find(start, last)
    tail = data[last + len(end):following if following >= 0 else len(data)]
    if tail != b"\r":
        sys.exit(1)
    pasted = data[first + len(start):last].decode().replace("\r\n", "\n").replace("\r", "\n")
    if index == count - 1 and pasted.rstrip("\n") != want:
        sys.exit(1)
    position = last
' "$@"
}

# --- The Chats view ---------------------------------------------------------------------------

dump_chat_view() {
  rm -f "$CHAT_VIEW"
  # A report just applied redraws the header; opening the palette while it does would let `act`
  # take that redraw for the palette and type into the chat.
  wait_for_settled
  palette "chat: dump view"
  assert_file "$CHAT_VIEW"
  cp "$CHAT_VIEW" "$RUN/chat-view-$1.txt"
}

view_has() { grep -qxE -- "$1" "$CHAT_VIEW"; }

# dump_until <label> <line regex>...: dumps the shown chat until every regex matches a whole line;
# the reports are applied on the main thread after the hook returns, so one dump can be early.
dump_until() {
  local label=$1 deadline=$((SECONDS + 30)) pattern ok
  shift
  while :; do
    dump_chat_view "$label"
    ok=1
    for pattern in "$@"; do
      view_has "$pattern" || ok=
    done
    [ -z "$ok" ] || return 0
    [ "$SECONDS" -lt "$deadline" ] || fail "the shown chat never read $*: $(cat "$CHAT_VIEW")"
  done
}

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

row_has() {
  local lines
  lines=$(row_lines "$1")
  grep -qE -- "$2" <<<"$lines"
}

# Starts a Claude chat of <type> on the focused project and prints nothing; the caller finds it.
new_claude_chat() {
  cpick n
  choose claude
  choose "$1:"
}

# ---------------------------------------------------------------------------------------------
step "Channels setup: the fake Claude wrapper, base_keymap Emacs, a project"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
mkdir -p "$RUN/config-backup" "$CHAT_BIN" "$EVENTS" "$CHAT_ENV" "$LAUNCHES" "$INPUTS"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-37.json"
: >"$FAKE_CLI_LOG"

cat >"$CHAT_BIN/fast-reader.py" <<'PY'
# Reads the chat terminal (raw mode, set by the wrapper) in blocks. Every byte is appended to
# $FAKE_CLI_INPUT as three octal digits and a space, as fake-session.sh does. Outside a bracketed
# paste each line is a command of the fixture (bell, title <t>, spin <n>, exit <code>, else answered
# `got <line>`); an Enter right after a paste is a submit, which fires UserPromptSubmit through the
# fixture's poller, as Claude would. Lines go to $FAKE_CLI_LOG as `claude <chat id> <what>`.
import os
import select
import sys
import time

chat = sys.argv[1]
log_path = os.environ.get("FAKE_CLI_LOG")
input_path = os.environ.get("FAKE_CLI_INPUT")
events = os.environ.get("FAKE_CLI_EVENT_DIR")
marker = os.environ.get("FAKE_MARKER_TEXT", "")
START, END = b"\x1b[200~", b"\x1b[201~"


def note(text):
    if log_path:
        with open(log_path, "a") as log:
            log.write(f"claude {chat} {text}\n")


def out(data):
    os.write(1, data)


def fire(event, number):
    if not events:
        return
    os.makedirs(events, exist_ok=True)
    temp = os.path.join(events, f".reader-{number}")
    with open(temp, "w") as marker_file:
        marker_file.write(marker + "\n")
    os.rename(temp, os.path.join(events, f"{9000 + number}-{event}"))


def spin(titles):
    note(f"spin start {time.time():.3f}")
    for count in range(1, titles + 1):
        out(b"\x1b]0;spinner %d\x07" % count)
        time.sleep(0.01)
    out(b"spun\r\n")
    note(f"spin done {time.time():.3f}")


def spin_requested():
    # A `.spin` file in the event directory (the poller skips dot files) holds a title count: the
    # spinner starts without any keyboard input, which bongos counts as activity.
    if not events:
        return
    path = os.path.join(events, ".spin")
    try:
        with open(path) as request:
            titles = int(request.read().strip())
    except (OSError, ValueError):
        return
    os.remove(path)
    spin(titles)


def command(line):
    text = line.decode(errors="replace")
    note(f"input {text}")
    if text == "bell":
        out(b"\x07")
    elif text.startswith("title "):
        out(b"\x1b]0;" + line[6:] + b"\x07")
    elif text.startswith("spin "):
        spin(int(text[5:]))
    elif text.startswith("exit "):
        note(f"exit {text[5:]}")
        sys.exit(int(text[5:]))
    else:
        out(b"got " + line + b"\r\n")


line = b""
pending = b""
in_paste = False
pasted = False
paste_bytes = 0
submits = 0


def ordinary(byte):
    global line, pasted, paste_bytes, submits
    if in_paste:
        paste_bytes += 1
    elif byte in (13, 10):
        if line:
            current, line = line, b""
            command(current)
        elif pasted:
            pasted = False
            submits += 1
            note("submit")
            fire("UserPromptSubmit", submits)
    else:
        line += bytes([byte])


while True:
    readable, _, _ = select.select([0], [], [], 0.1)
    spin_requested()
    if not readable:
        continue
    try:
        data = os.read(0, 65536)
    except OSError:
        data = b""
    if not data:
        note("exit eof")
        sys.exit(0)
    if input_path:
        with open(input_path, "a") as record:
            record.write("".join("%03o " % byte for byte in data))
    for byte in data:
        if pending or byte == 0x1B:
            pending += bytes([byte])
            target = END if in_paste else START
            if target.startswith(pending):
                if pending == target:
                    pending = b""
                    if in_paste:
                        in_paste = False
                        pasted = True
                        note(f"paste {paste_bytes} bytes")
                    else:
                        in_paste = True
                        paste_bytes = 0
                continue
            flushed, pending = pending, b""
            for other in flushed:
                ordinary(other)
            continue
        ordinary(byte)
PY

cat >"$CHAT_BIN/claude" <<SH
#!/bin/sh
FAKE_CLI_LOG='$FAKE_CLI_LOG'
export FAKE_CLI_LOG
case "\${1:-}" in
  --version | -v | --help | -h | auth) exec '$FIXTURES/fake-claude' "\$@" ;;
esac
FAKE_CLI_EVENT_DIR='$EVENTS'/"\$BONGOS_CHAT_ID.\$\$"
FAKE_CLI_INPUT='$INPUTS'/"\$BONGOS_CHAT_ID"
FAKE_MARKER_TEXT='$MARKER'
export FAKE_CLI_EVENT_DIR FAKE_CLI_INPUT FAKE_MARKER_TEXT
{ env | grep '^BONGOS_' | sort; echo "FAKE_EVENT_DIR=\$FAKE_CLI_EVENT_DIR"; } >'$CHAT_ENV'/"\$BONGOS_CHAT_ID"
printf '%s %s\n' "\$(date +%s.%N)" "\$*" >>'$LAUNCHES'/"\$BONGOS_CHAT_ID"
. '$FIXTURES/fake-session.sh'
fake_name=claude
fake_start_events "\$@"
trap 'kill "\${fake_events_pid:-}" 2>/dev/null' EXIT
trap 'exit 129' HUP
trap 'exit 143' TERM
stty raw -echo 2>/dev/null
printf '\033[?2004h'
printf 'ready\r\n'
python3 -I '$CHAT_BIN/fast-reader.py' "\$BONGOS_CHAT_ID"
SH
# The same CLI reporting a version older than the version table's hooks row.
cat >"$CHAT_BIN/claude-old" <<SH
#!/bin/sh
printf 'claude-old %s\n' "\$*" >>'$FAKE_CLI_LOG'
FAKE_CLI_VERSION=2.1.300
export FAKE_CLI_VERSION
exec '$CHAT_BIN/claude' "\$@"
SH
chmod +x "$CHAT_BIN/claude" "$CHAT_BIN/claude-old"

write_settings() {
  cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "project_repository": { "enabled": false },
  "chats": {
    "enabled": true,
    "agents": {
      "claude": { "command": "$1" }
    }
  }
}
JSON
}
write_settings "$CHAT_BIN/claude"
mkdir -p "$WORK_ROOT/oneoffs"
launch_drawn
focus_editor
wkeys p n
choose Relay
assert_file "$PROJECT_DIR/task.toml"
wkeys shift+p
choose relay
focus_editor
# The 1 MiB prompts buffer, written before the chat opens its prompts pane. No line holds a word of
# a session type's name, so the chat pickers below stay unambiguous.
python3 -I -c 'import sys
size = 1024 * 1024
lines, total, number = [], 0, 0
while total < size:
    number += 1
    line = f"Line {number:05d} of the large prompt, padded with filler text to a steady width"
    lines.append(line)
    total += len(line) + 1
text = "\n".join(lines)[:size - 1] + "\n"
open(sys.argv[1], "w").write(text)' "$PROJECT_DIR/prompts.txt"
[ "$(stat -c %s "$PROJECT_DIR/prompts.txt")" = 1048576 ] || fail "prompts.txt is not 1 MiB"
cp "$PROJECT_DIR/prompts.txt" "$RUN/prompts-1mib.txt"

# ---------------------------------------------------------------------------------------------
step "Channels 1: a Claude chat goes live and idle (exact); the socket is 600 in a 700 directory"
new_claude_chat thinking
poll 15 "a thinking record in $PROJECT_DIR/chats" have_record "$PROJECT_DIR" 'd.get("type") == "thinking"'
A_REC=$(record_where "$PROJECT_DIR" 'd.get("type") == "thinking"')
A_ID=$(toml "$A_REC" 'd["id"]')
poll 15 "the fake claude of chat A to start" launched_times "$A_ID" 1
poll 15 "the reader of chat A" reader_up "$A_ID"
SOCKET=$(chat_env "$A_ID" BONGOS_CHAT_SOCKET)
A_TOKEN=$(chat_env "$A_ID" BONGOS_CHAT_TOKEN)
[ -n "$SOCKET" ] && [ -n "$A_TOKEN" ] || fail "chat A has no socket or token in its environment: $(cat "$CHAT_ENV/$A_ID")"
grep -q -- '--settings' <<<"$(last_launch "$A_ID")" || fail "chat A was launched without --settings: $(last_launch "$A_ID")"
fire "$A_ID" SessionStart source=startup
wait_fired SessionStart
dump_until live-idle '^state: idle \(exact\)$' '^channel: live$'
LIVE_MS=$FIRED_MS
[ -S "$SOCKET" ] || fail "the report socket $SOCKET is not a socket"
[ "$(stat -c %a "$SOCKET")" = 600 ] || fail "the report socket has mode $(stat -c %a "$SOCKET"), not 600"
[ "$(stat -c %a "${SOCKET%/*}")" = 700 ] || fail "the socket directory has mode $(stat -c %a "${SOCKET%/*}"), not 700"
[ "$(stat -c %u "$SOCKET")" = "$(id -u)" ] || fail "the report socket is not owned by this user"
shot chat-live-idle
frame_has_ui chat-live-idle
say "smoke: chat A live and idle (exact) after its SessionStart hook (hook run ${LIVE_MS} ms); socket ${SOCKET##*/} 600 in a 700 directory ($RUN/chat-live-idle.png)"

# ---------------------------------------------------------------------------------------------
step "Channels 2: chat: send region with 1 MiB: one paste, one Enter, running then idle (exact)"
ckeys e
SUBMITS_BEFORE=$(grep -c "^claude $A_ID submit$" "$FAKE_CLI_LOG" || true)
palette "chat: send region"
poll 120 "the fake to read the 1 MiB paste and its Enter" grep -qx "claude $A_ID submit" "$FAKE_CLI_LOG"
[ "$SUBMITS_BEFORE" = 0 ] || fail "chat A was submitted before the send"
PASTE_BYTES=$(sed -n "s/^claude $A_ID paste \([0-9]*\) bytes\$/\1/p" "$FAKE_CLI_LOG" | tail -n1)
[ "${PASTE_BYTES:-0}" -ge 1048575 ] || fail "the paste did not carry the 1 MiB text: ${PASTE_BYTES:-no paste} bytes"
# The fake fired UserPromptSubmit on the Enter (marker 9001 of the reader).
poll 15 "the fake to run the UserPromptSubmit hooks" test ! -e "$(chat_env "$A_ID" FAKE_EVENT_DIR)/9001-UserPromptSubmit"
dump_until sent-running '^state: running \(exact\)$' '^channel: live$'
fire "$A_ID" Stop
wait_fired Stop
STOP_MS=$FIRED_MS
dump_until sent-idle '^state: idle \(exact\)$'
poll 30 "one bracketed paste of the 1 MiB text and one Enter" pastes_ok "$INPUTS/$A_ID" 1 "$RUN/prompts-1mib.txt"
say "smoke: one paste of 1 MiB and one Enter; UserPromptSubmit gave running (exact), Stop (hook run ${STOP_MS} ms) idle (exact)"

# ---------------------------------------------------------------------------------------------
step "Channels 3: a permission prompt is waiting (exact), the tool's end running (exact)"
fire "$A_ID" Notification notification_type=permission_prompt
wait_fired Notification
dump_until waiting '^state: waiting \(exact\)$'
shot chat-waiting-exact
frame_has_ui chat-waiting-exact
fire "$A_ID" PostToolUse
wait_fired PostToolUse
dump_until tool-done '^state: running \(exact\)$'
fire "$A_ID" Stop
wait_fired Stop
dump_until idle-again '^state: idle \(exact\)$'
say "smoke: permission_prompt -> waiting (exact), PostToolUse -> running (exact) ($RUN/chat-waiting-exact.png)"

# ---------------------------------------------------------------------------------------------
step "Channels 4: a spinner title for 20 s while idle leaves the record untouched"
# The state changes above are written at most once per 10 s; wait until the last of them is on disk.
poll 60 "chat A's record to be unchanged for 12 s" record_quiet "$A_REC" 12
MTIME_BEFORE=$(mtime_ns "$A_REC")
# Asked through a file, not typed: typing is activity and is written, whatever the state.
A_EVENTS=$(chat_env "$A_ID" FAKE_EVENT_DIR)
echo 2200 >"$A_EVENTS/.spin-request"
mv "$A_EVENTS/.spin-request" "$A_EVENTS/.spin"
poll 120 "the fake to finish the spinner" grep -q "^claude $A_ID spin done " "$FAKE_CLI_LOG"
SPIN_SECS=$(python3 -I -c 'import sys
lines = open(sys.argv[1]).read().splitlines()
start = [float(l.split()[-1]) for l in lines if l.startswith(sys.argv[2] + " spin start ")][-1]
done = [float(l.split()[-1]) for l in lines if l.startswith(sys.argv[2] + " spin done ")][-1]
print(int(done - start))' "$FAKE_CLI_LOG" "claude $A_ID")
[ "$SPIN_SECS" -ge 20 ] || fail "the spinner ran only $SPIN_SECS s"
MTIME_AFTER=$(mtime_ns "$A_REC")
[ "$MTIME_BEFORE" = "$MTIME_AFTER" ] || fail "the record of the idle chat was written while its title spun: $(cat "$A_REC")"
dump_until spun '^state: idle \(exact\)$'
say "smoke: ${SPIN_SECS} s of spinner titles on an idle live chat; the record's mtime is unchanged"

# ---------------------------------------------------------------------------------------------
step "Channels 5: two Claude chats in one directory, back to back, each keeps its own id"
new_claude_chat plan
new_claude_chat free
poll 15 "a plan record" have_record "$PROJECT_DIR" 'd.get("type") == "plan"'
poll 15 "a free record" have_record "$PROJECT_DIR" 'd.get("type") == "free"'
B_REC=$(record_where "$PROJECT_DIR" 'd.get("type") == "plan"')
C_REC=$(record_where "$PROJECT_DIR" 'd.get("type") == "free"')
B_ID=$(toml "$B_REC" 'd["id"]')
C_ID=$(toml "$C_REC" 'd["id"]')
poll 15 "the fake of the plan chat to start" launched_times "$B_ID" 1
poll 15 "the fake of the free chat to start" launched_times "$C_ID" 1
# No report from the first was sent yet, so nothing it reports can have let the second start.
reader_up "$B_ID" || fail "the plan chat's fake is not running after the free chat started"
python3 -I -c 'import sys; sys.exit(0 if float(sys.argv[2]) - float(sys.argv[1]) < 45 else 1)' \
  "$(launch_time "$B_ID" 1)" "$(launch_time "$C_ID" 1)" || fail "the free chat started long after the plan chat"
poll 15 "session_id in the plan record" has_session "$B_REC"
poll 15 "session_id in the free record" has_session "$C_REC"
B_SESSION=$(toml "$B_REC" 'd["session_id"]')
C_SESSION=$(toml "$C_REC" 'd["session_id"]')
[ "$B_SESSION" != "$C_SESSION" ] || fail "both chats hold the session id $B_SESSION"
grep -qF -- "--session-id $B_SESSION" <<<"$(last_launch "$B_ID")" || fail "the plan record's id $B_SESSION is not its launch's: $(last_launch "$B_ID")"
grep -qF -- "--session-id $C_SESSION" <<<"$(last_launch "$C_ID")" || fail "the free record's id $C_SESSION is not its launch's: $(last_launch "$C_ID")"
same_dir "$(toml "$B_REC" 'd["cwd"]')" "$(toml "$C_REC" 'd["cwd"]')" || fail "the two chats run in different directories"
fire "$B_ID" SessionStart source=startup
wait_fired SessionStart
fire "$C_ID" SessionStart source=startup
wait_fired SessionStart
# A turn in each, so closing keeps the records: a live chat with no reported prompt is deleted.
for id in "$B_ID" "$C_ID"; do
  fire "$id" UserPromptSubmit
  wait_fired UserPromptSubmit
  fire "$id" Stop
  wait_fired Stop
done
dump_until free-turn '^state: idle \(exact\)$' '^channel: live$'
# The free chat is the shown one: close it and open it again.
ckeys k
poll 15 "the free chat's fake to end" reader_gone "$C_ID"
cpick o
choose "free claude"
poll 15 "the free chat to launch again" launched_times "$C_ID" 2
grep -qF -- "--resume $C_SESSION" <<<"$(last_launch "$C_ID")" || fail "the free chat did not resume $C_SESSION: $(last_launch "$C_ID")"
fire "$C_ID" SessionStart source=resume
wait_fired SessionStart
dump_until free-resumed '^state: idle \(exact\)$' '^channel: live$'
palette "chat: open any"
choose "plan claude"
ckeys k
poll 15 "the plan chat's fake to end" reader_gone "$B_ID"
cpick o
choose "plan claude"
poll 15 "the plan chat to launch again" launched_times "$B_ID" 2
grep -qF -- "--resume $B_SESSION" <<<"$(last_launch "$B_ID")" || fail "the plan chat did not resume $B_SESSION: $(last_launch "$B_ID")"
fire "$B_ID" SessionStart source=resume
wait_fired SessionStart
dump_until plan-resumed '^state: idle \(exact\)$' '^channel: live$'
reader_up "$A_ID" || fail "chat A ended"
say "smoke: two chats in one directory started back to back with $B_SESSION and $C_SESSION; each resumed with its own"

# ---------------------------------------------------------------------------------------------
step "Channels 7: chat A's token cannot report for chat B"
# Run before 6, which changes the Claude command. The plan chat (B) is shown and idle (exact).
B_TOKEN=$(chat_env "$B_ID" BONGOS_CHAT_TOKEN)
[ -n "$B_TOKEN" ] && [ "$B_TOKEN" != "$A_TOKEN" ] || fail "chat B has no token of its own"
spoof_out=$(env BONGOS_CHAT_SOCKET="$SOCKET" BONGOS_CHAT_ID="$B_ID" BONGOS_CHAT_TOKEN="$A_TOKEN" \
  "$BONGOS_BIN" --chat-report state waiting 2>&1) || fail "the reporter exited nonzero"
[ -z "$spoof_out" ] || fail "the reporter printed: $spoof_out"
# The reporter waits for the server's reply, which comes after the token check.
dump_chat_view spoofed
view_has '^state: idle \(exact\)$' || fail "a report with chat A's token changed chat B: $(cat "$CHAT_VIEW")"
env BONGOS_CHAT_SOCKET="$SOCKET" BONGOS_CHAT_ID="$B_ID" BONGOS_CHAT_TOKEN="$B_TOKEN" \
  "$BONGOS_BIN" --chat-report state waiting
dump_until own-token '^state: waiting \(exact\)$'
say "smoke: chat A's token left chat B idle (exact); chat B's own token made it waiting (exact)"

# ---------------------------------------------------------------------------------------------
step "Channels 6: a Claude older than the hooks row runs with its state estimated"
write_settings "$CHAT_BIN/claude-old"
poll 30 "the health probe of the older Claude" log_has "claude-old --version"
new_claude_chat thinking
poll 15 "a second thinking record" have_record "$PROJECT_DIR" "d.get(\"type\") == \"thinking\" and d[\"id\"] != \"$A_ID\""
D_REC=$(record_where "$PROJECT_DIR" "d.get(\"type\") == \"thinking\" and d[\"id\"] != \"$A_ID\"")
D_ID=$(toml "$D_REC" 'd["id"]')
D_NAME=$(toml "$D_REC" 'd["name"]')
poll 15 "the older fake to start" launched_times "$D_ID" 1
if grep -q -- '--settings' <<<"$(last_launch "$D_ID")"; then fail "the older Claude was given hooks: $(last_launch "$D_ID")"; fi
[ -z "$(chat_env "$D_ID" BONGOS_CHAT_TOKEN)" ] || fail "the older Claude got a report token"
dump_until old-version '^state: [a-z]+ \(estimated: .*2\.1\.300 is older than the first version with hooks bongos knows \(2\.1\.319\)\)$' '^channel: off$'
dump_list old-version-list
grep -qE '^ *claude: ready \(2\.1\.300' "$CHAT_VIEW" || fail "the health line does not name 2.1.300: $(cat "$CHAT_VIEW")"
row_has "$D_NAME" 'state: [a-z]+ \(estimated\)$' || fail "the older chat's row has no estimated tag: $(cat "$CHAT_VIEW")"
row_has "$(toml "$C_REC" 'd["name"]')" 'state: idle$' || fail "the free chat's row is not idle without a tag: $(cat "$CHAT_VIEW")"
shot chat-estimated-row
frame_has_ui chat-estimated-row
say "smoke: Claude 2.1.300 runs without hooks; its state is estimated and names the version ($RUN/chat-estimated-row.png)"
quit_with_prompt quit-prompt

# ---------------------------------------------------------------------------------------------
step "Channels 8: no hook content in the logs, no network"
for file in "$HOME/$BONGOS_LOG_PATH" "$RUN/app.out"; do
  if [ -f "$file" ] && grep -qF -- "$MARKER" "$file"; then
    fail "hook content reached $file: $(grep -F -- "$MARKER" "$file" | head -n3)"
  fi
done
if grep -rqF -- "$MARKER" "$PROJECT_DIR/chats"; then fail "hook content reached a chat record"; fi
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
chat_requests=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log")
stray_requests=$(printf '%s\n' "$chat_requests" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
if [ -n "$stray_requests" ]; then
  echo "bongos: the channels scenario reached the proxy for other hosts:" >&2
  printf '%s\n' "$stray_requests" | sort | uniq -c >&2
  exit 1
fi
say "smoke: the marker text is in no log or record; no request beyond the app's startup ones"

cp "$RUN/config-backup/settings-37.json" "$SETTINGS_FILE"
