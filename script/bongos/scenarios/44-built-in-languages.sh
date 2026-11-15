# Scenario for feature 44-built-in-languages. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only: the app reads the pinned jdtls
# and terraform-ls builds from BONGOS_KOTLIN_SERVERS.
#
# Setup: the language fixtures (script/bongos/fixtures/languages) are copied under $RUN/langs. Every
# language server is fake_lsp.py, which records what the app sends it: the inherited languages'
# servers through `lsp.<server>.binary.path`, jdtls and terraform-ls as fake archives (built here,
# pinned in a fixture manifest) that the app downloads from a loopback host (start_fixture_http,
# reached through NO_PROXY=127.0.0.1, so the logging proxy sees none of it). jdtls runs on a fake
# JDK 25 (`java.server_jdk`) whose `java` starts the fake server with jdtls's arguments.
#   1. Highlighting: one window with the rust, python-venv, react-vite, java-maven, java-gradle and
#      terraform worktrees; a shot of each of `.rs`, `Cargo.toml`, `.py`, `.tsx`, `.html`,
#      `pom.xml`, `.java` and `.tf`.
#   2. Java on java-gradle: the jdtls archive is held part way by the host, the shot shows the
#      download in the status line (languages.log has the 25% milestone and no install yet), then
#      the archive is released, jdtls is installed and starts with the server JDK, the launcher
#      jar and a -data directory outside the project; `java: show jdk` opens.
#   3. Terraform with no `terraform` or `tofu` (the app runs with /usr/bin/terraform masked): the
#      notice line, and terraform-ls still starts with CHECKPOINT_DISABLE=1 and no terraform.path.
#      A fake `terraform` put on PATH, then `editor: restart language server`: the notice clears
#      and terraform-ls gets the fake as terraform.path.
#   4. Failure: the host stopped and no cached jdtls: "Java server unavailable: could not download
#      jdtls", `languages: show logs` opens languages.log with the cause; the host back on the same
#      port, `languages: retry server`, jdtls installs and starts.
#   5. Offline: the host stopped and no proxy at all (the sandbox has no route), both servers
#      cached: Java and Terraform both start within 30 seconds of the window, with no download and no
#      failure.
#   6. Bun: the bun fixture's `package.json > test` task runs `bun run test` (fake bun, npm, npx
#      and node on PATH record their calls; only bun is called).
#   7. Real tools, offline, each when found on PATH: terraform (Format of a misformatted .tf through
#      terraform-ls, which runs the real `terraform fmt`), bun (`bun test` on the fixture), uv (a
#      `uv venv` the app picks for Python) and poetry (a `poetry env use` environment the app picks).
#      A missing tool prints a line beginning `OWED`.
#   8. The proxy got no request but the two every launch makes, no Zed host, and no language server
#      but jdtls and terraform-ls was downloaded.
# The scenario leaves the app quit and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 44-built-in-languages skipped under BONGOS_STOCK=1: the stock run has no bongos languages"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LANGUAGES_LOG="$HOME/${BONGOS_LOG_PATH%/*}/languages.log"
# <data dir>/languages, which the app passes to bongos_java and bongos_terraform.
LANG_DIR="$HOME/${BONGOS_LOG_PATH%/logs/*}/languages"
FAKE_LSP="$BONGOS_ROOT/script/bongos/fixtures/fake_lsp.py"
LANGS="$RUN/langs"
RECORDS="$RUN/lsp-records"
HOST_ROOT="$RUN/language-host"
FAKE_JDK="$RUN/fake-jdk"
FAKE_TF_BIN="$RUN/fake-terraform-bin"
FAKE_JS_BIN="$RUN/fake-js-bin"
JS_CALLS="$RUN/js-calls.log"
JDTLS_RECORD="$RECORDS/jdtls.jsonl"
TFLS_RECORD="$RECORDS/terraform-ls.jsonl"
PYRIGHT_RECORD="$RECORDS/basedpyright.jsonl"
JDTLS_LAUNCHER_JAR=plugins/org.eclipse.equinox.launcher_0.0.2.jar
PAD_BYTES=3145728
HOLD_BYTES=1310720

for tool in jq sha256sum tar gzip curl python3 bwrap; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done
[ -x "$BONGOS_FAKE_KOTLIN_SERVER" ] || { echo "bongos: no $BONGOS_FAKE_KOTLIN_SERVER; run script/bongos/build" >&2; exit 1; }

