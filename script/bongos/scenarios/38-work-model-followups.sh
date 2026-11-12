# Scenario for feature 38-work-model-followups. Sourced by script/bongos/smoke after the base flow
# (the app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only: it reads the Work panel through
# `work: dump view` (<logs>/work-view.txt) and the repository overview and the store's counters
# through `work: dump repositories` (<logs>/work-repositories.txt).
#
# Setup, as scenarios 03 and 26 do: two git repositories with one commit, $HOME/src/invoicing and
# $HOME/src/api, both in `work.repositories`, `"base_keymap": "Emacs"` (the C-c w keys exist only
# there). The projects and tasks are written as files before the app starts, as an agent would. One
# task, `legacy`, records a worktree made by an older version: `git worktree add` under
# <repository>/.bongos-worktrees/legacy, plus the `/.bongos-worktrees/` line in info/exclude.
#   1. C-c w s on a leaf of invoicing: the worktree appears under <data dir>/worktrees/invoicing/
#      within ten seconds; the checkout has no new entry and `git status --porcelain` is empty.
#   2. worktree_root of api set to `{repo}.worktrees` in settings.json: the next task of api lands in
#      $HOME/src/api.worktrees/<slug>; invoicing still reads central in the overview.
#   3. A root under a read-only directory (chmod 0555): the start falls back to the central
#      directory, the overview names the problem, and the task's history records the fallback once.
#   4. legacy: its worktree, its .git file, its branch and its task.toml are unchanged (hashes), the
#      overview lists it as `old root`, and a commit made in it from the shell works.
#   5. Two tasks with 70-character slugs sharing a 60-character prefix get distinct worktree folders
#      of at most 48 characters, and keep their full slug as the branch.
#   6. A stacked pair: a commit on the parent's branch from the shell makes the Work view read
#      `rebase needed` on the child after two seconds; `git rebase` in the child clears it after two.
#   7. Idle for 40 seconds: `rebase_probes` in the overview dump does not change.
#   8. No file under $HOME other than task.toml, journals, chat runtime files and git metadata holds
#      the path of a created worktree, and no CLAUDE.md, AGENTS.md or .rules exists anywhere.
# Deviations from the plan, each forced by the harness:
# - The "log" of the fallback message is the task's own history (task.toml), the only place besides the
#   toast (which has no log) that says it.
# - Editing the root through `work: edit worktree root` is not run: the palette cannot pass the
#   repository name the action takes, and the settings.json edit of step 2 covers the same write.
# - "Within two seconds" is two seconds of sleep before the dump, which itself takes longer.
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 38-work-model-followups skipped under BONGOS_STOCK=1: the stock run has no Work view"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
DATA_DIR="${LOGS_DIR%/logs}"
VIEW_FILE="$LOGS_DIR/work-view.txt"
REPOS_FILE="$LOGS_DIR/work-repositories.txt"
CENTRAL="$DATA_DIR/worktrees"
INVOICING="$HOME/src/invoicing"
API="$HOME/src/api"
LOCKED="$HOME/locked"
WORK_ROOT="$HOME/.bongos"
INV_PROJECT="$WORK_ROOT/projects/invoicing"
API_PROJECT="$WORK_ROOT/projects/api"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)
PREFIX=reconcile-invoice-ledger-entries-with-bank-statement-exports
LONG_A="$PREFIX-aaaaaaaaa"
LONG_B="$PREFIX-bbbbbbbbb"

for tool in git python3 sha256sum grep find; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

[ "${#LONG_A}" = 70 ] && [ "${#PREFIX}" = 60 ] || fail "the long slugs are ${#LONG_A} characters with a ${#PREFIX}-character prefix"

# --- Input -----------------------------------------------------------------------------------

wkeys() { act keys ctrl+c w "$@"; }

choose() {
  act type_text "$1"
  act keys Return
}

palette() {
  act keys ctrl+shift+p
  choose "$1"
}

focus_center() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  xdo mousemove --window "$WIN" $((WIDTH * 3 / 4)) $((HEIGHT / 3)) click 1
  wait_for_settled
}

