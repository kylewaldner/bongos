# Shared helpers. Source after env.sh:  . script/bongos/env.sh; . script/bongos/lib.sh

BONGOS_ROOT="${BONGOS_ROOT:-$(cd -P "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

if [ "${BONGOS_STOCK:-}" = 1 ]; then
  BONGOS_BIN=${BONGOS_BIN:-$BONGOS_WORKSPACE/scratch/stock-zed/zed}
  BONGOS_CLASS=${BONGOS_CLASS:-dev.zed.Zed}
  BONGOS_TITLE=${BONGOS_TITLE:-project}
  BONGOS_LOG_PATH=.local/share/zed/logs/Zed.log
  BONGOS_CONFIG_PATH=.config/zed
fi
BONGOS_BIN=${BONGOS_BIN:-$CARGO_TARGET_DIR/debug/bongos}
BONGOS_CLASS=${BONGOS_CLASS:-dev.bongos.Bongos}
BONGOS_TITLE=${BONGOS_TITLE:-bongos}
BONGOS_LOG_PATH=${BONGOS_LOG_PATH:-.local/share/bongos/logs/bongos.log}
BONGOS_CONFIG_PATH=${BONGOS_CONFIG_PATH:-.config/bongos}
BONGOS_DISPLAY=${BONGOS_DISPLAY:-:99}
BONGOS_PROXY_PORT=${BONGOS_PROXY_PORT:-18080}

# Stop under 30 GB free on the target drive; drop incremental output under 40 GB.
require_space() {
  local avail_kb avail_gb
  avail_kb=$(df --output=avail -k "$CARGO_TARGET_DIR" | tail -n1 | tr -d ' ')
  avail_gb=$((avail_kb / 1024 / 1024))
  if [ "$avail_gb" -lt 30 ]; then
    echo "bongos: only ${avail_gb} GB free for $CARGO_TARGET_DIR; need at least 30 GB" >&2
    return 1
  fi
  if [ "$avail_gb" -lt 40 ]; then
    echo "bongos: ${avail_gb} GB free; removing target/debug/incremental" >&2
    rm -rf "$CARGO_TARGET_DIR/debug/incremental"
  fi
}

# Total memory in KB.
mem_total_kb() { awk '/^MemTotal:/ {print $2}' /proc/meminfo; }

# Run cargo in a memory-limited systemd scope, so an OOM ends cargo and not the desktop: the cap is
# 90% of total RAM and swap is 0. Falls back to plain cargo if the scope cannot start.
# BONGOS_NICE=<n> runs cargo under nice -n <n>.
cargo_limited() {
  local cmd=(cargo)
  [ -n "${BONGOS_NICE:-}" ] && cmd=(nice -n "$BONGOS_NICE" cargo)
  local cap=$(( $(mem_total_kb) * 9 / 10 ))K
  if systemd-run --user --scope -q -p "MemoryMax=$cap" -p MemorySwapMax=0 true >/dev/null 2>&1; then
    systemd-run --user --scope -q -p "MemoryMax=$cap" -p MemorySwapMax=0 "${cmd[@]}" "$@"
  else
    echo "bongos: systemd scope unavailable; running cargo without a memory limit" >&2
    "${cmd[@]}" "$@"
  fi
}

# Print the software (lavapipe) Vulkan driver file. Names differ by distro: lvp_icd.x86_64.json on
# Debian, lvp_icd.json on newer Arch. Prefers this machine's architecture, never the 32-bit file.
find_lvp_icd() {
  local d f arch found=""
  arch=$(uname -m)
  for d in /usr/share/vulkan/icd.d /etc/vulkan/icd.d; do
    for f in "$d/lvp_icd.$arch.json" "$d/lvp_icd.json"; do
      [ -f "$f" ] && { echo "$f"; return 0; }
    done
    for f in "$d"/lvp_icd*.json; do
      case "$f" in *i686*|*i386*) continue ;; esac
      [ -f "$f" ] && found=$f && break
    done
    [ -n "$found" ] && { echo "$found"; return 0; }
  done
  echo "bongos: no software Vulkan driver (lvp_icd*.json) in /usr/share/vulkan/icd.d or /etc/vulkan/icd.d;" >&2
  echo "bongos: install mesa-vulkan-drivers (Debian) or vulkan-swrast (Arch)" >&2
  return 1
}

