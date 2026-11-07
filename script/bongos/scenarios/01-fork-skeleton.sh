# Scenario for feature 01-fork-skeleton. Sourced by script/bongos/smoke after the base flow (the app
# is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY,
# the proxy and the helpers of lib.sh. Order matters: the one phase that runs the stock binary
# (Req 3) is last, because it puts Zed hosts in proxy.log and Req 7 needs that log clean.
# The scenario leaves the app quit and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 01-fork-skeleton skipped under BONGOS_STOCK=1: the stock run is the negative control"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
KEYMAP_FILE="$CONFIG_DIR/keymap.json"
STOCK_BIN="$BONGOS_WORKSPACE/scratch/stock-zed/zed"
STOCK_PID=""

for tool in strace jq compare identify; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

# Write a file in one rename, so a watcher never sees a half-written one.
atomic_write() {
  printf '%s\n' "$2" >"$1.tmp"
  mv "$1.tmp" "$1"
}

write_theme() { atomic_write "$SETTINGS_FILE" "{ \"theme\": \"$1\" }"; }

# Launch with no arguments (the session restores the project) and wait until it is drawn.
launch_ready() {
  if [ "${1:-}" = traced ]; then launch_traced; else launch; fi
  wait_for_window
  wait_for_title 'Main\.kt'
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# Same as launch, under strace: only connect calls, followed through forks, filtered in the kernel.
# APP_PID is strace's; it exits with the app.
launch_traced() {
  rm -f "$RUN/connect.trace"
  strace -f --seccomp-bpf -e trace=connect -o "$RUN/connect.trace" "$BONGOS_BIN" >>"$RUN/app.out" 2>&1 &
  APP_PID=$!
}

# One try at the user binding. The key is sent until the binding has loaded and the app is gone.
quit_with_binding() {
  kill -0 "$APP_PID" 2>/dev/null || return 0
  keys ctrl+alt+q || true
  sleep 1
  ! kill -0 "$APP_PID" 2>/dev/null
}

expect_quit_by_binding() {
  local pid=$APP_PID rc=0
  poll 30 "ctrl-alt-q to quit the app" quit_with_binding
  APP_PID=""
  wait "$pid" || rc=$?
  say "smoke: ctrl-alt-q quit the app (exit status $rc)"
}

# 0 when the screenshot's mean brightness is above (light) or below (dark) one half.
brightness_is() {
  local mean
  shot theme-probe
  mean=$(identify -format '%[fx:mean]' "$RUN/theme-probe.png")
  case $1 in
    light) awk -v m="$mean" 'BEGIN { exit !(m > 0.5) }' ;;
    dark) awk -v m="$mean" 'BEGIN { exit !(m < 0.5) }' ;;
  esac
}

# A hash of what is under the three Zed directories of the home: names, modes, link targets,
# modification times and file contents.
zed_tree_hash() {
  local d
  (
    cd "$HOME"
    for d in .config/zed .local/share/zed .cache/zed; do
      if [ -e "$d" ]; then
        find "$d" -printf '%y %p %m %T@ %l\n' | LC_ALL=C sort
        find "$d" -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum
      else
        echo "missing $d"
      fi
    done
  ) | sha256sum | awk '{print $1}'
}

# The stock settings file keeps its marker (17) and the bongos one its own (18).
expect_font_sizes() {
  if ! grep -q '"ui_font_size": 17' "$HOME/.config/zed/settings.json" ||
    ! grep -q '"ui_font_size": 18' "$SETTINGS_FILE"; then
    echo "bongos: the settings files are not separate: stock $(cat "$HOME/.config/zed/settings.json"), bongos $(cat "$SETTINGS_FILE")" >&2
    exit 1
  fi
}

mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$KEYMAP_FILE" "$RUN/config-backup/"

