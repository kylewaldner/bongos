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
  starts one, `C-c w a` advances, `C-c w c` closes, `C-c w g` opens the graph, and the `C-c w d ...`,
  `C-c w p ...` and artifact keys cover todos, projects and artifacts; the rest are in the command
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
- **Fallbacks are labeled.** bongos reads each CLI's own `--help` and uses only what it finds. When
  a feature is missing, the chat still starts and its row in the Chats list says what changed
  (`labels:`), for example "session id read from the CLI's files", "name not passed to the CLI",
  "context appended to the system prompt (no hooks)", or "started a new session (no session to
  resume)". A fork when the CLI has no fork command is a fresh session that starts from a note naming
  the chat it continues, and is labeled `handoff`; nothing passes one off as a native fork.
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
| C-x h | mark the whole buffer |
| C-SPC, C-x C-SPC, C-x C-x | set the mark, pop the mark ring, exchange point and mark |
| C-g | keyboard-quit (cancels a prompt, a mark or a search) |

| Key | Editing command |
|---|---|
| C-d, M-d, M-DEL | delete a character, kill word, kill word backward |
| C-k, C-w, M-w, C-y | kill line, kill region or line, copy, yank |
| C-/, C-_, C-x u | undo |
| M-u, M-l, M-c | upcase, downcase, capitalize word |
| C-t, M-t, C-x C-t | transpose characters, words, lines |
| M-z, M-Z | zap to char, zap up to char (prompts for the character in the minibuffer) |
| M-SPC, C-BS, C-x C-o | just one space, delete horizontal space, delete blank lines |
| M-^, M-q | join line, fill paragraph |
| M-up, M-down | move the line up or down |
| C-x C-;, C-c ;, C-c y | toggle comment, comment-dwim, duplicate line |
| C-=, C-- | expand and contract the region |

| Key | Command |
|---|---|
| M-x | execute-extended-command (the command palette) |
| C-x C-f, C-x b | find a file, switch buffer |
| C-x C-s, C-x C-w, C-x s | save, write file, save all |
| C-x k, C-x 0, C-x 1 | kill buffer, close the item, close the others |
| C-x 2, C-x 3, C-x o | split below, split right, other window |
| C-x C-c | quit |
| C-h e | show the echo-area messages |

To switch it off, set another base keymap, for example `"base_keymap": "VSCode"`. Any value other
than `"Emacs"` removes every Emacs key and brings back Zed's keys, and the cursor goes back to a
bar. The change applies as soon as the settings file is saved.

`emacs.cursor_style` picks the cursor: `"auto"` (the default) is a block while the base keymap is
Emacs and your `cursor_shape` otherwise, `"emacs"` is always a block and `"modern"` is always a
bar.

```json
{ "base_keymap": "VSCode", "emacs": { "cursor_style": "auto" } }
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

C-SPC pressed right after a pop keeps popping, as in Emacs. C-u is not a prefix key yet, so
C-x C-SPC stands in for C-u C-SPC. Inside editors C-. sets the mark instead of opening code
actions, which stay in the command palette.

The mark and its ring belong to the buffer, so two panes showing one file share them, and edits
move them with their text. C-x C-. crosses files: it goes back through the active pane's history.

`emacs.delete_selection` (default `false`) decides what typing does over an active region: while
it is false, the character goes in at point, the selected text stays and the region is
deactivated; set it to `true` to have typing replace the region, as in other editors.

```json
{ "emacs": { "delete_selection": true } }
```

## Minibuffer

As in Emacs, prompts open in a one-line strip at the bottom of the window, below the status bar,
instead of a centered modal: M-x (the command palette), the file finder, the tab switcher, the
Work prompts and questions, messages (the echo area) and the hint that lists what keys can follow a
prefix. It is on by default. The matches show in a list above the strip and focus returns where
it was when the prompt ends.

`minibuffer.enabled_for` names the prompts that use the strip. Its default is
`["command_palette", "file_finder", "tab_switcher", "work", "messages", "key_hint", "goto_line", "zap"]`. Remove a name
to bring that prompt back as a centered modal, for example:

```json
{ "minibuffer": { "enabled_for": ["command_palette", "work"] } }
```

An empty list turns the minibuffer off: there is no strip and every prompt is a modal. The rest of
the `minibuffer` settings are `max_rows` (candidate rows shown, 15), `show_count` (the match count
before the prompt, true), `message_timeout` (how long a message stays, 4000 ms) and
`key_hint_delay` (how long a pending key sequence waits before its keys are shown, 500 ms).

## Privacy

bongos's own code, and the telemetry and update checks of the language servers it runs, contact
no host beyond what a feature needs (the JetBrains license check for the IntelliJ server). A
project's own Gradle build is exempt: it may contact its hosts, such as services.gradle.org,
plugins.gradle.org, Maven repositories and Artifact Registry.

Linear (`api.linear.app`) is contacted only when `linear.enabled` is on (the default) and a token
is set, for a linked task or a Linear command. The token is stored in the credential store and
never written to a log or a file.

kotlin-lsp builds are early access and expire about 29 days after build, so bongos pins one
build per release and releases roughly monthly. If the pinned build has expired, update bongos.

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

## References

- Origin (Zed tag and commit this fork is based on): see [ORIGIN.md](ORIGIN.md)
- Kyle's Emacs config: https://github.com/kylewaldner02/emacs.d (mirror of
  https://github.com/kylewaldner/emacs.d)
- Kyle's emacsJ fork: https://github.com/kylewaldner02/emacsj (mirror of
  https://github.com/kylewaldner/emacsj; upstream https://github.com/strindberg/emacsj)
- Kotlin LSP: https://github.com/Kotlin/kotlin-lsp
