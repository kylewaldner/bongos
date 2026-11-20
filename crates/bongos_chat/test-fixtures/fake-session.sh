# The interactive session of the fake CLIs, sourced by `fake-claude` and `fake-codex`: what they run
# for any arguments that are not a probe. POSIX sh. It never touches the real home directory.
#
# At start it appends to $FAKE_CLI_LOG (when set), one line each, prefixed with the CLI's name:
#   start
#   arg <argument>             one line per argument, in order
#   cwd <directory>
#   env <NAME>=<value>         one line per BONGOS_* variable, sorted
#   session <id>               fake-codex only, when it wrote a session file (see FAKE_CLI_HOME)
# then turns on bracketed paste (ESC[?2004h) unless $FAKE_CLI_NO_BRACKETED is set, sets the OSC
# title to $FAKE_CLI_TITLE when set, prints `ready` and reads the terminal in raw mode, byte for
# byte. Each byte is appended to $FAKE_CLI_INPUT (when set) as three octal digits and a space, so a
# bracketed paste and its Enter can be checked exactly. Each line (ended by CR or LF) is logged as `input <line>` and run:
#   bell            rings the bell (BEL)
#   title <text>    sets the OSC title
#   spin <count>    sets <count> different OSC titles, 10 ms apart (a spinner in the title)
#   exit <code>     logs `exit <code>` and exits with that code
#   anything else   is answered with `got <line>`
# End of input (the terminal went away) logs `exit eof`.
#
# Knobs (environment variables): FAKE_CLI_LOG, FAKE_CLI_INPUT, FAKE_CLI_TITLE, FAKE_CLI_NO_BRACKETED,
# FAKE_CLI_SPIN (spin that many titles right after `ready`, then print `spun`),
# FAKE_CLI_EVENT_DIR (fake-claude only, with `--settings <file>`; see fake_events below),
# and for fake-codex FAKE_CLI_HOME: when set, a new or forked session writes the session file Codex would,
# `$FAKE_CLI_HOME/.codex/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl`, whose first line holds
# the id and the working directory.

fake_log() {
  if [ -n "${FAKE_CLI_LOG:-}" ]; then
    printf '%s %s\n' "$fake_name" "$1" >> "$FAKE_CLI_LOG"
  fi
}

fake_uuid() {
  if [ -r /proc/sys/kernel/random/uuid ]; then
    cat /proc/sys/kernel/random/uuid
  else
    uuidgen | tr 'ABCDEF' 'abcdef'
  fi
}

fake_codex_session_file() {
  if [ -z "${FAKE_CLI_HOME:-}" ] || [ "${1:-}" = resume ]; then
    return
  fi
  fake_id=$(fake_uuid)
  fake_dir="$FAKE_CLI_HOME/.codex/sessions/$(date -u +%Y/%m/%d)"
  mkdir -p "$fake_dir"
  printf '{"type":"session_meta","payload":{"id":"%s","cwd":"%s"}}\n' "$fake_id" "$(pwd)" \
    > "$fake_dir/rollout-$(date -u +%Y-%m-%dT%H-%M-%S)-$fake_id.jsonl"
  fake_log "session $fake_id"
}

fake_spin() {
  fake_count=0
  while [ "$fake_count" -lt "$1" ]; do
    fake_count=$((fake_count + 1))
    printf '\033]0;spinner %s\007' "$fake_count"
    sleep 0.01
  done
  printf 'spun\r\n'
}

fake_command() {
  # The line is a run of `\ooo` octal escapes, which printf turns back into bytes.
  fake_text=$(printf "$1")
  fake_log "input $fake_text"
  case $fake_text in
    bell) printf '\007' ;;
    "title "*) printf '\033]0;%s\007' "${fake_text#title }" ;;
    "spin "*) fake_spin "${fake_text#spin }" ;;
    "exit "*)
      fake_log "exit ${fake_text#exit }"
      exit "${fake_text#exit }"
      ;;
    *) printf 'got %s\r\n' "$fake_text" ;;
  esac
}

# Hook events (fake-claude only). When $FAKE_CLI_EVENT_DIR is set and the arguments hold
# `--settings <file>` (or `--settings=<file>`), a background poller watches that directory. A file
# named `<Event>` or `<seq>-<Event>` (a leading run of digits and a dash is dropped) is a marker:
# for each `command` hook the settings file lists under that event (`"hooks"` as serde_json's
# pretty printer writes it: event keys indented four spaces), the poller runs `sh -c <command>`
# with one JSON object on stdin, then deletes the marker. Hook timeouts are ignored and hook output
# is discarded. Files starting with `.` are skipped, so a scenario can write one and rename it.
# The marker's lines `source=`, `notification_type=` and `tool_name=` set those fields (defaults:
# startup, idle_prompt, Bash); the other lines are the marker text, put in `prompt`, `message`
# and `last_assistant_message`.
fake_json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr '\n\r\t' '   '
}

