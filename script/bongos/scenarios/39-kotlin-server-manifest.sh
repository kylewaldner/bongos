# Scenario for feature 39-kotlin-server-manifest. Sourced by script/bongos/smoke after the base flow
# (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only: the app trusts the test key of
# BONGOS_KOTLIN_MANIFEST_KEY and reads the pinned manifest from BONGOS_KOTLIN_SERVERS.
#
# Setup: script/bongos/fixtures/fake_manifest_host.py serves signed manifests and fake server
# archives from 127.0.0.1 and logs each request to $RUN/fake-manifest-host.log (the app reaches it
# through NO_PROXY=127.0.0.1, as scenario 28 does, so the logging proxy sees nothing of it). The
# test key comes from `bongos-kotlin-manifest keygen`. The embedded pin (fixtures/kotlin-servers/
# make-archives, then dated in the past) is expired, so no build runs until a manifest build is
# installed. The seeded lsp.kotlin.binary.path is dropped, as in scenario 02, or the app would skip
# the Kotlin manager. kotlin.manifest_url is written to settings.json and changed while the app runs
# (a change checks at once).
#   1. Expired pin, signed manifest: the download of 0.0.2 shows its progress in the status item
#      (held at one third, then two thirds, by the fake host), the build installs as the trial, the
#      server starts and the status item reads ready, and the trial passes.
#   2. A manifest whose bytes changed after signing: rejected and logged, no archive requested, the
#      cached manifest and the running server unchanged.
#   3. A signed manifest whose archive was tampered with: the download fails its SHA-256 check, the
#      cache directory shows no trace of that version, the running server is unchanged.
#   4. A signed build whose server exits at once: its trial fails, the previous build runs again
#      and the failure is announced once (one trial line in kotlin.log, a toast in the frame).
#   5. An empty kotlin.manifest_url: the fake host and the logging proxy get no request.
#   6. `kotlin: cancel install` in the middle of a download: the host sees the client leave, and
#      the install root is as it was before.
# The checks run in the order 1, 2, 3, 6, 4, 5, since 4 leaves a rejected build and ends the run of
# the app. Progress and the toast are read from screenshots (the status item's text changes between
# the two stages of the download; no step reads the text itself); everything else from kotlin.log,
# the fake host's log, the install root and the pid of the fake Kotlin server.
# The scenario leaves the app quit and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 39-kotlin-server-manifest skipped under BONGOS_STOCK=1: the stock run has no Kotlin manager"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
GLOBAL_SETTINGS_FILE="$CONFIG_DIR/global_settings.json"
KOTLIN_LOG="$HOME/${BONGOS_LOG_PATH%/*}/kotlin.log"
# <data dir>/languages/kotlin is the install root the app passes to bongos_kotlin::init.
INSTALL_ROOT="$HOME/${BONGOS_LOG_PATH%/logs/*}/languages/kotlin"
SERVER_DIR="$INSTALL_ROOT/kotlin-lsp"
KOTLIN_FIXTURES="$RUN/kotlin-servers"
HOST_ROOT="$RUN/manifest-host"
HOST_LOG="$RUN/fake-manifest-host.log"
HOST_PORT_FILE="$RUN/fake-manifest-host.port"
HOST_PID=""
KEY_FILE="$RUN/manifest-test-key.pk8"
MANIFEST_TOOL="$CARGO_TARGET_DIR/debug/bongos-kotlin-manifest"
PAD_BYTES=3145728
HOLD_FIRST=1310720
HOLD_SECOND=2621440