fail() {
  echo "bongos: $*" >&2
  exit 1
}

mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-44.json"
SAVED_PATH=$PATH
scenario_exit() {
  local rc=$?
  set +e
  stop_fixture_http
  cp "$RUN/config-backup/settings-44.json" "$SETTINGS_FILE"
  export PATH=$SAVED_PATH
  unset BONGOS_KOTLIN_SERVERS NO_PROXY
  (exit "$rc")
  cleanup
}
trap scenario_exit EXIT

# --- Helpers ----------------------------------------------------------------------------------

log_has() { file_has "$LANGUAGES_LOG" "$1"; }
wait_for_log() { poll "${2:-60}" "languages.log to match $1" log_has "$1"; }
LOG_MARK=0
mark_log() { LOG_MARK=$( [ -f "$LANGUAGES_LOG" ] && wc -l <"$LANGUAGES_LOG" || echo 0); }
log_since_mark() { [ -f "$LANGUAGES_LOG" ] && tail -n +"$((LOG_MARK + 1))" "$LANGUAGES_LOG" || true; }

# record_count <file> <event> [jq condition]: how many records of the fake server match.
record_count() {
  [ -f "$1" ] || { echo 0; return; }
  jq -s --arg event "$2" "map(select(.event == \$event and (${3:-true}))) | length" "$1"
}
record_count_above() { [ "$(record_count "$1" "$2" "${4:-true}")" -gt "$3" ]; }
# The newest record of an event, as JSON.
record_last() { jq -s -c --arg event "$2" 'map(select(.event == $event)) | last' "$1"; }

# Runs a command of the palette (VS Code keymap here) by its name.
palette() {
  act keys ctrl+shift+p
  act type_text "$1"
  keys Return
}

# open_file <path relative to $LANGS> <title pattern>: the file finder opens the file.
open_file() {
  act keys ctrl+p
  act type_text "$1"
  keys Return
  wait_for_title "$2"
}

title_lacks() { ! title_matches "$1"; }

