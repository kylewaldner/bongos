# Reports the state of the pull request's CI checks: `green` when every check passed or was
# skipped, `red` when one failed or was cancelled, `waiting` when checks are still pending or none
# has started. `waiting` makes the node sleep and check again with no agent running.

import json
import os
import subprocess
import sys


def gh(*args):
    return subprocess.run(["gh", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def main():
    with open(os.environ["BONGOS_STATE"]) as state_file:
        pr = json.load(state_file)["inputs"].get("pr")
    if not pr:
        sys.exit("this workflow needs a pull request: set the task's pull request first")

    view = gh("pr", "view", pr, "--json", "state")
    if view.returncode != 0:
        sys.exit("gh pr view failed: " + view.stderr.strip())
    state = json.loads(view.stdout)["state"]
    if state != "OPEN":
        sys.exit("the pull request is " + state.lower() + ", not open")

    checks = gh("pr", "checks", pr, "--json", "bucket")
    try:
        buckets = [check["bucket"] for check in json.loads(checks.stdout or "[]")]
    except (ValueError, KeyError, TypeError):
        sys.exit("gh pr checks gave no checks list: " + (checks.stderr.strip() or checks.stdout.strip()))

    if any(bucket in ("fail", "cancel") for bucket in buckets):
        result = "red"
    elif not buckets or any(bucket == "pending" for bucket in buckets):
        result = "waiting"
    else:
        result = "green"
    with open(os.environ["BONGOS_RESULT"], "w") as out:
        json.dump({"result": result}, out)


main()
