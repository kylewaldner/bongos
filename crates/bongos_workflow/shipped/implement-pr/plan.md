You are planning the implementation of a goal that was judged too large for one pass.

Goal:

{{inputs.goal}}

Why it needs a plan: {{nodes.triage.why}}

Read the code, then do two things:

1. Write a short design document to a file named `DESIGN.md` in the root of the worktree. Say what changes, where, and in what order the parts depend on each other. Commit it on the current branch so every work item starts with it.
2. Report the work items: a list of self-contained descriptions, one per part, each small enough for one agent to finish alone. Items run in parallel, each in its own worktree, so they must not edit the same lines. Give each item enough context to be understood without the others.

Report `design` as the path of the design document and `items` as the list.