focus_panel() {
  focus_center
  keys ctrl+c w w
  wait_for_settled
}

launch_drawn() {
  launch
  wait_for_window
  poll 60 "a rendered frame" ui_rendered
  wait_for_settled
}

# --- Reading the Work view and the overview ------------------------------------------------------

dump_view() {
  rm -f "$VIEW_FILE"
  palette "work: dump view"
  assert_file "$VIEW_FILE"
  cp "$VIEW_FILE" "$RUN/work-view-$1.txt"
}

breadcrumb() { head -n1 "$VIEW_FILE"; }

view_detail() {
  awk -v goal="$1" 'NR > 1 {
    t = substr($0, 2)
    if (hit) { sub(/^ +/, "", t); print t; exit }
    if (substr(t, 1, 1) != " " && (t == goal || index(t, goal "  ") == 1)) hit = 1
  }' "$VIEW_FILE"
}

select_row() {
  local pos sel tgt
  dump_view select
  pos=$(awk -v goal="$1" 'NR > 1 && substr($0, 2, 1) != " " {
    n++; t = substr($0, 2)
    if (substr($0, 1, 1) == ">") sel = n
    if (t == goal || index(t, goal "  ") == 1) tgt = n
  } END { print sel + 0, tgt + 0 }' "$VIEW_FILE")
  sel=${pos% *}
  tgt=${pos#* }
  [ "$tgt" -gt 0 ] || fail "no row '$1' in the Work view: $(cat "$VIEW_FILE")"
  focus_panel
  if [ "$sel" = 0 ]; then
    keys Down
    sel=1
  fi
  while [ "$sel" -lt "$tgt" ]; do keys Down; sel=$((sel + 1)); done
  while [ "$sel" -gt "$tgt" ]; do keys Up; sel=$((sel - 1)); done
  wait_for_settled
}

drill() {
  select_row "$1"
  act keys Return
  dump_view drill
  [ "$(breadcrumb | sed 's/.* > //')" = "$1" ] || fail "drilling into '$1' shows '$(breadcrumb)'"
}

go_top() {
  local i
  for i in 1 2 3 4 5; do
    dump_view top
    [ "$(breadcrumb)" = Work ] && return 0
    palette "work: up"
  done
  fail "work: up did not reach the top level: $(breadcrumb)"
}

dump_repos() {
  rm -f "$REPOS_FILE"
  palette "work: dump repositories"
  assert_file "$REPOS_FILE"
  cp "$REPOS_FILE" "$RUN/work-repositories-$1.txt"
}

# The lines of the block of repository <$1> in the last overview dump.
repo_block() {
  awk -v name="$1" '/^repository / { on = ($2 == name); next } on' "$REPOS_FILE"
}

repo_has() {
  repo_block "$1" | grep -qF -- "$2" || fail "the overview of $1 lacks '$2': $(cat "$REPOS_FILE")"
}

repo_lacks() {
  ! repo_block "$1" | grep -qF -- "$2" || fail "the overview of $1 has '$2': $(cat "$REPOS_FILE")"
}

probes() { awk '$1 == "rebase_probes:" { print $2 }' "$REPOS_FILE"; }

# Dumps the overview up to six times, two seconds apart, until the block of repository <$1> has <$2>:
# settings.json is reread in the background.
wait_repo() {
  local i
  for i in 1 2 3 4 5 6; do
    sleep 2
    dump_repos "$3"
    repo_block "$1" | grep -qF -- "$2" && return 0
  done
  fail "the overview of $1 never showed '$2': $(cat "$REPOS_FILE")"
}

# --- Reading files ----------------------------------------------------------------------------

toml() {
  python3 -I -c 'import sys, tomllib; d = tomllib.load(open(sys.argv[1], "rb")); print(eval(sys.argv[2]))' "$1" "$2"
}

toml_is() { [ -f "$1" ] && [ "$(toml "$1" "$2" 2>/dev/null)" = "$3" ]; }

wait_toml() { poll 10 "$1: $2 == $3" toml_is "$1" "$2" "$3"; }

iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

new_id() { cat /proc/sys/kernel/random/uuid; }

PR_TODOS='[[todos]]
name = "implement"
role = "produces-branch"
state = "open"

[[todos]]
name = "review"
state = "open"

[[todos]]
name = "ship"
role = "ships"
state = "open"'

# write_task <dir> <goal> <kind> [<more toml>]
write_task() {
  mkdir -p "$1"
  {
    printf 'schema = 1\nid = "%s"\ngoal = "%s"\nkind = "%s"\nstatus = "open"\ncreated = %s\n' \
      "$(new_id)" "$2" "$3" "$(iso_now)"
    printf 'default_visibility = "shared"\n'
    if [ -n "${4:-}" ]; then printf '%s\n' "$4"; fi
  } >"$1/task.toml"
}

# write_project <dir> <title> <repository>
write_project() {
  mkdir -p "$1/tasks"
  printf 'schema = 1\ntitle = "%s"\nrepositories = ["%s"]\n' "$2" "$3" >"$1/project.toml"
  write_task "$1" "$2" other
}

# write_leaf <project dir> <slug> <goal> [<extra toml before the todos>]
write_leaf() {
  write_task "$1/tasks/$2" "$3" pr-ship "${4:+$4
}$PR_TODOS"
}

