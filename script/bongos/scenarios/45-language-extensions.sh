# Scenario for feature 45-language-extensions. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only: the app reads the fixture list
# from BONGOS_EXTENSION_LIST.
#
# Setup: a fresh home (/tmp/h45). The fixture extensions (script/bongos/fixtures/language-extensions)
# become local git repositories under $RUN/ext-repos, served as file:// URLs; the fixture list names
# quokka and numbat at their commits. A fake cargo, rustc and rustup come first on PATH: they answer
# the toolchain probe (a sysroot with the wasm32-wasip2 target) and record every call, so no real
# build can run. A build is held open by git itself: the home's ~/.gitconfig sets
# uploadpack.packObjectsHook to a script that waits while a gate file exists, so the fetch of a
# file:// repository stalls until the scenario removes the gate.
#   1. hello.quok opens: the offer line "Quokka support is available" (offer.png).
#   2. extensions: install language, Quokka, held in its fetch: the editor types into hello.quok and
#      saves it while the install is held (install-held.png); the gate opens, the install finishes:
#      record, link, build log, the language in the extension index, Quokka in the status bar
#      (quokka-installed.png). Nothing ran cargo build or rustup beyond the probe.
#   3. Restart: the build log, the checkout and the record are byte for byte the same (no build), and
#      the buffer shows Quokka again (quokka-after-restart.png).
#   4. extensions: install from git, the off-list wombat repository: the strict question
#      (trust-question.png); "no" leaves no git/wombat folder, no link and no log.
#   5. extensions: install language, Numbat: refused before any build, the build log names extension
#      API 0.99.0 and the newest this bongos supports (api-refused.png); nothing left under git/numbat.
#   6. zed: extensions (ctrl-shift-x) shows quokka with its source URL and short commit (extensions-page.png).
#   7. extensions: remove, quokka, y: no git/quokka folder, no installed/quokka link, no record, and
#      the language leaves the index.
#   8. A repository that does not exist, at a full commit, typed yes: the failure line
#      (install-failed.png) and a build log with the error; extensions: show build log opens it.
#   9. The proxy got no request but the two every launch makes, no Zed host, no marketplace host.
# Capture: the feature has no design directory, so there are no design- shots. The text of the
# offer line, the status bar, the question and the Extensions page is read from the shots.
# Deviations: the offer's Install button is not clicked (its position is not known to the harness);
# `extensions: install language` dispatches the same action the button does. The held phase is the
# fetch ("cloning"), since a build without Rust or grammars finishes in milliseconds.
# The scenario leaves the app quit, puts HOME back and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 45-language-extensions skipped under BONGOS_STOCK=1: the stock run has no language extensions"
  return 0
fi

for tool in git jq sha256sum python3; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

FIXTURES="$BONGOS_ROOT/script/bongos/fixtures/language-extensions"
REPOS="$RUN/ext-repos"
PROJ="$RUN/ext-project"
FAKE_RUST_BIN="$RUN/fake-rust-bin"
FAKE_SYSROOT="$RUN/fake-sysroot"
RUST_CALLS="$RUN/rust-calls.log"
HOLD_HOOK="$RUN/hold-pack-objects"
HOLD_GATE="$RUN/hold.gate"
HOLD_ENTERED="$RUN/hold.entered"
MISSING_COMMIT=0123456789abcdef0123456789abcdef01234567

SAVED_HOME=$HOME
SAVED_PATH=$PATH
export HOME=/tmp/h45
mkdir -p "$RUN/home45"
ln -sfn "$RUN/home45" "$HOME"
DATA_DIR="$HOME/${BONGOS_LOG_PATH%/logs/*}"
EXT_DIR="$DATA_DIR/extensions"
GIT_EXT="$EXT_DIR/git"
INDEX="$EXT_DIR/index.json"