# The upstream commit we are based on: commit= from UPSTREAM, else the upstream tag.
upstream_commit() {
  local f="$BONGOS_ROOT/UPSTREAM" c=""
  if [ -f "$f" ]; then
    c=$(sed -n 's/^commit=//p' "$f" | head -n1)
  fi
  echo "${c:-upstream/v1.9.0}"
}

# Package names of crates changed since the upstream commit.
changed_crates() {
  local base dirs meta
  base=$(upstream_commit)
  dirs=$(git -C "$BONGOS_ROOT" diff --name-only "$base" HEAD -- crates \
    | awk -F/ 'NF>=2 {print $1"/"$2}' | sort -u)
  [ -n "$dirs" ] || return 0
  meta=$(cd "$BONGOS_ROOT" && cargo metadata --no-deps --format-version 1 --locked)
  printf '%s\n' "$dirs" | while read -r d; do
    printf '%s' "$meta" | python3 -I -c '
import json, sys, os
root = sys.argv[1]; d = sys.argv[2]
m = json.load(sys.stdin)
for p in m["packages"]:
    if os.path.dirname(p["manifest_path"]) == os.path.join(root, d):
        print(p["name"])
' "$BONGOS_ROOT" "$d"
  done
}

# The rest is for script/bongos/smoke and its scenarios, which set RUN and run in bwrap.

# poll <seconds> <what> <command...>: wait for a command to succeed; give up early if the app dies.
poll() {
  local deadline=$((SECONDS + $1)) what=$2
  shift 2
  until "$@"; do
    if [ -n "${APP_PID:-}" ] && ! kill -0 "$APP_PID" 2>/dev/null; then
      echo "bongos: app exited while waiting for $what" >&2
      return 1
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "bongos: timed out waiting for $what" >&2
      return 1
    fi
    sleep 0.2
  done
}

# Every X client goes through here, so nothing can reach another display.
xdo() {
  if [ "${DISPLAY:-}" != "$BONGOS_DISPLAY" ] || [ "$DISPLAY" = ":0" ]; then
    echo "bongos: refusing xdotool on DISPLAY=${DISPLAY:-unset}" >&2
    return 1
  fi
  timeout 15 xdotool "$@"
}

# No Composite: it adds the 32-bit visual GPUI prefers, and lavapipe's frames on it stay black.
start_display() {
  Xvfb "$BONGOS_DISPLAY" -screen 0 1280x800x24 -ac -nolisten tcp -extension Composite \
    >>"$RUN/xvfb.out" 2>&1 &
  XVFB_PID=$!
  poll 15 "Xvfb" test -S "/tmp/.X11-unix/X${BONGOS_DISPLAY#:}"
}

start_proxy() {
  python3 -I "$BONGOS_ROOT/script/bongos/fixtures/log_proxy.py" "$BONGOS_PROXY_PORT" "$RUN/proxy.log" \
    >>"$RUN/proxy.out" 2>&1 &
  PROXY_PID=$!
  poll 15 "proxy" test -f "$RUN/proxy.log"
}

# Point this shell at the throwaway home, the virtual display, lavapipe and the proxy.
sandbox_env() {
  mkdir -p "$RUN/home" "$RUN/tmp"
  mkdir -p -m 700 "$RUN/xdg"
  # A short HOME, as the app's socket in it must fit sun_path (108 bytes). /tmp is private here.
  ln -sfn "$RUN/home" /tmp/h
  export HOME=/tmp/h TMPDIR="$RUN/tmp" XDG_RUNTIME_DIR="$RUN/xdg"
  export DISPLAY="$BONGOS_DISPLAY" XDG_SESSION_TYPE=x11
  # Default is software rendering, so results compare across machines. BONGOS_REAL_GPU=1 leaves the
  # GPU and the Vulkan driver choice to the app.
  if [ "${BONGOS_REAL_GPU:-}" != 1 ]; then
    export ZED_ALLOW_EMULATED_GPU=1
    VK_ICD_FILENAMES=$(find_lvp_icd) || return 1
    export VK_ICD_FILENAMES
    export VK_DRIVER_FILES=$VK_ICD_FILENAMES
  fi
  export ALL_PROXY="http://127.0.0.1:$BONGOS_PROXY_PORT" HTTPS_PROXY="http://127.0.0.1:$BONGOS_PROXY_PORT"
  export HTTP_PROXY="http://127.0.0.1:$BONGOS_PROXY_PORT"
  unset WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS NO_PROXY no_proxy RUST_LOG
  unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME
}

