# Rebases the run's branch when the pull request is stacked on a parent pull request that has
# merged: the branch moves onto the default branch, dropping the parent's commits, and is pushed
# to the pull request. Reports `unchanged` (no parent, or it has not merged), `rebased`, or
# `conflict` (the rebase is left in progress for the next node to resolve). No model runs here.

import json
import os
import subprocess
import sys


def run(*argv):
    return subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def checked(*argv):
    done = run(*argv)
    if done.returncode != 0:
        sys.exit(" ".join(argv[:3]) + " failed: " + done.stderr.strip())
    return done.stdout


def report(result):
    with open(os.environ["BONGOS_RESULT"], "w") as out:
        json.dump({"result": result}, out)


def main():
    with open(os.environ["BONGOS_STATE"]) as state_file:
        pr = json.load(state_file)["inputs"].get("pr")
    if not pr:
        sys.exit("this workflow needs a pull request: set the task's pull request first")

    view = json.loads(checked("gh", "pr", "view", pr, "--json", "baseRefName,headRefName"))
    default = json.loads(checked("gh", "repo", "view", "--json", "defaultBranchRef"))
    default = default["defaultBranchRef"]["name"]
    if view["baseRefName"] == default:
        return report("unchanged")

    parents = json.loads(
        checked(
            "gh", "pr", "list", "--head", view["baseRefName"], "--state", "merged",
            "--json", "headRefOid", "--limit", "1",
        )
    )
    if not parents:
        return report("unchanged")

    checked("git", "fetch", "origin", default)
    rebase = run("git", "rebase", "--onto", "origin/" + default, parents[0]["headRefOid"])
    if rebase.returncode != 0:
        return report("conflict")
    checked("git", "push", "--force-with-lease", "origin", "HEAD:refs/heads/" + view["headRefName"])
    checked("gh", "pr", "edit", pr, "--base", default)
    report("rebased")


main()
