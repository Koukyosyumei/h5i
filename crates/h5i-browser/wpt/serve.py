#!/usr/bin/env python3
"""Serve a Web Platform Tests checkout, with the vendor reporter hook filled in.

WPT ships `resources/testharnessreport.js` as an empty seam for exactly this:
a vendor drops in code that collects results when a file finishes. We serve our
own rather than writing into the checkout, so the checkout stays a pristine
`git status` and can be shared with any other runner.

The results come back out through the console, because that is a channel the
engine already has and `open --json` already reports. Nothing new is added to
the engine to be measured, which matters: an instrument that requires the
subject to grow a port for it is measuring something other than the subject.
"""

import http.server
import os
import posixpath
import re
import socketserver
import sys
import threading
import uuid as uuid_module

WPT_ROOT = os.environ.get("WPT_ROOT", os.path.expanduser("~/Dev/wpt"))

# The marker is deliberately long and unlikely: console output is page-
# controlled, and a page that printed our marker could otherwise report its own
# score. Tests are trusted here, but the runner should not be forgeable by
# accident either.
MARKER = "H5I-WPT-RESULT-6a7f2c1b"

REPORTER = (
    """
// Do not build the results table.
setup({ output: false });

add_completion_callback(function (tests, status) {
  var out = {
    status: status.status,
    message: status.message,
    tests: []
  };
  for (var i = 0; i < tests.length; i++) {
    out.tests.push({
      name: tests[i].name,
      status: tests[i].status,
      message: tests[i].message
    });
  }
  console.log("%s" + JSON.stringify(out));
});
"""
    % MARKER
)


# ── wptserve substitution ───────────────────────────────────────────────────
#
# wptserve rewrites `{{...}}` placeholders in any file whose name contains
# `.sub.`, and in anything served through `?pipe=sub`. 2,424 files use it, and
# without it their URLs come out as literal `http://{{domains[www1]}}:NaN/...` —
# which is how a page ends up asking blitz to load an image from a host that
# cannot be parsed.
#
# **The domains get distinct loopback addresses on purpose.** 127.0.0.0/8 is all
# loopback, so 127.0.0.2 is reachable without configuration and is a *different
# origin* from 127.0.0.1 — same-origin policy keys on host, not on whether the
# host is local. Collapsing every domain to one address would have made every
# cross-origin test silently same-origin, which is a worse answer than failing:
# the test would pass while testing nothing.
#
# What is still missing is named honestly rather than faked: there is no TLS, so
# `{{ports[https][0]}}` resolves to the HTTP port and a test that checks
# `location.protocol` will fail; and the `.py` handlers are not executed.
DOMAINS = {
    "": "127.0.0.1",
    "www": "127.0.0.2",
    "www1": "127.0.0.3",
    "www2": "127.0.0.4",
    "xn--n8j6ds53lwwkrqhv28a": "127.0.0.5",
    "xn--lve-6lad": "127.0.0.6",
}
# The "alt" domain set, which tests use when they need an origin that is
# definitely not this one.
ALT_DOMAINS = {key: f"127.0.1.{index + 1}" for index, key in enumerate(DOMAINS)}

SUBSTITUTION = re.compile(r"\{\{([^}]+)\}\}")


def guess_type(path):
    """Content type from the extension, ignoring the `.sub.` infix."""
    if path.endswith((".html", ".htm", ".xhtml", ".xht")):
        return "text/html; charset=utf-8"
    if path.endswith(".js"):
        return "text/javascript; charset=utf-8"
    if path.endswith(".css"):
        return "text/css; charset=utf-8"
    if path.endswith(".json"):
        return "application/json"
    return "text/plain; charset=utf-8"


