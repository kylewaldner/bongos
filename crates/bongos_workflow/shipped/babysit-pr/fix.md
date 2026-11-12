The CI checks of pull request {{inputs.pr}} are failing, and the worktree holds its branch.

Find which checks failed and why: run `gh pr checks {{inputs.pr}}` for the list, and `gh run view <run id> --log-failed` for the log of a failed run, where those commands are available to you. Otherwise run the project's tests locally and read the failures.

Fix the cause rather than the symptom. Do not weaken or delete a test, or skip a check, to turn it green unless the test itself is wrong, and say so if it is. Run the tests that cover your fix before you finish.

Commit your fix on the current branch with a short message in the style of the repository's history; the next step pushes it. Report a short summary of what failed and what you changed.
