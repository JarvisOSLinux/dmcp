#!/usr/bin/env python3
"""Fake logging MCP server for the stderr-relay integration tests (#49).

Speaks newline-delimited JSON-RPC 2.0 (MCP) over stdio using only the Python
standard library. Two tools, each proving one half of relay-and-buffer:

  * ``blocking_log`` writes a marker to stderr WITHOUT a trailing newline
    (like an interactive prompt) mid-``tools/call``, then blocks until the
    sentinel file named in its arguments appears. The test creates the
    sentinel only after seeing the marker on dmcp's stderr, so the call can
    complete only if stderr is relayed while the call is still in flight —
    an end-buffered (or line-buffered) relay turns the test into a bounded,
    legible failure instead of a green run.
  * ``explode`` writes a traceback-shaped line to stderr and dies without
    answering, so the failed call's error text must carry that line as
    retained detail.
  * ``flood_and_explode`` writes a first marker, several hundred KiB of
    filler, and a last marker to stderr, then dies without answering — the
    failed call's error text must carry only the bounded TAIL of that flood
    (last marker present, first absent, truncation announced), while the
    live relay still streams the whole flood through.
"""

import json
import os
import sys
import time

TOOLS = [
    {
        "name": "blocking_log",
        "description": "Log to stderr, then block until the sentinel file exists",
        "inputSchema": {
            "type": "object",
            "properties": {"sentinel": {"type": "string"}},
        },
    },
    {
        "name": "explode",
        "description": "Log to stderr, then die without answering",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "flood_and_explode",
        "description": "Flood several hundred KiB to stderr, then die without answering",
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
    method = msg.get("method")
    req_id = msg.get("id")

    if method == "initialize":
        params = msg.get("params") or {}
        proto = params.get("protocolVersion", "2025-03-26")
        result(
            req_id,
            {
                "protocolVersion": proto,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-logging", "version": "0.1.0"},
            },
        )
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        result(req_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "blocking_log":
            # No trailing newline on purpose: a line-buffered relay would sit
            # on this until it never comes.
            sys.stderr.write("MARKER_LIVE: Proceed? [Y/n] ")
            sys.stderr.flush()
            sentinel = args.get("sentinel", "")
            deadline = time.time() + 60
            while not (sentinel and os.path.exists(sentinel)):
                if time.time() > deadline:
                    text_result(req_id, "sentinel never appeared", is_error=True)
                    return
                time.sleep(0.05)
            text_result(req_id, "unblocked")
        elif name == "explode":
            sys.stderr.write("FAKE_TRACEBACK: something terrible happened\n")
            sys.stderr.flush()
            os._exit(1)
        elif name == "flood_and_explode":
            # ~300 KiB between the markers: several times the 64 KiB the
            # caller may retain, so only the tail can survive.
            sys.stderr.write("FLOOD_FIRST_MARKER\n")
            sys.stderr.write(("F" * 127 + "\n") * 2400)
            sys.stderr.write("FLOOD_LAST_MARKER\n")
            sys.stderr.flush()
            os._exit(1)
        else:
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