# write_settings <api worktree_root line, with its leading comma, or empty>
write_settings() {
  cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "task_builder": { "enabled": false },
  "work": {
    "repositories": [
      { "name": "invoicing", "path": "~/src/invoicing", "deployments": true, "merge_is_ship": true, "branch_template": "{slug}" },
      { "name": "api", "path": "~/src/api", "deployments": true, "merge_is_ship": true, "branch_template": "{slug}"$1 }
    ]
  }
}
JSON
}

# Records the worktree of the task in <dir> after waiting for the line; prints its path.
started() {
  wait_worktree_line "$1" || fail "no worktree line in $1/task.toml: $(cat "$1/task.toml")"
  worktree_of "$1"
}

dir_hash() { (cd "$1" && find . -path ./.git -prune -o -print | LC_ALL=C sort | sha256sum); }

# ---------------------------------------------------------------------------------------------
step "Setup: two repositories, the legacy worktree, the projects and tasks as files"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-38.json"
for repo in "$INVOICING" "$API"; do
  git init -q -b main "$repo"
  printf '# %s\n' "$(basename "$repo")" >"$repo/README.md"
  git -C "$repo" add README.md
  git -C "$repo" "${GIT_ID[@]}" commit -qm "first commit"
done
# What an older version made: a worktree under the checkout, its exclude line, its task.
mkdir -p "$INVOICING/.bongos-worktrees"
git -C "$INVOICING" worktree add -q -b legacy "$INVOICING/.bongos-worktrees/legacy" main
printf '/.bongos-worktrees/\n' >>"$INVOICING/.git/info/exclude"
LEGACY_WT="$INVOICING/.bongos-worktrees/legacy"
write_settings ""
write_project "$INV_PROJECT" Invoicing invoicing
write_project "$API_PROJECT" Api api
write_leaf "$INV_PROJECT" legacy "Legacy cleanup" "repository = \"invoicing\"
branch = \"legacy\"
base = \"main\"
base_sha = \"$(git -C "$INVOICING" rev-parse main)\"
worktree = \"$LEGACY_WT\""
write_leaf "$INV_PROJECT" export-invoices "Export invoices"
write_leaf "$INV_PROJECT" "$LONG_A" "Reconcile alpha"
write_leaf "$INV_PROJECT" "$LONG_B" "Reconcile bravo"
# The parent has its implement todo done, so a task stacked on it at `implemented` is ready.
write_task "$INV_PROJECT/tasks/stack-parent" "Stack parent" pr-ship '[[todos]]
name = "implement"
role = "produces-branch"
state = "done"

[[todos]]
name = "review"
state = "open"

