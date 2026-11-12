#!/usr/bin/env python3
"""Fake manifest host for smoke runs (standard library only): the Kotlin server manifest, its
signature and the server archives, served as static files.

Usage: fake_manifest_host.py <root dir> <port file> <log file>. The server listens on 127.0.0.1 on a
free port and writes the port to <port file> once it is listening; the app is pointed at
http://127.0.0.1:<port>/<path under the root dir> through `kotlin.manifest_url`.

GET and HEAD serve the file at the request path under the root dir (a path that leaves the root,
or names a directory or a missing file, answers 404). Every such request is logged as one line,
`<method> <path> <status>`, written when the response headers go out; the control endpoint is not
logged. A line that starts with `#` is an event, not a request: `# aborted <path> after <n> bytes`
when the client closed a response before its end.

POST /control takes a JSON object and answers a JSON object:
  {"op": "replace", "path": "/a/b.tar.gz", "file": "/abs/file"}  serve that file at the path instead
  {"op": "restore", "path": "/a/b.tar.gz"}                       serve the file under the root again
  {"op": "hold", "path": "/a/b.tar.gz", "bytes": 1310720}        send at most that many body bytes of
                                                                  the path (all requests, counted per
                                                                  request), then wait; a larger value
                                                                  lets more through, null lifts it
  {"op": "sent", "path": "/a/b.tar.gz"}                          {"sent": <bytes the newest response
                                                                  of the path has sent>, "open": <how
                                                                  many responses are in flight>}
The Content-Length is always the whole file, so a held download shows a partial percentage.
"""
import json
import os
import select
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.realpath(sys.argv[1])
PORT_FILE = sys.argv[2]
LOG_FILE = sys.argv[3]
LIFETIME_SECONDS = 1800
CHUNK = 16 * 1024

LOCK = threading.Lock()
REPLACED = {}  # path -> absolute file
HOLDS = {}  # path -> byte limit
SENT = {}  # path -> bytes the newest response has sent
OPEN = {}  # path -> responses in flight


def log_line(text):
    with LOCK:
        with open(LOG_FILE, "a") as log:
            log.write(text + "\n")


def file_for(path):
    with LOCK:
        replaced = REPLACED.get(path)
    if replaced is not None:
        return replaced if os.path.isfile(replaced) else None
    target = os.path.realpath(os.path.join(ROOT, path.lstrip("/")))
    if os.path.commonpath([ROOT, target]) != ROOT or not os.path.isfile(target):
        return None
    return target


def peer_closed(connection):
    readable, _, _ = select.select([connection], [], [], 0)
    if not readable:
        return False
    try:
        return connection.recv(1, socket.MSG_PEEK) == b""
    except OSError:
        return True


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply_json(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def serve(self, send_body):
        path = self.path.split("?", 1)[0]
        target = file_for(path)
        if target is None:
            log_line("%s %s 404" % (self.command, path))
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        size = os.path.getsize(target)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(size))
        self.end_headers()
        log_line("%s %s 200" % (self.command, path))
        if not send_body:
            return
        with LOCK:
            SENT[path] = 0
            OPEN[path] = OPEN.get(path, 0) + 1
        sent = 0
        try:
            with open(target, "rb") as handle:
                while sent < size:
                    with LOCK:
                        limit = HOLDS.get(path)
                    allowed = size if limit is None else min(size, limit)
                    if sent >= allowed:
                        if peer_closed(self.connection):
                            raise BrokenPipeError
                        time.sleep(0.05)
                        continue
                    handle.seek(sent)
                    chunk = handle.read(min(CHUNK, allowed - sent))
                    self.wfile.write(chunk)
                    self.wfile.flush()
                    sent += len(chunk)
                    with LOCK:
                        SENT[path] = sent
        except (BrokenPipeError, ConnectionError):
            log_line("# aborted %s after %d bytes" % (path, sent))
            self.close_connection = True
        finally:
            with LOCK:
                OPEN[path] -= 1

    def do_GET(self):
        self.serve(True)

    def do_HEAD(self):
        self.serve(False)

    def do_POST(self):
        if self.path != "/control":
            self.reply_json(404, {"error": "not found"})
            return
        try:
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            command = json.loads(body)
            op, path = command["op"], command["path"]
        except (ValueError, KeyError, TypeError):
            self.reply_json(400, {"error": "expected {op, path, ...}"})
            return
        with LOCK:
            if op == "replace":
                REPLACED[path] = command["file"]
            elif op == "restore":
                REPLACED.pop(path, None)
            elif op == "hold":
                if command.get("bytes") is None:
                    HOLDS.pop(path, None)
                else:
                    HOLDS[path] = int(command["bytes"])
            elif op == "sent":
                answer = {"sent": SENT.get(path, 0), "open": OPEN.get(path, 0)}
                self.reply_json(200, answer)
                return
            else:
                self.reply_json(400, {"error": "unknown op"})
                return
        self.reply_json(200, {"ok": True})


def watchdog():
    time.sleep(LIFETIME_SECONDS)
    # A scenario that dies without stopping the fake must not leave it behind.
    os._exit(0)


open(LOG_FILE, "a").close()
server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.daemon_threads = True
threading.Thread(target=watchdog, daemon=True).start()
with open(PORT_FILE + ".tmp", "w") as handle:
    handle.write(str(server.server_address[1]))
os.replace(PORT_FILE + ".tmp", PORT_FILE)
server.serve_forever()
