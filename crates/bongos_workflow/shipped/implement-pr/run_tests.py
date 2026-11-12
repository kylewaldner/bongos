# Runs the project's tests in the worktree and reports `pass` or `fail`.
#
# It picks the first test command that fits the project: cargo, gradle, npm, then make. To use
# another command, copy this workflow (`workflow: new`), edit the list in `detect` and name the
# program in `permissions.commands` of the `test` node.

import json
import os
import subprocess
import sys

LOG_TAIL = 4000


def detect():
    if os.path.exists("Cargo.toml"):
        return ["cargo", "test"]
    if os.path.exists("build.gradle") or os.path.exists("build.gradle.kts"):
        return ["gradle", "test"]
    if os.path.exists("package.json"):
        return ["npm", "test"]
    if os.path.exists("Makefile"):
        return ["make", "test"]
    return None


def main():
    command = detect()
    if command is None:
        sys.exit("no test command found: add one to run_tests.py in this workflow")
    done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    result = {
        "status": "pass" if done.returncode == 0 else "fail",
        "log": done.stdout[-LOG_TAIL:],
    }
    with open(os.environ["BONGOS_RESULT"], "w") as out:
        json.dump(result, out)


main()