# ---------------------------------------------------------------------------------------------
step "Req 8: no collab, auto-update, onboarding, feedback, channel or call actions"
"$BONGOS_BIN" --dump-all-actions >"$RUN/actions.json"
jq -e '.actions | length > 100' "$RUN/actions.json" >/dev/null
bad_actions=$(jq -r '.actions[].name
  | select(split("::")[0] | IN("collab_panel", "auto_update", "onboarding", "feedback", "channel_modal", "call"))' \
  "$RUN/actions.json")
if [ -n "$bad_actions" ]; then
  echo "bongos: removed namespaces are still registered: $(xargs <<<"$bad_actions")" >&2
  exit 1
fi
say "smoke: $(jq '.actions | length' "$RUN/actions.json") actions registered, none in a removed namespace"

# ---------------------------------------------------------------------------------------------
step "Req 4: bongos leaves the Zed directories alone"
mkdir -p "$HOME/.config/zed" "$HOME/.local/share/zed/db" "$HOME/.local/share/zed/logs" "$HOME/.cache/zed"
printf '{ "ui_font_size": 17 }\n' >"$HOME/.config/zed/settings.json"
printf '[]\n' >"$HOME/.config/zed/keymap.json"
printf 'seed\n' >"$HOME/.local/share/zed/db/seed.txt"
printf 'seed\n' >"$HOME/.local/share/zed/logs/Zed.log"
printf 'seed\n' >"$HOME/.cache/zed/seed.txt"
hash_before=$(zed_tree_hash)
launch_ready
quit_app
hash_after=$(zed_tree_hash)
if [ "$hash_before" != "$hash_after" ]; then
  echo "bongos: the Zed directories changed during a bongos launch ($hash_before, then $hash_after)" >&2
  (cd "$HOME" && find .config/zed .local/share/zed .cache/zed -printf '%y %p %m %T@\n' | LC_ALL=C sort) >&2
  exit 1
fi
say "smoke: Zed directory tree hash unchanged ($hash_after)"

# ---------------------------------------------------------------------------------------------
step "Req 6, 14: keymap and settings hot reload, and the errors"
launch_ready
# Settings: change the theme while running; the two shots differ.
write_theme "One Dark"
poll 30 "the dark theme" brightness_is dark
wait_for_settled
shot theme-dark
write_theme "One Light"
poll 30 "the light theme" brightness_is light
wait_for_settled
shot theme-light
ae=$(compare -metric AE "$RUN/theme-dark.png" "$RUN/theme-light.png" null: 2>&1 || true)
ae=${ae%% *}
case $ae in
  '' | *[!0-9.e+]*) echo "bongos: compare printed '$ae', not a pixel count" >&2; exit 1 ;;
esac
if ! awk -v n="$ae" 'BEGIN { exit !(n + 0 > 0) }'; then
  echo "bongos: the dark and light theme shots are identical" >&2
  exit 1
fi
say "smoke: theme change alters the screen ($ae pixels differ)"
# Keymap: a new binding takes effect without a restart, and the key exits the app.
atomic_write "$KEYMAP_FILE" '[ { "bindings": { "ctrl-alt-q": "zed::Quit" } } ]'
expect_quit_by_binding
# Again, then invalid JSON in both files: the loaded binding stays, and the errors show.
launch_ready
act atomic_write "$KEYMAP_FILE" '{ not valid json'
shot keymap-error
frame_has_ui keymap-error
act atomic_write "$SETTINGS_FILE" '{ "theme": }'
shot settings-error
frame_has_ui settings-error
assert_file "$HOME/$BONGOS_LOG_PATH" 'Failed to load user settings'
say "smoke: errors on screen: $RUN/keymap-error.png $RUN/settings-error.png (the keymap error is a toast only, not in the log)"
expect_quit_by_binding
cp "$RUN/config-backup/settings.json" "$RUN/config-backup/keymap.json" "$CONFIG_DIR/"

# ---------------------------------------------------------------------------------------------
step "Req 9: the agent panel opens with the real key"
launch_ready traced
traced_win=$WIN
act keys ctrl+question
shot agent-panel
frame_has_ui agent-panel
say "smoke: agent panel screenshot for the orchestrator to read: $RUN/agent-panel.png"