scenario_exit() {
  local rc=$?
  set +e
  rm -f "$HOLD_GATE"
  export HOME=$SAVED_HOME PATH=$SAVED_PATH
  unset BONGOS_EXTENSION_LIST
  (exit "$rc")
  cleanup
}
trap scenario_exit EXIT

# --- Helpers ----------------------------------------------------------------------------------

# Runs a command of the palette (VS Code keymap here) by its name. Every command used here opens a
# prompt or an item, so the last key waits for the screen too: typing before the prompt is up would
# land in the editor.
palette() {
  act keys ctrl+shift+p
  act type_text "$1"
  act keys Return
}

window_ready() {
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# The extension index lists a language by its name.
index_has_language() { [ -f "$INDEX" ] && jq -e --arg l "$1" '.languages | has($l)' "$INDEX" >/dev/null; }
index_lacks_language() { ! index_has_language "$1"; }

# Everything under git/<id> and installed/<id>, with the content hash of each file, for comparing
# before and after a restart.
extension_state() {
  (cd "$EXT_DIR" && find "git/$1" "installed/$1" -path '*/.git' -prune -o -print 2>/dev/null | sort |
    while read -r path; do
      if [ -L "$path" ]; then printf '%s -> %s\n' "$path" "$(readlink "$path")"
      elif [ -f "$path" ]; then printf '%s %s\n' "$path" "$(sha256sum <"$path" | cut -d' ' -f1)"
      else printf '%s/\n' "$path"; fi
    done)
}

gone() { [ ! -e "$1" ] && [ ! -L "$1" ]; }

# Fails when the proxy got a request since the scenario began other than the two kinds every launch
# makes (the ACP registry and localhost, as in scenarios 04, 28 and 44).
assert_proxy_strays_none() {
  local registry_host strays
  registry_host=$(sed -nE 's|^const REGISTRY_URL: &str = "https://([^/"]+)/.*|\1|p' \
    "$BONGOS_ROOT/crates/project/src/agent_registry_store.rs")
  [ -n "$registry_host" ] || fail "no REGISTRY_URL host in crates/project/src/agent_registry_store.rs"
  strays=$(tail -n +"$((PROXY_LINES_BEFORE + 1))" "$RUN/proxy.log" |
    awk -v registry="CONNECT $registry_host" 'NF && $0 != registry && $0 != "GET localhost"')
  [ -z "$strays" ] || fail "the proxy got other requests during the scenario: $strays"
}

# make_repo <name>: the fixture folder as a one-commit repository; prints its commit.
make_repo() {
  local dir="$REPOS/$1"
  cp -r "$FIXTURES/$1" "$dir"
  if [ -f "$dir/Cargo.toml.fixture" ]; then mv "$dir/Cargo.toml.fixture" "$dir/Cargo.toml"; fi
  git -C "$dir" init -q -b main
  git -C "$dir" add -A
  git -C "$dir" -c user.name="bongos smoke" -c user.email=smoke@localhost commit -q -m "$1 fixture"
  git -C "$dir" rev-parse HEAD
}

# ---------------------------------------------------------------------------------------------
step "Extensions setup: fresh home, fixture repositories, fixture list, fake Rust tools, the fetch gate"
seed_settings
mkdir -p "$REPOS" "$PROJ" "$FAKE_RUST_BIN" "$FAKE_SYSROOT/lib/rustlib/wasm32-wasip2"
QUOKKA_COMMIT=$(make_repo quokka)
WOMBAT_COMMIT=$(make_repo wombat)
NUMBAT_COMMIT=$(make_repo numbat)
QUOKKA_URL="file://$REPOS/quokka"
WOMBAT_URL="file://$REPOS/wombat"
NUMBAT_URL="file://$REPOS/numbat"
MISSING_URL="file://$REPOS/gone"
sed -e "s|@QUOKKA_URL@|$QUOKKA_URL|" -e "s|@QUOKKA_COMMIT@|$QUOKKA_COMMIT|" \
  -e "s|@NUMBAT_URL@|$NUMBAT_URL|" -e "s|@NUMBAT_COMMIT@|$NUMBAT_COMMIT|" \
  "$FIXTURES/languages.toml" >"$RUN/extension-list.toml"
! grep -q '@' "$RUN/extension-list.toml" || fail "a placeholder is left in the fixture list: $(cat "$RUN/extension-list.toml")"
export BONGOS_EXTENSION_LIST="$RUN/extension-list.toml"
cp "$FIXTURES"/project/* "$PROJ/"
git -C "$PROJ" init -q

for tool in cargo rustup; do
  printf '#!/bin/sh\nprintf "%%s\\n" "%s $*" >>%s\necho "%s 0.0.0 (bongos smoke)"\n' \
    "$tool" "$RUST_CALLS" "$tool" >"$FAKE_RUST_BIN/$tool"
done
printf '#!/bin/sh\nprintf "%%s\\n" "rustc $*" >>%s\nif [ "$*" = "--print sysroot" ]; then echo %s; else echo "rustc 0.0.0 (bongos smoke)"; fi\n' \
  "$RUST_CALLS" "$FAKE_SYSROOT" >"$FAKE_RUST_BIN/rustc"
chmod +x "$FAKE_RUST_BIN"/*
: >"$RUST_CALLS"

printf '#!/bin/sh\nif [ -e %s ]; then\n  : >%s\n  while [ -e %s ]; do sleep 0.1; done\nfi\nexec "$@"\n' \
  "$HOLD_GATE" "$HOLD_ENTERED" "$HOLD_GATE" >"$HOLD_HOOK"
chmod +x "$HOLD_HOOK"
printf '[uploadpack]\n\tpackObjectsHook = %s\n' "$HOLD_HOOK" >"$HOME/.gitconfig"
export PATH="$FAKE_RUST_BIN:$SAVED_PATH"
PROXY_LINES_BEFORE=$(wc -l <"$RUN/proxy.log")
say "smoke: fixture commits quokka $QUOKKA_COMMIT, wombat $WOMBAT_COMMIT, numbat $NUMBAT_COMMIT"

# ---------------------------------------------------------------------------------------------
step "Extensions 1: a file of a listed language shows the offer line"
launch "$PROJ" "$PROJ/hello.quok"
window_ready
wait_for_title 'hello\.quok'
wait_for_settled
shot offer
frame_has_ui offer
gone "$GIT_EXT/quokka" || fail "opening a file wrote $GIT_EXT/quokka"
[ -z "$(ls -A "$GIT_EXT/logs" 2>/dev/null)" ] || fail "opening a file wrote a build log: $(ls -A "$GIT_EXT/logs")"

# ---------------------------------------------------------------------------------------------
step "Extensions 2: install Quokka, type and save while its fetch is held, then it loads"
: >"$HOLD_GATE"
palette 'extensions: install language'
act type_text Quokka
keys Return
poll 60 "the Quokka fetch to reach the gate" test -e "$HOLD_ENTERED"
keys ctrl+End
type_text 'edited = "while the install is held"'
keys ctrl+s
assert_file "$PROJ/hello.quok" 'while the install is held'
[ -e "$HOLD_ENTERED" ] && [ -e "$HOLD_GATE" ] || fail "the gate opened before the save"
gone "$GIT_EXT/quokka/record.json" || fail "quokka was recorded while its fetch was held"
gone "$EXT_DIR/installed/quokka" || fail "quokka was linked while its fetch was held"
wait_for_settled
shot install-held
frame_has_ui install-held
rm -f "$HOLD_GATE"
poll 120 "the quokka record" test -f "$GIT_EXT/quokka/record.json"
poll 60 "Quokka in the extension index" index_has_language Quokka
wait_for_settled
shot quokka-installed
frame_has_ui quokka-installed
record=$(cat "$GIT_EXT/quokka/record.json")
[ "$(jq -r .commit <<<"$record")" = "$QUOKKA_COMMIT" ] || fail "the quokka record has another commit: $record"
[ "$(jq -r .listed <<<"$record")" = true ] || fail "the quokka record is not listed: $record"
[ "$(jq -r .source_url <<<"$record")" = "$QUOKKA_URL" ] || fail "the quokka record has another source: $record"
link_target=$(readlink "$EXT_DIR/installed/quokka" || true)
[ "${link_target%/}" = "$GIT_EXT/quokka/$QUOKKA_COMMIT" ] ||
  fail "installed/quokka does not point at the checkout: $(readlink "$EXT_DIR/installed/quokka" || true)"
QUOKKA_LOG="$GIT_EXT/logs/quokka.log"
file_has "$QUOKKA_LOG" "installed quokka at $QUOKKA_COMMIT" || fail "the quokka build log has no install line: $(cat "$QUOKKA_LOG" 2>/dev/null)"
file_has "$QUOKKA_LOG" 'built \(no Rust part' || fail "the quokka build log has no build line: $(cat "$QUOKKA_LOG")"
grep -q 'while the install is held' "$PROJ/hello.quok" || fail "the edit made during the hold is gone"
say "smoke: offer, held install and loaded language: $RUN/offer.png, $RUN/install-held.png, $RUN/quokka-installed.png"

# ---------------------------------------------------------------------------------------------
step "Extensions 3: after a restart Quokka loads with no build"
quit_app
before=$(extension_state quokka; sha256sum <"$QUOKKA_LOG")
launch "$PROJ" "$PROJ/hello.quok"
window_ready
wait_for_title 'hello\.quok'
poll 60 "Quokka in the extension index" index_has_language Quokka
wait_for_settled
shot quokka-after-restart
frame_has_ui quokka-after-restart
after=$(extension_state quokka; sha256sum <"$QUOKKA_LOG")
[ "$before" = "$after" ] || fail "the restart changed the quokka install or its log: $(diff <(echo "$before") <(echo "$after") || true)"
[ -z "$(ls -A "$GIT_EXT/staging" 2>/dev/null)" ] || fail "the restart left a staging folder: $(ls -A "$GIT_EXT/staging")"
say "smoke: after the restart: $RUN/quokka-after-restart.png"

# ---------------------------------------------------------------------------------------------
step "Extensions 4: an off-list repository asks the strict question; no installs nothing"
quok_before=$(sha256sum <"$PROJ/hello.quok")
palette 'extensions: install from git'
act type_text "$WOMBAT_URL"
act keys Return
act keys Return
# The question opens after `git ls-remote`, a moment after the ref prompt closed.
sleep 1
wait_for_settled
shot trust-question
frame_has_ui trust-question
# The question is wider than the strip, so the typed answer is past its right edge: no act here.
type_text no
keys Return
wait_for_settled
shot trust-declined
# Had the prompts not been up, the URL and the answer would be in hello.quok: save it and compare.
keys ctrl+s
wait_for_settled
[ "$(sha256sum <"$PROJ/hello.quok")" = "$quok_before" ] || fail "the install prompts were not up: hello.quok got $(cat "$PROJ/hello.quok")"
gone "$GIT_EXT/wombat" || fail "answering no left $GIT_EXT/wombat"
gone "$EXT_DIR/installed/wombat" || fail "answering no left installed/wombat"
gone "$GIT_EXT/logs/wombat.log" || fail "answering no wrote a wombat build log"
[ -z "$(ls -A "$GIT_EXT/staging" 2>/dev/null)" ] || fail "answering no left a staging folder"
say "smoke: the trust question and after no: $RUN/trust-question.png, $RUN/trust-declined.png"

# ---------------------------------------------------------------------------------------------
step "Extensions 5: an extension that needs extension API 0.99.0 is refused before any build"
palette 'extensions: install language'
act type_text Numbat
keys Return
api_line='needs extension API 0\.99\.0; this bongos supports up to [0-9]+\.[0-9]+\.[0-9]+'
poll 60 "the numbat build log to name both API versions" file_has "$GIT_EXT/logs/numbat.log" "$api_line"
wait_for_settled
shot api-refused
frame_has_ui api-refused
gone "$GIT_EXT/numbat" || fail "the refused numbat left $GIT_EXT/numbat"
gone "$EXT_DIR/installed/numbat" || fail "the refused numbat was linked"
if grep -vE '^(cargo|rustc|rustup) (--version|--print sysroot)$' "$RUST_CALLS"; then
  fail "a Rust tool was run beyond the probe: $(cat "$RUST_CALLS")"
fi
say "smoke: refused: $(grep -oE "$api_line" "$GIT_EXT/logs/numbat.log" | head -n1): $RUN/api-refused.png"

# ---------------------------------------------------------------------------------------------
step "Extensions 6: the Extensions page shows the source and commit of quokka"
# ctrl-shift-x is zed: extensions in the default keymap. The page leaves the window title as it
# was, so the wait is on the screen (act).
act keys ctrl+shift+x
wait_for_settled
shot extensions-page
frame_has_ui extensions-page
say "smoke: the Extensions page ($QUOKKA_URL at ${QUOKKA_COMMIT:0:8}): $RUN/extensions-page.png"

# ---------------------------------------------------------------------------------------------
step "Extensions 7: extensions: remove leaves nothing of quokka"
palette 'extensions: remove'
act type_text quokka
act keys Return
keys y
poll 60 "git/quokka to go" gone "$GIT_EXT/quokka"
poll 30 "installed/quokka to go" gone "$EXT_DIR/installed/quokka"
poll 60 "Quokka to leave the extension index" index_lacks_language Quokka
wait_for_settled
shot quokka-removed
frame_has_ui quokka-removed
file_has "$QUOKKA_LOG" 'removed quokka' || fail "the quokka log has no remove line: $(cat "$QUOKKA_LOG")"

# ---------------------------------------------------------------------------------------------
step "Extensions 8: a repository that does not exist gives the failure line and a build log"
before_logs=$(ls "$GIT_EXT/logs")
palette 'extensions: install from git'
act type_text "$MISSING_URL"
act keys Return
act type_text "$MISSING_COMMIT"
act keys Return
sleep 1
wait_for_settled
type_text yes
keys Return
new_log() { comm -13 <(printf '%s\n' "$before_logs") <(ls "$GIT_EXT/logs") | grep -E '^source-[0-9a-f]{8}\.log$'; }
poll 60 "a build log of the missing repository" new_log >/dev/null
failed_log="$GIT_EXT/logs/$(new_log | head -n1)"
poll 30 "the error in $failed_log" file_has "$failed_log" 'error: '
grep -qF "$MISSING_URL" "$failed_log" || fail "the build log does not name the repository: $(cat "$failed_log")"
wait_for_settled
shot install-failed
frame_has_ui install-failed
palette 'extensions: show build log'
wait_for_title 'source-[0-9a-f]{8}\.log'
wait_for_settled
shot install-failed-log
gone "$GIT_EXT/gone" || fail "the failed install left a folder"
[ -z "$(ls -A "$GIT_EXT/staging" 2>/dev/null)" ] || fail "the failed install left a staging folder"
quit_app
say "smoke: the failure line and its log: $RUN/install-failed.png, $RUN/install-failed-log.png ($(grep -m1 'error: ' "$failed_log" | cut -c1-160))"

# ---------------------------------------------------------------------------------------------
step "Extensions 9: no request left the machine for language extensions"
assert_proxy_strays_none
assert_no_marketplace_host
assert_no_zed_hosts
say "smoke: the proxy got only the requests of every launch"

trap - EXIT
rm -f "$HOLD_GATE"
export HOME=$SAVED_HOME PATH=$SAVED_PATH
unset BONGOS_EXTENSION_LIST
trap cleanup EXIT