for tool in jq sha256sum tar curl python3 pgrep convert; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done
for binary in "$BONGOS_FAKE_KOTLIN_SERVER" "$MANIFEST_TOOL"; do
  [ -x "$binary" ] || { echo "bongos: no $binary; run script/bongos/build" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# The settings of other scenarios and the base flow come back on every exit; the smoke trap waits
# for every child, so the fake host goes first, with the exit status kept.
mkdir -p "$RUN/config-backup"
cp "$GLOBAL_SETTINGS_FILE" "$RUN/config-backup/global_settings.json"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-39.json"
scenario_exit() {
  local rc=$?
  set +e
  [ -z "${HOST_PID:-}" ] || kill "$HOST_PID" 2>/dev/null
  cp "$RUN/config-backup/global_settings.json" "$GLOBAL_SETTINGS_FILE"
  cp "$RUN/config-backup/settings-39.json" "$SETTINGS_FILE"
  unset BONGOS_KOTLIN_SERVERS BONGOS_KOTLIN_MANIFEST_KEY NO_PROXY
  (exit "$rc")
  cleanup
}
trap scenario_exit EXIT

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

kotlin_log_has() { file_has "$KOTLIN_LOG" "$1"; }

wait_for_kotlin_log() { poll "${2:-60}" "kotlin.log to match $1" kotlin_log_has "$1"; }

# The number of lines of kotlin.log that match the extended pattern <$1>.
kotlin_log_count() { grep -c -E -- "$1" "$KOTLIN_LOG" || true; }

# Lines of kotlin.log after the mark the last mark_kotlin_log took.
KOTLIN_LOG_MARK=0
mark_kotlin_log() { KOTLIN_LOG_MARK=$(wc -l <"$KOTLIN_LOG"); }
kotlin_log_since_mark() { tail -n +"$((KOTLIN_LOG_MARK + 1))" "$KOTLIN_LOG"; }

# A ready status after the line of the failed trial, in the lines since the mark.
ready_after_trial_failure() {
  kotlin_log_since_mark | awk '/trial: 0\.0\.6 failed/ {failed = 1} failed && /state .* kotlin-lsp ready/ {found = 1} END {exit !found}'
}

# --- The fake manifest host -------------------------------------------------------------------

start_host() {
  rm -f "$HOST_PORT_FILE"
  python3 -I -u "$BONGOS_ROOT/script/bongos/fixtures/fake_manifest_host.py" "$HOST_ROOT" "$HOST_PORT_FILE" "$HOST_LOG" \
    >>"$RUN/fake-manifest-host.out" 2>&1 &
  HOST_PID=$!
  poll 15 "the fake manifest host" test -s "$HOST_PORT_FILE"
  HOST_PORT=$(cat "$HOST_PORT_FILE")
}

# host_control <json>: changes the fake host; the request is not logged.
host_control() {
  curl -s -f --noproxy '*' -X POST --data "$1" "http://127.0.0.1:$HOST_PORT/control" ||
    fail "the fake manifest host refused $1"
}

host_hold() { host_control "{\"op\": \"hold\", \"path\": \"$1\", \"bytes\": $2}" >/dev/null; }
host_release() { host_control "{\"op\": \"hold\", \"path\": \"$1\", \"bytes\": null}" >/dev/null; }

# host_field <path> <sent|open>: what the host says of the newest response of the path.
host_field() { host_control "{\"op\": \"sent\", \"path\": \"$1\"}" | jq -r ".$2"; }

host_sent_at_least() { [ "$(host_field "$1" sent)" -ge "$2" ]; }
host_open_is() { [ "$(host_field "$1" open)" = "$2" ]; }

# The number of requests (not events) of the host's log that match the extended pattern <$1>.
host_requests() { grep -v '^#' "$HOST_LOG" | grep -c -E -- "${1:-.}" || true; }

host_has_event() { grep -qE -- "^# $1" "$HOST_LOG"; }

# --- Archives and signed manifests --------------------------------------------------------------

# build_archive <file> <ok|broken> <pad bytes>: a fake server like fixtures/kotlin-servers/
# make-archives builds, optionally with an incompressible file, so a download spans several
# progress steps. A broken one is a launcher that exits at once.
build_archive() {
  local out=$1 mode=$2 pad=$3 root
  root=$(mktemp -d "$RUN/archive.XXXXXX")
  mkdir -p "$root/bin"
  if [ "$mode" = broken ]; then
    printf '#!/bin/sh\nexit 1\n' >"$root/bin/launcher.sh"
  else
    cat >"$root/bin/launcher.sh" <<LAUNCHER
#!/bin/sh
BONGOS_FAKE_LSP_MODE="\${BONGOS_FAKE_LSP_MODE:-progress,push}"
export BONGOS_FAKE_LSP_MODE
exec "$BONGOS_FAKE_KOTLIN_SERVER" "\$@"
LAUNCHER
  fi
  chmod +x "$root/bin/launcher.sh"
  if [ "$pad" -gt 0 ]; then
    head -c "$pad" /dev/urandom >"$root/pad.bin"
  fi
  tar -C "$root" --owner=0 --group=0 --sort=name -cf - . | gzip -n >"$out"
  rm -rf "$root"
}

# write_manifest <dir> <serial> <version=archive file under a/>...: <dir>/servers.json with one
# kotlin-lsp entry per argument, signed with the test key into servers.json.sig. Archive URLs name
# the fake host; the digests are those of the files as they are now.
write_manifest() {
  local dir=$1 serial=$2 spec entries='[]'
  shift 2
  mkdir -p "$HOST_ROOT/$dir"
  for spec in "$@"; do
    entries=$(jq -c \
      --arg version "${spec%%=*}" \
      --arg url "http://127.0.0.1:$HOST_PORT/a/${spec#*=}" \
      --arg sha "$(sha256sum "$HOST_ROOT/a/${spec#*=}" | cut -d' ' -f1)" \
      '. + [{version: $version, built: "2026-11-01", expires: "2099-12-31", requires: 1,
             platform: (["linux-x64", "linux-arm64", "macos-arm64", "macos-x64"]
               | map({(.): {url: $url, sha256: $sha, archive: "tar.gz", strip_components: 0, launcher: "bin/launcher.sh"}})
               | add)}]' <<<"$entries")
  done
  jq -n --argjson serial "$serial" --argjson entries "$entries" \
    '{schema: 1, kind: "bongos-kotlin-servers", serial: $serial, published: "2026-11-12", "kotlin-lsp": $entries}' \
    >"$HOST_ROOT/$dir/servers.json"
  "$MANIFEST_TOOL" sign "$KEY_FILE" "$KEY_ID" "$HOST_ROOT/$dir/servers.json" >/dev/null
}

# set_manifest_url <path on the host, or empty>: kotlin.manifest_url in settings.json.
set_manifest_url() {
  local url=""
  [ -z "$1" ] || url="http://127.0.0.1:$HOST_PORT/$1"
  atomic_write "$SETTINGS_FILE" "$(jq -n -c --arg url "$url" '{kotlin: {server: "kotlin-lsp", manifest_url: $url}}')"
}

# The serial of the manifest in the cache, or nothing.
cached_serial() {
  jq -r '.servers' "$INSTALL_ROOT/manifest/cache.json" 2>/dev/null | base64 -d 2>/dev/null | jq -r '.serial' 2>/dev/null || true
}

# The names in the server directory, one per line. The lock file `.<version>.lock` of each version
# an install touched stays by design and is not listed: it is no build, partial file or marker.
server_dir_names() { ls -A "$SERVER_DIR" 2>/dev/null | grep -v -E '^\.[0-9.]+\.lock$' | sort || true; }

# The pid of the fake Kotlin server the app runs (one worktree, one server).
server_pid() { pgrep -f bongos-fake-kotlin-server | sort -n | head -n1; }

# The status item's text (without its spinner, which turns while a download runs) as a digest: the
# bottom left of the screen. The screen never settles during a download, so progress is read here.
status_text_hash() {
  DISPLAY="$BONGOS_DISPLAY" import -window root -crop 260x24+163+746 +repage rgb:- | md5sum
}
status_text_is_not() { [ "$(status_text_hash)" != "$1" ]; }
# Succeeds when the text is the same as at the previous call (the app drew what it had to).
LAST_STATUS_TEXT=""
status_text_stable() {
  local now
  now=$(status_text_hash)
  [ "$now" = "$LAST_STATUS_TEXT" ] && return 0
  LAST_STATUS_TEXT=$now
  return 1
}

# Fails when the proxy got a request since the scenario began other than the two kinds every launch
# makes (the ACP registry and localhost, as in scenarios 28, 03, 25 and 26), which rules out the
# manifest host, the bongos site and every archive host (the fake host is reached with NO_PROXY).
assert_proxy_strays_none() {
  local registry_host strays
  registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
    "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
  [ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
  strays=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log" |
    awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
  [ -z "$strays" ] || fail "the proxy got other requests during the scenario: $strays"
}

# Runs a command of the palette (VS Code keymap here) by its name.
palette() {
  act keys ctrl+shift+p
  act type_text "$1"
  keys Return
  wait_for_settled
}

# ---------------------------------------------------------------------------------------------
step "Manifest fixtures: test key, fake host, signed manifests, expired pin, no override of the language server"
# The seeded lsp.kotlin.binary.path would make the app skip the Kotlin manager.
jq 'del(.lsp)' "$RUN/config-backup/global_settings.json" >"$GLOBAL_SETTINGS_FILE"
keygen_out=$("$MANIFEST_TOOL" keygen --id smoke-test "$KEY_FILE")
KEY_ID=smoke-test
TRUSTED_KEY=$(sed -n 's/^trusted key: //p' <<<"$keygen_out")
[ -n "$TRUSTED_KEY" ] || fail "keygen printed no trusted key: $keygen_out"
export BONGOS_KOTLIN_MANIFEST_KEY="$TRUSTED_KEY"
export NO_PROXY=127.0.0.1

# The pin: 0.0.1, expired long ago. Its archive URLs name a port nothing listens on and are never
# fetched. make-archives also leaves the archive and its tampered copy that check 3 uses.
"$BONGOS_ROOT/script/bongos/fixtures/kotlin-servers/make-archives" "$KOTLIN_FIXTURES" "$BONGOS_FAKE_KOTLIN_SERVER" 9 >/dev/null
sed -i 's/^version = "0\.0\.1"$/&\nbuilt = "2019-12-01"\nexpires = "2020-01-01"/' "$KOTLIN_FIXTURES/servers.toml"
grep -q '^expires = "2020-01-01"$' "$KOTLIN_FIXTURES/servers.toml" || fail "the fixture pin has no expiry"
export BONGOS_KOTLIN_SERVERS="$KOTLIN_FIXTURES/servers.toml"

mkdir -p "$HOST_ROOT/a"
build_archive "$HOST_ROOT/a/b2.tar.gz" ok "$PAD_BYTES"
cp "$KOTLIN_FIXTURES/kotlin-lsp.tar.gz" "$HOST_ROOT/a/b4.tar.gz"
build_archive "$HOST_ROOT/a/b5.tar.gz" ok "$PAD_BYTES"
build_archive "$HOST_ROOT/a/b6.tar.gz" broken 0
cp "$HOST_ROOT/a/b2.tar.gz" "$HOST_ROOT/a/b9.tar.gz"
start_host
[ ! -s "$HOST_LOG" ] || fail "the fake manifest host has a request before the app started: $(cat "$HOST_LOG")"
write_manifest m1 10 0.0.2=b2.tar.gz
write_manifest m2 20 0.0.2=b2.tar.gz 0.0.9=b9.tar.gz
# One byte of the signed manifest changes after signing.
sed -i 's/"serial": 20/"serial": 21/' "$HOST_ROOT/m2/servers.json"
write_manifest m3 30 0.0.2=b2.tar.gz 0.0.4=b4.tar.gz
write_manifest m4 40 0.0.2=b2.tar.gz 0.0.5=b5.tar.gz
write_manifest m5 50 0.0.2=b2.tar.gz 0.0.6=b6.tar.gz
# The archive of 0.0.4 is the tampered one; the manifest holds the digest of the genuine file.
host_control "{\"op\": \"replace\", \"path\": \"/a/b4.tar.gz\", \"file\": \"$KOTLIN_FIXTURES/kotlin-lsp.tampered.tar.gz\"}" >/dev/null
rm -f "$KOTLIN_LOG"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")

# ---------------------------------------------------------------------------------------------
step "Manifest 1: expired pin, signed manifest: the download shows progress, the new build starts and is ready"
host_hold /a/b2.tar.gz "$HOLD_FIRST"
set_manifest_url m1/servers.json
launch_ready
wait_for_kotlin_log 'state .* kotlin-lsp failed: server expired'
wait_for_kotlin_log 'manifest: fetched serial 10 \(host 127\.0\.0\.1\)'
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.2 started'
poll 30 "the first third of the archive sent" host_sent_at_least /a/b2.tar.gz "$HOLD_FIRST"
LAST_STATUS_TEXT=""
poll 30 "the status item to show the first third" status_text_stable
shot kotlin-download-first
frame_has_ui kotlin-download-first
FIRST_TEXT=$(status_text_hash)
host_hold /a/b2.tar.gz "$HOLD_SECOND"
poll 30 "the status item to show more of the download" status_text_is_not "$FIRST_TEXT"
LAST_STATUS_TEXT=""
poll 30 "the status item to show the second stage" status_text_stable
shot kotlin-download-second
frame_has_ui kotlin-download-second
host_release /a/b2.tar.gz
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.2 installed as the trial'
wait_for_kotlin_log 'restart .*: kotlin-lsp 0\.0\.2 replaces the expired build'
wait_for_kotlin_log 'trial: 0\.0\.2 started'
wait_for_kotlin_log 'kotlin-lsp 0\.0\.2 \(trial\)'
wait_for_kotlin_log 'state .* kotlin-lsp ready'
wait_for_settled
shot kotlin-status-manifest-ready
frame_has_ui kotlin-status-manifest-ready
[ "$(host_requests '^GET /m1/servers\.json 200$')" = 1 ] || fail "the manifest was not fetched exactly once: $(cat "$HOST_LOG")"
[ "$(host_requests '^GET /m1/servers\.json\.sig 200$')" = 1 ] || fail "the signature was not fetched exactly once: $(cat "$HOST_LOG")"
[ "$(host_requests '^GET /a/b2\.tar\.gz 200$')" = 1 ] || fail "the archive was not fetched exactly once: $(cat "$HOST_LOG")"
[ "$(cached_serial)" = 10 ] || fail "the cache does not hold serial 10: $(cached_serial)"
wait_for_kotlin_log 'trial: 0\.0\.2 passed' 120
SERVER_PID=$(server_pid)
[ -n "$SERVER_PID" ] || fail "no fake Kotlin server runs after the trial passed"
BASE_NAMES=$(server_dir_names)
say "smoke: manifest 1, download in two stages and the ready status item: $RUN/kotlin-download-first.png, $RUN/kotlin-download-second.png, $RUN/kotlin-status-manifest-ready.png"

# ---------------------------------------------------------------------------------------------
step "Manifest 2: a manifest changed after signing is rejected and logged, the running build keeps running"
mark_kotlin_log
set_manifest_url m2/servers.json
wait_for_kotlin_log 'manifest: rejected: .*signature .*\(host 127\.0\.0\.1\)'
wait_for_settled
[ "$(host_requests '^GET /m2/servers\.json 200$')" = 1 ] || fail "the tampered manifest was not fetched: $(cat "$HOST_LOG")"
[ "$(host_requests '/a/b9')" = 0 ] || fail "the rejected manifest's archive was requested: $(cat "$HOST_LOG")"
[ "$(cached_serial)" = 10 ] || fail "the cache changed to serial $(cached_serial) after a rejected manifest"
[ "$(server_dir_names)" = "$BASE_NAMES" ] || fail "the install root changed after a rejected manifest: $(server_dir_names)"
[ "$(server_pid)" = "$SERVER_PID" ] || fail "the running server changed after a rejected manifest"
if kotlin_log_since_mark | grep -qE 'download:|start |restart|exit|crash|state .* (starting|failed)'; then
  fail "kotlin.log shows activity after a rejected manifest: $(kotlin_log_since_mark)"
fi
shot kotlin-manifest-rejected
frame_has_ui kotlin-manifest-rejected

# ---------------------------------------------------------------------------------------------
step "Manifest 3: a tampered archive is rejected and leaves no version directory"
mark_kotlin_log
set_manifest_url m3/servers.json
wait_for_kotlin_log 'manifest: fetched serial 30 \(host 127\.0\.0\.1\)'
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.4 failed: .*SHA-256'
wait_for_settled
[ "$(host_requests '^GET /a/b4\.tar\.gz 200$')" = 1 ] || fail "the tampered archive was not fetched once: $(cat "$HOST_LOG")"
[ "$(server_dir_names)" = "$BASE_NAMES" ] || fail "the install root shows a trace of the tampered build: $(server_dir_names)"
if [ -e "$SERVER_DIR/0.0.4" ]; then fail "$SERVER_DIR/0.0.4 exists"; fi
if kotlin_log_since_mark | grep -qE 'installed as the trial|trial: |restart|crash|state .* (starting|failed)'; then
  fail "kotlin.log shows a trial or a restart after a tampered archive: $(kotlin_log_since_mark)"
fi
[ "$(server_pid)" = "$SERVER_PID" ] || fail "the running server changed after a tampered archive"
shot kotlin-archive-rejected
frame_has_ui kotlin-archive-rejected

# ---------------------------------------------------------------------------------------------
step "Manifest 6: cancel in the middle of a download leaves the cache clean"
mark_kotlin_log
host_hold /a/b5.tar.gz "$HOLD_FIRST"
set_manifest_url m4/servers.json
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.5 started'
poll 30 "part of the archive sent" host_sent_at_least /a/b5.tar.gz "$HOLD_FIRST"
LAST_STATUS_TEXT=""
poll 30 "the status item to show the download" status_text_stable
palette 'kotlin: cancel install'
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.5 cancel requested'
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.5 cancelled'
poll 30 "the host to see the client leave" host_has_event 'aborted /a/b5\.tar\.gz'
host_release /a/b5.tar.gz
[ "$(server_dir_names)" = "$BASE_NAMES" ] || fail "the install root changed after a cancel: $(server_dir_names)"
if [ -e "$SERVER_DIR/0.0.5" ]; then fail "$SERVER_DIR/0.0.5 exists"; fi
if kotlin_log_since_mark | grep -qE 'installed as the trial|trial: |restart|crash|state .* (starting|failed)'; then
  fail "kotlin.log shows a trial or a restart after a cancel: $(kotlin_log_since_mark)"
fi
[ "$(server_pid)" = "$SERVER_PID" ] || fail "the running server changed after a cancel"
shot kotlin-download-cancelled
frame_has_ui kotlin-download-cancelled

# ---------------------------------------------------------------------------------------------
step "Manifest 4: a newer build whose server exits at once goes back to the previous build, with one notification"
mark_kotlin_log
set_manifest_url m5/servers.json
wait_for_kotlin_log 'download: kotlin-lsp 0\.0\.6 installed as the trial'
# The running build is not expired, so it picks the trial up at its next start.
palette 'kotlin: restart server'
wait_for_kotlin_log 'trial: 0\.0\.6 started'
wait_for_kotlin_log 'trial: 0\.0\.6 failed: .*; using kotlin-lsp 0\.0\.2'
wait_for_kotlin_log 'kotlin-lsp 0\.0\.2 \(downloaded\)'
poll 60 "the previous build to be ready again" ready_after_trial_failure
wait_for_settled
shot kotlin-trial-failed-toast
frame_has_ui kotlin-trial-failed-toast
[ "$(kotlin_log_since_mark | grep -c -E 'trial: 0\.0\.6 failed')" = 1 ] || fail "the failed trial was not announced exactly once: $(kotlin_log_since_mark)"
if kotlin_log_since_mark | grep -qE 'crash'; then
  fail "the failed trial was handled as a crash: $(kotlin_log_since_mark)"
fi
[ -n "$(server_pid)" ] || fail "no fake Kotlin server runs after the failed trial"
say "smoke: the failed trial notice: $RUN/kotlin-trial-failed-toast.png"
quit_app

# ---------------------------------------------------------------------------------------------
step "Manifest 5: an empty kotlin.manifest_url makes no request at all"
HOST_REQUESTS_BEFORE=$(host_requests)
rm -f "$KOTLIN_LOG"
set_manifest_url ""
launch_ready
wait_for_kotlin_log 'manifest: off \(kotlin\.manifest_url is empty\)'
wait_for_settled
[ "$(host_requests)" = "$HOST_REQUESTS_BEFORE" ] || fail "the fake manifest host got requests with an empty URL: $(tail -n +"$((HOST_REQUESTS_BEFORE + 1))" "$HOST_LOG")"
assert_proxy_strays_none
assert_no_manifest_host
shot kotlin-manifest-off
frame_has_ui kotlin-manifest-off
quit_app
say "smoke: the fake host logged $HOST_REQUESTS_BEFORE requests in all, none with an empty URL, and the proxy got only the requests of every launch ($HOST_LOG)"
cp "$HOST_LOG" "$RUN/fake-manifest-host-final.log"

# ---------------------------------------------------------------------------------------------
# The user manifest file: <config dir>/kotlin-server-manifest.toml. BONGOS_KOTLIN_SERVERS wins over
# it, so it is dropped here; the built-in table is then the real one, whose builds are never
# fetched (the file replaces the kotlin-lsp table whole, and the builds already installed above
# stay on disk). The signed manifest is off (empty kotlin.manifest_url).
USER_MANIFEST="$CONFIG_DIR/kotlin-server-manifest.toml"
unset BONGOS_KOTLIN_SERVERS
build_archive "$HOST_ROOT/a/b7.tar.gz" ok 0
build_archive "$HOST_ROOT/a/b8.tar.gz" ok 0

# write_user_manifest <version> <archive file under a/> <sha256>: a kotlin-lsp entry for every
# platform, with a loopback URL of the fake host.
write_user_manifest() {
  local version=$1 file=$2 sha=$3 platform
  {
    printf '[kotlin-lsp]\nversion = "%s"\nexpires = "2099-12-31"\n\n' "$version"
    for platform in linux-x64 linux-arm64 macos-arm64 macos-x64; do
      printf '[kotlin-lsp.platform.%s]\nurl = "http://127.0.0.1:%s/a/%s"\nsha256 = "%s"\narchive = "tar.gz"\nstrip_components = 0\nlauncher = "bin/launcher.sh"\n\n' \
        "$platform" "$HOST_PORT" "$file" "$sha"
    done
  } >"$USER_MANIFEST"
}

step "User manifest 1: a valid file with a loopback URL and a correct sha256 installs its build"
write_user_manifest 0.0.7 b7.tar.gz "$(sha256sum "$HOST_ROOT/a/b7.tar.gz" | cut -d' ' -f1)"
rm -f "$KOTLIN_LOG"
launch_ready
wait_for_kotlin_log 'state .* kotlin-lsp installing'
wait_for_kotlin_log 'state .* kotlin-lsp ready' 120
wait_for_settled
[ "$(host_requests '^GET /a/b7\.tar\.gz 200$')" = 1 ] || fail "the user manifest build was not fetched once: $(cat "$HOST_LOG")"
if kotlin_log_has 'ignoring the user manifest'; then fail "kotlin.log says the valid user manifest was ignored"; fi
[ -d "$SERVER_DIR/0.0.7" ] || fail "$SERVER_DIR/0.0.7 is not installed"
shot kotlin-user-manifest-installed
frame_has_ui kotlin-user-manifest-installed
quit_app

step "User manifest 2: a wrong sha256 is refused and nothing is installed"
write_user_manifest 0.0.8 b8.tar.gz "0000000000000000000000000000000000000000000000000000000000000000"
rm -f "$KOTLIN_LOG"
launch_ready
wait_for_kotlin_log 'state .* kotlin-lsp failed: .*SHA-256' 120
wait_for_settled
if [ -e "$SERVER_DIR/0.0.8" ]; then fail "$SERVER_DIR/0.0.8 exists after a wrong sha256"; fi
if kotlin_log_has 'kotlin-lsp ready'; then fail "a server started after a wrong sha256: $(cat "$KOTLIN_LOG")"; fi
shot kotlin-user-manifest-refused
frame_has_ui kotlin-user-manifest-refused
quit_app

step "User manifest 3: a malformed file is ignored with a toast, a log line and a status menu line"
printf 'this is [not valid toml\n' >"$USER_MANIFEST"
rm -f "$KOTLIN_LOG"
launch_ready
wait_for_kotlin_log 'manifest: ignoring the user manifest, using the built-in one: .*kotlin-server-manifest\.toml'
wait_for_settled
[ "$(kotlin_log_count 'ignoring the user manifest')" = 1 ] || fail "the malformed file was not logged exactly once: $(cat "$KOTLIN_LOG")"
shot kotlin-user-manifest-toast
frame_has_ui kotlin-user-manifest-toast
# Open the status menu (the Kotlin status item, bottom left): its first line names the file.
xdo mousemove --window "$WIN" 200 758 click 1
wait_for_settled
shot kotlin-user-manifest-menu
frame_has_ui kotlin-user-manifest-menu
keys Escape
assert_no_manifest_host
quit_app
rm -f "$USER_MANIFEST"
say "smoke: user manifest file: $RUN/kotlin-user-manifest-installed.png, $RUN/kotlin-user-manifest-refused.png, $RUN/kotlin-user-manifest-toast.png, $RUN/kotlin-user-manifest-menu.png"

trap - EXIT
cp "$RUN/config-backup/global_settings.json" "$GLOBAL_SETTINGS_FILE"
cp "$RUN/config-backup/settings-39.json" "$SETTINGS_FILE"
kill "$HOST_PID" 2>/dev/null || true
HOST_PID=""
unset BONGOS_KOTLIN_SERVERS BONGOS_KOTLIN_MANIFEST_KEY NO_PROXY
trap cleanup EXIT
