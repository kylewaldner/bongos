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

## Privacy

bongos's own code, and the telemetry and update checks of the language servers it runs, contact
no host beyond what a feature needs (the JetBrains license check for the IntelliJ server). A
project's own Gradle build is exempt: it may contact its hosts, such as services.gradle.org,
plugins.gradle.org, Maven repositories and Artifact Registry.

kotlin-lsp builds are early access and expire about 29 days after build, so bongos pins one
build per release and releases roughly monthly. If the pinned build has expired, update bongos.

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
