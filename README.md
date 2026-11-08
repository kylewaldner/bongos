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
`artifacts.toml`.

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
