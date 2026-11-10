#!/usr/bin/env python3
"""Fake Linear GraphQL server for smoke runs (standard library only).

Usage: fake_linear.py <port file> <log file>. The server listens on 127.0.0.1 on a free port and
writes the port to <port file> once it is listening; the app is pointed at
http://127.0.0.1:<port>/graphql through BONGOS_LINEAR_API_URL.

It holds one team with workflow states and the issues SEQ-120 (a group) with its subtickets
SEQ-121 to SEQ-124, answers the queries and mutations the bongos client sends (viewer, recent,
issue reads with one level of subtickets, team states, issueUpdate, issueCreate) and applies
issueUpdate and issueCreate to its state. Every GraphQL request is logged as
`<operation> <ids> auth=<present|absent>`, one line, never the value of the Authorization header.

POST /control changes the state from the scenario: {"op": "add", "parent": "SEQ-120",
"identifier": "SEQ-127", "title": "Handle refunds"} adds a subticket (not logged).
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT_FILE = sys.argv[1]
LOG_FILE = sys.argv[2]
LIFETIME_SECONDS = 1800

TEAM_ID = "00000000-0000-4000-8000-00000000aaaa"
STATES = [
    {"id": "00000000-0000-4000-8000-0000000000b1", "name": "Backlog", "type": "backlog", "position": 0},
    {"id": "00000000-0000-4000-8000-0000000000b2", "name": "Todo", "type": "unstarted", "position": 1},
    {"id": "00000000-0000-4000-8000-0000000000b3", "name": "In Progress", "type": "started", "position": 2},
    {"id": "00000000-0000-4000-8000-0000000000b4", "name": "In Review", "type": "started", "position": 3},
    {"id": "00000000-0000-4000-8000-0000000000b5", "name": "Done", "type": "completed", "position": 4},
    {"id": "00000000-0000-4000-8000-0000000000b6", "name": "Canceled", "type": "canceled", "position": 5},
]
BACKLOG = STATES[0]

LOCK = threading.Lock()
ISSUES = {}
NEXT_CREATED = [140]


def issue_id(number):
    return "00000000-0000-4000-8000-%012d" % number


def add_issue(identifier, title, parent=None, description="", uuid=None):
    number = int(identifier.split("-")[1])
    ISSUES[identifier] = {
        "id": uuid or issue_id(number),
        "identifier": identifier,
        "title": title,
        "description": description,
        "url": "https://linear.app/fake/issue/%s" % identifier,
        "state": BACKLOG,
        "parent": parent,
    }


add_issue("SEQ-120", "Proration for billing", description="Charge by the day when a plan changes.")
for number, title in [
    (121, "Add proration columns"),
    (122, "Proration engine"),
    (123, "Invoice integration"),
    (124, "Proration endpoint"),
]:
    add_issue("SEQ-%d" % number, title, parent="SEQ-120", description="Part of SEQ-120.")


def find(ref):
    """The issue whose identifier or UUID is <ref>, or None."""
    if ref in ISSUES:
        return ISSUES[ref]
    for issue in ISSUES.values():
        if issue["id"] == ref:
            return issue
    return None


def summary(issue):
    return {
        "id": issue["id"],
        "identifier": issue["identifier"],
        "title": issue["title"],
        "url": issue["url"],
        "team": {"id": TEAM_ID},
        "state": issue["state"],
    }


def child_node(issue):
    node = summary(issue)
    node["description"] = issue["description"]
    return node


def children_of(issue):
    return [i for i in ISSUES.values() if i["parent"] == issue["identifier"]]


def full_node(issue):
    node = child_node(issue)
    parent = ISSUES.get(issue["parent"]) if issue["parent"] else None
    node["parent"] = summary(parent) if parent else None
    node["relations"] = {"nodes": []}
    node["children"] = {
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": [child_node(c) for c in children_of(issue)],
    }
    return node


def log(operation, ids, auth):
    line = "%s%s auth=%s\n" % (operation, (" " + ids) if ids else "", auth)
    with open(LOG_FILE, "a") as handle:
        handle.write(line)


def error(message, code="INTERNAL", path=None):
    entry = {"message": message, "extensions": {"code": code}}
    if path:
        entry["path"] = path
    return entry


def handle_graphql(body, auth):
    query = body.get("query", "")
    variables = body.get("variables") or {}
    with LOCK:
        if "mutation SetState" in query:
            target = find(variables.get("id", ""))
            state = next((s for s in STATES if s["id"] == variables.get("stateId")), None)
            shown = target["identifier"] if target else "unknown"
            log("issueUpdate", "%s state=%s" % (shown, state["name"] if state else "unknown"), auth)
            if target is None or state is None:
                return {"errors": [error("Entity not found", "ENTITY_NOT_FOUND")]}
            target["state"] = state
            return {"data": {"issueUpdate": {"success": True}}}
        if "mutation CreateIssue" in query:
            fields = variables.get("input") or {}
            parent = find(fields.get("parentId", ""))
            identifier = "SEQ-%d" % NEXT_CREATED[0]
            NEXT_CREATED[0] += 1
            add_issue(
                identifier,
                fields.get("title", ""),
                parent=parent["identifier"] if parent else None,
                description=fields.get("description", ""),
                uuid=fields.get("id"),
            )
            log(
                "issueCreate",
                "parent=%s id=%s" % (parent["identifier"] if parent else "none", identifier),
                auth,
            )
            return {"data": {"issueCreate": {"success": True, "issue": summary(ISSUES[identifier])}}}
        if "query Viewer" in query:
            log("viewer", "", auth)
            return {"data": {"viewer": {"organization": {"name": "Fake Org"}}}}
        if "query Recent" in query:
            log("recent", "", auth)
            return {"data": {"viewer": {"assignedIssues": {"nodes": [summary(ISSUES["SEQ-120"])]}}}}
        if "query TeamStates" in query:
            log("teamStates", "", auth)
            return {"data": {"team": {"states": {"nodes": STATES}}}}
        if "query ChildrenPage" in query:
            target = find(variables.get("id", ""))
            log("childrenPage", target["identifier"] if target else "unknown", auth)
            if target is None:
                return {"data": {"issue": None}, "errors": [error("Entity not found", "ENTITY_NOT_FOUND")]}
            return {
                "data": {
                    "issue": {
                        "children": {
                            "pageInfo": {"hasNextPage": False, "endCursor": None},
                            "nodes": [child_node(c) for c in children_of(target)],
                        }
                    }
                }
            }
        if "query Issues(" in query:
            data = {}
            errors = []
            shown = []
            index = 0
            while "id%d" % index in variables:
                ref = variables["id%d" % index]
                found = find(ref)
                alias = "i%d" % index
                shown.append(found["identifier"] if found else ref)
                if found is None:
                    data[alias] = None
                    errors.append(error("Entity not found: Issue", "ENTITY_NOT_FOUND", [alias]))
                else:
                    data[alias] = full_node(found)
                index += 1
            log("issues", ",".join(shown), auth)
            response = {"data": data}
            if errors:
                response["errors"] = errors
            return response
    log("unknown", "", auth)
    return {"errors": [error("unsupported operation")]}


def handle_control(body):
    with LOCK:
        if body.get("op") == "add":
            add_issue(
                body["identifier"],
                body["title"],
                parent=body.get("parent"),
                description=body.get("description", ""),
            )
            return {"ok": True}
    return {"ok": False}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            self.reply(400, {"errors": [error("bad request")]})
            return
        if self.path == "/control":
            self.reply(200, handle_control(body))
            return
        auth = "present" if self.headers.get("Authorization") else "absent"
        if auth == "absent":
            log("unauthenticated", "", auth)
            self.reply(401, {"errors": [error("Authentication required", "AUTHENTICATION_ERROR")]})
            return
        self.reply(200, handle_graphql(body, auth))


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