def substitute(text, port, query=""):
    """Apply wptserve's `{{...}}` substitutions."""
    from urllib.parse import parse_qs

    params = parse_qs(query)

    def replace(match):
        token = match.group(1).strip()
        if token in ("uuid()", "uuid"):
            return str(uuid_module.uuid4())
        if token == "host":
            return DOMAINS[""]
        indexed = re.match(r"([a-z_]+)\[([^\]]*)\](?:\[([^\]]*)\])?", token)
        if indexed:
            name, first, second = indexed.group(1), indexed.group(2), indexed.group(3)
            if name == "domains":
                return DOMAINS.get(first, DOMAINS[""])
            if name == "hosts":
                table = ALT_DOMAINS if first == "alt" else DOMAINS
                return table.get(second or "", table[""])
            if name == "ports":
                # One server, one port. Tests that need a *second* port to make a
                # second origin get a different loopback address instead.
                return str(port)
            if name == "location":
                return {
                    "host": f"{DOMAINS['']}:{port}", "hostname": DOMAINS[""],
                    "port": str(port), "scheme": "http", "protocol": "http:",
                }.get(first, "")
            if name == "GET":
                values = params.get(first)
                return values[0] if values else ""
        # Anything unrecognised is left as it stands rather than blanked: a
        # visible `{{whatever}}` in the output says which substitution is
        # missing, where an empty string would just be a broken URL.
        return match.group(0)

    return SUBSTITUTION.sub(replace, text)


def parse_pipes(query):
    """The `?pipe=` directives this server understands."""
    from urllib.parse import parse_qs

    directives = []
    for chunk in parse_qs(query).get("pipe", []):
        for piece in chunk.split("|"):
            call = re.match(r"([a-z_]+)(?:\((.*)\))?$", piece.strip())
            if call:
                directives.append((call.group(1), call.group(2) or ""))
    return directives


# ── generated endpoints ─────────────────────────────────────────────────────
#
# WPT keeps a large share of its tests as bare JavaScript and builds the HTML
# around them at serve time: `x.any.js` is served as `x.any.html`,
# `x.any.worker.html` and more, and none of those files exist on disk. Skipping
# them left 3,083 files — and the several thousand subtests inside them —
# outside every measurement this harness produced.
#
# Only the *window* wrapper is built here. The worker variants need Workers,
# which this engine does not have, and inventing an HTML page that pretends to
# be a worker scope would produce failures that blame the engine for the
# harness's fiction.

META = re.compile(r"^//\s*META:\s*([a-z]+)=(.*)$")


def directives(source):
    """The `// META:` lines at the top of a WPT script, as a list of pairs.

    They stop at the first line that is not a META comment, which is what
    wptserve does — a `// META:` further down is a comment, not a directive.
    """
    found = []
    for line in source.splitlines():
        match = META.match(line.strip())
        if not match:
            if line.strip().startswith("//") or not line.strip():
                continue
            break
        found.append((match.group(1), match.group(2).strip()))
    return found


def runs_in_window(source):
    """Whether this test has a window variant at all.

    `// META: global=worker` means exactly that, and building a window page for
    it would score a test the author never wrote.
    """
    for key, value in directives(source):
        if key == "global":
            scopes = {scope.strip() for scope in value.split(",")}
            return bool(scopes & {"window", "!dedicatedworker", "!worker"}) or not scopes
    return True


def wrapper_for(js_path: str, source: str) -> str:
    """The HTML wptserve would have generated for this script."""
    title = ""
    scripts = []
    for key, value in directives(source):
        if key == "title":
            title = value
        elif key == "script":
            scripts.append(value)

    base = posixpath.dirname(js_path)
    tags = []
    for script in scripts:
        src = script if script.startswith("/") else posixpath.normpath(posixpath.join(base, script))
        tags.append(f'<script src="{src}"></script>')

    return (
        "<!doctype html>\n<meta charset=utf-8>\n"
        f"<title>{title}</title>\n"
        '<script src="/resources/testharness.js"></script>\n'
        '<script src="/resources/testharnessreport.js"></script>\n'
        + "\n".join(tags)
        + '\n<div id="log"></div>\n'
        f'<script src="{js_path}"></script>\n'
    )


