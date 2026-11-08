# Scenario for feature 02-kotlin-lsp-profiles. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. It needs no network and no real server: both servers
# are the fake server (script/bongos/fixtures/kotlin-servers), unpacked into the install root of the
# throwaway home before the app starts, and the pinned manifest is the fixture one
# (BONGOS_KOTLIN_SERVERS, honored by debug builds only). Nothing is ever downloaded; the proxy log
# and kotlin.log are checked for that.
#   1. kotlin.server kotlin-lsp (the default): the status item reads "Kotlin: kotlin-lsp, ready".
#   2. kotlin.server intellij, the bundle installed, its EULA not accepted: bongos falls back to
#      kotlin-lsp with the toast "Kotlin: using kotlin-lsp, the IntelliJ server is not available
#      (IntelliJ EULA not accepted)" and the status item shows the fallback in the warning color.
#   3. kotlin.server intellij, kotlin.intellij.download false (the default), the bundle not
#      installed: no download, and the status item reads "failed: intellij is not installed".
# The scenario leaves the app quit and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 02-kotlin-lsp-profiles skipped under BONGOS_STOCK=1: the stock run has no Kotlin manager"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
GLOBAL_SETTINGS_FILE="$CONFIG_DIR/global_settings.json"
KOTLIN_LOG="$HOME/${BONGOS_LOG_PATH%/*}/kotlin.log"
# <data dir>/languages/kotlin is the install root the app passes to bongos_kotlin::init.
INSTALL_ROOT="$HOME/${BONGOS_LOG_PATH%/logs/*}/languages/kotlin"
KOTLIN_FIXTURES="$RUN/kotlin-servers"
FAKE_VERSION=0.0.1

for tool in jq sha256sum tar; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done
if [ ! -x "$BONGOS_FAKE_KOTLIN_SERVER" ]; then
  echo "bongos: no fake Kotlin server at $BONGOS_FAKE_KOTLIN_SERVER; run script/bongos/build" >&2
  exit 1
fi

# Write in place: a rename over the file would end the app's watch on it after the first write.
atomic_write() {
  printf '%s\n' "$2" >"$1"
}

