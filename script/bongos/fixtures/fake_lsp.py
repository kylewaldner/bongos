#!/usr/bin/env python3
"""A fake language server for smoke runs (standard library only): LSP over stdio with
Content-Length framing, which records what the app sends it.

Environment:
  BONGOS_FAKE_LSP_NAME     the name written in each record (default "fake")
  BONGOS_FAKE_LSP_RECORD   a file that gets one JSON object per line: `start` (pid, argv, cwd and
                           the CHECKPOINT_DISABLE and PATH of the environment), `initialize` (root
                           and initialization options), `initialized`, `configuration` (the answer
                           to the workspace/configuration request below), `did_change_configuration`,
                           `format` (the command run and its exit status) and `exit`
  BONGOS_FAKE_LSP_CONFIG   comma-separated sections the server asks for with workspace/configuration
                           once the client says `initialized`
  BONGOS_FAKE_LSP_FORMAT   `terraform`: textDocument/formatting runs `<terraform.path> fmt -` on the
                           document, where terraform.path comes from the initialization options, as
                           terraform-ls does; without a path the request fails

Every other request is answered with a null result; notifications are ignored.
"""
import json
import os
import subprocess
import sys

NAME = os.environ.get("BONGOS_FAKE_LSP_NAME", "fake")
RECORD = os.environ.get("BONGOS_FAKE_LSP_RECORD")
CONFIG_SECTIONS = [s for s in os.environ.get("BONGOS_FAKE_LSP_CONFIG", "").split(",") if s]
FORMAT = os.environ.get("BONGOS_FAKE_LSP_FORMAT", "")

STDIN = sys.stdin.buffer
STDOUT = sys.stdout.buffer


def record(event, **fields):
    if not RECORD:
        return
    fields.update({"server": NAME, "event": event, "pid": os.getpid()})
    with open(RECORD, "a") as handle:
        handle.write(json.dumps(fields) + "\n")


def read_message():
    length = None
    while True:
        line = STDIN.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        key, _, value = line.decode("ascii", "replace").partition(":")
        if key.lower() == "content-length":
            length = int(value.strip())
    if length is None:
        return None
    return json.loads(STDIN.read(length))


def send(message):
    message["jsonrpc"] = "2.0"
    body = json.dumps(message).encode()
    STDOUT.write(b"Content-Length: %d\r\n\r\n" % len(body))
    STDOUT.write(body)
    STDOUT.flush()


def whole_document_edit(old, new):
    lines = old.split("\n")
    return {
        "range": {
            "start": {"line": 0, "character": 0},
            "end": {"line": len(lines) - 1, "character": len(lines[-1])},
        },
        "newText": new,
    }


def main():
    record(
        "start",
        argv=sys.argv[1:],
        cwd=os.getcwd(),
        checkpoint_disable=os.environ.get("CHECKPOINT_DISABLE"),
        path=os.environ.get("PATH", ""),
    )
    options = {}
    documents = {}
    pending = {}
    next_id = 1
    while True:
        message = read_message()
        if message is None:
            record("exit", reason="stdin closed")
            return 0
        method = message.get("method")
        if method is None:
            # The answer to a request of ours.
            section = pending.pop(message.get("id"), None)
            if section is not None:
                record("configuration", sections=section, result=message.get("result"))
            continue
        params = message.get("params") or {}
        if method == "initialize":
            options = params.get("initializationOptions") or {}
            record(
                "initialize",
                root_uri=params.get("rootUri"),
                initialization_options=options,
            )
            send({
                "id": message["id"],
                "result": {
                    "capabilities": {
                        "textDocumentSync": 1,
                        "documentFormattingProvider": FORMAT != "",
                    },
                    "serverInfo": {"name": NAME},
                },
            })
        elif method == "initialized":
            record("initialized")
            if CONFIG_SECTIONS:
                pending[next_id] = CONFIG_SECTIONS
                send({
                    "id": next_id,
                    "method": "workspace/configuration",
                    "params": {"items": [{"section": s} for s in CONFIG_SECTIONS]},
                })
                next_id += 1
        elif method == "workspace/didChangeConfiguration":
            record("did_change_configuration", settings=params.get("settings"))
        elif method == "textDocument/didOpen":
            document = params["textDocument"]
            documents[document["uri"]] = document["text"]
        elif method == "textDocument/didChange":
            changes = params.get("contentChanges") or []
            if changes and "range" not in changes[-1]:
                documents[params["textDocument"]["uri"]] = changes[-1]["text"]
        elif method == "textDocument/formatting":
            uri = params["textDocument"]["uri"]
            text = documents.get(uri, "")
            binary = ((options.get("terraform") or {}).get("path")) if FORMAT == "terraform" else None
            if not binary:
                record("format", command=None, status=None)
                send({"id": message["id"], "error": {"code": -32603, "message": "no terraform binary"}})
                continue
            command = [binary, "fmt", "-"]
            result = subprocess.run(command, input=text.encode(), capture_output=True)
            record("format", command=command, status=result.returncode,
                   stderr=result.stderr.decode(errors="replace"))
            if result.returncode != 0:
                send({"id": message["id"], "error": {"code": -32603, "message": "terraform fmt failed"}})
                continue
            formatted = result.stdout.decode()
            edits = [] if formatted == text else [whole_document_edit(text, formatted)]
            send({"id": message["id"], "result": edits})
        elif method == "shutdown":
            send({"id": message["id"], "result": None})
        elif method == "exit":
            record("exit", reason="exit notification")
            return 0
        elif "id" in message:
            send({"id": message["id"], "result": None})


if __name__ == "__main__":
    sys.exit(main())
