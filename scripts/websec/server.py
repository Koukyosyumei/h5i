#!/usr/bin/env python3
"""A deliberately small target for the websec and recon smoke tests.

Four behaviours, each the smallest thing that exercises one verb end to end:
an IDOR the tool should find, an ownership check it should *not* be able to
break, a single-use CSRF token that makes a two-step flow necessary, and a page
with subresources so the request log has something to be narrowed.

The `/site/` tree is recon's half: pages that link to each other, a bundle that
names endpoints, robots.txt and a sitemap, and a directory that answers 200 with
its own "not found" page for everything (docs/design/design-recon.md N11). A
soft 404 is the case triage exists for, so the fixture has to have one.

Not a benchmark. The benchmark the roadmap asks for (docs/design/design-websec.md
W20) is a corpus of real problems; this is the fixture that proves the verbs
work at all.
"""
import http.server
import json
import secrets
import sys
from urllib.parse import parse_qs, urlparse

PORT = int(sys.argv[1])

# Two users, one document each. `/profile` does not check who is asking (the
# IDOR); `/doc` does (the negative case that has to stay negative).
SESSIONS = {"alice-token": "alice", "bob-token": "bob"}
USERS = {
    "1": {"id": 1, "name": "alice", "email": "alice@example.test", "role": "user"},
    "2": {"id": 2, "name": "bob", "email": "bob@example.test", "role": "admin"},
}
DOCS = {"1": {"owner": "alice", "secret": "alice's diary"},
        "2": {"owner": "bob", "secret": "bob's diary"}}
ISSUED = set()
# Coupons, each redeemable once. `/redeem?coupon=x` checks and then acts with a
# gap in between, which is the window a race test is looking for. Per-coupon so
# a warm-up request does not spend the one the burst is aimed at.
COUPONS = {}
CREDIT = 0