[[todos]]
name = "ship"
role = "ships"
state = "open"'
write_leaf "$INV_PROJECT" stack-child "Stack child" 'stacked_on = { task = "stack-parent", unblock = "implemented" }'
write_leaf "$API_PROJECT" payments-sync "Payments sync"
write_leaf "$API_PROJECT" webhook-retry "Webhook retry"
LEGACY_TASK="$INV_PROJECT/tasks/legacy/task.toml"
LEGACY_ID=$(toml "$LEGACY_TASK" 'd["id"]')
LEGACY_GIT_HASH=$(sha256sum <"$LEGACY_WT/.git")
LEGACY_BRANCH=$(git -C "$INVOICING" rev-parse legacy)
LEGACY_TREE_HASH=$(dir_hash "$LEGACY_WT")
LEGACY_TASK_HASH=$(sha256sum <"$LEGACY_TASK")
launch_drawn

# ---------------------------------------------------------------------------------------------
step "Worktrees 1: a new task's worktree goes under the central directory, nothing is added to the checkout"
CHECKOUT_BEFORE=$(find "$INVOICING" -path "$INVOICING/.git" -prune -o -print | LC_ALL=C sort | sha256sum)
focus_center
wkeys s
choose "export invoices"
FIRST_DIR="$INV_PROJECT/tasks/export-invoices"
FIRST_WT=$(started "$FIRST_DIR")
poll 10 "the worktree $FIRST_WT within ten seconds" test -d "$FIRST_WT"
[ "$FIRST_WT" = "$CENTRAL/invoicing/export-invoices" ] || fail "the worktree is $FIRST_WT, not $CENTRAL/invoicing/export-invoices"
[ "$(find "$INVOICING" -path "$INVOICING/.git" -prune -o -print | LC_ALL=C sort | sha256sum)" = "$CHECKOUT_BEFORE" ] ||
  fail "the checkout changed: $(ls -A "$INVOICING")"
[ -z "$(git -C "$INVOICING" status --porcelain)" ] || fail "the checkout is not clean: $(git -C "$INVOICING" status --porcelain)"
git -C "$INVOICING" worktree list --porcelain | grep -qxF "worktree $(realpath "$FIRST_WT")" || fail "git does not list $FIRST_WT: $(git -C "$INVOICING" worktree list --porcelain)"
say "smoke: the worktree is $FIRST_WT; the checkout gained nothing and is clean"

# ---------------------------------------------------------------------------------------------
step "Worktrees 2: a repository's worktree_root puts its next worktree beside the checkout; the other repository is unchanged"
write_settings ', "worktree_root": "{repo}.worktrees"'
wait_repo api "root: own {repo}.worktrees" root-api
repo_has api "new worktrees go to: $HOME/src/api.worktrees/<task>"
repo_has invoicing "root: central -"
repo_has invoicing "new worktrees go to: $CENTRAL/invoicing/<task>"
wkeys s
choose "payments sync"
PAYMENTS_DIR="$API_PROJECT/tasks/payments-sync"
PAYMENTS_WT=$(started "$PAYMENTS_DIR")
poll 10 "the worktree $PAYMENTS_WT within ten seconds" test -d "$PAYMENTS_WT"
[ "$PAYMENTS_WT" = "$HOME/src/api.worktrees/payments-sync" ] || fail "the api worktree is $PAYMENTS_WT"
[ -z "$(git -C "$API" status --porcelain)" ] || fail "the api checkout is not clean: $(git -C "$API" status --porcelain)"
[ "$(toml "$FIRST_DIR/task.toml" 'd["worktree"]')" = "$FIRST_WT" ] || fail "the invoicing task moved"
say "smoke: api's next worktree is $PAYMENTS_WT; invoicing still reads central"

# ---------------------------------------------------------------------------------------------
step "Worktrees 3: a root under a read-only directory falls back to the central directory, once"
if [ "$(id -u)" = 0 ]; then
  say "smoke: skipped: running as root, so a 0555 directory is still writable"
