# Scenario for feature 46-project-repository. Sourced by script/bongos/smoke after the base flow (the
# app is quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME,
# DISPLAY, the proxy and the helpers of lib.sh. Debug builds only: it reads the Work panel through
# `work: dump view` (<logs>/work-view.txt); everything else is read from disk (git, the project
# folder, the per-machine record of project locations).
#
# Setup: `"base_keymap": "Emacs"` (the C-c w keys exist only there). An old-format project, Legacy,
# is made first with project repositories off, then the app is relaunched with them on.
#   1. C-c w p n "Invoicing": the project folder in the default place has the four parts
#      (repository, working-copies, state, files); the repository has one commit.
#   2. Ten tasks and three edges: the repository gains one commit each, every one a person's
#      (Bongos-Author-Kind: person); no row is left "landing" or "not landed". An edge that closes a
#      cycle is refused and adds no commit; the same cycle written in a working copy is refused at
#      landing, with the chain named, and the working copy stays.
#   3. C-c w a on a task changes its state and adds no commit.
#   4. A working copy holding a file under state/ and a 6 MB file is refused at landing, naming both.
#   5. C-c w g b with a local bare remote: bongos/state appears there with FORMAT.
#   6. A second home: project: add existing from the bare remote, then project: restore: the
#      project's Work view reads as it did on the first home.
#   7. The old-format project converts (project: convert); its old folder is byte for byte as it was.
# Deviations from the brief, each forced by the harness or by the app:
# - C-c w n makes a one-off, which goes to the separate One-offs project, so the ten tasks are made
#   with C-c w N under the project's top group.
# - There is no `project: dump repository`; the working copy is read from the project folder.
# - A "landing" row lasts a fraction of a second, too short for a dump, so the scenario asserts the
#   landed result (a commit and no row left marked).
# The scenario leaves the app quit, restores settings.json and never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 46-project-repository skipped under BONGOS_STOCK=1: the stock run has no Work view"
  return 0
fi

for tool in git python3 sha256sum find; do
  command -v "$tool" >/dev/null || { echo "bongos: $tool is not installed; the scenario needs it" >&2; exit 1; }
done

fail() {
  echo "bongos: $*" >&2
  exit 1
}

HOME_ONE=$HOME
HOME_TWO=/tmp/h2
mkdir -p "$RUN/home2"
ln -sfn "$RUN/home2" "$HOME_TWO"

# Points every path at one of the two homes.
set_home() {
  export HOME=$1
  CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
  SETTINGS_FILE="$CONFIG_DIR/settings.json"
  LOGS_DIR="$HOME/${BONGOS_LOG_PATH%/*}"
  DATA_DIR="${LOGS_DIR%/logs}"
  VIEW_FILE="$LOGS_DIR/work-view.txt"
  PROJECTS="$DATA_DIR/projects"
  OLD_ROOT="$HOME/.bongos"
  PROJ="$PROJECTS/invoicing"
  REPO="$PROJ/repository"
}
set_home "$HOME_ONE"

BARE="$RUN/remote/invoicing.git"
GIT_ID=(-c user.name=bongos -c user.email=bongos@example.invalid)

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

# --- Reading the Work view (see scenario 03 for the format) -----------------------------------

dump_view() {
  rm -f "$VIEW_FILE"
  palette "work: dump view"
  assert_file "$VIEW_FILE"
  cp "$VIEW_FILE" "$RUN/work-view-$1.txt"
}

breadcrumb() { head -n1 "$VIEW_FILE"; }

view_title() {
  awk -v goal="$1" 'NR > 1 && substr($0, 2, 1) != " " {
    t = substr($0, 2); if (t == goal || index(t, goal "  ") == 1) { print t; exit }
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

go_top() {
  local i
  for i in 1 2 3 4 5; do
    dump_view top
    [ "$(breadcrumb)" = Work ] && return 0
    palette "work: up"
  done
  fail "work: up did not reach the top level: $(breadcrumb)"
}

# Dumps the level that lists the tasks of Invoicing's top group, as <name>.
project_level() {
  go_top
  select_row Invoicing
  act keys Return
  dump_view "$1"
  [ "$(breadcrumb)" = "Work > Invoicing" ] || fail "drilling into Invoicing shows '$(breadcrumb)'"
}

# Waits until the Work view lists the top-level row <title> (the store has read the project).
view_lists() { dump_view listed; [ -n "$(view_title "$1")" ]; }
wait_listed() {
  go_top
  poll 40 "$1 in the Work view" view_lists "$1" || fail "$1 is not in the Work view: $(cat "$VIEW_FILE")"
}

# --- Reading the repository and the project folder ---------------------------------------------

commits() { git -C "$REPO" rev-list --count main; }
commits_are() { [ "$(commits)" = "$1" ]; }
wait_commits() { poll 40 "$2: the repository to have $1 commits (it has $(commits 2>/dev/null || echo none))" commits_are "$1"; }

is_dir() { [ -d "$1" ]; }

# The files of a tree and their contents, as one hash.
tree_hash() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum); }

