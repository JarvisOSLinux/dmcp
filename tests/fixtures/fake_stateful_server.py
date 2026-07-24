#!/usr/bin/env python3
"""Fake stateful MCP server for dmcp broker integration tests.

Speaks newline-delimited JSON-RPC 2.0 (MCP) over stdio using only the Python
standard library -- no third-party MCP framework, so it runs anywhere python3
does. State lives in-process:

  * ``counter`` increments a module-global integer and returns it.
  * ``pid``     returns ``os.getpid()``.

A fresh process per call would always answer ``counter == 1``; a persisted
process answers 1, 2, 3, ... with a stable pid. That difference is precisely
what the session broker must guarantee, so these two tools are enough to prove
"same process across calls" and to expose "no orphan left behind" after a close
or TTL expiry (the test kills / waits on the reported pid).

Two behaviours are toggled for broker edge-case tests:

  * An unknown tool name in ``tools/call`` is answered with a JSON-RPC error
    (protocol-level ``-32602``), NOT an ``isError`` result. rmcp surfaces that
    as ``ServiceError::McpError`` on a still-live child, so the broker must keep
    the session alive and report a tool/protocol error rather than "session
    lost" (regression #36).
  * ``FAKE_HANG_ON_INIT=1`` in the environment makes the server block forever on
    ``initialize`` without replying, standing in for a server that wedges before
    completing the MCP handshake (browser download, missing credential). The
    broker must bound spawn+initialize and reap the child.
"""

import json
import os
import sys
import time

COUNTER = 0

TOOLS = [
    {
        "name": "counter",
        "description": "Increment and return an in-process counter",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "pid",
        "description": "Return the server process id",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def result(req_id, res):
    send({"jsonrpc": "2.0", "id": req_id, "result": res})


def error(req_id, code, message):
    send({"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}})


def text_result(req_id, text, is_error=False):
    result(req_id, {"content": [{"type": "text", "text": text}], "isError": is_error})


def handle(msg):
    global COUNTER
    method = msg.get("method")
    req_id = msg.get("id")

    if method == "initialize":
        if os.environ.get("FAKE_HANG_ON_INIT"):
            # Never reply: exercise the broker's spawn/initialize timeout.
            while True:
                time.sleep(3600)
        params = msg.get("params") or {}
        proto = params.get("protocolVersion", "2025-03-26")
        result(
            req_id,
            {
                "protocolVersion": proto,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-stateful", "version": "0.1.0"},
            },
        )
    elif method == "notifications/initialized":
        # Notification: no response.
        pass
    elif method == "tools/list":
        result(req_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        if name == "counter":
            COUNTER += 1
            text_result(req_id, str(COUNTER))
        elif name == "pid":
            text_result(req_id, str(os.getpid()))
        else:
            # Protocol-level rejection (JSON-RPC error), not an isError result:
            # the child stays alive and its state is intact.
            error(req_id, -32602, "unknown tool: %s" % name)
    elif req_id is not None:
        error(req_id, -32601, "method not found: %s" % method)


def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        handle(msg)


if __name__ == "__main__":
    main()
