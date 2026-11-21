# bongos

A fast IDE for Kotlin work with an Emacs-shaped UX, built as a fork of
[Zed](https://github.com/zed-industries/zed), with projects and tasks as a first-class
model, terminal-hosted Claude Code and Codex sessions, headless agent workflows, and a
native diagram canvas for reviewing and sketching systems.

Status: early development. The Zed v1.9.0 tree is imported; bongos-specific changes land feature by
feature.

## License

bongos is licensed under GPL-3.0-or-later. The crates that declare `Apache-2.0` in their
`Cargo.toml`, including `gpui`, remain Apache-2.0. See [LICENSE](LICENSE) for the notice and
full GPL text, and [ORIGIN.md](ORIGIN.md) for the Zed fork it is derived from.

## Why

- Fast: Rust and GPU rendering, nothing web.
- Kotlin: works with the open-source kotlin-lsp and with the proprietary IntelliJ language
  server, selected per machine.
- Emacs: every action is a command, any command takes a key, a minibuffer at the bottom,
  kill ring, mark ring, isearch, universal argument, window letters.
- Agents: Claude Code and Codex run in embedded terminals bound to the focused task, with
  sessions saved, archived and forkable. Headless workflows produce PRs, documents and
  tasks, or fail explicitly.
- Diagrams: clickable code-bound diagrams for review, and a freeform canvas for describing
  systems to agents.

## Projects and tasks

The Work view keeps projects and tasks as plain files under `~/.bongos`, so an agent, an editor and
the Work panel all work on the same tree. It is on by default; set `work.enabled` to `false` to
turn it off (bongos then neither reads nor watches `~/.bongos`, and the Work panel and commands
are hidden).

```text
~/.bongos/projects/<slug>/                 project.toml, task.toml, plan.md, prompts.txt, artifacts.toml, tasks/<slug>/...
~/.bongos/oneoffs/<activity>/<date>-<slug>/ task.toml, prompts.txt, artifacts.toml, tasks/<slug>/...
```

A project holds groups and tasks, nested through `tasks/<slug>/` directories; a one-off is a task
that belongs to no project. Every task directory has a `task.toml` (goal, kind, todos, worktree).
Any other file in a task directory is an artifact, described and made visible or human-only in
`artifacts.toml`. A new task starts with a `plan` todo; the task builder (below) lays out the tree.

- **Shared set.** What chat sessions learn about a task (its goal, paths and artifacts) is
  computed fresh from the files on every call, never from a cache, so the agent, the panel and the
  terminal see one set. Human-only artifacts are left out: their path and description are never
  told to a session. Locking is not file protection; if the manifest cannot be read, nothing is
  listed.
- **Reread, then rename.** bongos writes only after rereading each file it replaces and comparing
  it with what it loaded; if another writer changed it, the tree is reloaded and the command runs
  again on the fresh state. An agent that moves or renames task files should do the same: reread
  `task.toml` first, and never write from a stale copy.
- **Keys (Emacs keymap only).** `C-c w w` toggles the panel, `C-c w n` makes a task, `C-c w s`
  starts one, `C-c w a` advances, `C-c w c` completes (`task: complete`; `task: close` is its unbound alias), `C-c w G` opens the graph, and the `C-c w d ...`,
  `C-c w p ...`, `C-c w g ...` and artifact keys cover todos, projects, repositories and artifacts; the rest are in the command
  palette (`assets/keymaps/linux/emacs.json`). In the panel, `tab` expands a row inline and `^`
  goes up one level.

### Worktrees

`C-c w s` (`task: start`) makes a git worktree and a branch for a `pr-ship`, `pr-merge`, `spike` or
`babysit` leaf. Nothing is added to your checkout unless you ask for it.

- **Default location.** `<data dir>/worktrees/<repository>/<slug>`: on Linux
  `~/.local/share/bongos/worktrees/...` (or under `$XDG_DATA_HOME`), on macOS
  `~/Library/Application Support/bongos/worktrees/...`.
- **Choosing a root.** A repository's `worktree_root` in `work.repositories` and
  `work.default_worktree_root` (for repositories that name none) are path templates with two
  tokens: `{repo}`, the repository's name, and `{slug}`, the task's slug. `{slug}` is appended
  when missing, so a worktree never lands on the root itself. A leading `~` is your home directory;
  any other path that is not absolute is relative to the parent of the checkout, so `{repo}.worktrees`
  puts the worktrees of `~/src/api` in `~/src/api.worktrees/<slug>`. Unknown tokens, unbalanced braces and
  `..` are refused with the setting named. A root inside the checkout works too; its top directory
  is added to the repository's `.git/info/exclude`, never to `.gitignore`. A new root applies to
  tasks started from then on.
- **Fallback.** When the parent of the chosen path cannot be made or written, the worktree is made
  in the default location instead, once, and the toast and the task's history say which parent
  was not writable.
- **Limits.** A slug longer than 48 characters is cut at a dash and gets `-` and six hex digits of
  the task's id, so two long names with one prefix still get different folders; the task, its
  directory and its branch keep the full slug. A whole worktree path is limited to 200 characters:
  the slug is shortened further to find one that fits, and when none does the start is refused and
  names the setting to shorten.
- **Old worktrees stay.** Worktrees that older versions made under `<repository>/.bongos-worktrees/`
  are never moved, renamed or deleted, and a task that records a worktree is never given another.
  The `/.bongos-worktrees/` line stays in `info/exclude`.
- **Repositories page.** Settings, Work, Repositories lists each entry of `work.repositories` with
  where its new worktrees go, what is wrong with that (a bad template, a path over 200 characters,
  an unwritable parent, a root inside the checkout) and a notice while tasks still have a worktree
  under `.bongos-worktrees/`. `work: edit worktree root` writes the `worktree_root` of one
  repository.
- **Rebase needed.** A task stacked on a sibling reads "rebase needed" when the sibling's branch
  moves past what the task was cut from and the task's branch does not contain it. bongos watches
  the refs of repositories that have such a task, so the flag appears within a moment of a `git
  commit` in the parent's worktree and clears the same way after a `git rebase`; nothing runs on a
  timer. On a file system that delivers no change notifications (network mounts, most FUSE mounts)
  the refs cannot be watched, so the check runs every 5 minutes instead. Starting bongos with
  `ZED_FILE_WATCHER_MODE=poll` does the same for every repository.

## Project repositories

A project is a folder with four parts: `repository` (a git repository holding the definitions of
its tasks, which only a landing changes), `working-copies` (where changes are made before they
land), `state` (what stays on this machine: status, todos done, runs, history) and `files`.
Every change to the definitions, from a command or a working copy, goes through checks and lands
as one commit on `main` with trailers saying who made it; state changes never make a commit.

- **Keys (Emacs keymap).** `C-c w p n` makes a project in `<data dir>/projects/<name>`. `C-c w g e`
  makes a working copy and opens it, `C-c w g l` lands it, `C-c w g c` only checks it, `C-c w g
  v` shows the changes and `C-c w g h` the history. `C-c w G` shows the tasks of a group as a graph.
- **Setting.** `project_repository.enabled` turns this on (it is on by default); while it is off,
  projects keep the old format. The same page of the settings window has the folder, the limits
  and the backup intervals.
- **Backup and restore.** The state is snapshotted to a branch (`bongos/state`) of the repository
  and pushed to the project's backup remote when it has one. `C-c w g b` takes and pushes a
  snapshot now, `C-c w g s` shows the backup. On a new machine, `C-c w g a` (`project: add
  existing`) adds a project from a folder or a remote address and `C-c w g R` (`project: restore`)
  rebuilds its state from the newest snapshot; the state it replaces is kept in `state/replaced/`.
- **Old projects.** `C-c w g C` (`project: convert`) copies an old-format project into a project
  repository after showing what it will make. The old folder is only read.

## Chat sessions

Chats run Claude Code and Codex in terminals bound to a task. They are on by default and need the
Work view; set `chats.enabled` to `false` to turn them off (no record is read, no session type is
seeded, and the `chat:` commands are hidden). `chats.agents.claude.command` and
`chats.agents.codex.command` name the programs to run, and `extra_args` adds arguments to every
launch.

```text
<task dir>/chats/<id>.toml                a chat on that task (name, agent, type, layout, session id, parent)
<project dir>/chats/<id>.toml             a project chat: one on the project's top task
~/.bongos/chat-types/<type>/              session types: type.toml, context.md, optional claude.toml, codex.toml
~/.bongos/chat-types/skills/<name>/       skills a type lists (then ~/.claude/skills/<name>/)
```

- **Session types.** `implement`, `review`, `plan`, `thinking` and `free` are seeded on first run
  and never overwritten: edit them, add your own directory, or run `chat: edit session type`. A
  type sets the layout (`dual` shows the terminal with the task's `prompts.txt` beside it,
  `chat-only` the terminal alone), the skills, and which of the task file, notes directory,
  worktree, PR links and artifact list the session is told about. An implement chat runs in the
  task's worktree (start the task first); a chat on a group, document or project runs in the task
  directory, or in the project's repository. Put skills in `~/.bongos/chat-types/skills/<name>/SKILL.md`
  and list the name in a type's `skills`; a skill that cannot be found is labeled on the chat's
  row as `missing skill`. A type is read at every start, so an edit applies to the next chat.
- **What the session is told.** Each start writes its own launch files under the data directory
  (`chats/<id>/`, readable by you only): `context.md` with the goal, paths and the shared
  artifact list, which Claude prints through a `SessionStart` hook (or gets appended to its system
  prompt when its CLI has no hooks) and Codex reads from an instructions file. The list is computed from the task's files at the start. The
  Chats list says when it has changed since a running chat began; the next resume gets the new one.
- **Human-only is a guarantee.** An artifact marked human-only (`C-c w h`) is never told to a
  session: its path, name and description appear in no generated file, no command line argument and
  no environment variable. Hide one and resume the chat, and the regenerated list no longer has it.
  This is not file protection: a CLI can still read any file it can reach, and a manifest that
  cannot be parsed lists nothing.
- **Chat state.** Each chat shows `running`, `waiting` or `idle`. A Claude chat's state is exact:
  Claude's own hooks (session start, prompt submitted, turn done, notification, tool done) run
  `bongos --chat-report`, which tells bongos the event and nothing else, never the prompt, message,
  tool data, transcript path or directory. A Codex chat's state is estimated as before, from the
  bell, terminal output and an idle timer. An estimated state carries a muted "estimated" tag in the
  chat's header and its list row, whose tooltip says why: reports are off (`chats.channels`), the
  report socket is unavailable, the Claude version is older than the first with hooks bongos knows,
  no report has come yet, a turn ended without a report (interrupted or denied), the agent does not
  report its state yet, the CLI exited when given the hooks (it is then started once more without
  them), or the bongos path cannot be quoted. `chats.channels` (default `true`) turns reports on for
  new launches; turning it off stops them at once and running chats go back to estimates.
  `chats.idle_after_secs` and `chats.submit_delay_ms` apply only to the fallback: a chat without
  reports, or a turn that ended without one. Reports arrive on one socket per bongos process,
  `$XDG_RUNTIME_DIR/bongos/chat-<pid>.sock`, else `<temp dir>/bongos-<uid>/chat-<pid>.sock`: the
  directory is yours alone (mode `0700`, refused if it is a symlink or open to others), the socket
  is mode `0600`, a connection from another user is dropped, and a report must name a running chat
  and carry that launch's token, which lives only in bongos's memory and the chat's environment.
  This keeps out other users and accidental cross-talk (a stale process, a copied environment, a
  nested CLI). It does not stop a hostile program running as you, which could read a chat's
  environment from `/proc`; such a program can already do anything you can.
- **Fallbacks are labeled.** bongos checks each Claude version against a table of the versions it
  has verified (flags, the login probe and hooks), instead of parsing `--help`: a newer version is
  assumed to keep everything, and an older or unreadable one gets the fallbacks; Codex is still read
  from its `--help`. When a feature is missing, the chat still starts and its row in the Chats list
  says what changed (`labels:`), for example "session id read from the CLI's files", "name not
  passed to the CLI", "context appended to the system prompt (no hooks)", or "started a new session
  (no session to resume)". A fork when the CLI has no fork command is a fresh session that starts
  from a note naming the chat it continues, and is labeled `handoff`; nothing passes one off as a
  native fork.
- **Keys (Emacs keymap only).** `C-c c n` starts a chat on the focused task, `C-c c o` opens one
  (resuming it when closed), `C-c c ]` and `C-c c [` cycle, `C-c c k` closes, `C-c c a` archives and
  `C-c c u` restores, `C-c c l` lists the archive, `C-c c r` renames, `C-c c f` forks into another
  task and `C-c c F` forks in place, `C-c c p` shows or hides the prompts pane, `C-c c t` and
  `C-c c e` focus the terminal and the prompts pane, `C-c c v` shows what the chat was given, and
  `C-c c P` toggles the panel. The rest, such as `chat: send region` (sends the selection, or the
  whole prompts buffer, as one paste and one Enter), are in the command palette. In a chat terminal
  `C-c` starts a prefix, so `C-c C-c` sends the interrupt to the CLI at once, and a lone `C-c` reaches
  it after the one-second prefix timeout.
- **Closing and quitting.** Closing keeps the record; a chat that was never prompted is deleted
  only when bongos is sure nothing was said in it. Quitting asks while chats are running, naming
  them, then ends them; each resumes from the Chats panel with the CLI's own resume
  (`claude --resume <id>`, `codex resume <id>`) in its recorded directory. Archiving moves a record to
  the archive and keeps it. Only `chat: delete` removes a record, and the CLIs' own transcripts are
  never touched.

## Prompt editor

`prompts.txt`, the file beside a task or project that its chat reads prompts from, gets an editing
mode: each prompt is a block, and the mode sends a block to the chat. It is on by default;
`prompts.enabled` set to `false` turns it off (no rules or mode line, and the commands are hidden).
The file stays plain text: the mode never writes anything you did not type or send, and it saves
exactly what is in the buffer (no trailing-whitespace trim, no final newline added, no formatting).

```text
=== prompt 2026-11-05 13:40 ===
Add proration to the invoice lines.

- cover credits
- cover refunds
=== prompt 2026-11-05 13:52 | sent 2026-11-05 14:02 ===
Finish with unit tests.
```

- **Markers.** A block starts at a marker line, `=== prompt <created> ===`, and runs to the next
  marker. The stamps are `YYYY-MM-DD HH:MM`; once the block is sent the marker reads
  `=== prompt <created> | sent <sent> ===`, and a later send updates the sent stamp. Text before the
  first marker is a block without one, and a send inserts the marker above it. Only a line that is
  exactly a marker counts; anything else is prompt text. `prompts.marker_open` and `prompts.marker_close` change the marker text (the default syntax is always recognized too). The editor draws each marker as a rule
  with its stamps, dims the bodies of sent blocks and brightens the current one; the mode line
  shows `prompt 3/7 · pending · auto-submit on` (or `sent 14:02`).
- **Keys (Emacs keymap only, in the prompts editor).** `C-c C-c` sends the prompt at point (it asks
  first when the block was sent before), `C-c C-r` sends the selection exactly and marks nothing (with no selection it is `C-c C-c`),
  `C-c C-n` starts a new prompt, `C-c C-p` and `C-c C-f` go to the previous and next prompt,
  `C-c C-j` to the next one not sent yet, `C-c C-w` copies the prompt (or the selection) and
  `C-c C-a` turns auto-submit on or off for the chat. `C-c C-u` and `C-c C-d` choose the file
  and go back to the default. `C-c c e` focuses the prompts pane of the shown chat. In the
  command palette, `prompts: send without marking` sends the block and leaves its marker alone,
  `prompts: open` and `prompts: mode` open the prompts and turn the mode on for the current file.
  In a chat terminal these keys are not bound: `C-c C-c` and the rest reach the CLI as typed.
- **A send is one paste.** The text of the block (without blank lines at its edges, and without the
  marker) reaches the CLI as one bracketed paste, then Enter when auto-submit is on. After it
  arrives the marker is stamped sent and point moves to the next block, or to a new one after the
  last. Nothing is marked when the text could not be delivered.
- **Auto-submit.** `prompts.auto_submit` (default `true`) says whether a send presses Enter. `C-c C-a`
  writes an override into the chat's own record (`auto_submit = false` or `true`), so one chat can
  paste without submitting while the others follow the setting; toggling back to the setting
  removes the override. The mode line shows the value in effect.
- **Use file.** A chat reads the `prompts.txt` of its task or project. `C-c C-u` picks another one,
  among the project's and its tasks' or by a path you type, and records it as `prompts_file` in
  the chat's record; the pane follows it, and it survives a restart. A file that is not there is
  created empty. `C-c C-d` removes the field, back to the default. Both refuse while the buffer has
  unsaved changes ("Save the prompts file first"), so no text is dropped.
- **Save on send.** `prompts.save_on_send` (default `true`) saves the file right after a send has
  stamped it, unless it changed on disk meanwhile; then a toast says so and the buffer is left as
  it is. Typing is never saved on its own: use `C-x C-s`.
- **Not there yet.** Spell check.

## Task builder

A new task starts with one todo, `plan`. A plan chat looks at the task and writes a proposal, a plain
text file that lays out the tasks below it; you read it one level at a time, edit it, preview it and
apply it. It is on by default; `task_builder.enabled` set to `false` turns it off (new tasks get their
kind's todos as before, nothing is added to a chat's context, nothing applies on its own, and the
commands are hidden). Proposals are plain files, so they can still be written and edited.

```text
<task dir>/proposals/<n>.proposal.txt     proposal n for that task (the root), numbered from 1, never overwritten
~/.bongos/task-builder/format.txt         the format, written by bongos for chats to read
```

```text
proposal 1
for 0196f1c2-7c1e-7a8b-9d55-2f6c0e1a4b33
by chat 0196f2aa-1d3e-7f00-8c11-0a9b8c7d6e5f
why one PR per layer

task billing: implement proration for billing | id 0196f1c2-7c1e-7a8b-9d55-2f6c0e1a4b33
  task schema: add proration columns to plans
    result a PR adding proration columns
    todo implement | role produces-branch
    todo ship | role ships
  task engine: proration engine
    stacked on schema
    result a PR with the proration engine
```

- **The proposal.** Two-space indentation. A line with an `id` is an existing task (its goal may be
  renamed); a line without one is new. `after` and `stacked on` join siblings only, so a cycle or an
  edge across levels is an error that names the edge. The files are ordinary artifacts, so they
  follow the task's visibility (`artifact: hide`). `proposals/` never becomes a child task.
- **The splitting test.** Anything with its own result, a PR, a document or a spike's findings, is a
  task; anything on the way to that result (implement, review, tests, CI) is a todo of that task. A
  leaf names one result, so two `result` lines on a leaf are an error, a group has no todos, and
  more than `task_builder.todo_warning` (default 8) todos on a leaf is a warning. A plan chat is
  told the test, the format and the task's tree; any other chat gets a pointer to the format.
- **Keys (Emacs keymap only).** `C-c w b` plans the task at point (starts its plan chat, or shows
  the one it has, and pastes "Plan this task and write proposal <n>"), `C-c w B` proposes: it writes
  the next proposal for the shown chat's task, pre-filled with the task's tree, and pastes a line
  into the chat for you to finish (it also works in a chat terminal), `C-c w v` shows the proposal
  one level at a time (`enter` goes into a task, `^` goes up), `C-c w V` opens the file, `C-c w A`
  previews and applies it and `C-c w X` rejects it. `task: new without plan` and `task: toggle auto
  apply` have no key; find them in the command palette.
- **Apply.** The preview lists what is created, changed, cancelled, kept, split and left alone,
  then asks before anything is written. The apply is one journaled step, so it finishes or leaves
  the tree as it was; a tree or file that changed since the preview refuses it. Done tasks,
  tasks with a worktree, branch or open PR, running chats and started todos are never disturbed: a
  line that would do so is kept, with its reason, and the rest applies. A task the proposal does not
  mention is untouched. A `drop` line cancels a task (it can be reopened); nothing is ever deleted.
  Splitting a leaf that has work asks which new child keeps its worktree and branch (`keeps work`
  marks it), and is kept while a chat runs on that leaf.
- **Safe re-apply.** A proposal applied as it is cannot be applied twice. Edit the file and it is
  pending again. Tasks are matched by `id` (a line without one is new), so edit the tree `C-c w B` prints, which carries every `id`, rather than a copy that lacks them.
  Reject keeps the file and records it, so it stops waiting; an applied proposal cannot be rejected.
- **Auto-apply.** A proposal whose header says `by chat <id>` applies on its own, and is logged in
  the root's history and a toast, when that chat's task is the root or an ancestor of it, the root's
  plan is done, it has no error, and it only adds or edits: no `drop`, move, kept line, rename of
  the root or split without a marked keeper. Anything else, and every proposal by you or a
  workflow, waits and shows as "needs you" on the root and its ancestors. `task_builder.auto_apply`
  (default `true`) is the switch, and `task: toggle auto apply` sets `auto_apply_proposals` in the
  project's `project.toml` to override it for that project. A proposal that is already there when
  bongos starts is classified only; it never applies on its own.
- **Not there yet.** Named workflows are not checked (a warning), because there is no workflow
  engine yet.

## Linear sync

Link tasks to Linear tickets, import a ticket with its subtickets, and let a few writes go back.
It is on by default and does nothing until you link or import a ticket and set a token: a task that
is not linked is never touched or requested. `linear.enabled` set to `false` hides the commands and
stops every request. The keys are for the Emacs keymap; every command is also in the palette.

- **Linking.** `C-c w L l` links the task at point to a ticket (the subtickets of the nearest
  linked group first, then your recent tickets, or type an identifier or URL) after a question,
  `C-c w L u` removes the link and `C-c w L o` opens the ticket in the browser. A linked task shows
  its identifier on its row. Unlinking and opening send nothing.
- **Import.** `C-c w L L` (`task: new from linear`) picks a ticket, then where it goes, and
  shows a preview: a ticket with subtickets becomes a group and each one a leaf with one `plan`
  todo, down four levels (a ticket with more than 200 subtickets imports as a leaf). Nothing is
  written before you confirm, and a ticket a task already links focuses that task instead.
  Relations (`blocks`) show as hints and never become edges.
- **Ownership.** Linear owns the title and the description: a linked task takes them from Linear
  and its goal cannot be edited here (a rename in a proposal is kept, not applied), and an edit
  made outside bongos is overwritten by the next refresh. Bongos owns the structure, todos,
  artifacts, edges, chats and workflows, and never edits a ticket's title or description.
- **Refresh.** Linear is read when the Work view opens, when you focus a linked task (not again
  within a minute) and on `C-c w L R`. There is no polling. A failed read shows `linear
  unreachable`, `linear rate limited` or `linear: not found` on the row and never blocks work.
- **Suggestions.** A ticket under a linked group's ticket that no task links is a suggestion.
  `C-c w L s` lists them: `enter` makes a linked leaf (or links a leaf of the same title), `d`
  dismisses one for good. A suggestion never becomes a task on its own, and accepting writes
  nothing to Linear.
- **Three writes.** Bongos writes to Linear in three cases only, never a status that a pull request
  sets (opened, review, merged) and never completed. Moving a ticket to the team's first `started`
  state, once, when its leaf's plan todo starts, only from backlog, unstarted or triage (a group's
  ticket moves when its first leaf does). Creating subtickets: after you apply a proposal under a
  linked group, and on `C-c w L C`, a preview, a checklist and a question come first; a proposal an
  agent applied only marks the group `N need tickets` until you run it. Cancelling a ticket when you
  drop its task and answer yes (a group asks: leave all, cancel leaves only, cancel all).