else
  mkdir -p "$LOCKED"
  chmod 0555 "$LOCKED"
  write_settings ", \"worktree_root\": \"$LOCKED/{slug}\""
  wait_repo api "root: own $LOCKED/{slug}" root-locked
  repo_has api "is not writable"
  wkeys s
  choose "webhook retry"
  WEBHOOK_DIR="$API_PROJECT/tasks/webhook-retry"
  WEBHOOK_WT=$(started "$WEBHOOK_DIR")
  poll 10 "the worktree $WEBHOOK_WT within ten seconds" test -d "$WEBHOOK_WT"
  [ "$WEBHOOK_WT" = "$CENTRAL/api/webhook-retry" ] || fail "the fallback worktree is $WEBHOOK_WT, not $CENTRAL/api/webhook-retry"
  [ "$(grep -c 'worktree created in the central directory' "$WEBHOOK_DIR/task.toml")" = 1 ] ||
    fail "the history does not say once that the worktree fell back: $(cat "$WEBHOOK_DIR/task.toml")"
  grep -qF "$LOCKED is not writable" "$WEBHOOK_DIR/task.toml" || fail "the history does not name $LOCKED: $(cat "$WEBHOOK_DIR/task.toml")"
  [ -z "$(ls -A "$LOCKED")" ] || fail "$LOCKED gained $(ls -A "$LOCKED")"
  chmod 0755 "$LOCKED"
  say "smoke: with $LOCKED read-only the worktree is $WEBHOOK_WT and the history says so once"
fi
write_settings ""

# ---------------------------------------------------------------------------------------------
step "Worktrees 4: the old worktree is left where it is, listed as an old root, and still works"
dump_repos old-root
repo_has invoicing "old root: legacy ($LEGACY_ID)"
[ "$(sha256sum <"$LEGACY_WT/.git")" = "$LEGACY_GIT_HASH" ] || fail "the .git file of legacy changed"
[ "$(git -C "$INVOICING" rev-parse legacy)" = "$LEGACY_BRANCH" ] || fail "the branch legacy moved"
[ "$(dir_hash "$LEGACY_WT")" = "$LEGACY_TREE_HASH" ] || fail "the files of the legacy worktree changed"
[ "$(sha256sum <"$LEGACY_TASK")" = "$LEGACY_TASK_HASH" ] || fail "the task.toml of legacy changed: $(cat "$LEGACY_TASK")"
printf 'legacy work\n' >"$LEGACY_WT/legacy.txt"
git -C "$LEGACY_WT" add legacy.txt
git -C "$LEGACY_WT" "${GIT_ID[@]}" commit -qm "work in the old worktree"
[ "$(git -C "$INVOICING" rev-parse legacy)" != "$LEGACY_BRANCH" ] || fail "the commit in the legacy worktree did not land"
say "smoke: legacy is listed as an old root, byte for byte as made; a commit in it from the shell works"

# ---------------------------------------------------------------------------------------------
step "Worktrees 5: two long slugs get distinct folders of at most 48 characters"
for goal_word in alpha bravo; do
  wkeys s
  choose "reconcile $goal_word"
done
A_WT=$(started "$INV_PROJECT/tasks/$LONG_A")
B_WT=$(started "$INV_PROJECT/tasks/$LONG_B")
poll 10 "the worktrees of the long tasks" test -d "$A_WT" -a -d "$B_WT"
A_NAME=$(basename "$A_WT")
B_NAME=$(basename "$B_WT")
[ "$(dirname "$A_WT")" = "$CENTRAL/invoicing" ] && [ "$(dirname "$B_WT")" = "$CENTRAL/invoicing" ] || fail "the long worktrees are $A_WT and $B_WT"
[ "$A_NAME" != "$B_NAME" ] || fail "both long tasks got the folder $A_NAME"
[ "${#A_NAME}" -le 48 ] && [ "${#B_NAME}" -le 48 ] || fail "folders of ${#A_NAME} and ${#B_NAME} characters: $A_NAME, $B_NAME"
[ "$(toml "$INV_PROJECT/tasks/$LONG_A/task.toml" 'd["branch"]')" = "$LONG_A" ] || fail "the branch of the long task is not its full slug"
say "smoke: folders $A_NAME (${#A_NAME}) and $B_NAME (${#B_NAME}); the branches keep the 70-character slugs"

