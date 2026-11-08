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
#   exit <code>     logs `exit <code>` and exits with that code
#   anything else   is answered with `got <line>`
# End of input (the terminal went away) logs `exit eof`.
#
# Knobs (environment variables): FAKE_CLI_LOG, FAKE_CLI_INPUT, FAKE_CLI_TITLE, FAKE_CLI_NO_BRACKETED,
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

fake_command() {
  # The line is a run of `\ooo` octal escapes, which printf turns back into bytes.
  fake_text=$(printf "$1")
  fake_log "input $fake_text"
  case $fake_text in
    bell) printf '\007' ;;
    "title "*) printf '\033]0;%s\007' "${fake_text#title }" ;;
    "exit "*)
      fake_log "exit ${fake_text#exit }"
      exit "${fake_text#exit }"
      ;;
    *) printf 'got %s\r\n' "$fake_text" ;;
  esac
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

  stty raw -echo 2>/dev/null
  if [ -z "${FAKE_CLI_NO_BRACKETED:-}" ]; then
    printf '\033[?2004h'
  fi
  if [ -n "${FAKE_CLI_TITLE:-}" ]; then
    printf '\033]0;%s\007' "$FAKE_CLI_TITLE"
  fi
  printf 'ready\r\n'

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
