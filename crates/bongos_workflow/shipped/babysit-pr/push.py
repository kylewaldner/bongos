# Pushes the run's branch to the branch of the pull request, so CI runs on what the agent changed.
# A push the host rejects (the pull request branch moved) reports `rejected`.

import json
import os
import subprocess
import sys


def run(*argv):
    return subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def main():
    with open(os.environ["BONGOS_STATE"]) as state_file:
        pr = json.load(state_file)["inputs"].get("pr")
    if not pr:
        sys.exit("this workflow needs a pull request: set the task's pull request first")

    view = run("gh", "pr", "view", pr, "--json", "headRefName")
    if view.returncode != 0:
        sys.exit("gh pr view failed: " + view.stderr.strip())
    head = json.loads(view.stdout)["headRefName"]

    pushed = run("git", "push", "origin", "HEAD:refs/heads/" + head)
    with open(os.environ["BONGOS_RESULT"], "w") as out:
        json.dump({"result": "pushed" if pushed.returncode == 0 else "rejected"}, out)


main()
