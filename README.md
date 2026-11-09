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

## Privacy

bongos's own code, and the telemetry and update checks of the language servers it runs, contact
no host beyond what a feature needs (the JetBrains license check for the IntelliJ server). A
project's own Gradle build is exempt: it may contact its hosts, such as services.gradle.org,
plugins.gradle.org, Maven repositories and Artifact Registry.

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
