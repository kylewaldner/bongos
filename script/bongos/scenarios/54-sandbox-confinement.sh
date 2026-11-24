# Scenario for feature 54-sandbox-confinement. Sourced by script/bongos/smoke after the base flow
# (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. It reads the Confinement section of the Work page of
# the settings window through `settings editor: dump confinement` (<logs>/settings-confinement.txt),
# as the other scenarios read their views through a dump action.
#
#   1. The settings window opens (ctrl+comma) on settings.json `{}`. The section shows the Linux
#      promise "Jobs are confined" and the beginning of its body, the two switches, and the line
#      "Agent steps are not confined (confinement.agent_steps is off)" (agent_steps is off by
#      default); `confinement.task_checks` is on and has no line. The scenario fails if Bubblewrap
#      cannot run here (the title would read "Jobs are not confined"); it is never skipped.
#   2. confinement.agent_steps set to true: the line goes while the window stays open.
#   3. confinement.agent_steps set to false: the line returns.
# Deviations: the switches are changed through settings.json, which the running app reloads and
# which rebuilds the open page, instead of a click on each switch; the Work entry of the window is
# clicked by position for the screenshot only, and the text is read from the dump.
# The scenario leaves the app quit and restores settings.json.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 54-sandbox-confinement skipped under BONGOS_STOCK=1: the stock run has no confinement"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DUMP_FILE="$LOGS_DIR/settings-confinement.txt"
AGENT_LINE="line: Agent steps are not confined (confinement.agent_steps is off)"

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# Write in place: a rename over the file would end the app's watch on it after the first write.
write_settings() { printf '%s\n' "$1" >"$SETTINGS_FILE"; }

# Runs a command by its palette name, in the workspace window.
palette() {
  act keys ctrl+shift+p
  act type_text "$1"
  act keys Return
}

# dump_section: the window of the app has the focus, the dump is written afresh to $DUMP_FILE.
dump_section() {
  rm -f "$DUMP_FILE"
  xdo windowfocus --sync "$MAIN_WIN"
  palette "settings editor: dump confinement"
  assert_file "$DUMP_FILE" 'confinement.task_checks'
}

# wait_for_line <present|absent>: dumps until the agent step line is (not) in the section.
wait_for_line() {
  local want=$1 deadline=$((SECONDS + 30))
  while :; do
    dump_section
    if grep -qxF "$AGENT_LINE" "$DUMP_FILE"; then
      [ "$want" = present ] && return 0
    else
      [ "$want" = absent ] && return 0
    fi
    [ "$SECONDS" -lt "$deadline" ] || fail "the agent step line is not $want in the section: $(cat "$DUMP_FILE")"
    sleep 1
  done
}

# ---------------------------------------------------------------------------------------------
say "smoke: confinement settings: promise, switches and notes"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-54.json"
write_settings '{}'

launch "$RUN/project"
wait_for_window
poll 60 "a rendered frame" ui_rendered
wait_for_settled
MAIN_WIN=$WIN

act keys ctrl+comma
# The settings window opens over the app at a fixed place; click its Work entry for the screenshot.
act xdo mousemove 252 521 click 1
shot confinement-settings
say "smoke: screenshot $RUN/confinement-settings.png"

# 1. The defaults.
wait_for_line present
cp "$DUMP_FILE" "$RUN/confinement-default.txt"
grep -qxF "line: Jobs are confined" "$DUMP_FILE" ||
  fail "the promise title is not 'Jobs are confined' (is Bubblewrap usable here?): $(cat "$DUMP_FILE")"
grep -q "^body: Jobs (workflow programs, agent steps and task checks) can write only to their job" "$DUMP_FILE" ||
  fail "the promise body is missing: $(cat "$DUMP_FILE")"
grep -qxF "switch: Confine Agent Steps" "$DUMP_FILE" || fail "no agent steps switch: $(cat "$DUMP_FILE")"
grep -qxF "switch: Confine Task Checks" "$DUMP_FILE" || fail "no task checks switch: $(cat "$DUMP_FILE")"
grep -qxF "confinement.task_checks: true" "$DUMP_FILE" || fail "confinement.task_checks is not on by default: $(cat "$DUMP_FILE")"
grep -qxF "confinement.agent_steps: false" "$DUMP_FILE" || fail "confinement.agent_steps is not off by default: $(cat "$DUMP_FILE")"
if grep -q "Task checks are not confined" "$DUMP_FILE"; then
  fail "a task checks line is shown while the switch is on: $(cat "$DUMP_FILE")"
fi

# 2. agent_steps on: the line goes.
write_settings '{ "confinement": { "agent_steps": true } }'
wait_for_line absent
grep -qxF "confinement.agent_steps: true" "$DUMP_FILE" || fail "agent_steps did not read true: $(cat "$DUMP_FILE")"

# 3. agent_steps off again: the line returns.
write_settings '{ "confinement": { "agent_steps": false } }'
wait_for_line present
grep -qxF "confinement.agent_steps: false" "$DUMP_FILE" || fail "agent_steps did not read false: $(cat "$DUMP_FILE")"

xdo windowfocus --sync "$MAIN_WIN"
quit_app
cp "$RUN/config-backup/settings-54.json" "$SETTINGS_FILE"
