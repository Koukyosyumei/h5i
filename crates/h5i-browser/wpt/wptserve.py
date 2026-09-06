#!/usr/bin/env python3
"""Run the tests against WPT's own server instead of ours.

roadmap-history.md §B19.3, item 10. `serve.py` is a static server, and §B12.1
chose that deliberately: it needs no checkout modification, so the WPT tree
stays a pristine `git status` and can be shared with any other runner. That
reasoning still holds and this does not replace it.

What it cannot reach is now the problem. Three subsystems this engine grew
*after* §B12 are only testable on the real server, and all three are the
security-relevant ones:

* **§B17's same-origin policy and CORS.** Every meaningful test needs a second
  origin, which means the `web-platform.test` subdomains, which means a hosts
  file and wptserve.
* **§B16.5's PSL cookies.** Cookie scoping is about `Domain=` across
  registrable boundaries, which is several real hostnames rather than several
  loopback addresses.
* **§B17.4's compression.** `Content-Encoding` behaviour is produced by
  wptserve's Python handlers, not by files on disk.

So today those three have this crate's own unit tests and nothing external, and
the external suite that would exercise them is one server away.

## What this costs, stated plainly

**The checkout stops being pristine while the server runs.** wptserve serves
`resources/testharnessreport.js` off disk, and that file is WPT's own empty
vendor seam — the one `serve.py` fills in flight. Here it has to be written.
So it is written, with the original moved aside, and put back on exit including
on Ctrl-C. `--keep-overlay` leaves it in place for a debugging session, and
`restore()` is idempotent so a crashed run is repaired by the next one.

**HTTPS variants are out of scope, and structurally so.** wptserve serves them
under its own certificate authority, and this engine trusts `webpki-roots` with
no way to add a root — that is the hermetic-build rule in `Cargo.toml`, not an
oversight. So the https tests would fail on a trust decision rather than on
anything they are testing, which is worse than not running them. The http half
still covers the cross-origin CORS suites, cookie scoping across subdomains,
and the `Content-Encoding` handlers, which is what §B19.3 wanted it for.

## Setup, once

    cd ~/Dev/wpt && ./wpt make-hosts-file | sudo tee -a /etc/hosts

Without that, `web-platform.test` does not resolve and every test fails
identically, which this module checks for and refuses to start over.
"""

import os
import re
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import serve  # noqa: E402

# The base wptserve listens on, and the alternate origins the CORS suites use.
# Fixed rather than discovered: they are in WPT's own `config.json` and in the
# hosts file, and a runner that guessed them would disagree with the tests.
HOST = "web-platform.test"
PORT = 8000
BASE = f"http://{HOST}:{PORT}/"

# What the engine has to be granted to reach any of it. The alternate origins
# are what make the cross-origin suites meaningful, so they are granted
# explicitly rather than by a blanket flag: this run is *about* the origin
# boundary, and a policy that allowed everything would be measuring the tests
# with the subject switched off.
GRANTS = [
    f"http://{HOST}:{PORT}",
    f"http://*.{HOST}:{PORT}",
]

OVERLAY = Path("resources") / "testharnessreport.js"
BACKUP = Path("resources") / "testharnessreport.js.h5i-original"


def check_hosts():
    """Refuse to start when the hostnames do not resolve.

    Without them every single test fails the same way, which reads as an engine
    that scores zero rather than as a machine that is not set up.
    """
    for name in (HOST, f"www.{HOST}", f"www1.{HOST}"):
        try:
            socket.getaddrinfo(name, PORT)
        except socket.gaierror:
            sys.exit(
                f"`{name}` does not resolve, so wptserve cannot be reached.\n"
                "Add the WPT hostnames once:\n"
                "    ( cd <wpt> && ./wpt make-hosts-file ) | sudo tee -a /etc/hosts"
            )


def install_overlay(root: Path):
    """Put our reporter where wptserve will serve it, keeping the original.

    Idempotent in the direction that matters: if a backup already exists, a
    previous run died before restoring, and the backup is the real original —
    so it is left alone rather than overwritten with our own overlay.
    """
    target, backup = root / OVERLAY, root / BACKUP
    if not target.exists():
        sys.exit(f"{target} is missing; is {root} a WPT checkout?")
    if not backup.exists():
        shutil.copy2(target, backup)
    target.write_text(serve.REPORTER)
    return target


def restore(root: Path):
    """Put WPT's own file back. Safe to call twice, and on a path that failed."""
    target, backup = root / OVERLAY, root / BACKUP
    if backup.exists():
        shutil.move(str(backup), str(target))


def start(root: Path, ready_timeout=90):
    """Start `./wpt serve` and wait for it to answer.

    Returns the process. The caller stops it with `stop`, which kills the whole
    process group: `./wpt serve` spawns one child per port and killing only the
    parent leaves the ports held, so the next run fails to bind and blames
    itself.
    """
    check_hosts()
    log = HERE / "wptserve.log"
    handle = open(log, "wb")
    process = subprocess.Popen(
        ["./wpt", "serve"],
        cwd=str(root),
        stdout=handle,
        stderr=subprocess.STDOUT,
        # Its own group, so `stop` can take the children with it.
        start_new_session=True,
    )

    deadline = time.time() + ready_timeout
    while time.time() < deadline:
        if process.poll() is not None:
            sys.exit(
                f"wptserve exited immediately ({process.returncode}). See {log}.\n"
                "The usual cause is a leftover server holding the ports: "
                "`pkill -f 'wpt serve'`."
            )
        try:
            with socket.create_connection((HOST, PORT), timeout=2):
                print(f"wptserve is up on {BASE} (log: {log})", flush=True)
                return process
        except OSError:
            time.sleep(1)

    stop(process)
    sys.exit(f"wptserve did not answer within {ready_timeout}s. See {log}.")


def stop(process):
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(process.pid), 15)
        process.wait(timeout=15)
    except Exception:
        try:
            os.killpg(os.getpgid(process.pid), 9)
        except Exception:
            pass


# Tests wptserve builds that this runner still cannot score, and why.
#
# `.https.` and `.serviceworker.` need the WPT certificate authority, which the
# engine cannot be told to trust (see the module header). Skipped by name rather
# than run-and-failed, so a trust decision never lands in the score as if it
# were a conformance result.
UNREACHABLE = re.compile(r"\.https\.|\.serviceworker\.|\.h2\.")


def reachable(rel_path: str) -> bool:
    return not UNREACHABLE.search(rel_path)
