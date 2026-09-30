#!/usr/bin/env python3
"""Fake hosted MCP server with its own OAuth, per the MCP authorization spec.

One process plays both roles on a loopback port, the way mcp.notion.com does:

  /mcp                                      Streamable HTTP MCP endpoint. Without a
                                            valid bearer: 401 + WWW-Authenticate
                                            naming the resource metadata.
  /.well-known/oauth-protected-resource/mcp RFC 9728: the authorization server
  /.well-known/oauth-authorization-server   RFC 8414: the endpoints
  POST /register                            RFC 7591 dynamic client registration
  GET  /authorize                           the "consent page": redirects back with
                                            a code (or access_denied)
  POST /token                               authorization_code (PKCE S256 checked,
                                            resource checked) and refresh_token

The checks are real, so a client that skips PKCE, sends the wrong verifier,
drops `resource`, or redirects somewhere it did not register fails here.
Every request is appended to the log file (first argument) as a JSON line.
Mode (second argument): "ok", "deny" (consent refused), or "confidential"
(registration issues a client secret the refresh must present).

Prints the port as its first stdout line. Stdlib only.
"""

import base64
import hashlib
import json
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[1]
MODE = sys.argv[2] if len(sys.argv) > 2 else "ok"
CLIENTS = {}  # client_id -> {"redirect_uris": [...], "secret": str|None}
CODES = {}  # code -> {"challenge", "redirect_uri", "client_id", "resource"}
TOKENS = {"at-1", "at-2"}
ISSUED = {"count": 0}