# ---------------------------------------------------------------------------------------------
step "Req 7: no network beyond loopback, idle with the agent panel, extensions and settings open"
# Open the extensions page and the settings window. The screen may not change for these, and the
# panel opens are only the load; the trace is what is asserted, so a missed open is a warning.
act keys ctrl+shift+x || say "smoke: WARN the extensions page did not change the screen"
act keys ctrl+comma || say "smoke: WARN the settings window did not change the screen"
shot panels-open
idle_end=$((SECONDS + 120))
while [ "$SECONDS" -lt "$idle_end" ]; do
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    echo "bongos: the app exited during the 120 s idle" >&2
    exit 1
  fi
  read -r -t 10 <> <(:) || true
done
xdo windowfocus "$traced_win" || true
quit_app
connects=$(grep -c 'connect(' "$RUN/connect.trace" || true)
if [ "$connects" -lt 1 ]; then
  echo "bongos: strace recorded no connect calls; the trace proves nothing" >&2
  exit 1
fi
stray=$(grep -E 'sa_family=AF_INET6?[,}]' "$RUN/connect.trace" \
  | grep -vE 'inet_addr\("127\.|inet_pton\(AF_INET6, "::1"|inet_pton\(AF_INET6, "::ffff:127\.' || true)
if [ -n "$stray" ]; then
  echo "bongos: connect to a non-loopback address:" >&2
  printf '%s\n' "$stray" | head -n 20 >&2
  exit 1
fi
say "smoke: $connects connect calls traced; every IP one is loopback"
assert_no_zed_hosts
assert_no_marketplace_host
say "smoke: no Zed host in proxy.log, no blocked request in the log"
# Req 9: nothing in the log asks to sign in.
if grep -qiE 'sign[ _-]?in' "$HOME/$BONGOS_LOG_PATH"; then
  echo "bongos: the log mentions signing in:" >&2
  grep -iE 'sign[ _-]?in' "$HOME/$BONGOS_LOG_PATH" | head -n 10 >&2
  exit 1
fi
say "smoke: no sign-in text in the log"

# ---------------------------------------------------------------------------------------------
step "Req 3: bongos and the stock binary together"
if [ ! -x "$STOCK_BIN" ]; then
  say "smoke: SKIP Req 3: no stock binary at $STOCK_BIN (run script/bongos/stock-build)"
else
  mkdir -p "$HOME/.config/zed"
  printf '{ "ui_font_size": 17 }\n' >"$HOME/.config/zed/settings.json"
  cp "$CONFIG_DIR/global_settings.json" "$HOME/.config/zed/global_settings.json"
  atomic_write "$SETTINGS_FILE" '{ "ui_font_size": 18 }'
  launch_ready
  bongos_win=$WIN
  "$STOCK_BIN" >>"$RUN/stock.out" 2>&1 &
  STOCK_PID=$!
  stock_window() { STOCK_WIN=$(xdo search --onlyvisible --class dev.zed.Zed 2>/dev/null | tail -n1); [ -n "$STOCK_WIN" ]; }
  poll 120 "a window of class dev.zed.Zed" stock_window
  if [ "$STOCK_WIN" = "$bongos_win" ]; then
    echo "bongos: the stock and bongos windows are the same window" >&2
    exit 1
  fi
  say "smoke: two classes at once: dev.bongos.Bongos ($bongos_win) and dev.zed.Zed ($STOCK_WIN)"
  # Separate settings: each has its own file and marker.
  expect_font_sizes
  # Both take keys: each one quits on ctrl+q while holding the focus.
  xdo windowfocus --sync "$bongos_win"
  quit_app
  xdo windowfocus --sync "$STOCK_WIN"
  keys ctrl+q
  if ! poll 30 "the stock binary to quit" pid_gone "$STOCK_PID"; then
    kill -TERM "$STOCK_PID" 2>/dev/null || true
    echo "bongos: the stock binary did not take ctrl+q" >&2
    exit 1
  fi
  wait "$STOCK_PID" 2>/dev/null || true
  STOCK_PID=""
  expect_font_sizes
  say "smoke: both took ctrl+q; the settings files stayed apart"
  say "smoke: hosts the stock run reached (informational): $(zed_hosts | xargs || true)"
  cp "$RUN/config-backup/settings.json" "$RUN/config-backup/keymap.json" "$CONFIG_DIR/"
fi
