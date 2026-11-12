# Scenario for feature 20-workflow-engine. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh.
#
# Real runs are NOT exercised here: the engine of the app is built with a runner that is not yet
# connected to the node runners, so no run can execute. What this scenario proves is everything the
# app does without a run, in a throwaway HOME with the default settings (workflows are on by
# default, so no setting is written to turn them on):
#   1. The shipped definitions are present under <data dir>/workflows/shipped/<version>/ (all four,
#      with their prompts and scripts), and neither starting nor validating writes anything under
#      ~/.bongos/workflows.
#   2. `workflow: validate` from the palette opens the "Workflow diagnostics" buffer.
#   3. `workflow: new` from the palette (the Minimal choice) creates a workflow in the profile
#      layer, ~/.bongos/workflows/new-workflow/workflow.toml, and opens it in an editor.
#   4. The Emacs key C-c f n runs the same command: after the directory is removed, the key
#      creates it again.
#   5. `workflow: show runs` brings up the Runs panel and a second one hides it (the screen
#      changes each time); `workflow: run` with no task at point and `workflow: pause` with no run
#      answer with a message and leave the app running and nothing written under the runs
#      directory.
#   6. With workflows.enabled set to false in settings.json, the Runs panel goes away and C-c f n
#      does nothing (no workflow is created); set back to true, the same key creates one again.
#   7. The proxy got no request beyond the two kinds every launch makes (the ACP registry and
#      localhost, as in scenarios 28, 03, 25 and 26).
# Not covered: the Work page of the settings window (the harness cannot show a second window
# reliably, see scenario 01).
# Reading a message or the diagnostics is not possible (a toast and a read-only buffer have no
# dump), so those are read from screenshots and the effects are checked on disk.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 20-workflow-engine skipped under BONGOS_STOCK=1: the stock run has no workflows"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
SHIPPED_ROOT="$DATA_DIR/workflows/shipped"
PROFILE_DIR="$HOME/.bongos/workflows"
NEW_DIR="$PROFILE_DIR/new-workflow"
SHIPPED_NAMES=(implement-pr implement-tasks ship-pr babysit-pr)

fail() {
  echo "bongos: $*" >&2
  exit 1
}

mkdir -p "$RUN/config-backup"
if [ -f "$SETTINGS_FILE" ]; then cp "$SETTINGS_FILE" "$RUN/config-backup/settings-20.json"; else : >"$RUN/config-backup/settings-20.absent"; fi
scenario_exit() {
  local rc=$?
  set +e
  if [ -f "$RUN/config-backup/settings-20.json" ]; then
    cp "$RUN/config-backup/settings-20.json" "$SETTINGS_FILE"
  else
    rm -f "$SETTINGS_FILE"
  fi
  (exit "$rc")
  cleanup
}
trap scenario_exit EXIT

# Emacs base keymap (the C-c f keys exist only there). Written in place, as the other scenarios do.
write_settings() {
  printf '{ "base_keymap": "Emacs", "workflows": { "enabled": %s } }\n' "$1" >"$SETTINGS_FILE"
}

palette() {
  act keys ctrl+shift+p
  act type_text "$1"
  keys Return
  wait_for_settled
}

path_exists() { [ -e "$1" ]; }