# Launch with no arguments (the session restores the project) and wait until it is drawn.
launch_ready() {
  launch
  wait_for_window
  wait_for_title 'Main\.kt'
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# kotlin_log_has <extended regex>: a line of kotlin.log matches.
kotlin_log_has() { file_has "$KOTLIN_LOG" "$1"; }

wait_for_kotlin_log() { poll 60 "kotlin.log to match $1" kotlin_log_has "$1"; }

# Installs a fixture server the way the installer leaves it: the archive unpacked in
# <install root>/<server>/<version>, and the marker with the version and the archive digest.
preinstall() {
  local name=$1 dir="$INSTALL_ROOT/$1/$FAKE_VERSION"
  mkdir -p "$dir"
  tar -C "$dir" -xzf "$KOTLIN_FIXTURES/$name.tar.gz"
  printf '{"version":"%s","sha256":"%s"}\n' "$FAKE_VERSION" \
    "$(sha256sum "$KOTLIN_FIXTURES/$name.tar.gz" | cut -d' ' -f1)" >"$dir/.bongos-installed"
}

# Nothing may have reached JetBrains or any other host, and no install may have run.
assert_no_download() {
  if [ -s "$RUN/proxy.log" ]; then
    echo "bongos: a request reached the download proxy:" >&2
    cat "$RUN/proxy.log" >&2
    exit 1
  fi
  if grep -qE ' install |install error' "$KOTLIN_LOG"; then
    echo "bongos: kotlin.log shows an install, and every server was already installed:" >&2
    grep -E ' install |install error' "$KOTLIN_LOG" >&2
    exit 1
  fi
}

# ---------------------------------------------------------------------------------------------
step "Kotlin fixtures: fake servers installed, fixture manifest, no override of the language server"
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_SETTINGS_FILE" "$RUN/config-backup/global_settings.json"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-02.json"
# The seeded lsp.kotlin.binary.path would make the app skip the Kotlin manager.
jq 'del(.lsp)' "$RUN/config-backup/global_settings.json" >"$GLOBAL_SETTINGS_FILE"
# The archive URLs point at a port nothing listens on: they are never fetched.
"$BONGOS_ROOT/script/bongos/fixtures/kotlin-servers/make-archives" "$KOTLIN_FIXTURES" "$BONGOS_FAKE_KOTLIN_SERVER" 9 >/dev/null
export BONGOS_KOTLIN_SERVERS="$KOTLIN_FIXTURES/servers.toml"
preinstall kotlin-lsp
preinstall intellij
rm -f "$KOTLIN_LOG"

# ---------------------------------------------------------------------------------------------
step "Kotlin status item: kotlin-lsp is ready"
atomic_write "$SETTINGS_FILE" '{ "kotlin": { "server": "kotlin-lsp" } }'
launch_ready
wait_for_kotlin_log 'state .* kotlin-lsp ready'
wait_for_settled
shot kotlin-status-ready
frame_has_ui kotlin-status-ready
if kotlin_log_has 'fallback .* intellij -> kotlin-lsp'; then
  echo "bongos: kotlin.log shows a fallback with kotlin.server kotlin-lsp" >&2
  exit 1
fi
assert_no_download
quit_app
say "smoke: Kotlin status item, normal state: $RUN/kotlin-status-ready.png"

# ---------------------------------------------------------------------------------------------
step "Kotlin fallback: IntelliJ installed, EULA not accepted"
rm -f "$KOTLIN_LOG" "$HOME/.local/state/bongos/kotlin-eula.json"
atomic_write "$SETTINGS_FILE" '{ "kotlin": { "server": "intellij" } }'
launch_ready
wait_for_kotlin_log 'fallback .* intellij -> kotlin-lsp: IntelliJ EULA not accepted'
wait_for_kotlin_log 'state .* kotlin-lsp ready'
wait_for_settled
# The toast and the status item are in the same frame: the toast has the notice text, the status
# item "Kotlin: kotlin-lsp (fallback: IntelliJ EULA not accepted)" in the warning color.
shot kotlin-fallback-toast
frame_has_ui kotlin-fallback-toast
cp "$RUN/kotlin-fallback-toast.png" "$RUN/kotlin-status-fallback.png"
assert_no_download
quit_app
say "smoke: Kotlin fallback toast and status item: $RUN/kotlin-fallback-toast.png (copy: $RUN/kotlin-status-fallback.png)"

# ---------------------------------------------------------------------------------------------
step "Kotlin IntelliJ not installed and kotlin.intellij.download false: reported, never downloaded"
rm -rf "${INSTALL_ROOT:?}/intellij"
rm -f "$KOTLIN_LOG"
atomic_write "$SETTINGS_FILE" '{ "kotlin": { "server": "intellij", "intellij": { "download": false } } }'
launch_ready
wait_for_kotlin_log 'intellij download is off'
wait_for_kotlin_log 'state .* intellij failed: intellij is not installed'
wait_for_settled
shot kotlin-status-not-installed
frame_has_ui kotlin-status-not-installed
assert_no_download
if [ -e "$INSTALL_ROOT/intellij" ]; then
  echo "bongos: $INSTALL_ROOT/intellij exists; the IntelliJ server was installed with kotlin.intellij.download false" >&2
  exit 1
fi
quit_app
say "smoke: Kotlin status item, IntelliJ not installed: $RUN/kotlin-status-not-installed.png"

cp "$RUN/config-backup/global_settings.json" "$GLOBAL_SETTINGS_FILE"
cp "$RUN/config-backup/settings-02.json" "$SETTINGS_FILE"
unset BONGOS_KOTLIN_SERVERS