def generated_source(root: str, path: str):
    """The `.js` behind a generated `.html` endpoint, or None if there is none."""
    for suffix in (".any.html", ".window.html"):
        if not path.endswith(suffix):
            continue
        js_path = path[: -len(".html")] + ".js"
        on_disk = os.path.join(root, js_path.lstrip("/"))
        if not os.path.isfile(on_disk):
            return None
        try:
            with open(on_disk, encoding="utf8", errors="replace") as handle:
                source = handle.read()
        except OSError:
            return None
        if not runs_in_window(source):
            return None
        return js_path, source
    return None


# The *second* empty vendor seam, and the reason it is worth filling.
TESTDRIVER = """
(function () {
  function fire(element, type, init) {
    var Ctor = window.MouseEvent || window.Event;
    var event = type.indexOf('key') === 0
      ? new (window.KeyboardEvent || window.Event)(type, init)
      : new Ctor(type, init);
    element.dispatchEvent(event);
    return event;
  }

  function refuse(name) {
    return Promise.reject(new Error(
      name + '() needs automation authority this engine does not have, and is ' +
      'refused rather than approximated (h5i testdriver-vendor shim).'));
  }

  window.test_driver_internal = Object.assign(window.test_driver_internal || {}, {
    // Says the harness is driving, which several tests branch on.
    in_automation: true,

    async click(element) {
      if (!element) throw new Error('click: no element');
      // A testdriver click *is* the user gesture the test asked for, so it
      // arms transient user activation the way a real click would — this is
      // what makes `test_driver.bless()` mean something here.
      if (window.__h5iNoteUserActivation) window.__h5iNoteUserActivation();
      // Scroll-into-view and hit-testing are what a real driver does first.
      // There is nothing here that can be occluded, so the click is delivered
      // to the element the test named — which is what the test is asserting on.
      var init = { bubbles: true, cancelable: true, composed: true, detail: 1 };
      fire(element, 'pointerdown', init);
      fire(element, 'mousedown', init);
      fire(element, 'pointerup', init);
      fire(element, 'mouseup', init);
      if (typeof element.click === 'function') element.click();
      else fire(element, 'click', init);
      return null;
    },

    async send_keys(element, keys) {
      if (!element) throw new Error('send_keys: no element');
      if (typeof element.focus === 'function') element.focus();
      var text = String(keys);
      for (var i = 0; i < text.length; i++) {
        var key = text[i];
        var init = { bubbles: true, cancelable: true, key: key, composed: true };
        fire(element, 'keydown', init);
        fire(element, 'keypress', init);
        if ('value' in element) element.value = (element.value || '') + key;
        fire(element, 'input', { bubbles: true, composed: true });
        fire(element, 'keyup', init);
      }
      return null;
    },

    // A driver's "release everything held". Nothing is held here, so this is
    // honestly a no-op rather than a refusal: the postcondition is already met.
    async release_actions() { return null; },

    /// The WebDriver action sequence, performed rather than approximated.
    async action_sequence(sources) {
      if (!Array.isArray(sources)) return refuse('action_sequence');
      // **Pointer state outlives the call**, because a real pointer does.
      // A test that moves and presses in one `send()` and releases in the
      // next used to lose its press target between them — no synthesized
      // click — and reset to (0,0), so a following `{origin: "pointer"}`
      // move landed in the corner. It also makes `release_actions()`
      // honest: the no-op is only correct if there is state to have none of.
      var pointer = window.__h5iPointerState ||
        (window.__h5iPointerState = { x: 0, y: 0, target: null, down: false, downTarget: null });

      function resolve(action) {
        var origin = action.origin;
        var x = Number(action.x) || 0;
        var y = Number(action.y) || 0;
        if (origin === 'pointer') return { x: pointer.x + x, y: pointer.y + y };
        if (origin && typeof origin === 'object' && origin.getBoundingClientRect) {
          // WebDriver measures from the element's **centre**, not its corner.
          var box = origin.getBoundingClientRect();
          return { x: box.left + box.width / 2 + x, y: box.top + box.height / 2 + y };
        }
        return { x: x, y: y };
      }

      function at(x, y) {
        return document.elementFromPoint(x, y) || document.body ||
          document.documentElement;
      }

      function pointerInit(extra) {
        // `button` is the one the action named. Hardcoding 0 delivered a
        // right-button press as a left one, and then synthesized a `click`
        // from it — a context-menu test would have reported an engine failure
        // for something this shim did.
        var button = pointer.button || 0;
        var init = {
          bubbles: true, cancelable: true, composed: true, detail: 1,
          clientX: pointer.x, clientY: pointer.y,
          screenX: pointer.x, screenY: pointer.y,
          button: button, buttons: pointer.down ? (1 << button) : 0,
        };
        for (var k in (extra || {})) init[k] = extra[k];
        return init;
      }

      function perform(source, action) {
        var kind = action.type;
        if (kind === 'pause') return;
        if (source.type === 'key') {
          if (kind !== 'keyDown' && kind !== 'keyUp') {
            throw new Error('action_sequence: key action `' + kind + '` is not implemented');
          }
          var target = document.activeElement || document.body;
          fire(target, kind === 'keyDown' ? 'keydown' : 'keyup',
               { bubbles: true, cancelable: true, composed: true, key: action.value });
          return;
        }
        if (source.type !== 'pointer') {
          if (source.type === 'none') return;
          throw new Error('action_sequence: source `' + source.type + '` is not implemented');
        }
        if (kind === 'pointerMove') {
          var to = resolve(action);
          pointer.x = to.x; pointer.y = to.y;
          var over = at(pointer.x, pointer.y);
          if (over !== pointer.target) {
            if (pointer.target) fire(pointer.target, 'pointerout', pointerInit());
            pointer.target = over;
            if (over) fire(over, 'pointerover', pointerInit());
          }
          if (over) { fire(over, 'pointermove', pointerInit()); fire(over, 'mousemove', pointerInit()); }
          return;
        }
        var el = pointer.target || at(pointer.x, pointer.y);
        if (kind === 'pointerDown') {
          pointer.down = true;
          pointer.button = Number(action.button) || 0;
          pointer.downTarget = el;
          if (window.__h5iNoteUserActivation) window.__h5iNoteUserActivation();
          if (el) { fire(el, 'pointerdown', pointerInit()); fire(el, 'mousedown', pointerInit()); }
          return;
        }
        if (kind === 'pointerUp') {
          pointer.down = false;
          if (el) { fire(el, 'pointerup', pointerInit()); fire(el, 'mouseup', pointerInit()); }
          // A down and an up on the same element is a click, which is what
          // every light-dismiss test is really performing.
          // Only the primary button makes a `click`; a right-button release
          // is a `contextmenu` gesture and must not be turned into one.
          if (el && el === pointer.downTarget && (pointer.button || 0) === 0) {
            if (typeof el.click === 'function') el.click();
            else fire(el, 'click', pointerInit());
          }
          pointer.downTarget = null;
          pointer.button = 0;
          return;
        }
        throw new Error('action_sequence: pointer action `' + kind + '` is not implemented');
      }

      var ticks = 0;
      for (var s = 0; s < sources.length; s++) {
        var list = (sources[s] && sources[s].actions) || [];
        if (list.length > ticks) ticks = list.length;
      }
      for (var t = 0; t < ticks; t++) {
        for (var i = 0; i < sources.length; i++) {
          var source = sources[i];
          var action = source && source.actions && source.actions[t];
          if (action) perform(source, action);
        }
      }
      return null;
    },
    async set_permission() { return refuse('set_permission'); },
    async get_computed_role() { return refuse('get_computed_role'); },
    async get_computed_label() { return refuse('get_computed_label'); },
    async delete_all_cookies() { return refuse('delete_all_cookies'); },
    async get_named_cookie() { return refuse('get_named_cookie'); },
    async minimize_window() { return refuse('minimize_window'); },
    async set_window_rect() { return refuse('set_window_rect'); },
    async add_virtual_authenticator() { return refuse('add_virtual_authenticator'); },
    async create_virtual_sensor() { return refuse('create_virtual_sensor'); },
  });
})();
"""


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=WPT_ROOT, **kw)

    def do_GET(self):
        path = self.path.split("?")[0].split("#")[0]
        query = self.path.split("?", 1)[1] if "?" in self.path else ""
        pipes = parse_pipes(query)

        # A file whose name carries `.sub.`, or anything asked for through
        # `?pipe=sub`, is served with its placeholders resolved. `header()` and
        # `status()` are the other two directives that appear often enough to
        # matter; `trickle` and `gzip` are about *how* bytes arrive rather than
        # what they are, and are ignored rather than approximated.
        wants_sub = ".sub." in path or any(name == "sub" for name, _ in pipes)
        if wants_sub or pipes:
            on_disk = os.path.join(WPT_ROOT, path.lstrip("/"))
            if os.path.isfile(on_disk):
                try:
                    with open(on_disk, encoding="utf8", errors="replace") as handle:
                        text = handle.read()
                except OSError:
                    text = None
                if text is not None:
                    if wants_sub:
                        text = substitute(text, self.server.server_address[1], query)
                    body = text.encode()
                    status = 200
                    extra = []
                    for name, argument in pipes:
                        if name == "status":
                            try:
                                status = int(argument.strip())
                            except ValueError:
                                pass
                        elif name == "header":
                            parts = argument.split(",", 1)
                            if len(parts) == 2:
                                extra.append((parts[0].strip(), parts[1].strip()))
                    self.send_response(status)
                    self.send_header("Content-Type", guess_type(path))
                    self.send_header("Content-Length", str(len(body)))
                    for name, value in extra:
                        self.send_header(name, value)
                    self.end_headers()
                    self.wfile.write(body)
                    return

        built = generated_source(WPT_ROOT, path)
        if built is not None:
            js_path, source = built
            body = wrapper_for(js_path, source).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        # `resources/WebIDLParser.js` is a build artifact, not a checked-in
        # file: `webidl2/build.sh` copies the bundle there. A checkout that
        # never ran it 404s, and the 211 `idlharness` endpoints across WPT then
        # hang on a script that will never arrive and report a timeout that says
        # nothing about this engine. Served from the bundle that is present,
        # which is exactly what the build would have produced.
        if path == "/resources/WebIDLParser.js":
            bundle = os.path.join(WPT_ROOT, "resources", "webidl2", "lib", "webidl2.js")
            if os.path.isfile(bundle):
                with open(bundle, "rb") as handle:
                    body = handle.read()
                self.send_response(200)
                self.send_header("Content-Type", "text/javascript")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return

        if path == "/resources/testdriver-vendor.js":
            body = TESTDRIVER.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/javascript")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path == "/resources/testharnessreport.js":
            body = REPORTER.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/javascript")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        return super().do_GET()

    def log_message(self, *a):
        pass


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def start(port=0):
    """Start the server on a background thread. Returns the bound port.

    Bound to every address rather than to 127.0.0.1 alone, because the
    substitution above hands out 127.0.0.2 and friends as distinct origins and
    they have to actually answer.
    """
    httpd = Server(("0.0.0.0", port), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    return httpd, httpd.server_address[1]


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
    httpd, port = start(port)
    print(f"serving {WPT_ROOT} on http://127.0.0.1:{port} "
          f"(and 127.0.0.2-6 as separate origins)", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass
