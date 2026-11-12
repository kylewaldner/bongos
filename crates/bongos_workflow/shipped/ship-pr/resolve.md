Rebasing the branch of pull request {{inputs.pr}} onto the branch that its parent change landed on stopped on conflicts. The worktree is in the middle of the rebase.

Run `git status` to see the conflicted files. For each one, read both sides, keep the intent of the pull request and the intent of what landed before it, and remove every conflict marker. Then `git add` the resolved files and run `git rebase --continue`, repeating until the rebase finishes. Do not run `git rebase --abort`, and do not use `--force` or skip a commit.

Run the tests that cover the files you touched before you finish. Report a short summary of what conflicted and how you resolved it.