window_ready() {
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# The app with /usr/bin/terraform (and tofu, if any) masked by an empty, non-executable file, so
# nothing on PATH is a terraform binary.
launch_without_terraform() {
  local masks=() binary
  for binary in /usr/bin/terraform /usr/bin/tofu /usr/local/bin/terraform /usr/local/bin/tofu; do
    [ -e "$binary" ] && masks+=(--ro-bind "$RUN/not-a-binary" "$binary")
  done
  bwrap --dev-bind / / --die-with-parent "${masks[@]}" -- "$BONGOS_BIN" "$@" >>"$RUN/app.out" 2>&1 &
  APP_PID=$!
}

# write_launcher <file> <server name> <record> [VAR=value...]: a script that runs fake_lsp.py.
write_launcher() {
  local file=$1 name=$2 record=$3 assignment
  shift 3
  mkdir -p "$(dirname "$file")"
  {
    printf '#!/bin/sh\nexport BONGOS_FAKE_LSP_NAME=%s BONGOS_FAKE_LSP_RECORD=%s\n' "$name" "$record"
    for assignment in "$@"; do printf 'export %s\n' "$assignment"; done
    printf 'exec python3 -I %s "$@"\n' "$FAKE_LSP"
  } >"$file"
  chmod +x "$file"
}

# write_recorder <dir> <tool>: a fake tool that appends `<tool> <args>` to $JS_CALLS.
write_recorder() {
  mkdir -p "$1"
  printf '#!/bin/sh\nprintf "%%s\\n" "%s $*" >>%s\n' "$2" "$JS_CALLS" >"$1/$2"
  chmod +x "$1/$2"
}

# The jdtls record's start arguments of the newest start, one per line.
jdtls_argv() { record_last "$JDTLS_RECORD" start | jq -r '.argv[]'; }

# The python.pythonPath basedpyright got for the worktree whose root ends with <name>: the answer
# to its configuration request, else the pushed settings.
pythonpath_for() {
  jq -s -r --arg root "/$1" '
    (map(select(.event == "initialize" and (.root_uri // "" | endswith($root)))) | map(.pid)) as $pids
    | map(select((.pid as $p | $pids | index($p)) != null))
    | (map(select(.event == "configuration") | .result[0].pythonPath // empty)
       + map(select(.event == "did_change_configuration") | .settings.python.pythonPath // empty))
    | last // empty' "$PYRIGHT_RECORD" 2>/dev/null
}
pythonpath_under() { case "$(pythonpath_for "$1")" in "$2"/*) return 0 ;; *) return 1 ;; esac; }

# Fails when the proxy got a request since the scenario began other than the two kinds every launch
# makes (the ACP registry and localhost, as in scenarios 04 and 28).
assert_proxy_strays_none() {
  local registry_host strays
  registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
    "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
  [ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
  strays=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log" |
    awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
  [ -z "$strays" ] || fail "the proxy got other requests during the scenario: $strays"
}

# ---------------------------------------------------------------------------------------------
step "Language fixtures: fixture copies, fake servers, fake JDK, fake archives on a loopback host"
mkdir -p "$LANGS" "$RECORDS" "$HOST_ROOT" "$FAKE_TF_BIN"
for name in rust python-venv python-uv python-poetry react-vite java-maven java-gradle terraform bun; do
  cp -r "$BONGOS_ROOT/script/bongos/fixtures/languages/$name" "$LANGS/$name"
done
: >"$RUN/not-a-binary"

# The servers the inherited languages would download; each is the fake instead.
lsp_settings='{}'
for server in rust-analyzer basedpyright ruff vtsls typescript-language-server eslint \
  tailwindcss-language-server tailwindcss-intellisense-css json-language-server \
  package-version-server yaml-language-server; do
  if [ "$server" = basedpyright ]; then
    write_launcher "$RUN/fake-servers/$server" "$server" "$RECORDS/$server.jsonl" BONGOS_FAKE_LSP_CONFIG=python
  else
    write_launcher "$RUN/fake-servers/$server" "$server" "$RECORDS/$server.jsonl"
  fi
  lsp_settings=$(jq -c --arg s "$server" --arg p "$RUN/fake-servers/$server" \
    '. + {($s): {binary: {path: $p, arguments: []}}}' <<<"$lsp_settings")
done

# A JDK 25 whose `java` is the fake jdtls; it gets jdtls's whole command line.
mkdir -p "$FAKE_JDK/bin"
printf 'IMPLEMENTOR="bongos smoke"\nJAVA_VERSION="25.0.1"\n' >"$FAKE_JDK/release"
write_launcher "$FAKE_JDK/bin/java" jdtls "$JDTLS_RECORD"

# The jdtls archive in the real layout (bin/jdtls, the launcher jar, config_linux), padded with
# incompressible bytes so the download spans several progress steps.
archive_root=$(mktemp -d "$RUN/jdtls-archive.XXXXXX")
mkdir -p "$archive_root/bin" "$archive_root/plugins" "$archive_root/config_linux"
write_launcher "$archive_root/bin/jdtls" jdtls "$JDTLS_RECORD"
printf 'fake launcher jar\n' >"$archive_root/$JDTLS_LAUNCHER_JAR"
printf 'osgi.bundles=fake\n' >"$archive_root/config_linux/config.ini"
head -c "$PAD_BYTES" /dev/urandom >"$archive_root/pad.bin"
tar -C "$archive_root" --owner=0 --group=0 --sort=name -cf - . | gzip -n >"$HOST_ROOT/jdtls.tar.gz"
rm -rf "$archive_root"

# The terraform-ls zip: the launcher at the root, as in the real release.
write_launcher "$RUN/terraform-ls-launcher" terraform-ls "$TFLS_RECORD" BONGOS_FAKE_LSP_FORMAT=terraform
python3 -I -c '
import sys, zipfile
info = zipfile.ZipInfo("terraform-ls", date_time=(1980, 1, 1, 0, 0, 0))
info.external_attr = 0o100755 << 16
info.compress_type = zipfile.ZIP_DEFLATED
with open(sys.argv[1], "rb") as source, zipfile.ZipFile(sys.argv[2], "w") as archive:
    archive.writestr(info, source.read())
' "$RUN/terraform-ls-launcher" "$HOST_ROOT/terraform-ls.zip"

start_fixture_http "$HOST_ROOT"
HOST_PORT=$FIXTURE_HTTP_PORT
export NO_PROXY=127.0.0.1

# The manifest: make-archives's kotlin-lsp and intellij pins (never fetched), then jdtls and
# terraform-ls pinned at 0.0.2 with the digests of the archives above and loopback URLs.
"$BONGOS_ROOT/script/bongos/fixtures/kotlin-servers/make-archives" "$RUN/kotlin-servers" "$BONGOS_FAKE_KOTLIN_SERVER" 9 >/dev/null
{
  awk '/^\[jdtls\]/ {exit} {print}' "$RUN/kotlin-servers/servers.toml"
  printf '[jdtls]\nversion = "0.0.2"\n\n'
  for platform in linux-x64 linux-arm64 macos-arm64 macos-x64; do
    printf '[jdtls.platform.%s]\nurl = "http://127.0.0.1:%s/jdtls.tar.gz"\nsha256 = "%s"\narchive = "tar.gz"\nstrip_components = 0\nlauncher = "bin/jdtls"\n\n' \
      "$platform" "$HOST_PORT" "$(sha256sum "$HOST_ROOT/jdtls.tar.gz" | cut -d' ' -f1)"
  done
  printf '[terraform-ls]\nversion = "0.0.2"\nargs = ["serve"]\n\n[terraform-ls.env]\nCHECKPOINT_DISABLE = "1"\n\n'
  for platform in linux-x64 linux-arm64 macos-arm64 macos-x64; do
    printf '[terraform-ls.platform.%s]\nurl = "http://127.0.0.1:%s/terraform-ls.zip"\nsha256 = "%s"\narchive = "zip"\nstrip_components = 0\nlauncher = "terraform-ls"\n\n' \
      "$platform" "$HOST_PORT" "$(sha256sum "$HOST_ROOT/terraform-ls.zip" | cut -d' ' -f1)"
  done
} >"$RUN/language-servers.toml"
export BONGOS_KOTLIN_SERVERS="$RUN/language-servers.toml"

# Prettier is a formatter the app installs from the npm registry (with a managed Node.js from
# nodejs.org when no usable node is on PATH) as soon as a buffer of a language that allows it
# opens. Like the servers above it is a download this scenario keeps out, so it is off for every
# language the scenario opens that allows it by default.
languages='{}'
for language in TSX TypeScript JavaScript HTML XML JSON JSONC CSS Markdown YAML; do
  languages=$(jq -c --arg l "$language" '. + {($l): {prettier: {allowed: false}}}' <<<"$languages")
done
jq -n --argjson lsp "$lsp_settings" --argjson languages "$languages" --arg jdk "$FAKE_JDK" \
  '{lsp: $lsp, languages: $languages, java: {server_jdk: $jdk}}' >"$SETTINGS_FILE"
rm -f "$LANGUAGES_LOG"
rm -rf "${LANG_DIR:?}/java" "${LANG_DIR:?}/terraform"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")

# ---------------------------------------------------------------------------------------------
step "Languages 1: each fixture opens with highlighting"
fixture_http_hold /jdtls.tar.gz "$HOLD_BYTES"
PATH="$FAKE_TF_BIN:$SAVED_PATH" launch_without_terraform \
  "$LANGS/rust" "$LANGS/python-venv" "$LANGS/react-vite" "$LANGS/java-maven" "$LANGS/java-gradle" "$LANGS/terraform"
window_ready
shots=()
# Highlighting itself is verified by reading the shots; frame_has_ui only proves a UI rendered.
open_highlighted() {
  open_file "$1" "$2"
  wait_for_settled
  shot "$3"
  frame_has_ui "$3"
  shots+=("$RUN/$3.png")
}
open_highlighted rust/src/lib.rs 'lib\.rs' highlight-rs
open_highlighted rust/Cargo.toml 'Cargo\.toml' highlight-cargo-toml
open_highlighted python-venv/src/app.py 'app\.py' highlight-py
open_highlighted react-vite/src/App.tsx 'App\.tsx' highlight-tsx
open_highlighted react-vite/index.html 'index\.html' highlight-html
open_highlighted java-maven/pom.xml 'pom\.xml' highlight-pom-xml
[ "$(record_count "$RECORDS/rust-analyzer.jsonl" initialized)" -ge 1 ] || fail "the fake rust-analyzer did not start for lib.rs"
[ "$(record_count "$PYRIGHT_RECORD" initialized)" -ge 1 ] || fail "the fake basedpyright did not start for app.py"
[ "$(record_count "$RECORDS/vtsls.jsonl" initialized)" -ge 1 ] || fail "the fake vtsls did not start for App.tsx"

# ---------------------------------------------------------------------------------------------
step "Languages 2: Java on java-gradle downloads jdtls with progress, then it is ready"
# The download holds the screen busy (a spinner), so nothing waits for it to settle until the release.
act keys ctrl+p
act type_text java-gradle/src/main/java/example/App.java
keys Return
wait_for_title 'App\.java'
wait_for_log 'Java: downloading jdtls'
poll 30 "the held part of the jdtls archive sent" fixture_http_sent_at_least /jdtls.tar.gz "$HOLD_BYTES"
wait_for_log 'Java: download 25% \(1 MB of 3 MB\)'
sleep 1
shot java-download-progress
frame_has_ui java-download-progress
if log_has 'Java: installed jdtls|Java: download (75|100)%'; then fail "jdtls finished while its archive was held: $(cat "$LANGUAGES_LOG")"; fi
[ "$(record_count "$JDTLS_RECORD" start)" = 0 ] || fail "jdtls started before its archive was complete"
fixture_http_release /jdtls.tar.gz
wait_for_log 'Java: installed jdtls'
poll 60 "the fake jdtls to be initialized" record_count_above "$JDTLS_RECORD" initialized 0
wait_for_settled
shot java-ready
frame_has_ui java-ready
shots+=("$RUN/java-ready.png")
argv=$(jdtls_argv)
grep -qxF -- "-jar" <<<"$argv" || fail "jdtls was not started with -jar: $argv"
grep -qxF -- "$LANG_DIR/java/jdtls/0.0.2/$JDTLS_LAUNCHER_JAR" <<<"$argv" || fail "jdtls was not started with the installed launcher jar: $argv"
grep -qxF -- "$LANG_DIR/java/jdtls/0.0.2/config_linux" <<<"$argv" || fail "jdtls was not given config_linux: $argv"
data_dir=$(awk 'prev == "-data" {print; exit} {prev = $0}' <<<"$argv")
case "$data_dir" in
  "$LANG_DIR/java/workspaces/"[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
  *) fail "the jdtls -data directory is not <languages>/java/workspaces/<16 hex>: $data_dir" ;;
esac
case "$data_dir" in "$LANGS"/*) fail "the jdtls -data directory is inside the project: $data_dir" ;; esac
# The fake server writes none, so this proves only that the app itself writes no jdtls metadata there.
[ -z "$(cd "$LANGS/java-gradle" && find . -name '.project' -o -name '.classpath' -o -name '.settings')" ] ||
  fail "jdtls metadata appeared inside java-gradle"
[ "$(grep -c '^GET /jdtls.tar.gz 200$' "$FIXTURE_HTTP_LOG")" = 1 ] || fail "the jdtls archive was not fetched once: $(cat "$FIXTURE_HTTP_LOG")"
palette 'java: show jdk'
# The report is a buffer with no file, so the window title drops App.java rather than naming it.
poll 30 "the JDK report to replace App.java" title_lacks 'App\.java'
wait_for_settled
shot java-show-jdk
frame_has_ui java-show-jdk
say "smoke: Java download, ready and JDK report: $RUN/java-download-progress.png, $RUN/java-ready.png, $RUN/java-show-jdk.png"

# ---------------------------------------------------------------------------------------------
step "Languages 3: Terraform with no terraform or tofu shows the notice; a terraform on PATH clears it"
open_file terraform/main.tf 'main\.tf'
wait_for_log 'Terraform: installed terraform-ls'
wait_for_log 'Terraform: notice: Neither terraform nor tofu was found'
poll 60 "the fake terraform-ls to be initialized" record_count_above "$TFLS_RECORD" initialized 0
wait_for_settled
shot highlight-tf
frame_has_ui highlight-tf
shots+=("$RUN/highlight-tf.png")
cp "$RUN/highlight-tf.png" "$RUN/terraform-notice.png"
[ "$(record_last "$TFLS_RECORD" initialize | jq -r '.initialization_options.terraform.path // "none"')" = none ] ||
  fail "terraform-ls got a terraform.path with no terraform on PATH: $(record_last "$TFLS_RECORD" initialize)"
[ "$(record_last "$TFLS_RECORD" start | jq -r '.checkpoint_disable')" = 1 ] ||
  fail "terraform-ls runs without CHECKPOINT_DISABLE=1: $(record_last "$TFLS_RECORD" start)"
[ "$(record_last "$TFLS_RECORD" start | jq -r '.argv | join(" ")')" = serve ] ||
  fail "terraform-ls was not started with serve: $(record_last "$TFLS_RECORD" start)"
printf '#!/bin/sh\nprintf "%%s\\n" "terraform $*" >>%s\ncat\n' "$RUN/fake-terraform.log" >"$FAKE_TF_BIN/terraform"
chmod +x "$FAKE_TF_BIN/terraform"
initializes=$(record_count "$TFLS_RECORD" initialize)
palette 'editor: restart language server'
wait_for_log 'Terraform: notice cleared \(terraform-binary\)'
poll 60 "terraform-ls to start again" record_count_above "$TFLS_RECORD" initialize "$initializes"
[ "$(record_last "$TFLS_RECORD" initialize | jq -r '.initialization_options.terraform.path')" = "$FAKE_TF_BIN/terraform" ] ||
  fail "terraform-ls did not get the terraform on PATH: $(record_last "$TFLS_RECORD" initialize)"
wait_for_settled
shot terraform-notice-cleared
frame_has_ui terraform-notice-cleared
quit_app
say "smoke: highlighting: ${shots[*]}"
say "smoke: Terraform notice and after the restart: $RUN/terraform-notice.png, $RUN/terraform-notice-cleared.png"

# ---------------------------------------------------------------------------------------------
step "Languages 4: the host stopped and no cached jdtls: one line, the log has the cause, Retry recovers"
stop_fixture_http
rm -rf "${LANG_DIR:?}/java/jdtls"
mark_log
launch "$LANGS/java-gradle" "$LANGS/java-gradle/src/main/java/example/App.java"
window_ready
wait_for_title 'App\.java'
wait_for_log 'Java: jdtls failed: could not download jdtls'
wait_for_settled
shot java-unavailable
frame_has_ui java-unavailable
# The cause follows the failure line: the full error chain, one log line each.
cause=$(log_since_mark | awk '/Java: jdtls failed: could not download jdtls/ {on = 1; next} on {print; exit}')
grep -qiE 'refused|127\.0\.0\.1|localhost' <<<"$cause" || fail "languages.log has no cause after the failure line: $(log_since_mark)"
# The start reports the failure twice (the adapter, then the language server store, with a status
# of None between them that logs "jdtls: available again"); the line shown is the last state, so
# the last jdtls event must be the failure.
last_event=$(log_since_mark | grep -E 'Java: jdtls failed|jdtls: available again|Java: installed jdtls' | tail -n1)
case "$last_event" in *'Java: jdtls failed: could not download jdtls') ;; *) fail "jdtls is not left failed: $(log_since_mark)" ;; esac
palette 'languages: show logs'
wait_for_title 'languages\.log'
wait_for_settled
shot java-show-logs
frame_has_ui java-show-logs
start_fixture_http "$HOST_ROOT" "$HOST_PORT"
starts=$(record_count "$JDTLS_RECORD" initialized)
palette 'languages: retry server'
wait_for_log 'Java: retrying jdtls'
wait_for_log 'Java: installed jdtls'
poll 60 "the fake jdtls to start after the retry" record_count_above "$JDTLS_RECORD" initialized "$starts"
wait_for_settled
shot java-retry-ready
frame_has_ui java-retry-ready
quit_app
say "smoke: Java unavailable, its log and the retry: $RUN/java-unavailable.png, $RUN/java-show-logs.png, $RUN/java-retry-ready.png"
say "smoke: the logged cause: $cause"

# ---------------------------------------------------------------------------------------------
step "Languages 5: offline with cached servers, Java and Terraform start and nothing hangs"
stop_fixture_http
mark_log
jdtls_starts=$(record_count "$JDTLS_RECORD" initialized)
tfls_starts=$(record_count "$TFLS_RECORD" initialized)
saved_proxy=("$ALL_PROXY" "$HTTPS_PROXY" "$HTTP_PROXY")
unset ALL_PROXY HTTPS_PROXY HTTP_PROXY
launch "$LANGS/java-gradle" "$LANGS/terraform" \
  "$LANGS/java-gradle/src/main/java/example/App.java" "$LANGS/terraform/main.tf"
export ALL_PROXY=${saved_proxy[0]} HTTPS_PROXY=${saved_proxy[1]} HTTP_PROXY=${saved_proxy[2]}
wait_for_window
offline_t0=$SECONDS
offline_both_started() {
  record_count_above "$JDTLS_RECORD" initialized "$jdtls_starts" &&
    record_count_above "$TFLS_RECORD" initialized "$tfls_starts"
}
poll 30 "jdtls and terraform-ls to start offline" offline_both_started
offline_secs=$((SECONDS - offline_t0))
poll 60 "a rendered frame" ui_rendered
wait_for_settled
shot offline-started
frame_has_ui offline-started
if log_since_mark | grep -qE 'downloading|failed'; then fail "languages.log shows a download or a failure offline: $(log_since_mark)"; fi
quit_app
say "smoke: offline, both servers started ${offline_secs}s after the window: $RUN/offline-started.png"

# ---------------------------------------------------------------------------------------------
step "Languages 6 and 7: the Bun test task runs bun only; real tools on PATH, offline"
start_fixture_http "$HOST_ROOT" "$HOST_PORT"
for tool in bun npm npx node; do write_recorder "$FAKE_JS_BIN" "$tool"; done
: >"$JS_CALLS"
owed=()
real_tool() { PATH=$SAVED_PATH command -v "$1"; }

# uv and poetry environments, made offline before the app starts.
UV=$(real_tool uv || true)
if [ -n "$UV" ]; then
  (cd "$LANGS/python-uv" && UV_OFFLINE=1 UV_PYTHON_DOWNLOADS=never "$UV" venv --python python3 .venv) >>"$RUN/uv.out" 2>&1 ||
    fail "uv venv failed offline: $(cat "$RUN/uv.out")"
else
  owed+=("OWED: uv is not on PATH; check by hand that a uv venv in a python-uv project is the interpreter bongos picks (manual check M5)")
fi
POETRY=$(real_tool poetry || true)
POETRY_ENV=""
if [ -n "$POETRY" ]; then
  (cd "$LANGS/python-poetry" && POETRY_VIRTUALENVS_IN_PROJECT=false POETRY_VIRTUALENVS_OPTIONS_NO_PIP=true \
    "$POETRY" env use python3) >>"$RUN/poetry.out" 2>&1 || fail "poetry env use failed offline: $(cat "$RUN/poetry.out")"
  POETRY_ENV=$(cd "$LANGS/python-poetry" && "$POETRY" env info --path 2>>"$RUN/poetry.out") ||
    fail "poetry env info failed: $(cat "$RUN/poetry.out")"
else
  owed+=("OWED: poetry is not on PATH; check by hand that a poetry environment is the interpreter bongos picks for a python-poetry project (manual check M5)")
fi
TERRAFORM=$(real_tool terraform || true)
cp -r "$LANGS/terraform" "$LANGS/terraform-fmt"
sed -i 's/^  value = var.greeting$/  value     =    var.greeting/' "$LANGS/terraform-fmt/main.tf"
! cmp -s "$LANGS/terraform-fmt/main.tf" "$LANGS/terraform/main.tf" || fail "the misformatted copy of main.tf is unchanged"

PATH="$FAKE_JS_BIN:$SAVED_PATH" launch "$LANGS/bun" "$LANGS/python-uv" "$LANGS/python-poetry" "$LANGS/terraform-fmt"
window_ready

open_file bun/index.ts 'index\.ts'
wait_for_settled
: >"$JS_CALLS"
palette 'task: spawn'
act type_text 'package.json > test'
keys Return
poll 60 "the test task to call bun" file_has "$JS_CALLS" '^bun run test$'
wait_for_settled
shot bun-test-task
frame_has_ui bun-test-task
if grep -qE '^(npm|npx|node)( |$)' "$JS_CALLS"; then fail "a Bun task called npm, npx or node: $(cat "$JS_CALLS")"; fi
say "smoke: the Bun test task called: $(xargs <"$JS_CALLS")"

if [ -n "$UV" ]; then
  open_file python-uv/src/app.py 'app\.py'
  poll 60 "basedpyright to get the uv environment" pythonpath_under python-uv "$LANGS/python-uv/.venv"
  wait_for_settled
  shot python-uv-env
  say "smoke: uv: basedpyright got python.pythonPath $(pythonpath_for python-uv)"
fi
if [ -n "$POETRY" ]; then
  open_file python-poetry/src/app.py 'app\.py'
  poll 60 "basedpyright to get the poetry environment" pythonpath_under python-poetry "$POETRY_ENV"
  wait_for_settled
  shot python-poetry-env
  say "smoke: poetry: basedpyright got python.pythonPath $(pythonpath_for python-poetry)"
fi
if [ -n "$TERRAFORM" ]; then
  formats=$(record_count "$TFLS_RECORD" format)
  open_file terraform-fmt/main.tf 'main\.tf'
  poll 60 "terraform-ls to start for terraform-fmt" record_count_above "$TFLS_RECORD" initialize 0 \
    '(.root_uri // "" | endswith("/terraform-fmt"))'
  palette 'editor: format'
  poll 30 "terraform fmt to run" record_count_above "$TFLS_RECORD" format "$formats"
  [ "$(record_last "$TFLS_RECORD" format | jq -r '.status')" = 0 ] || fail "terraform fmt failed: $(record_last "$TFLS_RECORD" format)"
  [ "$(record_last "$TFLS_RECORD" format | jq -r '.command[0]')" = "$TERRAFORM" ] ||
    fail "Format did not run the terraform on PATH: $(record_last "$TFLS_RECORD" format)"
  keys ctrl+s
  poll 30 "the formatted main.tf to be saved" cmp -s "$LANGS/terraform-fmt/main.tf" "$LANGS/terraform/main.tf"
  wait_for_settled
  shot terraform-formatted
  say "smoke: terraform: Format ran $TERRAFORM fmt through terraform-ls and saved the formatted file"
else
  owed+=("OWED: terraform is not on PATH; check by hand that Format on a .tf file runs terraform fmt through terraform-ls (manual check M5)")
fi
# Nothing else ran npm, npx or node while the Bun project was open, except `npm root -g`: the
# language server delegate's local lookup of a globally installed server package (lsp_store's
# npm_package_installed_version), which runs no task and makes no request.
others=$(grep -E '^(npm|npx|node)( |$)' "$JS_CALLS" | grep -vxF 'npm root -g' || true)
[ -z "$others" ] || fail "npm, npx or node was called with the Bun project open: $others"
quit_app

BUN=$(real_tool bun || true)
if [ -n "$BUN" ]; then
  cp -r "$BONGOS_ROOT/script/bongos/fixtures/languages/bun" "$RUN/bun-real"
  (cd "$RUN/bun-real" && "$BUN" test) >>"$RUN/bun-test.out" 2>&1 || fail "bun test failed on the fixture offline: $(cat "$RUN/bun-test.out")"
  grep -qE '1 pass' "$RUN/bun-test.out" || fail "bun test did not pass the fixture's test: $(cat "$RUN/bun-test.out")"
  say "smoke: bun: bun test passed the fixture's test offline"
else
  owed+=("OWED: bun is not on PATH; check by hand that bun test passes on the bun fixture and that its test task runs bun (manual check M5)")
fi
for line in "${owed[@]}"; do say "$line"; done

# ---------------------------------------------------------------------------------------------
step "Languages 8: no request left the machine and no inherited server was downloaded"
assert_proxy_strays_none
assert_no_zed_hosts
strays=$(grep -E ': downloading |: installing ' "$LANGUAGES_LOG" | grep -vE 'Java: downloading jdtls|Terraform( Vars)?: downloading terraform-ls' || true)
[ -z "$strays" ] || fail "a language server other than jdtls and terraform-ls was downloaded: $strays"
host_log=$(grep -v '^#' "$FIXTURE_HTTP_LOG" | sort | uniq -c | sed -E 's/^ +//')
host_paths=$(printf '%s\n' "$host_log" | tr '\n' ';')
# Step 4 legitimately downloads jdtls again (once after the failure), so jdtls is served twice.
[ "$(grep -c '^GET /terraform-ls.zip 200$' "$FIXTURE_HTTP_LOG")" = 1 ] || fail "terraform-ls was not served exactly once: $host_paths"
[ "$(grep -c '^GET /jdtls.tar.gz 200$' "$FIXTURE_HTTP_LOG")" = 2 ] || fail "jdtls was not served exactly twice: $host_paths"
[ "$(grep -vc '^#' "$FIXTURE_HTTP_LOG")" = 3 ] || fail "the loopback host served other requests (HEAD or more): $host_paths"
say "smoke: the proxy got only the requests of every launch; the loopback host served $host_paths"

trap - EXIT
stop_fixture_http
cp "$RUN/config-backup/settings-44.json" "$SETTINGS_FILE"
export PATH=$SAVED_PATH
unset BONGOS_KOTLIN_SERVERS NO_PROXY
trap cleanup EXIT
