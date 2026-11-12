# Merges the pull request on the repository host. The method comes from `with.method` (`squash`,
# `merge` or `rebase`). A merge the host refuses (a required check, a review, a conflict) reports
# `blocked` and leaves the pull request as it was.

import json
import os
import subprocess
import sys


def main():
    with open(os.environ["BONGOS_STATE"]) as state_file:
        state = json.load(state_file)
    pr = state["inputs"].get("pr")
    if not pr:
        sys.exit("this workflow needs a pull request: set the task's pull request first")
    method = state["settings"].get("method", "squash")
    if method not in ("squash", "merge", "rebase"):
        sys.exit("unknown merge method '" + method + "': use squash, merge or rebase")

    merged = subprocess.run(
        ["gh", "pr", "merge", pr, "--" + method],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    with open(os.environ["BONGOS_RESULT"], "w") as out:
        json.dump({"result": "merged" if merged.returncode == 0 else "blocked"}, out)


main()