class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, code, body, kind="application/json", extra=()):
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        for name, value in extra:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def who(self):
        cookie = self.headers.get("Cookie", "")
        if "session=" not in cookie:
            return None
        return SESSIONS.get(cookie.split("session=")[-1].split(";")[0])

    def do_GET(self):
        parsed = urlparse(self.path)
        query = parse_qs(parsed.query)
        path = parsed.path

        if path == "/login":
            who = query.get("who", ["alice"])[0]
            self.reply(200, b"logged in", "text/plain",
                       [("Set-Cookie", f"session={who}-token; Path=/")])

        elif path == "/profile":
            # No ownership check: whoever asks gets whatever id they name.
            want = query.get("user_id", ["1"])[0]
            found = USERS.get(want)
            self.reply(200 if found else 404,
                       json.dumps(found or {"error": "no such user"}).encode())

        elif path == "/doc":
            who, want = self.who(), query.get("id", ["1"])[0]
            doc = DOCS.get(want)
            if who is None:
                self.reply(401, b'{"error":"who are you"}')
            elif doc is None:
                self.reply(404, b'{"error":"no such doc"}')
            elif doc["owner"] != who:
                self.reply(403, json.dumps({"error": "not yours"}).encode())
            else:
                self.reply(200, json.dumps(doc).encode())

        elif path == "/form":
            token = "tok_" + secrets.token_hex(4)
            ISSUED.add(token)
            self.reply(200, f'<html><body><form>'
                            f'<input name="csrf" value="{token}"></form></body></html>'.encode(),
                       "text/html")

        elif path == "/settings":
            token = self.headers.get("X-CSRF-Token", "")
            if token not in ISSUED:
                self.reply(403, json.dumps({"error": "bad or missing csrf token"}).encode())
            else:
                ISSUED.discard(token)  # single use, like a real one
                self.reply(200, json.dumps(
                    {"ok": True, "role": self.headers.get("X-Role", "user")}).encode())

        elif path == "/slow":
            # A server that decides slowly: the shape a blind, time-based
            # injection produces, and the only thing a timing test can see.
            import time
            time.sleep(0.4 if query.get("wait", ["0"])[0] == "1" else 0)
            self.reply(200, b'{"ok":true}')

        elif path == "/redeem":
            # Check, pause, then act. The pause stands in for the round trip a
            # real one makes to its database. A coupon that may be redeemed once
            # is redeemed as many times as fit in the gap.
            global CREDIT
            import time
            name = query.get("coupon", ["default"])[0]
            if COUPONS.get(name):
                self.reply(409, json.dumps({"error": "already redeemed"}).encode())
            else:
                time.sleep(0.15)
                COUPONS[name] = True
                CREDIT += 10
                self.reply(200, json.dumps({"ok": True, "credit": CREDIT}).encode())

        elif path == "/credit":
            self.reply(200, json.dumps({"credit": CREDIT}).encode())

        elif path == "/page":
            self.reply(200, b'<html><head><link rel="stylesheet" href="/a.css">'
                            b'<script src="/b.js"></script></head><body>hi</body></html>',
                       "text/html")

        elif path == "/missing":
            self.reply(404, b"gone", "text/plain")

        elif path == "/robots.txt":
            self.reply(200, b"User-agent: *\nDisallow: /site/admin/\n"
                            b"Sitemap: /sitemap.xml\n", "text/plain")

        elif path == "/sitemap.xml":
            self.reply(200, b'<?xml version="1.0"?><urlset><url><loc>/site/help</loc>'
                            b'</url></urlset>', "application/xml")

        elif path == "/site/" or path == "/site/index.html":
            self.reply(200, b'<html><body><h1>site</h1>'
                            b'<a href="/site/one">one</a>'
                            b'<a href="/site/two?page=1">two</a>'
                            b'<script src="/site/app.js"></script>'
                            b'<form action="/site/login" method="POST">'
                            b'<input name="user"><input name="pass"></form>'
                            b"</body></html>", "text/html")

        elif path == "/site/app.js":
            self.reply(200, b'fetch("/site/api/cart");\n'
                            b'fetch("/site/api/order", { method: "POST" });\n'
                            b'const u = "/site/api/item/" + id;\n',
                       "text/javascript")

        elif path in ("/site/one", "/site/two", "/site/help"):
            self.reply(200, ("<html><body><article><p>%s</p></article></body></html>"
                             % path).encode(), "text/html")

        elif path.startswith("/site/"):
            # The soft 404: 200, the site's own template, and the path echoed
            # back so no two of them are the same length.
            self.reply(200, ("<html><body><div><span>no such page: %s</span></div>"
                             "</body></html>" % path).encode(), "text/html")

        else:
            self.reply(200, b"x", "text/plain")

    def do_POST(self):
        path = urlparse(self.path).path
        if path != "/upload":
            self.reply(404, b'{"error":"no"}')
            return
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        kind = self.headers.get("Content-Type", "")
        if "multipart/form-data" not in kind or "boundary=" not in kind:
            self.reply(400, json.dumps({"error": "expected multipart"}).encode())
            return
        # A filter that reads the declared type and trusts it, and a store that
        # uses the filename as given: the pair that makes an upload interesting.
        declared = ""
        filename = ""
        for chunk in body.split(b"--" + kind.split("boundary=")[-1].strip().encode()):
            head = chunk.split(b"\r\n\r\n")[0].decode("utf-8", "replace")
            if 'filename="' in head:
                filename = head.split('filename="')[1].split('"')[0]
                for line in head.splitlines():
                    if line.lower().startswith("content-type:"):
                        declared = line.split(":", 1)[1].strip()
        if not filename:
            self.reply(400, json.dumps({"error": "no file part"}).encode())
        elif not declared.startswith("image/"):
            self.reply(415, json.dumps({"error": f"type {declared!r} not allowed"}).encode())
        else:
            self.reply(200, json.dumps({"stored_as": filename, "declared": declared}).encode())

    def log_message(self, *args):
        pass


# Threaded on purpose. A single-threaded server serialises every request, which
# would close the check-then-act window above by construction and make a race
# test look like a race that does not exist.
http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
