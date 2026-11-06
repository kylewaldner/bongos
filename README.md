# bongos

**Continuing in a new session:** clone this repo, open Claude Code in it, and run
`/bongos-continue`. The skill reads `plans/STATE.md` and `plans/index.md`, reports where
things stand, and continues the planning and build cycle. Nothing else is needed.
To set up a new machine to build on, follow `plans/MACHINE-SETUP.md`.
To build features, run `/bongos-ship`: it picks up implementation where it stopped and
ships one feature at a time, each in its own PR.


A fast IDE for Kotlin work with an Emacs-shaped UX, built as a fork of
[Zed](https://github.com/zed-industries/zed), with projects and tasks as a first-class
model, terminal-hosted Claude Code and Codex sessions, headless agent workflows, and a
native diagram canvas for reviewing and sketching systems.

Status: planning. The Zed v1.9.0 tree is imported; bongos-specific changes land feature by
feature, and the plans are in `plans/`.

bongos is derived from Zed (https://github.com/zed-industries/zed), and Zed's licences are
retained: see `LICENSE-GPL`, `LICENSE-APACHE` and `legal/`.

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

## Layout

| Path | What |
|---|---|
| `DISCOVERY.md` | Discovery findings and the decisions taken. Read this first. |
| `EMACS-FEATURES.md` | The scoped list of Emacs and IntelliJ behaviours to build. |
| `plans/index.md` | The feature order and each feature's status. Authoritative. |
| `plans/<nn-feature>/` | One directory per feature: `index.md` (status and artifacts), `BRIEF.md` (product opinions and starter prompt), `PRD.md` (product spec), later `EDD.md` (engineering design and implementation plan). |
| `plans/PRD-TEMPLATE.md` | Template for product specs. |
| `plans/TESTING.md` | The testing harness: gates every PR passes. |
| `plans/MACHINE-SETUP.md` | Setting up a machine to build and test on, from the repository alone. |
| `script/bongos/` | Build, test and smoke scripts, and the workspace environment. |
| `.claude/skills/bongos-continue/` | Skill that lets a fresh Claude Code session pick up where the last one stopped. |
| `.claude/skills/bongos-ship/` | Skill that ships features from EDD to merged PR, and resumes interrupted work. |

## How work happens

Features are developed one at a time, in the order in `plans/index.md`. Each feature
moves through: brief, PRD, Kyle's review, high-level plan, EDD, implementation, review,
done. The orchestrating session runs on Fable; subagents are routed by the model-routing
skill. Later the factory grows to run features in parallel.

## References

- Upstream: https://github.com/zed-industries/zed
- Kyle's Emacs config: https://github.com/kylewaldner02/emacs.d (mirror of
  https://github.com/kylewaldner/emacs.d)
- Kyle's emacsJ fork: https://github.com/kylewaldner02/emacsj (mirror of
  https://github.com/kylewaldner/emacsj; upstream https://github.com/strindberg/emacsj)
- Kotlin LSP: https://github.com/Kotlin/kotlin-lsp