# What the checks of the working copy <name> found, as the state store records it: the text of every
# failed result of its change (the app also shows it in a toast, which a scenario cannot read).
failed_checks() {
  python3 -I - "$PROJ/state/state.db" "work/$1" <<'PY'
import sqlite3, sys
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
for check, result in db.execute(
    "select r.check_name, r.result_toml from check_results r join changes c on c.change_id = r.change_id"
    " where c.branch = ? and r.result_toml like '%outcome = \"fail\"%'", (sys.argv[2],)):
    print(check + ": " + result)
PY
}

# A preview buffer opens with the focus, ahead of the question that follows it, so the question is
# answered with the mouse: its first row ("Restore", "Convert").
answer_first_row() {
  LAST_SCREEN=$(screen_hash)
  xdo mousemove 100 728 click 1
  poll 30 "the screen to change after answering" screen_changed
  wait_for_settled
}

has_failed_checks() { [ -n "$(failed_checks "$1")" ]; }

copy_names() { ls -1 "$PROJ/working-copies" 2>/dev/null || true; }

copy_is_new() {
  local now
  now=$(copy_names)
  [ "$now" != "$1" ]
}

# `project: edit` with the name offered, the new working copy's name on stdout.
make_working_copy() {
  local before after
  before=$(copy_names)
  wkeys g e
  act keys Return
  poll 40 "a new working copy" copy_is_new "$before"
  after=$(copy_names)
  comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort) | head -n1
}

# `project: land`: the only working copy in the list, then the subject offered.
land_copy() {
  # The window of the working copy opens after the directory exists; keys sent before its prompt is
  # up are lost, so give it time to settle first.
  sleep 4
  wait_for_settled
  focus_center
  wkeys g l
  sleep 2
  wait_for_settled
  choose "$1"
  wait_for_settled
  act keys Return
  wait_for_settled
}

wait_task_file() {
  poll 30 "$1 task.toml in $2" sh -c 'find "$1" -path "*$2*" -name task.toml | grep -q .' _ "$1" "$2"
}

# ---------------------------------------------------------------------------------------------
step "setup: Legacy, an old-format project made before the app runs with project repositories"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-46.json"
printf '%s\n' '{
  "base_keymap": "Emacs",
  "task_builder": { "enabled": false },
  "project_repository": { "enabled": false }
}' >"$SETTINGS_FILE"
launch_drawn
focus_center
wkeys p n
choose Legacy
assert_file "$OLD_ROOT/projects/legacy/project.toml"
wait_listed Legacy
focus_center
wkeys shift+n
choose legacy
choose "Reconcile the ledger"
assert_file "$OLD_ROOT/projects/legacy/tasks/reconcile-the-ledger/task.toml"
quit_app
printf '%s\n' '{
  "base_keymap": "Emacs",
  "task_builder": { "enabled": false },
  "project_repository": { "enabled": true }
}' >"$SETTINGS_FILE"
[ ! -e "$PROJECTS/legacy" ] || fail "Legacy was made as a project repository"
launch_drawn

# ---------------------------------------------------------------------------------------------
step "Project repository 1: project: new makes the four parts and one commit"
focus_center
wkeys p n
choose Invoicing
poll 40 "the project folder $PROJ" is_dir "$PROJ/repository/.git"
for part in repository working-copies state files; do
  poll 20 "$PROJ/$part" is_dir "$PROJ/$part"
done
wait_commits 1 "the new project"
[ "$(git -C "$REPO" log --oneline | wc -l)" = 1 ] || fail "the new repository has more than one commit: $(git -C "$REPO" log --oneline)"
[ ! -e "$OLD_ROOT/projects/invoicing" ] || fail "an old-format Invoicing was made: $(ls -la "$OLD_ROOT/projects")"
say "smoke: $PROJ has repository, working-copies, state and files; the repository has 1 commit"

# ---------------------------------------------------------------------------------------------
step "Project repository 2: ten tasks and three edges land as a person's commits; a cycle is refused"
wait_listed Invoicing
expected=1
for n in 01 02 03 04 05 06 07 08 09 10; do
  wkeys shift+n
  choose invoicing
  choose "Task $n"
  expected=$((expected + 1))
  wait_commits "$expected" "task $n"
done
add_edge() {
  wkeys e
  choose "$1"
  choose "$2"
  choose after
  act keys Return
}
add_edge "task 02" "task 01"
add_edge "task 03" "task 02"
add_edge "task 04" "task 03"
wait_commits 14 "three edges"
# Every commit after the first is a person's, trailer included.
persons=$(git -C "$REPO" log main --format='%(trailers:key=Bongos-Author-Kind,valueonly)' | grep -cx 'person' || true)
others=$(git -C "$REPO" log main --format='%(trailers:key=Bongos-Author-Kind,valueonly)' | grep -vxE 'person|' | sort | uniq -c || true)
[ "$persons" -ge 10 ] || fail "only $persons commits carry Bongos-Author-Kind: person: $(git -C "$REPO" log main --format=%B)"
[ -z "$others" ] || fail "a commit has an author kind other than person: $others"
project_level tasks
for n in 01 02 03 04 05 06 07 08 09 10; do
  [ -n "$(view_title "Task $n")" ] || fail "no row 'Task $n' under Invoicing: $(cat "$VIEW_FILE")"
done
if grep -qE 'landing|not landed' "$VIEW_FILE"; then fail "a row is still marked landing or not landed: $(cat "$VIEW_FILE")"; fi
# The edge that closes the cycle (01 after 04 would, with 02 after 01 ...) is refused by the command.
add_edge "task 01" "task 04"
wait_for_settled
sleep 3
commits_are 14 || fail "the cycle edge added a commit: $(git -C "$REPO" log --oneline | head -n3)"
# The same cycle written in a working copy is refused when it lands, naming the chain.
MAIN_BEFORE=$(git -C "$REPO" rev-parse main)
CYCLE=$(make_working_copy)
[ -n "$CYCLE" ] || fail "no working copy was made"
wait_task_file "$PROJ/working-copies/$CYCLE" task-01
FIRST_FILE=$(find "$PROJ/working-copies/$CYCLE" -path '*task-01*' -name task.toml | head -n1)
printf '\n[[after]]\ntask = "task-04"\nunblock = "merged"\n' >>"$FIRST_FILE"
land_copy "$CYCLE"
sleep 3
[ "$(git -C "$REPO" rev-parse main)" = "$MAIN_BEFORE" ] || fail "the cycle landed: $(git -C "$REPO" log --oneline | head -n3)"
[ -d "$PROJ/working-copies/$CYCLE" ] || fail "the working copy of the refused cycle is gone"
poll 30 "the failed checks of $CYCLE" has_failed_checks "$CYCLE" ||
  fail "no check of $CYCLE failed: $(git -C "$REPO" log --oneline | head -n3)"
failed_checks "$CYCLE" | grep -q 'no-cycle' || fail "the cycle was not refused by no-cycle: $(failed_checks "$CYCLE")"
failed_checks "$CYCLE" | grep -q 'task-0[1-4].*task-0[1-4]' || fail "the refusal of the cycle names no chain of tasks: $(failed_checks "$CYCLE")"
say "smoke: 14 commits, all a person's; the cycle edge refused with no commit; the working copy $CYCLE stays"

# ---------------------------------------------------------------------------------------------
step "Project repository 3: advancing a task changes its state and adds no commit"
before_row=$(view_title "Task 01")
wkeys a
choose "task 01"
wait_for_settled
sleep 2
project_level advanced
after_row=$(view_title "Task 01")
[ "$before_row" != "$after_row" ] || fail "advancing Task 01 did not change its row: '$before_row'"
commits_are 14 || fail "advancing a task added a commit: $(git -C "$REPO" log --oneline | head -n3)"
cp "$VIEW_FILE" "$RUN/view-home-one.txt"
say "smoke: Task 01 went from '$before_row' to '$after_row' with no new commit"

# ---------------------------------------------------------------------------------------------
step "Project repository 4: state and a large file in a working copy are refused naming both"
BAD=$(make_working_copy)
[ -n "$BAD" ] || fail "no working copy was made"
BAD_DIR="$PROJ/working-copies/$BAD/definitions"
poll 20 "$BAD_DIR" is_dir "$BAD_DIR"
mkdir -p "$BAD_DIR/state"
printf 'copied from the state part\n' >"$BAD_DIR/state/ledger.txt"
head -c $((6 * 1024 * 1024)) /dev/zero >"$BAD_DIR/diagram.png"
MAIN_BEFORE=$(git -C "$REPO" rev-parse main)
land_copy "$BAD"
sleep 3
[ "$(git -C "$REPO" rev-parse main)" = "$MAIN_BEFORE" ] || fail "the working copy with state and a 6 MB file landed"
poll 30 "the failed checks of $BAD" has_failed_checks "$BAD" ||
  fail "no check of $BAD failed"
failed_checks "$BAD" | grep -q 'state/ledger.txt' || fail "the refusal does not name state/ledger.txt: $(failed_checks "$BAD")"
failed_checks "$BAD" | grep -q 'diagram.png' || fail "the refusal does not name diagram.png: $(failed_checks "$BAD")"
say "smoke: the landing was refused, naming state/ledger.txt and diagram.png"