def log(entry):
    with open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    @property
    def base(self):
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def reply(self, status, body=None, headers=None):
        data = json.dumps(body).encode() if body is not None else b""
        self.send_response(status)
        if body is not None:
            self.send_header("Content-Type", "application/json")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length).decode() if length else ""

    def authorized(self):
        auth = self.headers.get("Authorization", "")
        return auth.startswith("Bearer ") and auth[len("Bearer "):] in TOKENS

    def challenge(self):
        meta = f"{self.base}/.well-known/oauth-protected-resource/mcp"
        self.reply(401, {"error": "invalid_token"}, {"WWW-Authenticate": f'Bearer resource_metadata="{meta}"'})

    # -- GET -------------------------------------------------------------
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        log({"method": "GET", "path": url.path, "query": query})
        if url.path.startswith("/.well-known/oauth-protected-resource"):
            self.reply(200, {"resource": f"{self.base}/mcp", "authorization_servers": [self.base], "scopes_supported": ["read"]})
        elif url.path.startswith("/.well-known/oauth-authorization-server"):
            # Shaped like mcp.notion.com's: no response_types_supported.
            self.reply(200, {
                "issuer": self.base,
                "authorization_endpoint": f"{self.base}/authorize",
                "token_endpoint": f"{self.base}/token",
                "registration_endpoint": f"{self.base}/register",
                "code_challenge_methods_supported": ["S256"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
                "scopes_supported": ["read"],
            })
        elif url.path == "/authorize":
            self.authorize(query)
        elif url.path == "/mcp":
            if not self.authorized():
                return self.challenge()
            self.reply(405, {"error": "no standalone stream"})
        else:
            self.reply(404, {"error": "not_found"})

    def authorize(self, q):
        client = CLIENTS.get(q.get("client_id"))
        redirect = q.get("redirect_uri", "")
        if client is None or redirect not in client["redirect_uris"]:
            return self.reply(400, {"error": "invalid_request", "detail": "unregistered client or redirect"})
        if q.get("code_challenge_method") != "S256" or not q.get("code_challenge"):
            return self.reply(400, {"error": "invalid_request", "detail": "PKCE S256 required"})
        if q.get("resource") != f"{self.base}/mcp":
            return self.reply(400, {"error": "invalid_target", "detail": q.get("resource")})
        state = urllib.parse.quote(q.get("state", ""))
        if MODE == "deny":
            location = f"{redirect}?error=access_denied&state={state}"
        else:
            ISSUED["count"] += 1
            code = f"code-{ISSUED['count']}"
            CODES[code] = {"challenge": q["code_challenge"], "redirect_uri": redirect, "client_id": q["client_id"]}
            location = f"{redirect}?code={code}&state={state}"
        self.reply(302, None, {"Location": location})

    # -- POST ------------------------------------------------------------
    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        raw = self.body()
        if url.path == "/register":
            req = json.loads(raw or "{}")
            log({"method": "POST", "path": "/register", "json": req})
            client_id = f"client-{len(CLIENTS) + 1}"
            secret = "cs-1" if MODE == "confidential" else None
            CLIENTS[client_id] = {"redirect_uris": req.get("redirect_uris", []), "secret": secret}
            resp = {"client_id": client_id, "redirect_uris": req.get("redirect_uris", []),
                    "token_endpoint_auth_method": "client_secret_post" if secret else "none"}
            if secret:
                resp["client_secret"] = secret
            return self.reply(201, resp)
        if url.path == "/token":
            form = dict(urllib.parse.parse_qsl(raw))
            log({"method": "POST", "path": "/token", "form": form})
            return self.token(form)
        if url.path == "/mcp":
            if not self.authorized():
                log({"method": "POST", "path": "/mcp", "authorized": False})
                return self.challenge()
            msg = json.loads(raw)
            log({"method": "POST", "path": "/mcp", "rpc": msg.get("method"), "token": self.headers["Authorization"][7:]})
            return self.rpc(msg)
        self.reply(404, {"error": "not_found"})

    def do_DELETE(self):
        self.reply(200, {})

    def token(self, form):
        # client_secret_basic as well as _post: an OAuth library picks one.
        auth = self.headers.get("Authorization", "")
        if auth.startswith("Basic "):
            user, _, password = base64.b64decode(auth[6:]).decode().partition(":")
            form.setdefault("client_id", urllib.parse.unquote(user))
            form.setdefault("client_secret", urllib.parse.unquote(password))
        client = CLIENTS.get(form.get("client_id"))
        if client is None:
            return self.reply(401, {"error": "invalid_client"})
        if client["secret"] and form.get("client_secret") != client["secret"]:
            return self.reply(401, {"error": "invalid_client", "error_description": "client secret"})
        if form.get("resource") != f"{self.base}/mcp":
            return self.reply(400, {"error": "invalid_target"})
        grant = form.get("grant_type")
        if grant == "authorization_code":
            pending = CODES.pop(form.get("code", ""), None)
            if pending is None or pending["redirect_uri"] != form.get("redirect_uri"):
                return self.reply(400, {"error": "invalid_grant"})
            digest = hashlib.sha256(form.get("code_verifier", "").encode()).digest()
            if base64.urlsafe_b64encode(digest).rstrip(b"=").decode() != pending["challenge"]:
                return self.reply(400, {"error": "invalid_grant", "error_description": "PKCE"})
            return self.reply(200, {"access_token": "at-1", "token_type": "Bearer", "expires_in": 3600,
                                    "refresh_token": "rt-1", "scope": "read"})
        if grant == "refresh_token" and form.get("refresh_token") == "rt-1":
            return self.reply(200, {"access_token": "at-2", "token_type": "Bearer", "expires_in": 3600})
        self.reply(400, {"error": "invalid_grant"})

    def rpc(self, msg):
        method, rid = msg.get("method"), msg.get("id")
        if rid is None:
            return self.reply(202)
        if method == "initialize":
            result = {"protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                      "capabilities": {"tools": {}}, "serverInfo": {"name": "fake-hosted", "version": "0.1.0"}}
            return self.reply(200, {"jsonrpc": "2.0", "id": rid, "result": result}, {"Mcp-Session-Id": "s-1"})
        if method == "tools/list":
            tools = [{"name": "whoami", "description": "Which token called", "inputSchema": {"type": "object"}}]
            return self.reply(200, {"jsonrpc": "2.0", "id": rid, "result": {"tools": tools}})
        if method == "tools/call":
            token = self.headers["Authorization"][7:]
            return self.reply(200, {"jsonrpc": "2.0", "id": rid,
                                    "result": {"content": [{"type": "text", "text": f"called with {token}"}]}})
        self.reply(200, {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": method}})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
