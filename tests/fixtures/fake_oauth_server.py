#!/usr/bin/env python3
"""Fake OAuth provider for dmcp's sign-in tests: device flow, refresh, identity.

Binds 127.0.0.1 on a free port and prints the port as its first stdout line.
Every request is appended as a JSON line to the file named by the first
argument, so a test can assert on exactly what dmcp sent (client id, scopes,
grant types) — and that it sent nothing it should not have.

  POST /device   device authorization: a fixed device_code and user_code
  POST /token    device_code grant: `authorization_pending` on the first poll
                 (as HTTP 400, per RFC 8628), then per the mode:
                   ok      a token for scope "repo"
                   deny    access_denied
                 refresh_token grant: a fresh access token, no new refresh token
  GET  /user     the account for a known bearer token, else 401

The mode is the second argument (default "ok"). Stdlib only.
"""

import json
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = sys.argv[1]
MODE = sys.argv[2] if len(sys.argv) > 2 else "ok"
TOKENS = {"gho_fake_device_token": "octocat", "gho_fake_refreshed_token": "octocat"}
POLLS = {"count": 0}


def log(entry):
    with open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        form = dict(urllib.parse.parse_qsl(self.rfile.read(length).decode()))
        log({"method": "POST", "path": self.path, "form": form})
        port = self.server.server_address[1]
        if self.path == "/device":
            self.reply(
                200,
                {
                    "device_code": "dev-code-1",
                    "user_code": "WDJB-MJHT",
                    "verification_uri": f"http://127.0.0.1:{port}/activate",
                    "expires_in": 60,
                    "interval": 1,
                },
            )
        elif self.path == "/token" and form.get("grant_type") == "refresh_token":
            self.reply(200, {"access_token": "gho_fake_refreshed_token", "expires_in": 3600})
        elif self.path == "/token":
            POLLS["count"] += 1
            if POLLS["count"] == 1:
                self.reply(400, {"error": "authorization_pending"})
            elif MODE == "deny":
                self.reply(400, {"error": "access_denied"})
            else:
                self.reply(
                    200,
                    {"access_token": "gho_fake_device_token", "token_type": "bearer", "scope": "repo"},
                )
        else:
            self.reply(404, {"error": "not_found"})

    def do_GET(self):
        auth = self.headers.get("Authorization", "")
        log({"method": "GET", "path": self.path, "auth": auth[:7]})
        token = auth[len("Bearer "):] if auth.startswith("Bearer ") else ""
        if self.path == "/user" and token in TOKENS:
            self.reply(200, {"login": TOKENS[token], "id": 1})
        else:
            self.reply(401, {"message": "Bad credentials"})


server = HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