# ---------------------------------------------------------------------------------------------
step "Project repository 5: project: backup now pushes bongos/state with FORMAT to a local bare remote"
quit_app
mkdir -p "$RUN/remote"
git init -q --bare -b main "$BARE"
LOCATIONS=$(find -L "$HOME" -name project-repositories.toml | head -n1)
[ -n "$LOCATIONS" ] || fail "no project-repositories.toml under $HOME"
python3 -I -c 'import re, sys
p, remote = sys.argv[1], sys.argv[2]
t = open(p).read()
t, n = re.subn(r"^(folder = .*)$", lambda m: m.group(1) + "\nremote = \"" + remote + "\"", t, count=1, flags=re.M)
assert n == 1
open(p, "w").write(t)' "$LOCATIONS" "$BARE" ||
  fail "could not add the remote to $LOCATIONS: $(cat "$LOCATIONS")"
launch_drawn
focus_center
wkeys g b
bongos_state_has_format() { git --git-dir="$BARE" ls-tree --name-only bongos/state 2>/dev/null | grep -qx FORMAT; }
poll 60 "bongos/state with FORMAT in $BARE" bongos_state_has_format ||
  fail "the remote has no bongos/state with FORMAT: $(git --git-dir="$BARE" branch -a 2>&1); $(cat "$LOCATIONS")"
say "smoke: $BARE has bongos/state: $(git --git-dir="$BARE" ls-tree --name-only bongos/state | xargs)"
# Backup sends only the state branch; the repository's main reaches the same remote the way a person
# shares the project (here the remote fetches it).
git --git-dir="$BARE" fetch -q "$REPO" main:main || fail "could not give $BARE the repository's main"
project_level before-move
cp "$VIEW_FILE" "$RUN/view-home-one.txt"
quit_app

# ---------------------------------------------------------------------------------------------
step "Project repository 6: a second home adds the project from the remote and restores it"
mkdir -p "$HOME_TWO/$BONGOS_CONFIG_PATH"
for f in global_settings.json settings.json keymap.json; do
  [ ! -f "$CONFIG_DIR/$f" ] || cp "$CONFIG_DIR/$f" "$HOME_TWO/$BONGOS_CONFIG_PATH/$f"
done
set_home "$HOME_TWO"
launch_drawn
focus_center
wkeys g a
choose "$BARE"
poll 90 "the clone in $PROJ" is_dir "$PROJ/repository/.git"
# The project opens at once; restoring now still finds the state that came with the clone (an empty
# snapshot a later start takes would be newer than the remote's).
wait_listed Invoicing
focus_center
# The cloned state branch is this machine's snapshot, taken on the first home.
palette "project: restore"
# The preview is read first; the question comes after it.
sleep 4
wait_for_settled
shot restore-question
answer_first_row
poll 60 "the replaced state under $PROJ/state" sh -c 'find "$1/state" -path "*replaced*" -type f | grep -q .' _ "$PROJ"
quit_app
launch_drawn
project_level second-home
# The files part (a task's artifacts) is not in a backup, so the artifact counts are left out; every
# state, blocker and edge must read the same.
no_counts() { tail -n +2 "$1" | sed -E 's/ · [0-9]+ artifacts?//'; }
diff <(no_counts "$RUN/view-home-one.txt") <(no_counts "$VIEW_FILE") >"$RUN/view-diff.txt" ||
  fail "the second home's Invoicing reads differently from the first's: $(cat "$RUN/view-diff.txt")"
say "smoke: after add existing and restore the Invoicing level matches the first home's"
quit_app

# ---------------------------------------------------------------------------------------------
step "Project repository 7: the old-format project converts and its folder is unchanged"
set_home "$HOME_ONE"
launch_drawn
focus_center
OLD_DIR="$OLD_ROOT/projects/legacy"
old_hash=$(tree_hash "$OLD_DIR")
wkeys g shift+c
choose legacy
sleep 3
wait_for_settled
shot convert-question
answer_first_row
poll 60 "the converted project in $PROJECTS/legacy" is_dir "$PROJECTS/legacy/repository/.git"
for part in repository working-copies state files; do
  poll 20 "$PROJECTS/legacy/$part" is_dir "$PROJECTS/legacy/$part"
done
[ "$(tree_hash "$OLD_DIR")" = "$old_hash" ] || fail "the old folder changed while converting: $OLD_DIR"
LOCATIONS=$(find -L "$HOME" -name project-repositories.toml | head -n1)
grep -q 'converted_from' "$LOCATIONS" || fail "the record names no converted_from: $(cat "$LOCATIONS")"
quit_app
say "smoke: Legacy converted into $PROJECTS/legacy; the old folder's hash is unchanged"

cp "$RUN/config-backup/settings-46.json" "$HOME_ONE/$BONGOS_CONFIG_PATH/settings.json"