# The "Unrecognized Project" modal takes all keys, and a blinking cursor would hide when the UI has
# settled. global_settings.json fixes both, and leaves settings.json and keymap.json for the app
# to create.
seed_settings() {
  mkdir -p "$HOME/$BONGOS_CONFIG_PATH"
  cat >"$HOME/$BONGOS_CONFIG_PATH/global_settings.json" <<'JSON'
{
  "session": { "trust_all_worktrees": true },
  "cursor_blink": false,
  "terminal": { "blinking": "off" }
}
JSON
}

launch() {
  "$BONGOS_BIN" "$@" >>"$RUN/app.out" 2>&1 &
  APP_PID=$!
}

window_found() {
  WIN=$(xdo search --onlyvisible --class "$BONGOS_CLASS" 2>/dev/null | tail -n1)
  [ -n "$WIN" ]
}

wait_for_window() {
  poll 120 "a window of class $BONGOS_CLASS" window_found
  xdo windowfocus --sync "$WIN"
}

title_matches() { xdo getwindowname "$WIN" 2>/dev/null | grep -qE -- "$1"; }

wait_for_title() { poll 60 "title matching $1" title_matches "$1"; }

keys() { xdo key --delay 80 "$@"; }

type_text() { xdo type --delay 40 "$1"; }

shot() { DISPLAY="$BONGOS_DISPLAY" import -window root "$RUN/$1.png"; }

# Reject a blank or black frame. Reading the picture stays the orchestrator's job.
frame_has_ui() { [ "$(identify -format '%k' "$RUN/$1.png")" -ge 64 ]; }

screen_hash() { DISPLAY="$BONGOS_DISPLAY" import -window root rgb:- | md5sum; }

screen_changed() { [ "$(screen_hash)" != "$LAST_SCREEN" ]; }

screen_settled() {
  local now
  now=$(screen_hash)
  [ "$now" = "$LAST_SCREEN" ] && return
  LAST_SCREEN=$now
  sleep 0.3
  return 1
}

wait_for_settled() { LAST_SCREEN=""; poll 30 "the screen to settle" screen_settled; }

# A frame presented before the window is mapped is lost, and an idle UI never repaints, so the
# window stays black until something redraws it. Moving the pointer over it does.
ui_rendered() {
  xdo mousemove $((600 + RANDOM % 80)) 400
  sleep 0.5
  shot launch && frame_has_ui launch
}

# act <command...>: run an input command, then wait for its effect to reach the screen. Keys sent
# before a modal or panel takes focus are lost, and no title or file shows when that has happened.
act() {
  LAST_SCREEN=$(screen_hash)
  "$@"
  poll 30 "the screen to change after: $*" screen_changed
  wait_for_settled
}

file_has() { [ -f "$1" ] && { [ -z "${2:-}" ] || grep -qE -- "$2" "$1"; }; }

# assert_file <path> [pattern]: waits, since saves and shell commands land asynchronously.
assert_file() { poll 30 "$1${2:+ to match $2}" file_has "$1" "${2:-}"; }

# The app's child on a terminal: the shell of the terminal panel, which rules out helpers.
terminal_shell_up() { ps --ppid "$APP_PID" -o tty= | grep -q pts; }

wait_for_terminal() { poll 60 "a terminal shell" terminal_shell_up; }

pid_gone() { ! kill -0 "$1" 2>/dev/null; }

quit_app() {
  local pid=$APP_PID
  keys ctrl+q
  APP_PID=
  if ! poll 30 "the app to quit" pid_gone "$pid"; then
    kill -TERM "$pid"
    return 1
  fi
  wait "$pid"
}

zed_hosts() {
  awk '{print $2}' "$RUN/proxy.log" | grep -E '(^|\.)(zed\.dev|zed\.industries)$' | sort -u
}

# Warns until step 17, when it starts to fail.
assert_no_zed_hosts() {
  local hosts blocked
  hosts=$(zed_hosts | xargs || true)
  blocked=$(grep -c 'bongos_net: blocked' "$HOME/$BONGOS_LOG_PATH" || true)
  if [ -n "$hosts" ] || [ "$blocked" != 0 ]; then
    echo "bongos: warning: Zed hosts: ${hosts:-none}; blocked requests: $blocked" >&2
  fi
}

# Negative control: stock must reach Zed's hosts, or the detector proves nothing.
assert_zed_hosts() {
  poll 120 "a Zed host in proxy.log" zed_hosts >/dev/null
  echo "bongos: Zed hosts seen: $(zed_hosts | xargs)"
}