- **Branches and pull requests.** A linked leaf's branch carries the lowercase identifier
  (`seq-121-proration-engine` from `{slug}`; `{linear}` in a branch template places it yourself) and
  its PR title starts with `SEQ-121 ` so Linear's GitHub integration links the PR with no magic
  words. A chat on a linked leaf is told both in its context, and `task: copy pr title` copies the
  title. Bongos opens no PRs.
- **Stacked leaves.** Linear may mark a ticket Done when its PR merges into the parent's branch,
  which is not the default branch. Bongos writes no status for a stacked leaf, tells you once, and
  marks the row `stacked: check Linear rule`: add a 'no action' rule for target branches matching
  `^[a-z]+-[0-9]+-` in Linear's GitHub settings. When a stack ships as one PR, add `Closes <id>` to its
  description for each stacked ticket (the chat's context says so). The result of a real stacked
  pair against a team's rules is not recorded yet; until it is, treat the rule as a recommendation.
- **Token.** `linear: set token` asks for a Linear personal API key in a masked box, checks it with
  one request and keeps it in the system credential store; `linear: clear token` removes it. The
  `LINEAR_API_KEY` environment variable is the fallback when the store has none (bongos neither adds
  it to nor removes it from what a terminal inherits). The token is never written to a task file,
  proposal, log, dump, toast or clipboard, and bongos does not hand it to chats. With none set, the link and
  import commands say how to add one and everything else works.
- **Not there yet.** Todos are not synced, a ticket's status is not mirrored back, and there are no
  webhooks.

## Workflows

A workflow is a graph of steps that carries a task's todo from its goal toward a pull request: an
agent plans, agents implement the work items in parallel, a program runs the tests, an agent
reviews, and the run stops at a gate to ask you before it merges. It is on by default, and it starts
nothing by itself: no run, no process and nothing under a task's `runs/` directory until you run a
workflow. `workflows.enabled` set to `false` hides the commands and the Runs panel and loads no
definition. The keys are for the Emacs keymap; every command is also in the palette.

- **Definitions and layers.** A definition is a directory holding `workflow.toml` (with its prompts
  and scripts beside it). Three layers are read, and the one with the highest precedence wins per
  `name` (the key in the file, not the directory name): the project's
  (`~/.bongos/projects/<project>/workflows`) over yours (`~/.bongos/workflows`) over the ones bongos
  ships (`implement PR`, `implement tasks`, `ship PR` and `babysit PR`, written to the data directory
  under `workflows/shipped/<version>/` and never edited). `workflow: new` makes a definition in your
  layer, from a minimal one-agent template or as a copy of a shipped one to edit; `workflow: edit
  definition` opens one; `workflow: validate` checks them all and lists each finding with its line in
  a read-only buffer. A key bongos does not know is a warning, not an error.
- **The graph.** One TOML file with `schema = 1`, a `name`, a `start` node, typed `[inputs]` (from
  the task: its goal, plan, artifacts or pull request), optional `[defaults]` for the settings every
  node shares, `[nodes.<name>]` and `[ends.<name>]`. A node declares typed `output` fields and
  `routes` that pick the next node from them (`when`, a `default`, a bounded `max_passes` for a loop
  back, and `fan_out` to run a branch per item with a `join` that waits for `all` or `any`). An end
  is a `success` or a `failure` and may report outcomes (`pr`, `document` or `tasks`) that the task
  takes as its own. A node can set `gate = { when = "before" | "after", question = "..." }` to stop
  for you, and `polls` to run again after an interval until its result changes.
- **Agent and program nodes.** An `agent` node runs one headless `claude` (or `codex`) process per
  try with the rendered prompt, in the run's worktree, with `model`, `permission_mode`,
  `allowed_tools`, `max_turns`, `max_cost_usd`, `max_minutes` and `retries` taken from the node or
  `[defaults]`. Its result is only the typed output the CLI returns, checked against the declared
  fields; the free text is never parsed. A `program` node runs a Python 3.8 or newer script, or one
  of the shipped `use` nodes (`merge` and `wait`), with no model and no cost, and writes its typed
  result to a file. Both stop at the caps they declare and fail with a named reason (`turn cap
  hit`, `time cap hit`, `output cap hit`, `permission denied: <tool>`, `node output invalid`).
- **Permissions and the sandbox.** A program node declares `permissions = { commands, network, mcp }`
  and gets nothing else: it may run only the listed commands and reach only the listed hosts, and MCP
  is refused. This is enforced twice: by an operating-system sandbox (Bubblewrap on Linux) that
  allows writes only in the node's worktree and scratch directory, the shared git directory's
  `objects/`, `refs/` and `logs/` and the worktree's own git directory, with the network blocked
  unless declared; and by an audit hook inside the Python process, which is a guardrail and not a
  boundary. Known caveat: those shared git paths are writable by design, so a program can write
  objects and refs to the repository's git directory that every worktree uses, and a git run by
  you or by another run can then read them. bongos keeps the dangerous parts read-only (`config`,
  `hooks/`, `packed-refs`, replace refs and alternates, other worktrees' directories), removes
  planted symlinks after each pass and fails the node if git metadata changed, but it cannot protect
  the integrity of loose objects and packs while `objects/` is writable; a stricter allowlist for
  these paths is a later feature. Without Bubblewrap the script runs under the audit hook alone, and
  the try's summary says `not sandboxed` and why. Treat a definition from another source as code.
- **Worktrees and merge.** A run works in its own worktree on its own branch, made from the head of
  the task's branch, and each fan-out branch gets one from the run's branch. They live under
  `<repository>/.bongos-worktrees/.runs/` unless `workflows.worktrees_dir` says otherwise. Nothing
  writes your checkout or the task's worktree except the shipped `merge` node, which brings the run's
  branch into the task's: it refuses a task worktree with uncommitted changes, never rebases (the
  style is `ff-or-merge`, `ff-only` or `merge-commit`), and on a conflict it aborts and leaves both
  branches as they were. A run does not start with less than `workflows.min_free_disk_mb` free.
- **Lifecycle and keys.** `C-c f r` runs a workflow on the task at point (you pick the definition and
  see what it needs first), `C-c f p` pauses (running steps finish first), `C-c f h` halts now and
  opens the worktree, `C-c f c` resumes, `C-c f t` retries the failed step, `C-c f R` restarts,
  `C-c f k` cancels, `C-c f a` approves a gate and `C-c f v` sends it back with notes. `C-c f w`
  shows the Runs panel (the runs of every task, filtered with `t`, `p` and `s`; `enter` and `l` open the
  selected run's logs); `C-c f l` opens a run's logs, `C-c f s` its run view, `C-c f
  o` its worktree, `C-c f d` its diff and `C-c f x` clears finished runs (the
  worktrees of those older than `workflows.keep_finished_days`). `C-c f n`, `C-c f e` and `C-c f V`
  are `workflow: new`, `edit definition` and `validate`. A run waiting at a gate or failed marks its
  task as needing you. A run survives a restart of bongos; a run that was running comes back
  interrupted and waits for `resume`. Runs execute: `workflow: run` starts the agent, program and
  built-in nodes of the definition you pick, in the run's own worktree.
- **Settings.** All under `workflows`, the main ones also on the Work page of the settings window:
  `enabled` (true), `worktrees_dir` (null), `min_free_disk_mb` (2048), `keep_finished_days` (14),
  `max_concurrent_runs` (3), `max_concurrent_sessions` (null, twice the runs), `max_parallel_branches`
  (3), `fan_out_width` (8), `retries` (2), `poll_interval_minutes` (5), `wait_give_up_days` (7) and
  `programs` with `python` (null, `python3` then `python` on `PATH`), `max_minutes` (10) and
  `max_output_mb` (10).
- **Privacy.** Workflows run only the local CLIs and scripts a definition names. bongos itself
  contacts no host for them; what leaves the machine is what those programs do themselves: the model
  provider of the `claude` or `codex` you run, and `gh` or the network hosts a program node declares
  (the shipped `wait` and `merge` programs use `gh` and git). A program node with no declared hosts
  has no network under the sandbox.

## Task checks

A check is a script or command that bongos starts, watches and judges by its exit status, its output
and an optional result file, so a task is done because a check passed and not because someone
wrote that it did. It is on by default; `task_checks.enabled` set to `false` runs no check, writes no
record and hides every task check command (a key still says "Task checks are off
(task_checks.enabled)"). A check never runs by itself, except as a retry of its own no-verdict result and
at a landing; a person runs it, or a workflow step does.

```toml
# definitions/tasks/<task>/task.toml
[checks]
inherit = "parent"                 # or "none"; the default is "parent"
use = ["standard"]                 # named sets, in order
drop = ["format"]                  # remove an inherited check

[[checks.check]]
name = "tests"                     # unique in its set
run = "checks/run-tests"           # a script in definitions/, or: command = ["make", "test"]
scope = { script = "checks/scope" }   # or scope = "all"
parts = 8                          # or { plan = "checks/plan-parts" }; default one part
required = true                    # false: shown, never counted
or_accepted = "flake-verdict"      # another check's pass on the same tree also satisfies this one
pinned = ["checks/registry/flaky-tests.md"]   # definitions files the script reads from the pin
environment = [3]                  # exit codes that mean "environment"; pass defaults to [0], fail to [1]

[[checks.closing]]                 # on a group: checked once, on its last open child
name = "tests-full"
run = "checks/tests-full"
scope = "all"
```

- **Declaration.** A task's `checks` table lives in its `task.toml` in the project repository.
  Other keys of a check: `repos`, `pass`, `fail`, `summary` (a regex a passing run needs a matching
  output line for), `result_file`, `needs` (pool names), `parallel`, `stop_at_first_failure`,
  `unset`, `key_inputs`, `params` and `budget_minutes`. Exactly one of `run` and `command`. A path
  in `run`, `scope.script`, `parts.plan` or `pinned` is relative to `definitions/` and cannot leave
  it. A broken declaration is refused at landing by the check `checks-valid`.
- **Sets and inheritance.** A task's effective set starts from its parent's (`inherit = "none"`
  starts empty), adds each `use` set (`definitions/checks/sets/<name>.toml`, holding `[[check]]`
  and `[[closing]]` entries), then its own entries; a name that is already there is replaced and
  `drop` removes one. A group's ordinary checks pass down to its children and are judged there. A
  document or spike task has no checks to apply. A task with no tree to check (it was never started)
  is held: "not done: no tree to check".
- **Closing checks.** A group's closing checks are added to the last open child, whose section shows
  them as "closing check of <group>". If a sibling is still held when that child finishes, the group
  judges them itself, on that child's tree.
- **Scope scripts.** A check with a `scope` script is asked first what is in scope. The script gets
  `BONGOS_CHANGED_FILES` (the changed files of the task against its base branch) and writes JSON to
  `BONGOS_SCOPE_RESULT`: `{"scope": "items", "items": [...], "reason": "..."}`, or `"all"`, or
  `"none"`; `reason` is required. The check then gets `BONGOS_CHECK_SCOPE` (`all` or `items`) and
  `BONGOS_CHECK_ITEMS` (a file). A run on `items` or `none` satisfies the check; `scope = "all"`
  needs a run on everything.
- **Parts.** A check with `parts = N` runs its processes one after another (`parallel = true` also
  uses free pool slots, up to `task_checks.parallel_parts_max`), each with `BONGOS_CHECK_PART` and
  `BONGOS_CHECK_PARTS`. A run stopped halfway keeps the parts that ended; the next run of the same
  check on the same tree and scope shows "running part 5 of 8, 4 kept". Any difference (an edit,
  another scope, a changed script) starts again. Any failing part fails the check; else any no
  verdict is the check's.
- **Evidence and the tree key.** Every run is a record in the project's state store: the check's
  digest (its declaration and the bytes of every definitions file it names), the tree key, the
  scope, the parts, the verdict, the output file. The tree key is the git tree id of every tracked
  and untracked file `.gitignore` does not exclude, taken in a private index, so it does not depend
  on the commit or on your real index; a check with `key_inputs` also keys on those files. A record
  counts only while its key and digest are the current ones. Scripts are read from a pin: the last
  verified commit of the definitions (`BONGOS_CHECK_PINNED`), so an edit in a working copy is not
  seen by a run, and an agent cannot loosen a check by editing its script. The scripts' own channels
  are the exit status, the output and `BONGOS_CHECK_RESULT` (JSON: `summary`, and optionally
  `failing` and `counts`; at most `task_checks.result_file_limit_kb`). Nothing else writes records.
- **No verdict.** A run that does not end as a pass or a fail gives no verdict, with its reason:
  an environment exit code (`no verdict: environment (exit 3)`), an undeclared exit code, a signal,
  a missing summary line, an invalid result file, a tool not found, a time budget, a wait for a pool
  that ran out, a cancel. A no-verdict run is run again after `task_checks.no_verdict_rerun_minutes`,
  at most `task_checks.no_verdict_reruns` times, keeping parts that ended; then it is reported. It
  never counts as a pass.
- **Landing checks.** A project's `[[landing.checks]]` entry whose `run` is a `check` table runs
  one check on the tree a change would land, with the scripts of master's copy (never the
  change's own):

  ```toml
  [[landing.checks]]
  name = "guard"
  phase = "before-queue"
  applies = "always"
  run = { check = { name = "guard", run = "checks/guard", result_file = true } }
  ```

  A fail refuses the landing and keeps its summary, failing items and output with the change; a
  no verdict keeps it waiting. A change that loosens a required check of a task that has started
  (a removed check, wider pass codes, a narrower scope) needs a person's approval.
- **The detail view.** `C-c w i` opens a task's details: goal, kind, state, documents, todos,
  children and edges, then the Checks section: a header ("all required checks pass", "not done: 2
  checks missing", "checks waiting") and one row per check with where it came from (own,
  `from set <name>`, `inherited from ...`, `overrides ...`, `closing check of ...`), whether it is
  required, its result (`not run`, `pass`, `fail (1 item)`, `no verdict: ...`, `stale: ...`,
  `accepted by <check>`, or what it is doing now), scope, tree key, time and output. Failing items
  and the output file are listed under a row. In the view, `x` runs the checks, `r` runs one, `k`
  cancels, `c` completes the task, `s` shows where each row is declared and `g` refreshes.
- **`task: complete`.** `C-c w c` marks the task done, unless a required check of it is unmet: then
  a message names the checks and nothing changes. A task whose last todo closed with required
  checks is held out of Done until its checks pass; the pass writes a done record (the tree key and
  the evidence it rests on), and reopening the task removes it. `task: close` is the same command
  with no key.
- **Keys (Emacs keymap only).** `C-c w x x` runs the task's checks, `C-c w x r` runs one check
  (you pick it), `C-c w x k` cancels the running checks, `C-c w x s` shows the Checks section,
  `C-c w x o` shows the output of a check in a read-only buffer that follows a running one,
  `C-c w x e` edits the task's checks in a new working copy, `C-c w i` shows the details and
  `C-c w c` completes. The command acts on the task at point (the details tab, the selected Work
  row or the focused task), else asks. `task: run checks clean` (ignore kept parts) and `task:
  show check sources` have no key; find them in the command palette.
- **Settings.** All under `task_checks`, on the Work page of the settings window as "Task Checks":
  `enabled` (true), `output_dir` (null, the task's files folder `checks/`), `output_retention_days`
  (30; the record stays after its output file is deleted), `output_limit_mb` (256),
  `result_file_limit_kb` (256), `default_budget_minutes` (null, no budget), `no_verdict_reruns` (2),
  `no_verdict_rerun_minutes` (5), `pool_wait_limit_hours` (4), `kill_grace_seconds` (5),
  `parallel_parts_max` (4), `scratch_dir` (null, `<data dir>/checks-scratch`), `tree_key_debounce_ms`
  (2000) and `live_write_ms` (1000).
- **Detached processes.** A check's process runs in its own session. Stopping a check (a cancel,
  a time budget, quitting) sends a stop signal to that process group and to every process of yours
  that carries the part's `BONGOS_CHECK_TOKEN` in its environment, then kills what is left after
  `task_checks.kill_grace_seconds`. On Linux that finds a process that detached from the check
  (a daemon it started). On macOS bongos cannot read another process's environment, so a process
  that detached and was reparented is not found; only the group and its live descendants stop.
  After a crash, the next start stops the processes it finds through the unfinished records.
- **Not there yet.** The Checks section is computed when a check ends, not when a file changes, so
  a row does not turn `stale` while you edit; the done decision recomputes the tree key, so
  `task: complete` still refuses a tree edited after its pass. The `task_checks.tree_key_debounce_ms`
  setting is not used yet.

## Emacs keys

Emacs is the default base keymap, so a fresh install has these keys with no setting. They work in
editors; terminals keep their own keys except C-c, C-g and the C-x chords C-x C-c, C-x C-f,
C-x C-s, C-x C-w and C-x s, and the command keys (M-x, C-x ...) work everywhere else. The tables list the Linux keys; on macOS Command keys stay as they are in Zed.

| Key | Command |
|---|---|
| C-f, C-b, C-n, C-p | forward-char, backward-char, next-line, previous-line |
| C-a, C-e, M-m | beginning of line, end of line, back to indentation |
| M-f, M-b | forward-word, backward-word |
| M-<, M->, C-x [, C-x ] | beginning and end of the buffer (M-< and M-> push the mark first) |
| C-v, M-v | scroll down, scroll up |
| M-{, M-} | backward and forward paragraph |
| M-g | goto-line (a prompt in the minibuffer, with a preview) |
| C-l, M-r | recenter, reposition |
| C-s, C-r | isearch |
| M-%, C-M-% | query-replace, query-replace-regexp |
| C-x h | mark the whole buffer |
| C-SPC, C-x C-SPC, C-x C-x | set the mark, pop the mark ring, exchange point and mark |
| C-g | keyboard-quit (cancels a prompt, a mark or a search) |

| Key | Editing command |
|---|---|
| C-d, M-d, M-DEL | delete a character, kill word, kill word backward |
| C-k, C-w, M-w, C-y | kill line, kill region or line, copy, yank |
| M-y | yank-pop: right after a yank, replace the yanked text with the previous entry of the kill ring |
| M-Y | browse-kill-ring: pick an entry from the kill ring in the minibuffer strip |
| C-M-k | kill-whole-line |
| C-M-w | append-next-kill: the next kill is added to the last entry |
| C-o | open line: insert a newline after the cursor and stay before it |
| C-/, C-_, C-x u | undo |
| M-u, M-l, M-c | upcase, downcase, capitalize word |
| C-t, M-t, C-x C-t | transpose characters, words, lines |
| M-z, M-Z | zap to char, zap up to char (prompts for the character in the minibuffer) |
| M-SPC, C-BS, C-x C-o | just one space, delete horizontal space, delete blank lines |
| M-^, M-q | join line, fill paragraph |
| M-up, M-down | move the line up or down |
| C-x C-;, C-c ;, C-c y | toggle comment, comment-dwim, duplicate line |
| C-=, C-- | expand and contract the region |
| C-x SPC | rectangle-mark-mode: start or end selecting a rectangle |
| C-x r M-w | copy-rectangle-as-kill |
| C-x r k, C-x r d | kill rectangle, delete rectangle |
| C-x r o, C-x r c | open rectangle (blank column), clear rectangle (spaces) |
| C-x r t | string-rectangle: replace each row of the rectangle with a string (prompts in the minibuffer) |
| C-x r y | yank-rectangle |
| C-x r K | keep-rectangle: keep only the rectangle's text on its rows |

| Key | Command |
|---|---|
| M-x | execute-extended-command (the command palette) |
| C-x C-f, C-x b | find a file, switch buffer |
| C-x C-s, C-x C-w, C-x s | save, write file, save all |
| C-x k | kill buffer, close the item |
| C-x o | other window: pick one by its letter |
| C-x 1, C-x 0 | delete other windows (toggles back), delete this window |
| C-x 2, C-x 3 | split below, split right |
| C-x \|, C-x _ | lay two windows out side by side, stacked |
| C-c down | toggle window dedication |
| C-x +, C-x \, C-x M-o | balance windows, swap windows, move the buffer to another window |
| C-x C-c | quit |
| C-h e | show the echo-area messages |

To switch it off, set another base keymap, for example `"base_keymap": "VSCode"`. `"VSCode"` and
`"Vim"` remove every Emacs key, and the cursor goes back to a bar under VS Code (a block in Vim's
normal mode). The change applies as soon as the settings file is saved. See
[Keybinding profiles](#keybinding-profiles).

`emacs.cursor_style` picks the cursor: `"auto"` (the default) is a block while the base keymap is
Emacs and your `cursor_shape` otherwise, `"emacs"` is always a block and `"modern"` is always a
bar.

```json
{ "base_keymap": "VSCode", "emacs": { "cursor_style": "auto" } }
```

### Windows

Emacs windows are panes. The window commands act on the panes of the editor area and, for picking,
on the panels that show an editor or a terminal. Nothing here closes a tab or a buffer.

- C-x o switches window. With two windows it goes to the other one; with more, every window shows
  a letter (a, s, d, f, ...) in reading order, and the letter you press picks it. The letter goes
  to the picker, never to the editor or the shell in the window; any other key cancels.
- C-x 1 zooms the current window to fill the frame, and C-x 1 again restores the layout.
  C-x 0 deletes the current window and hands its tabs to a neighbor.
- C-x 2 and C-x 3 split below and to the right. C-u before either keeps focus in the old window.
- C-x _ stacks two windows and C-x | puts them side by side. With more than two windows, the
  current one and the next stay and the others hand their tabs to the next.
- C-c down dedicates the current window to what it shows: files opened later go to another
  window. Press it again to undo.
- C-x + balances the windows to the same size, C-x \ swaps the current window with a picked one
  and C-x M-o moves the current tab to a picked window.

### Kill ring

Every kill and copy goes into one kill ring, shared by all editors, and C-y yanks the newest
entry. Kills made one after another (C-k, C-M-k, M-d and the like) join into a single entry, so
three C-M-k in a row followed by C-y brings all three lines back; C-M-w makes the next kill join
the last entry even after other commands. M-y, right after a yank, swaps the yanked text for the
next older entry, and again for the one before it. M-Y opens the ring in the minibuffer strip,
newest first, narrowed as you type, with the highlighted entry previewed under the list; Enter
inserts it. A prefix argument changes how many lines these commands take; see Universal argument.

Terminals keep their own copy and paste unless `emacs.terminal_uses_kill_ring` is `true`
(default `false`); then a copy in a terminal goes into the ring and a paste there reads it.
`emacs.save_kill_ring` (default `true`) keeps the ring across restarts; see Privacy.

```json
{ "emacs": { "save_kill_ring": true, "terminal_uses_kill_ring": true } }
```

### Mark and region

| Key | Command |
|---|---|
| C-SPC, C-@, C-. | set the mark and start the region; with a region, deactivate it (the mark stays) |
| C-x C-SPC | pop the mark ring: go to the mark, and the previous mark becomes the mark |
| C-x C-. | pop the global mark: back to the newest earlier location in another file |
| C-x C-x | exchange point and mark (with no region, it selects from point to the mark) |
| M-<, M-> | push the mark, then jump to the start or end of the buffer |
| M-. | push the mark, then go to the definition |
| M-t, C-x C-t | with a mark away from point, swap the word or line at point with the one at the mark |

C-SPC pressed right after a pop keeps popping, as in Emacs. C-u C-SPC does the same as C-x C-SPC
(one entry per press, whatever the count). Inside editors C-. sets the mark instead of opening code
actions, which stay in the command palette.

The mark and its ring belong to the buffer, so two panes showing one file share them, and edits
move them with their text. C-x C-. crosses files: it goes back through the active pane's history.

`emacs.delete_selection` (default `false`) decides what typing does over an active region: while
it is false, the character goes in at point, the selected text stays and the region is
deactivated; set it to `true` to have typing replace the region, as in other editors.

```json
{ "emacs": { "delete_selection": true } }
```

### Rectangles

A rectangle is the block of columns between the mark and the cursor, over every row from one to the
other. C-x SPC starts rectangle-mark-mode, which shows the block as you move; the `C-x r` commands
also work on a plain mark and the cursor.

| Key | What it does |
|---|---|
| C-x r M-w | copy the rectangle to the kill ring |
| C-x r k, C-x r d | kill it (to the ring), delete it |
| C-x r o | open it: insert blank space as wide as the rectangle and push the text right |
| C-x r c | clear it: replace it with spaces |
| C-x r t | ask for a string in the minibuffer (`String rectangle: `) and replace each row with it |
| C-x r y | yank the newest rectangle with its top left at the cursor |
| C-x r K | keep only the rectangle's text on its rows |

Columns are the displayed ones: a tab that crosses an edge of the rectangle is cut there and the
rest is filled with spaces. A wide character (such as a Chinese one) is never split: it belongs to
the rectangle when its first column is inside it. A yank pads short rows with
spaces and adds lines at the end of the buffer. Each command is one undo step. C-y yanks a killed
rectangle row by row, as C-x r y does. The rectangle commands never touch the system clipboard.

### Universal argument

C-u, M-0 to M-9 and M-- give the next command a count, as in Emacs. The keys work in a full
editor (a file you edit); in a picker's query editor, the minibuffer and other small editors they
keep the meaning they have there, and where M-1 to M-9 switch tabs (outside editors, on Linux) they keep doing that.

- **C-u** alone is 4, and each further C-u multiplies by 4: C-u C-u is 16, C-u C-u C-u is 64.
- **Digits** after C-u, or M-0 to M-9 from the start, build the number: C-u 1 0 and M-1 0 are 10.
  A C-u typed after digits ends the number, so C-u 3 C-u 7 is a 7 typed with the count 3, which
  inserts 777; a digit typed with no argument pending is only a digit.
- **Minus** makes the count negative, as in Emacs: M-- and C-u - are -1, and digits after a minus
  give their negative (C-u - 2 is -2). C-u after a minus multiplies by four: C-u - C-u is -4,
  C-u - C-u C-u is -16. A second minus cancels the argument, and a minus after digits is a typed
  character. Most commands run the opposite way with a negative count; C-u - M-f goes one word
  back, C-u - 2 M-f two, C-u - C-u M-f four, M-- C-f one character back.
- **The echo.** While an argument is pending the echo area shows it: `C-u 4`, `C-u 10`, `C-u -`,
  `C-u (-4)`. It goes when the next command takes the argument. The argument belongs to the editor
  it was started in: moving focus to a picker or the minibuffer drops it.
- **C-g** cancels a pending argument and its echo, so C-u 7 C-g changes nothing and the next
  key is typed as usual. C-g also stops a long repeat after the repeat in progress, keeping what
  was done.
- **Typed characters.** C-u 5 and then a character inserts it 5 times. The count is limited to
  65536, and the echo then says "Count limited to 65536".

Commands that read the count themselves:

| Command | With a count N |
|---|---|
| C-f, C-b, C-n, C-p | move N characters or lines (a negative count goes the other way) |
| M-f, M-b, M-d, M-DEL, M-u, M-l, M-c | move over, kill or change the case of N words |
| C-k, C-w, M-w, C-M-k | kill or copy N lines as one entry (C-k from the cursor, with a region C-w ignores N) |
| C-y | yank the Nth newest entry of the kill ring (it wraps round); C-u C-y is the 4th |
| M-y | step N entries older in the kill ring |
| C-SPC | pop one entry of the mark ring and go to the mark, whatever N is |
| M-g | go to line N |
| M-z, M-Z, C-M-z, C-M-Z | the Nth occurrence of the character |
| C-l | put the cursor line at row N of the window, with no cycling |
| C-t, M-t, C-x C-t | move the character, word or line N places |
| C-o | insert N newlines |
| C-c y, C-c a | duplicate the line N times, copy N characters from the line above |
| M-SPC | leave N spaces (0 leaves none) |
| C-BS | any argument deletes only the space before the cursor |
| M-up, M-down | move the line N lines |

Commands that give the argument another meaning (the plain command runs with no argument):

| Command | With C-u or any argument |
|---|---|
| C-c w a (advance) | advance even when the task it is stacked on is not Done, leaving a marker |
| C-c w n (new task) | create the task without the plan todo |
| C-c c n (new chat) | ask also for the name, model and layout |
| C-c c o (open chat) | pick from the chats of any task |
| C-c c f (fork to task) | ask for the session type |
| C-c C-c in the prompts file (send) | send without marking the prompt as sent |
| C-c f t (retry step) | read the workflow definition again before the retry |
| C-c C-p, C-c C-f in the prompts file | move N prompts; a negative count goes the other way |

Commands that have no use for a count (C-x C-x, C-s, C-r, C-M-s, C-M-r, M-%, C-M-%, C-x C-.)
clear it and run once; C-u C-x C-x swaps point and mark once, not four times.

Any other command is repeated: C-u 3 on a command with no count of its own runs it three times
(a negative count runs it |N| times (the sign means nothing to it), and 0 runs nothing). The whole repeat is one undo step, so
one C-/ takes it all back. It runs in slices so the window keeps drawing, and C-g ends it early.

Where C-u is not the prefix, such as a picker's query editor, a command that takes an argument has
another way in: the variants above are commands in the command palette (`task: advance override`,
`chat: new with options`, `chat: open any`, `chat: fork to task as`, `task: new without plan`,
`prompts: send without marking`, `workflow: retry step fresh`), and C-x C-SPC stands in for
C-u C-SPC.

## Keybinding profiles

bongos has three keybinding profiles, set with `base_keymap` in settings: `"Emacs"` (the default),
`"VSCode"` and `"Vim"`; `"None"` loads no base keymap. Every bongos command is bound under all three, and every command is also in
the command palette and in M-x. The old values `"JetBrains"`, `"Atom"`, `"SublimeText"`, `"TextMate"`
and `"Cursor"` are retired: bongos shows a notice, uses Emacs, and leaves your setting as it is.

To switch, edit `base_keymap` in `settings.json` (it applies when saved) or run `settings: set
keybindings` from the command palette and pick a profile.

- **Leaders.** Emacs binds the bongos commands under C-c. VS Code binds them under `ctrl-k b`
  (`cmd-k b` on macOS). Vim binds `ctrl-k b` and also `space` in normal and visual mode. A lone
  `space` waits for the chord timeout before it moves the cursor right.
- **Terminals.** Under VS Code and Vim a terminal sends its keys to the shell, so the leaders are
  not bound there. The way out is `alt-1` (`cmd-1` on macOS), which focuses the first editor pane;
  commands from a terminal go through the command palette.
- **Cursor.** With `emacs.cursor_style` at `"auto"`, the cursor is a block under Emacs and your
  `cursor_shape` otherwise. Under Vim the cursor follows the mode: a block in normal mode and a
  bar in insert mode.
- **Your keymap.** `keymap.json` loads last, so a binding of yours wins under every profile.

The keys of every command under each profile are in [docs/keybindings.md](docs/keybindings.md).

## Incremental search

C-s and C-r search as you type, as in Emacs. The prompt opens in the minibuffer strip, the cursor
moves to the nearest match with every character, and the matches near the visible text are
highlighted. Folds that hide the match open while it is the current one. Under the Emacs base
keymap (the default) these keys start a search in an editor:

| Key | Command |
|---|---|
| C-s, C-r | search forward, search backward |
| C-M-s, C-M-r | regexp search forward, backward |

Inside a search these keys apply (Linux names; they are in `assets/keymaps/specific-overrides.json`
and the macOS file):

| Key | Command |
|---|---|
| C-s, C-r | next match, previous match; with an empty string, recall the last search |
| DEL | remove the last character and go back to the match before it |
| RET, C-RET | stop at the match: select it, or leave the cursor at its other end |
| C-g, ESC | remove a failing end of the string, or else stop where the search began |
| M-p, M-n | previous and next search of the history |
| C-w, C-M-y, M-s C-e | add the word, the character or the rest of the line at the cursor |
| M-s . | search for the symbol at the cursor, as a whole symbol |
| C-y | add the top of the kill ring, or the clipboard |
| C-j | add a newline |
| M-e | edit the string freely; RET, C-s or C-r resume |
| M-c, M-r, M-s SPC | toggle case, regexp, lax whitespace |
| M-s M-<, M-s M-> | jump to the first or the last match |
| C-l | recenter the match (middle, top, bottom) |
| C-c C-o | occur: list every matching line in a results tab |

Any other key stops the search at the match and then runs as it does in the editor, so C-a, C-n
or a command key keep working. C-M-s and C-M-r inside a search stop it and start a regexp search.
A stopped search pushes where it began onto the mark ring, so C-x C-SPC goes back.

The prompt tells the state: `Failing` when nothing matches, `Wrapped` after the search passed an
end of the buffer and `Overwrapped` when it went on past the place it started from, `case-sensitive` or
`case-insensitive` once you toggled the case, `strict-space`, `Regexp` and `Symbol`, then
`I-search` and `backward` for a backward search, as in `Wrapped Regexp I-search backward: `. The
count of the matches shows in front of the prompt as `3/12`.

Case is smart by default: a string with an uppercase letter is case-sensitive, any other is not.
With lax whitespace (the default for text) a space matches any text on the line up to the next
term; toggle it off for exact spaces. A regexp search uses the regexp syntax of the project
search.

Occur lists the lines that match in a read-only tab, up to 10,000 lines (a message says when it
stopped); open a result with the open-excerpts command to jump to its line in the buffer.

`minibuffer.enabled_for` names `isearch` by default, so the search prompt is in the strip. Remove
`"isearch"` from that list to get the prompt as a centered modal; the keys are the same.

On a large file typing never waits. A search runs in the background in line-aligned windows,
nearest to the cursor first: the first is 64 KB and each next one doubles up to 4 MB, so a match
close to the cursor shows at once and a search that you extend or cancel stops between windows.
The match count runs apart from it, in 1 MB windows on every core. While it is still running the
counter is blank, or shows an ellipsis while a search is pending, and it stops at 10,000 matches
and reads `3/10000+`.

## Query replace

M-% replaces a string, asking about each match, as in Emacs. The prompts and the questions show in
the minibuffer strip. Under the Emacs base keymap (the default) these keys start it in an editor:

| Key | Command |
|---|---|
| M-% | query-replace: ask for a string, then its replacement, then ask at each match |
| C-M-% | query-replace-regexp: the same, with a regexp; `\1` to `\9` in the replacement name its groups, `\&` the whole match and `\#` the count |
| M-% in a search | end the incremental search and replace its string and settings from the match on, asking only for the replacement (C-M-% does the same with a regexp) |

`replace-string` and `replace-regexp` have no key. Run them from the command palette (M-x): they
ask for the strings, then replace every match without asking. All four refuse a read-only buffer.

The first prompt starts from the region, else the symbol at the cursor, else the last string; the
second starts from the last replacement when the string is the last one. M-p and M-n walk the
history. C-q makes the next key literal, so `C-q C-j` puts a newline in the string. At the
second prompt RET asks at each match; with no match the message reads `Nothing to replace`.

At each match the strip asks `Query replacing X with Y:` and these keys answer (`?` lists them):

| Key | Answer |
|---|---|
| y, SPC | replace this match and go on |
| n, DEL | skip it and go on |
| ! | replace this and every remaining match without asking; C-g stops it and keeps what was replaced |
| . | replace this match and end |
| , | replace this match and wait, showing the result; the next key goes on |
| ^ | go back to the previous replaced match and show it, one older each time |
| u | undo the last replacement and ask about that match again |
| e | edit the replacement; RET resumes with the new text |
| q, RET | end; the cursor stays at the current match |
| C-g | end like q; the replacements made stay |
| C-l | recenter the match (middle, top, bottom) |
| ? | show this list of keys |

Any other key ends the replace and then runs as it does in the editor.

Case is smart. A string with an uppercase letter is matched case-sensitively and any other
case-insensitively; M-c at a prompt switches it. When the match ignores case and the string has no
uppercase letter, the replacement takes the case of each match: `foo` to `bar` turns `Foo` into
`Bar` and `FOO` into `BAR`.

With a region, only the region is searched. Without one the search runs from the cursor to the end
of the buffer (in a multibuffer, to the end of the excerpt at the cursor). The mark is pushed where
the command started, so C-x C-SPC goes back, and the cursor ends after the last replacement.

The whole replace is one undo step: after it, undo (C-/) puts back every replacement of the batch,
the answered ones, the `!` ones and the ones of `replace-string` alike. The echo area says what
happened, such as `Replaced 12 occurrences` or `Replaced 12, skipped 3`.

`minibuffer.enabled_for` names `query_replace` by default, so the prompts and the questions are in
the strip. Remove it from that list to get them as a centered modal; the keys are the same.

## Symbol jump and goto stack

M-n and M-p jump between the occurrences of the symbol at the cursor, as `symbol-overlay` does in
Emacs, and a goto stack lets you go back to where you were after a jump. Under the Emacs base keymap
(the default) these keys work in an editor:

| Key | Command |
|---|---|
| M-n, M-p | jump to the next, previous occurrence of the symbol at the cursor |
| M-i | toggle the highlight (overlay) of the symbol at the cursor |
| M-I | clear every symbol highlight in the buffer |
| left, right | goto stack: back, forward |
| up | goto stack: push the current place |
| C-c x h | xref history list |

- **Jumps.** M-n and M-p match the symbol as a whole word (`n` does not match `println`), flash the
  occurrence they land on, and wrap around the buffer; the echo area says `wrapped` when they do. With
  no symbol at the cursor the echo area says `No symbol at point`.
- **Overlay.** M-i highlights every occurrence of the symbol at the cursor and M-i on it again removes
  the highlight; M-I removes them all.
- **Goto stack.** `left` goes back to the previous place of the stack, `right` goes forward again, and
  `up` pushes the current place so that `left` can return to it later. `C-c x h` opens the stack as a
  picker, newest first; RET goes to the entry.
- **Navigation history settings.** Two settings decide what else the stack records:
  `navigation_history.record_large_moves` (a cursor move of ten rows or more) and
  `navigation_history.record_item_switches` (switching to another item). Both default to off under
  the Emacs base keymap without vim or helix mode, and to on otherwise; an explicit `true` or
  `false` overrides the default.
- **Panes.** The arrow keys are bound only in editors of the center panes (not in docks, in agent
  threads, in editors that are not full-mode, in vim mode, or while completions, code actions or
  signature help show), so they move the cursor everywhere else. Each pane has its
  own stack. The Emacs keymap no longer binds `alt-,`; on Linux it reaches
  `editor::GoToPreviousHunk`. `alt-n` also reaches the query editor of full-mode pickers.

## Minibuffer

As in Emacs, prompts open in a one-line strip at the bottom of the window, below the status bar,
instead of a centered modal: M-x (the command palette), the file finder, the tab switcher, the
Work prompts and questions, messages (the echo area) and the hint that lists what keys can follow a
prefix. It is on by default. The matches show in a list above the strip and focus returns where
it was when the prompt ends.

`minibuffer.enabled_for` names the prompts that use the strip. Its default is
`["command_palette", "file_finder", "tab_switcher", "work", "messages", "key_hint", "goto_line", "zap", "isearch", "kill_ring", "query_replace", "set_keybindings", "outline", "project_symbols", "string_rectangle", "language_extensions"]`. Remove a name
to bring that prompt back as a centered modal, for example:

```json
{ "minibuffer": { "enabled_for": ["command_palette", "work"] } }
```

An empty list turns the minibuffer off: there is no strip and every prompt is a modal. The rest of
the `minibuffer` settings are `max_rows` (candidate rows shown, 15), `show_count` (the match count
before the prompt, true), `message_timeout` (how long a message stays, 4000 ms) and
`key_hint_delay` (how long a pending key sequence waits before its keys are shown, 500 ms).

## Languages

Each language below gets highlighting, a language server and formatting with no setup. A server
is downloaded the first time a file of its language opens (see Privacy for the hosts), unless
`lsp.<server>.binary.path` points at one you installed. The status line names a language whose
server cannot be downloaded or started ("Java server unavailable: could not download jdtls"), or
a tool it is missing, and shows download progress. Its menu has Show logs (`languages.log` in the
logs directory, with the full cause), Retry, Cancel download and Dismiss; the same are in the
palette as `languages: show logs`, `languages: retry server` and `languages: cancel download`.

| Language | Server | Formatting | Tasks and tests |
|---|---|---|---|
| Rust | rust-analyzer, building into `<target>/rust-analyzer` so it never blocks your `cargo build` | rust-analyzer (rustfmt) | cargo tasks, gutter test runs |
| Python | basedpyright and ruff | ruff | pytest and unittest; the project's venv, uv, poetry or conda environment is picked with no setting |
| JavaScript, TypeScript, TSX | vtsls, ESLint, Tailwind | Prettier, or Biome | `package.json` scripts and test runners with the project's package manager |
| Java | jdtls, or the IntelliJ server where `kotlin.server` runs it (one server for Kotlin and Java) | jdtls | `java: show jdk` reports the JDKs |
| Terraform, OpenTofu (`.tf`, `.tofu`, `.tfvars`) | terraform-ls | `terraform fmt` through terraform-ls | |
| HTML, XML (`pom.xml`), HCL, TOML | none (highlighting only) | | |

Go, C, C++, CSS, JSON, YAML and Bash keep the servers bongos inherited.

- JavaScript package manager: `packageManager` in `package.json`, else the nearest lockfile
  (`bun.lock` or `bun.lockb`, `pnpm-lock.yaml`, `yarn.lock`, `package-lock.json`), else npm.
  Tasks run that manager only: a Bun project's scripts run as `bun run <script>`, its tests with
  `bun test` (or `bun x <runner>` for Jest and Vitest), and bongos never runs `npm`, `npx` or
  `node` for them. The language servers themselves still run on Node.js.
- Biome: with `biome.json` or `biome.jsonc` in the project, bongos runs the project's own
  `node_modules/@biomejs/biome` (it never downloads Biome) and formats with it instead of
  Prettier; ESLint stays off there unless the project also has an ESLint config.
- Java: jdtls runs on a JDK of its own, chosen apart from the JDK that imports the project. The
  server JDK is `java.server_jdk`, else the newest JDK 25 or newer found (`JAVA_HOME`, then the
  usual install locations). The import JDK is `java.jdk_home`, else the Gradle toolchain, the
  Maven `release` or `source` level, then `JAVA_HOME`. jdtls keeps its workspace in the data
  directory, never in the project.
- Terraform: the binary terraform-ls runs is `terraform.binary`, else `terraform` on the
  project's `PATH`, else `tofu`. With none, the server still gives completion and syntax and the
  status line says formatting and validation need one.

Tools bongos uses but does not install:

- A JDK 25 or newer for jdtls (and the JDK your project builds with).
- `terraform` or `tofu` for Terraform formatting and validation.
- Node.js 22 or later for the Node-based servers and Prettier; without one, bongos downloads
  Node.js from `nodejs.org`.
- `bun` for Bun projects, `uv` or `poetry` for those projects' environments (bongos finds the
  environments they made; it does not create them), and the project's own Biome.

## Language extensions

A language bongos does not ship can come from a language extension, built from a git repository
on your machine. There is no marketplace: bongos carries a short list of extensions, each pinned
to a commit, and you can install any other repository by its URL.

When you open a file of a language bongos does not know but the list does, the status bar says
"Swift support is available" with Install, Not now (hides the offer for that language in that window
until restart) and Don't offer again
(adds the language to `language_extensions.never_offer`). Opening the file only reads the list;
nothing is cloned or built until you choose Install.

| Command | What it does |
|---|---|
| `extensions: install language` | Pick a language from the list and install it at its pinned commit |
| `extensions: install from git` | Install a repository by URL, then a branch, tag or commit (empty for the default branch) |
| `extensions: update` | Move an extension to its new pin, or refetch the ref you installed; `latest` leaves the pin |
| `extensions: rebuild` | Build the same commit again, with no network for the extension itself |
| `extensions: remove` | Unload an extension and delete its checkout, link and record |
| `extensions: show build log` | Open the build log of an extension, of the running build, or of the last failed one |

The status line shows each install as it goes ("Swift extension: building (12s)"), a failure
with Show log and Dismiss, and "Swift installed" for a few seconds after one finishes. Installed
extensions are also on the Extensions page (`zed: extensions`), with their source URL and short
commit and Update, Rebuild and Remove buttons.

Settings:

```json
{
  "language_extensions": {
    "enabled": true,
    "offer": true,
    "never_offer": []
  }
}
```

`enabled: false` turns the commands off (each then only says that language extensions are off),
and the startup repair, the offer line and the build status item too; extensions already installed
still load. `offer: false` turns the offer line off.

Trust: an extension runs code. Its build runs on your machine, and once loaded it can download and
start a language server. An install from the list at its pinned commit does not ask. Anything else,
a URL not on the list, another commit of a listed repository, or an update to a new commit, asks a
strict question naming the URL and the full commit, which only a typed `yes` or `no` answers. What
a loaded extension may do is `granted_extension_capabilities`; its default allows any process, any
download and any npm package:

```json
{
  "granted_extension_capabilities": [
    { "kind": "process:exec", "command": "*", "args": ["**"] },
    { "kind": "download_file", "host": "*", "path": ["**"] },
    { "kind": "npm:install", "package": "*" }
  ]
}
```

Tools: `git` is needed for every install. An extension with a Rust part (a `Cargo.toml`) needs
Rust with the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`); without them the install
stops before the build and says what to run. bongos never runs `rustup` for an
install from git. An extension whose `zed_extension_api` is newer than this bongos supports is
refused before the build, naming both versions. Builds run one at a time, in the background; the
editor stays usable while one runs.

Hosts, each contacted only when you run an install, update or rebuild, never at start and never on
a timer:

| Host | When |
|---|---|
| The extension's repository host | Install and update (`git ls-remote` and `git fetch`); a rebuild uses the local checkout |
| The hosts of its grammar repositories (from its `extension.toml`) | Building an extension with grammars |
| `github.com` | The first build of an extension with grammars, to download the WASI SDK (kept for later builds) |
| Cargo's registry (`index.crates.io` and `static.crates.io` unless Cargo is configured otherwise) | Building an extension with a Rust part |

A loaded extension may then download its language server from wherever it names, within
`granted_extension_capabilities`. The test `crates/bongos_extensions/tests/idle.rs` checks the
"never on a timer" part: a day passes with a file of a listed language open and nothing is
contacted. The language extensions smoke run has no idle window of its own.

Disk: everything is under the data directory (`~/.local/share/bongos` on Linux), in
`extensions/git`: one checkout per extension (`git/<id>/<commit>`, with its build output), its
install record, the build logs in `git/logs` and the lock files in `git/locks`.
`extensions/installed/<id>` is a link to the checkout. The build output stays with the checkout,
so a restart loads the extension again with no build. A build log is kept until that extension's
next install, update, rebuild or remove replaces it; `extensions: remove` deletes the checkout,
the link and the record.

Before a release, a developer runs `script/bongos/check-language-list` by hand, with network: it
fetches every listed commit, runs the preflight, builds each extension and checks its extension
API version. No gate runs it, since it needs the network and a Rust toolchain.

## Privacy

bongos's own code, and the telemetry and update checks of the language servers it runs, contact
no host beyond what a feature needs (the JetBrains license check for the IntelliJ server). A
project's own Gradle build is exempt: it may contact its hosts, such as services.gradle.org,
plugins.gradle.org, Maven repositories and Artifact Registry. An installed language extension's
own code may contact hosts when its language server starts, within the capabilities granted to
extensions.

Linear (`api.linear.app`) is contacted only when `linear.enabled` is on (the default) and a token
is set, for a linked task or a Linear command. The token is stored in the credential store and
never written to a log or a file.

The kill ring is saved in the app database so it survives a restart. Text marked secret on Linux
and the contents of masked fields (passwords) are never saved, nor is an entry over 1 MiB. Setting
`emacs.save_kill_ring` to `false` turns saving off and deletes the saved ring.
The secret hint is advisory: text from a password manager that sets no hint, from any macOS
manager, or typed by hand is saved like any other kill. `emacs.save_kill_ring: false` turns saving off.

Language extensions contact nothing on their own: no update check and no list download (the list
is compiled in). An install, update or rebuild you run contacts only the extension's repository
host and the hosts its build needs (see Language extensions for the table).

bongos never contacts bongos.rocks. The list of kotlin-lsp builds is compiled in.

kotlin-lsp builds are early access and expire about 29 days after build, so bongos pins one
build per release and releases roughly monthly. A new Kotlin build needs a new bongos release. If
the pinned build has expired, update bongos.

### Language server and debugger downloads

bongos downloads a language server or debug adapter when it first installs it and when it checks
for an update, never while you edit. It sends no telemetry to these hosts, and a server already on
your `PATH` is used without a download. Each one comes from:

- GitHub (`api.github.com` for the latest release, `github.com` for the download, which redirects
  to GitHub's release CDN, `objects.githubusercontent.com` or `release-assets.githubusercontent.com`,
  or for the ESLint source archive, `codeload.github.com`):
  rust-analyzer, ruff, ty, clangd, package-version-server, the ESLint server's source (pinned
  version), CodeLLDB, js-debug and delve-shim-dap (Delve's adapter shim).
- The npm registry that npm's own configuration names (`registry.npmjs.org` unless you set
  another): vtsls, typescript-language-server, basedpyright, pyright, the Tailwind servers, and
  the YAML, Bash, CSS and JSON servers, the ESLint server's dependencies, and Prettier. Every npm
  command runs with `npm_config_update_notifier=false` and `npm_config_audit=false`, so npm does
  not check for a newer npm or send an audit report.
- `nodejs.org`: Node.js itself, only when no suitable system Node.js (22 or later) is found.
- `pypi.org`: the latest debugpy version. The debugpy wheel is then fetched by your Python's pip,
  from the index pip's configuration names, as is pylsp.
- Go's module proxy, as Go's configuration names it (`proxy.golang.org` by default): gopls and
  Delve, built with your `go install`. The latest gopls version is read from `api.github.com`.

- `download.eclipse.org`: jdtls, the Java server (pinned build, checked against its SHA-256).
- `releases.hashicorp.com`: terraform-ls, the Terraform server (pinned build, checked against its
  SHA-256).

The YAML server is started with `redhat.telemetry.enabled` set to `false`. terraform-ls, and every
`terraform` it runs, get `CHECKPOINT_DISABLE=1`, which turns off HashiCorp's version check; no
setting turns it back on. jdtls and terraform-ls are installed once and then start from the cache
with no request, also offline.

A running server may still fetch what your files ask for: the YAML server reads its schema
catalog from `www.schemastore.org` (turn it off with `yaml.schemaStore.enable: false` in the
server's settings), the JSON and YAML servers fetch the schemas a file names, and the TypeScript
servers may fetch type packages from the npm registry. terraform-ls itself
always asks `registry.terraform.io` about every Terraform module with a registry source; no setting
turns that off. When you complete a `source` or `version` in a Terraform `module` block,
terraform-ls asks its module search service (Algolia) for suggestions. When jdtls imports a Maven
or Gradle project it downloads what the build asks for, from Maven Central and the project's other
repositories, and Buildship from the Gradle distribution host.

### Kotlin server manifest file

To run a Kotlin server build other than the one pinned in your bongos release (a newer kotlin-lsp
build, say), put a file named `kotlin-server-manifest.toml` in the config directory, next to
`settings.json` (`~/.config/bongos/` on Linux). It is optional; bongos reads it at start.

The schema is that of `crates/bongos_kotlin/servers.toml`. There is one table per server,
`kotlin-lsp` or `intellij`:

- `version`: the build's version.
- `built` (optional) and `expires`: dates, `YYYY-MM-DD`; `expires` is the last day a kotlin-lsp
  build runs.
- `args`: the start arguments (a list of strings).
- `[<server>.env]`: environment variables for the server.
- `[<server>.platform.<key>]`, one per platform (`linux-x64`, `linux-arm64`, `macos-arm64`,
  `macos-x64`): `url`, `sha256`, `archive` (`tar.gz` or `zip`), `strip_components`, `launcher`
  (the executable inside the archive) and, optionally, `runtime` (a bundled Java, relative to the
  archive root).

`sha256` is required for every platform, and the download is checked against it before it is
extracted. An example with two servers:

```toml
[kotlin-lsp]
version = "263.9999.0"
expires = "2026-12-20"

[kotlin-lsp.platform.linux-x64]
url = "https://download.jetbrains.com/language-server/kotlin-server/263.9999.0/kotlin-server-263.9999.0.tar.gz"
sha256 = "<64 hex digits>"
archive = "tar.gz"
strip_components = 1
launcher = "bin/intellij-server"
runtime = "jbr/bin/java"

[intellij]
version = "2026.1"

[intellij.platform.linux-x64]
url = "https://example.com/intellij-server-2026.1.tar.gz"
sha256 = "<64 hex digits>"
archive = "tar.gz"
strip_components = 1
launcher = "bin/intellij-server"
```

Get a `sha256` by downloading the archive yourself and running `sha256sum kotlin-server.tar.gz`
(`shasum -a 256` on macOS).

Rules:

- A server table in the file replaces the built-in one whole; omitted `args` and `[<server>.env]`
  keep the built-in values. A server you leave out keeps its built-in table.
- A `url` must be `https` (a debug build also accepts loopback `http`), carry no user name or
  password, and never name `bongos.rocks` or a subdomain of it.
- In a debug build, `BONGOS_KOTLIN_SERVERS` still wins over this file.
- A malformed or unreadable file is ignored and the built-in table is used. You see one toast, one
  line in the Kotlin status menu ("Server manifest ignored: <path>: <error>") and one line in
  `kotlin.log`.

bongos never contacts bongos.rocks, and it reads nothing from the network to find this file.

## JetBrains servers and their terms

bongos ships neither Kotlin language server, so the GPL work does not include them. Both come from
JetBrains and stay under JetBrains' terms.

- kotlin-lsp (partly closed source) is downloaded on first need from `download.jetbrains.com`,
  under JetBrains' terms. The pinned build and its SHA-256 are in
  `crates/bongos_kotlin/servers.toml`, and the download is checked against that digest.
- The IntelliJ language server is proprietary and is never downloaded unless the setting
  `kotlin.intellij.download` is `true` (default `false`). It is under the JetBrains EULA, which you
  must accept before it runs; until you do, bongos uses kotlin-lsp. With the setting off, you can
  point `lsp.kotlin.binary.path` at a server you installed yourself.
- `kotlin.log` in the logs directory records each install and every fallback from IntelliJ to
  kotlin-lsp.

## Layout

| Path | What |
|---|---|
| `script/bongos/` | Build, test and smoke scripts, and the workspace environment. |
| `docs/` | Generated reference pages, such as the keybindings of each profile. |

## References

- Origin (Zed tag and commit this fork is based on): see [ORIGIN.md](ORIGIN.md)
- Kyle's Emacs config: https://github.com/kylewaldner02/emacs.d (mirror of
  https://github.com/kylewaldner/emacs.d)
- Kyle's emacsJ fork: https://github.com/kylewaldner02/emacsj (mirror of
  https://github.com/kylewaldner/emacsj; upstream https://github.com/strindberg/emacsj)
- Kotlin LSP: https://github.com/Kotlin/kotlin-lsp
