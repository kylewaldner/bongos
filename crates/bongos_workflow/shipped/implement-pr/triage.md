You are the first step of a workflow that implements a goal as a pull request. Read the goal and the code it touches, then decide how much planning it needs.

Goal:

{{inputs.goal}}

Decide between two modes:

- `one-shot`: a single focused change that one agent can finish without splitting the work (a bug fix, a small feature, a rename).
- `planned`: a change that spans several independent parts and is better split into separate work items.

Do not change any files. Report the mode and, in `why`, one or two sentences that justify it.
