#!/usr/bin/env python3
"""Logging proxy for smoke runs: records the method and host of each request, replies 403.

Usage: log_proxy.py <port> <log file>. The log exists once the port is listening.
"""
import socketserver
import sys
from urllib.parse import urlsplit


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        parts = self.rfile.readline(65536).decode("latin-1").split()
        if len(parts) < 2:
            return
        method, target = parts[0], parts[1]
        netloc = target if method == "CONNECT" else urlsplit(target).netloc
        host = netloc.rpartition("@")[2].rpartition(":")[0] or netloc
        with open(sys.argv[2], "a") as log:
            log.write(f"{method} {host.lower()}\n")
        self.wfile.write(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


with Server(("127.0.0.1", int(sys.argv[1])), Handler) as server:
    open(sys.argv[2], "a").close()
    server.serve_forever()
