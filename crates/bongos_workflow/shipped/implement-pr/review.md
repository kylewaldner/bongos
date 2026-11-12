You are reviewing a finished change before it is shipped. The tests already pass.

Goal:

{{inputs.goal}}

Read the whole change with `git diff` against the branch it started from, and the code around it. Judge it against the goal:

- Does it do everything the goal asks, and nothing unrelated?
- Is there a bug, a missed edge case or a broken caller?
- Do the tests cover the new behavior, and would they fail without the change?
- Does it match the style and conventions of the surrounding code?

Report `verdict` as `approve` when the change is ready to ship, or `changes` when something must be fixed first; put each required change in the text you report. Report `pr` as a pull request title and a short description for the change (the title on the first line), written whatever the verdict.