# ---------------------------------------------------------------------------------------------
step "Worktrees 6: a commit on the parent's branch reads rebase needed on the child; rebasing clears it"
wkeys s
choose "stack parent"
PARENT_WT=$(started "$INV_PROJECT/tasks/stack-parent")
poll 10 "the parent worktree" test -d "$PARENT_WT"
wkeys s
choose "stack child"
CHILD_WT=$(started "$INV_PROJECT/tasks/stack-child")
poll 10 "the child worktree" test -d "$CHILD_WT"
wait_toml "$INV_PROJECT/tasks/stack-child/task.toml" 'd.get("base")' stack-parent
dump_repos stack
repo_has invoicing "slow poll: false"
go_top
drill Invoicing
case $(view_detail "Stack child") in *"rebase needed"*) fail "the child needs a rebase before the parent moved: $(cat "$VIEW_FILE")" ;; esac
git -C "$PARENT_WT" "${GIT_ID[@]}" commit -q --allow-empty -m "parent moves on"
sleep 2
dump_view rebase-needed
case $(view_detail "Stack child") in *"rebase needed"*) ;; *) fail "the child does not read rebase needed two seconds after the parent moved: $(cat "$VIEW_FILE")" ;; esac
shot work-rebase-needed
frame_has_ui work-rebase-needed
git -C "$CHILD_WT" "${GIT_ID[@]}" rebase -q stack-parent
sleep 2
dump_view rebase-cleared
case $(view_detail "Stack child") in *"rebase needed"*) fail "the child still reads rebase needed two seconds after its rebase: $(cat "$VIEW_FILE")" ;; esac
say "smoke: the child read rebase needed after the parent's commit and not after its own rebase ($RUN/work-rebase-needed.png)"

# ---------------------------------------------------------------------------------------------
step "Worktrees 7: an idle store runs no rebase probe"
dump_repos idle-before
PROBES_BEFORE=$(probes)
[ -n "$PROBES_BEFORE" ] || fail "no rebase_probes line: $(cat "$REPOS_FILE")"
sleep 40
dump_repos idle-after
[ "$(probes)" = "$PROBES_BEFORE" ] || fail "rebase_probes went from $PROBES_BEFORE to $(probes) while idle: $(cat "$REPOS_FILE")"
say "smoke: rebase_probes stayed at $PROBES_BEFORE over 40 idle seconds"

# ---------------------------------------------------------------------------------------------
step "Worktrees 8: the worktree paths are in no file an agent reads, and no instruction file was made"
quit_app
WORKTREES=("$FIRST_WT" "$PAYMENTS_WT" "$A_WT" "$B_WT" "$PARENT_WT" "$CHILD_WT")
[ -z "${WEBHOOK_WT:-}" ] || WORKTREES+=("$WEBHOOK_WT")
patterns=()
for path in "${WORKTREES[@]}"; do patterns+=(-e "$path"); done
leaks=$(grep -rlIF "${patterns[@]}" --exclude=task.toml --exclude=.op.toml --exclude=.git --exclude-dir=.git \
  --exclude-dir=chats --exclude-dir=logs "$HOME" 2>/dev/null || true)
[ -z "$leaks" ] || fail "worktree paths are written in files that are not task.toml, journals, chat files or git metadata: $leaks"
instructions=$(find "$HOME" \( -name CLAUDE.md -o -name AGENTS.md -o -name .rules \) -print)
[ -z "$instructions" ] || fail "instruction files were created: $instructions"
say "smoke: ${#WORKTREES[@]} worktree paths appear only in task.toml and git metadata; no CLAUDE.md, AGENTS.md or .rules"

cp "$RUN/config-backup/settings-38.json" "$SETTINGS_FILE"
