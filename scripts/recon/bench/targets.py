#!/usr/bin/env python3
"""Four small targets with declared ground truth, for scoring discovery.

Each target knows which of its paths are real, and serves that list at
`/__truth`. The scorer reads it directly rather than through h5i, so a score is
measured against what the application actually has rather than against what
recon thought it found (docs/design/design-recon.md N15).

The four shapes are the ones that separate a discovery tool from a request
counter:

  plain  a site with honest 404s, links and a form
  soft   the same site answering 200 for every path, in its own template
  bundle endpoints reachable only through a JavaScript file
  quiet  nothing linked, and everything real behind a wordlist

    python3 targets.py <shape> <port>
"""
import http.server
import json
import sys
from urllib.parse import urlparse

SHAPES = {}

SHAPES["plain"] = {
    "/": "<html><body><a href='/one'>1</a><a href='/two?page=1'>2</a>"
         "<form action='/login' method='POST'><input name='user'></form></body></html>",
    "/one": "<html><body><article>one</article></body></html>",
    "/two": "<html><body><article>two</article></body></html>",
    "/login": "<html><body><article>login</article></body></html>",
    "/robots.txt": "User-agent: *\nDisallow: /admin/\n",
    "/admin/": "<html><body><table>admin</table></body></html>",
}

SHAPES["soft"] = dict(SHAPES["plain"])
SHAPES["bundle"] = {
    "/": "<html><body><script src='/app.js'></script></body></html>",
    "/app.js": 'fetch("/api/cart");fetch("/api/orders");const u="/api/item/"+id;',
    "/api/cart": '{"items":[]}',
    "/api/orders": '{"orders":[1,2,3]}',
}
SHAPES["quiet"] = {
    "/": "<html><body><p>nothing to see</p></body></html>",
    "/backup.sql": "-- dump",
    "/config.php.bak": "<?php $db = 'secret';",
    "/admin/": "<html><body><table>admin</table></body></html>",
}


def handler_for(shape):
    pages = SHAPES[shape]
    soft = shape == "soft"

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            path = urlparse(self.path).path
            if path == "/__truth":
                return self.reply(200, json.dumps(sorted(pages)).encode(), "application/json")
            body = pages.get(path)
            if body is not None:
                kind = "text/javascript" if path.endswith(".js") else \
                       "application/json" if body.startswith("{") else "text/html"
                return self.reply(200, body.encode(), kind)
            if soft:
                # 200 with the site's own template, echoing the path so no two
                # answers are the same length.
                return self.reply(
                    200,
                    ("<html><body><div><span>no such page: %s</span></div></body></html>"
                     % path).encode(),
                    "text/html",
                )
            return self.reply(404, b"<html><body><p>not found</p></body></html>", "text/html")

        do_POST = do_GET

        def reply(self, code, body, kind):
            self.send_response(code)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    return Handler


if __name__ == "__main__":
    shape, port = sys.argv[1], int(sys.argv[2])
    http.server.HTTPServer(("127.0.0.1", port), handler_for(shape)).serve_forever()
