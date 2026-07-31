#!/usr/bin/env python3
"""Fake stateful MCP server that asks a question mid-tool-call.

Exists to prove dmcp's elicitation path end to end: a real server that issues a
real `elicitation/create` REQUEST while a `tools/call` is in flight and blocks
on the reply, which is the whole shape the feature exists to support. Stdlib
only, no MCP SDK.

Note the direction: this server is the JSON-RPC *client* for the duration of the
elicitation. Its stdin therefore carries two kinds of message — requests from
dmcp (which have `method`) and responses to the question it asked (which have
`result`/`error` and echo its id) — so `_read_reply` has to sort them out
instead of assuming everything inbound is a request.

`ask` reports what the answer was, so a test can tell an accept from a decline
by reading the tool result rather than trusting the plumbing.
"""
import json
import sys

PROTOCOL = "2024-11-05"


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def result(rid, payload):
    send({"jsonrpc": "2.0", "id": rid, "result": payload})


def _read_reply(want_id):
    """Read stdin until the reply to `want_id` arrives; return its result.

    Requests that arrive while we are waiting are answered as errors rather
    than dropped: leaving dmcp waiting on a request we silently ate would
    reproduce the very hang this feature removes.
    """
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if msg.get("id") == want_id and ("result" in msg or "error" in msg):
            return msg.get("result") or {}
        if msg.get("method") is not None and msg.get("id") is not None:
            send({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32603, "message": "busy awaiting elicitation"},
            })
    return {}


TOOLS = [
    {
        "name": "ask",
        "description": "Ask the user a question mid-call and report the answer.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "rounds": {
                    "type": "number",
                    "description": "How many questions to ask before returning.",
                }
            },
        },
    }
]


def call_ask(arguments, state):
    rounds = int(arguments.get("rounds") or 1)
    answers = []
    for i in range(rounds):
        state["req_id"] += 1
        rid = state["req_id"]
        send({
            "jsonrpc": "2.0",
            "id": rid,
            "method": "elicitation/create",
            "params": {
                "message": f"Question {i + 1}: partition type?",
                "requestedSchema": {
                    "type": "object",
                    "properties": {"choice": {"type": "string"}},
                },
            },
        })
        reply = _read_reply(rid)
        action = reply.get("action", "<none>")
        content = reply.get("content") or {}
        answers.append(f"{action}:{content.get('choice', '')}")
    return {
        "content": [{"type": "text", "text": " ".join(answers)}],
        "isError": False,
    }


def main():
    state = {"req_id": 9000}
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        rid = msg.get("id")
        if rid is None:
            continue
        if method == "initialize":
            result(rid, {
                "protocolVersion": PROTOCOL,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-eliciting", "version": "0.1.0"},
            })
        elif method == "ping":
            result(rid, {})
        elif method == "tools/list":
            result(rid, {"tools": TOOLS})
        elif method == "tools/call":
            params = msg.get("params") or {}
            if params.get("name") == "ask":
                result(rid, call_ask(params.get("arguments") or {}, state))
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": {"code": -32601, "message": "unknown tool"},
                })
        else:
            send({
                "jsonrpc": "2.0",
                "id": rid,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            })


if __name__ == "__main__":
    main()
