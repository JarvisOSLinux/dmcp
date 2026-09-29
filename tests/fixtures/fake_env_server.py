#!/usr/bin/env python3
"""Fake MCP server that reports the credential environment it was started with.

One tool, ``whoami``, answers with the values of ``DEMO_TOKEN`` and
``DEMO_USER`` as JSON (``null`` when unset). That is the whole observable a
credential test needs: what dmcp put in the server's environment at spawn.
Stdlib only, newline-delimited JSON-RPC over stdio.
"""

import json
import os
import sys

TOOLS = [
    {
        "name": "whoami",
        "description": "Report the credential environment",
        "inputSchema": {"type": "object", "properties": {}},
    }
]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def handle(msg):
    method = msg.get("method")
    req_id = msg.get("id")
    if method == "initialize":
        params = msg.get("params") or {}
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": params.get("protocolVersion", "2025-03-26"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-env", "version": "0.1.0"},
                },
            }
        )
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        seen = {k: os.environ.get(k) for k in ("DEMO_TOKEN", "DEMO_USER")}
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"content": [{"type": "text", "text": json.dumps(seen)}]},
            }
        )
    elif req_id is not None:
        send({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": method}})


for line in sys.stdin:
    line = line.strip()
    if line:
        try:
            handle(json.loads(line))
        except ValueError:
            pass