# Succeeds when the shipped directory of some version holds <$1>/workflow.toml.
shipped_has() {
  local dir
  for dir in "$SHIPPED_ROOT"/*/; do
    [ -f "${dir}$1/workflow.toml" ] && return 0
  done
  return 1
}

# ---------------------------------------------------------------------------------------------
step "Setup: Emacs keymap, workflows on by default"
write_settings true
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
launch
wait_for_window
poll 60 "a rendered frame" ui_rendered
wait_for_settled
[ ! -e "$PROFILE_DIR" ] || fail "$PROFILE_DIR exists before any workflow command ran"

# ---------------------------------------------------------------------------------------------
step "Definitions 1: the engine has written the four shipped definitions and validate does not touch the profile layer"
palette "workflow: validate"
for name in "${SHIPPED_NAMES[@]}"; do
  poll 30 "the shipped definition $name" shipped_has "$name"
done
for dir in "$SHIPPED_ROOT"/*/; do
  [ -f "${dir}implement-pr/run_tests.py" ] || fail "the shipped implement-pr has no run_tests.py: $(ls -R "$dir")"
  [ -f "${dir}implement-pr/triage.md" ] || fail "the shipped implement-pr has no triage.md: $(ls -R "$dir")"
done
[ ! -e "$PROFILE_DIR" ] || fail "validating created $PROFILE_DIR: $(ls -R "$PROFILE_DIR")"
shot workflow-validate
frame_has_ui workflow-validate
say "smoke: the diagnostics of workflow: validate: $RUN/workflow-validate.png"

# ---------------------------------------------------------------------------------------------
step "Definitions 2: workflow: new (Minimal) creates a workflow in the profile layer and opens it"
palette "workflow: new"
shot workflow-new-picker
frame_has_ui workflow-new-picker
act keys Return
poll 30 "the new definition" path_exists "$NEW_DIR/workflow.toml"
[ -s "$NEW_DIR/workflow.toml" ] || fail "$NEW_DIR/workflow.toml is empty"
grep -qE '^\[nodes\.' "$NEW_DIR/workflow.toml" || fail "the new definition has no node: $(cat "$NEW_DIR/workflow.toml")"
[ -s "$NEW_DIR/work.md" ] || fail "the minimal template left no $NEW_DIR/work.md"
wait_for_settled
shot workflow-new-opened
frame_has_ui workflow-new-opened
say "smoke: the new workflow opened in an editor: $RUN/workflow-new-opened.png"

# ---------------------------------------------------------------------------------------------
step "Keys: C-c f n runs workflow: new"
rm -rf "$NEW_DIR"
act keys ctrl+c f n
act keys Return
poll 30 "the definition made by C-c f n" path_exists "$NEW_DIR/workflow.toml"
wait_for_settled

# ---------------------------------------------------------------------------------------------
step "Runs panel: show runs brings it up and hides it; commands with no run answer and change nothing"
act keys ctrl+c f w
shot workflow-runs-panel
frame_has_ui workflow-runs-panel
PANEL_HASH=$(screen_hash)
act keys ctrl+c f w
[ "$(screen_hash)" != "$PANEL_HASH" ] || fail "the screen did not change when show runs ran a second time"
palette "workflow: run"
shot workflow-run-no-task
frame_has_ui workflow-run-no-task
palette "workflow: pause"
shot workflow-pause-no-run
frame_has_ui workflow-pause-no-run
kill -0 "$APP_PID" 2>/dev/null || fail "the app exited after the run commands with no run"
if [ -e "$HOME/.bongos/runs" ] && [ -n "$(ls -A "$HOME/.bongos/runs" 2>/dev/null)" ]; then
  fail "a run record was written with no run started: $(ls -R "$HOME/.bongos/runs")"
fi
say "smoke: the Runs panel and the messages: $RUN/workflow-runs-panel.png, $RUN/workflow-run-no-task.png, $RUN/workflow-pause-no-run.png"

# ---------------------------------------------------------------------------------------------
step "Quit before the off check"
quit_app

# ---------------------------------------------------------------------------------------------
step "Off: workflows.enabled false removes the panel and the key does nothing; true brings it back"
rm -rf "$PROFILE_DIR"
write_settings false
launch
wait_for_window
poll 60 "a rendered frame" ui_rendered
wait_for_settled
act keys ctrl+c f n
wait_for_settled
shot workflow-off
frame_has_ui workflow-off
[ ! -e "$NEW_DIR" ] || fail "C-c f n created $NEW_DIR while workflows.enabled is false"
write_settings true
sleep 2
act keys ctrl+c f n
act keys Return
poll 30 "the definition made after workflows came back on" path_exists "$NEW_DIR/workflow.toml"
say "smoke: workflows off: $RUN/workflow-off.png"

# ---------------------------------------------------------------------------------------------
step "Privacy: no request beyond the two kinds every launch makes"
registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
  "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
[ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
strays=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log" |
  awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
[ -z "$strays" ] || fail "the proxy got other requests during the scenario: $strays"
quit_app

trap - EXIT
if [ -f "$RUN/config-backup/settings-20.json" ]; then
  cp "$RUN/config-backup/settings-20.json" "$SETTINGS_FILE"
else
  rm -f "$SETTINGS_FILE"
fi
trap cleanup EXIT