fake_hook_commands() {
  awk -v event="$1" '
    /^    "[^"]+": \[$/ { current = $1; gsub(/[":]/, "", current); next }
    current == event && /^ *"command": "/ {
      line = $0
      sub(/^ *"command": "/, "", line)
      sub(/",?$/, "", line)
      print line
    }
  ' "$2" | sed -e 's/\\"/"/g' -e 's/\\\\/\\/g'
}

fake_fire() {
  fake_event=$1
  fake_settings=$2
  fake_marker=$3
  fake_source=startup
  fake_ntype=idle_prompt
  fake_tool=Bash
  fake_text=''
  while IFS= read -r fake_row || [ -n "$fake_row" ]; do
    case $fake_row in
      source=*) fake_source=${fake_row#source=} ;;
      notification_type=*) fake_ntype=${fake_row#notification_type=} ;;
      tool_name=*) fake_tool=${fake_row#tool_name=} ;;
      *) fake_text="${fake_text:+$fake_text }$fake_row" ;;
    esac
  done < "$fake_marker"
  fake_text=$(fake_json_escape "$fake_text")
  fake_payload=$(printf '{"hook_event_name":"%s","session_id":"%s","source":"%s","notification_type":"%s","tool_name":"%s","prompt":"%s","message":"%s","last_assistant_message":"%s"}' \
    "$fake_event" "${fake_session_id:-00000000-0000-4000-8000-000000000000}" "$(fake_json_escape "$fake_source")" \
    "$(fake_json_escape "$fake_ntype")" "$(fake_json_escape "$fake_tool")" "$fake_text" "$fake_text" "$fake_text")
  fake_hook_commands "$fake_event" "$fake_settings" | while IFS= read -r fake_cmd; do
    printf '%s\n' "$fake_payload" | sh -c "$fake_cmd" > /dev/null 2>&1
  done
}

fake_events() {
  fake_dir=$1
  fake_settings=$2
  while :; do
    for fake_file in "$fake_dir"/*; do
      [ -f "$fake_file" ] || continue
      fake_base=${fake_file##*/}
      case $fake_base in
        .*) continue ;;
        [0-9]*-*) fake_base=${fake_base#*-} ;;
      esac
      fake_fire "$fake_base" "$fake_settings" "$fake_file"
      rm -f "$fake_file"
    done
    sleep 0.05
  done
}

fake_start_events() {
  [ -n "${FAKE_CLI_EVENT_DIR:-}" ] || return 0
  fake_settings=''
  fake_session_id=''
  fake_prev=''
  for fake_arg in "$@"; do
    case $fake_prev in
      --settings) fake_settings=$fake_arg ;;
      --session-id | --resume | -r) fake_session_id=$fake_arg ;;
    esac
    case $fake_arg in
      --settings=*) fake_settings=${fake_arg#--settings=} ;;
    esac
    fake_prev=$fake_arg
  done
  [ -f "$fake_settings" ] || return 0
  mkdir -p "$FAKE_CLI_EVENT_DIR"
  fake_events "$FAKE_CLI_EVENT_DIR" "$fake_settings" < /dev/null &
  fake_events_pid=$!
  trap 'kill "$fake_events_pid" 2>/dev/null' EXIT
}

fake_session() {
  fake_name=$1
  shift
  fake_log start
  for fake_arg in "$@"; do
    fake_log "arg $fake_arg"
  done
  fake_log "cwd $(pwd)"
  env | grep '^BONGOS_' | sort | while IFS= read -r fake_line; do
    fake_log "env $fake_line"
  done
  if [ "$fake_name" = codex ]; then
    fake_codex_session_file "$@"
  fi

  fake_start_events "$@"

  stty raw -echo 2>/dev/null
  if [ -z "${FAKE_CLI_NO_BRACKETED:-}" ]; then
    printf '\033[?2004h'
  fi
  if [ -n "${FAKE_CLI_TITLE:-}" ]; then
    printf '\033]0;%s\007' "$FAKE_CLI_TITLE"
  fi
  printf 'ready\r\n'
  if [ -n "${FAKE_CLI_SPIN:-}" ]; then
    fake_spin "$FAKE_CLI_SPIN"
  fi

  fake_buffer=''
  while :; do
    fake_byte=$(dd bs=1 count=1 2>/dev/null | od -An -to1 | tr -d ' \n')
    if [ -z "$fake_byte" ]; then
      fake_log "exit eof"
      exit 0
    fi
    if [ -n "${FAKE_CLI_INPUT:-}" ]; then
      printf '%s ' "$fake_byte" >> "$FAKE_CLI_INPUT"
    fi
    case $fake_byte in
      015 | 012)
        fake_command "$fake_buffer"
        fake_buffer=''
        ;;
      *) fake_buffer="$fake_buffer\\$fake_byte" ;;
    esac
  done
}
