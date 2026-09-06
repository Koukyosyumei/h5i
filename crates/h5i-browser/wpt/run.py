#!/usr/bin/env python3
"""Run the Web Platform Tests against this engine and report what happened.

Usage:
    python3 wpt/run.py --dirs dom css/cssom --jobs 8
    python3 wpt/run.py --all --out wpt/results/baseline.json

Counting, and why it is done this way
-------------------------------------
A WPT file contains many `test()` calls, each one a *subtest*. The number
vendors quote is subtests, and that is what this reports as the headline.

The thing this instrument refuses to do is let an unmeasured file look like a
measured one. A file can end in six distinguishable ways and they are kept
apart, because §8.3 of the roadmap was written after an instrument that could
not tell "nothing is wrong" from "I cannot see":

  ok             the harness ran and reported. Its subtests are real data.
  harness_error  the harness ran, reported, and said the file itself errored.
  harness_timeout the harness ran, reported, and said it timed out internally.
  no_report      the engine exited cleanly and the harness never reported.
                 This is *unmeasured*, not zero passes.
  engine_timeout the engine did not exit. Unmeasured.
  engine_crash   the engine died. Unmeasured.

Only the first three contribute subtests. `no_report` is the interesting
bucket: it is where an engine gap stops a file before it can even say what it
failed. Chasing that bucket down is how the pass count goes up in steps rather
than in ones.

What this cannot reach, stated up front
---------------------------------------
WPT generates a large share of its endpoints at serve time: `x.any.js` becomes
`x.any.html`, `x.any.worker.html` and more, none of which exist on disk. A
static server cannot serve them, so they are outside this run entirely. The
summary prints how many such files were skipped so the denominator is never
mistaken for "all of WPT".
"""

import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import sys

import time
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import serve  # noqa: E402

HERE = Path(__file__).resolve().parent
CRATE = HERE.parent
REPO = CRATE.parent.parent
# The engine, as the shipping binary reaches it.
#
# It used to be `target/release/h5i-browser`, and that path is a trap now:
# the crate became a library when the engine moved inside `h5i __engine`, so
# nothing rebuilds that file — but a stale copy from the last release that did
# build it stays on disk, executable, and happily answers every request. The
# harness went on scoring it for eight days without a word, because "the binary
# is there" was the only question anyone asked.
BINARY = REPO / "target" / "release" / "h5i"
ENGINE_ARGS = ["__engine"]

# Directories that hold machinery rather than tests. Running them produces
# noise that looks like failure and is not.
SKIP_DIRS = {
    "resources", "support", "tools", "common", "conformance-checkers",
    "docs", "interfaces", "fonts", "images", "media", "css/reference",
    "infrastructure", ".git", ".github", "webdriver", "wasm",
}

# Files that are not tests a harness can score: reference renderings for
# reftests, and tests that need a human.
SKIP_FILE = re.compile(r"(-ref|-notref|-manual|\.tentative\.tentative)\.x?html?$")

MARKER = serve.MARKER

# How many failing subtests a result file keeps, per test file.
MAX_FAILURES = int(os.environ.get("WPT_MAX_FAILURES", "5"))

# testharness.js status codes.
SUBTEST_STATUS = {0: "PASS", 1: "FAIL", 2: "TIMEOUT", 3: "NOTRUN", 4: "PRECONDITION_FAILED"}
HARNESS_STATUS = {0: "OK", 1: "ERROR", 2: "TIMEOUT", 3: "PRECONDITION_FAILED"}


def find_tests(root: Path, dirs, limit=None):
    """Every on-disk HTML test under `dirs`, plus counts of what was left out.

    Returns (tests, generated, unscoreable). A file that never loads
    testharness.js cannot report a result no matter how well the engine runs it
    — reftests compare renderings, crashtests only have to not crash — so
    counting them as engine failures would inflate the unmeasured bucket with
    files that were never ours to pass. They are counted and named instead.
    """
    tests, generated, unscoreable, needs_server = [], 0, 0, 0
    roots = [root / d for d in dirs] if dirs else [root]
    for base in roots:
        if not base.exists():
            print(f"warning: {base} does not exist, skipping", file=sys.stderr)
            continue
        for path in sorted(base.rglob("*")):
            rel = path.relative_to(root)
            parts = set(rel.parts[:-1])
            if parts & SKIP_DIRS or any(p.startswith(".") for p in rel.parts):
                continue
            name = path.name
            # `x.any.js` and `x.window.js` have no HTML on disk: wptserve builds
            # a page around them. `serve.py` builds the *window* page, so those
            # two are tests here and are named by the endpoint the server will
            # answer. The worker variants stay out — this engine has no Workers,
            # and a page pretending to be a worker scope would produce failures
            # that blame the engine for the harness's fiction.
            if name.endswith((".any.js", ".window.js")):
                try:
                    source = path.read_text(encoding="utf8", errors="replace")
                except OSError:
                    continue
                if not serve.runs_in_window(source):
                    generated += 1
                    continue
                tests.append(str(rel)[: -len(".js")] + ".html")
                continue
            if name.endswith((".worker.js", ".sharedworker.js", ".serviceworker.js")):
                generated += 1
                continue
            if not name.endswith((".html", ".xht", ".xhtml")):
                continue
            if SKIP_FILE.search(name):
                continue
            try:
                body = path.read_text(encoding="utf8", errors="replace")
            except OSError:
                continue
            if "testharness.js" not in body:
                unscoreable += 1
                continue
            # The multi-origin security suites — referrer-policy, mixed-content,
            # upgrade-insecure-requests and their kin — are built on `common/security-features`.
            if "common/security-features" in body:
                needs_server += 1
                continue
            tests.append(str(rel))
    if limit:
        tests = tests[:limit]
    return tests, generated, unscoreable + needs_server


def capped(command, megabytes):
    """Wrap a command so the child runs under an address-space limit.

    A WPT file is allowed to be hostile — several exist precisely to allocate
    until something gives — and without a cap the kernel picks the victim, which
    on this 8 GiB box has been the whole session rather than the test. A capped
    child dies alone and is recorded as one crash.

    Done through the shell rather than `preexec_fn`, which CPython documents as
    unsafe in the presence of threads: this runner is a thread pool, and a fork
    hook that takes a lock another thread holds deadlocks the worker with no
    output to explain it. `exec` keeps the process count the same, so the child
    the timeout kills is still the engine.
    """
    if os.name != "posix" or not megabytes:
        return command
    return ["/bin/sh", "-c", f'ulimit -v {megabytes * 1024}; exec "$0" "$@"', *command]


def panic_reason(stderr: bytes, returncode: int) -> str:
    """The line that says what went wrong, not the line that says how to find out.

    Taking the *last* line of stderr recorded "note: run with `RUST_BACKTRACE=1`"
    for 139 of 140 crashes — the one line guaranteed to be useless. A Rust panic
    puts the location on the `panicked at` line and the message on the next, so
    both are kept, and a non-panic exit says what it actually was.
    """
    lines = stderr.decode("utf8", "replace").strip().splitlines()
    for index, line in enumerate(lines):
        if "panicked at" in line:
            message = lines[index + 1].strip() if index + 1 < len(lines) else ""
            return f"{line.strip()} :: {message}"[:400]
    for line in lines:
        if line.strip() and not line.startswith("note:"):
            return line.strip()[:400]
    return f"exit {returncode}"


def run_one(args):
    """Run one test file. Returns a dict that always names its own outcome.

    One process per file, and roadmap-history.md §B19.3 proposed changing that.
    It was built, measured, and reverted; the measurement is worth more than the
    feature would have been.

    The idea was that `open` takes several URLs in one invocation, so batching
    twelve test files per process would amortise process start and font
    loading. It produced **identical scores** and was slower almost everywhere:

        dom          (587 files, 4 jobs)   75s -> 392s
        url          ( 34 files, 4 jobs)  1.7s -> 2.2s
        domparsing   ( 60 files, 4 jobs)  2.6s -> 3.1s
        css/cssom    (190 files, 4 jobs)   23s -> 20s

    The `dom` figure is the one that settles it, and the cause is structural
    rather than a matter of tuning. A batch shares one process, so a batch that
    crashes loses every file in it — which means a crashed batch has to be
    re-run one file at a time for the survivors to keep their real outcomes.
    WPT is a corpus where crashing and hanging files are *common*, so on `dom`
    most batches split and most files ran twice. Batching also takes the
    harness's per-file timeout away: the ceiling becomes per-process, so one
    hanging file holds eleven others instead of being killed on its own worker.

    And the ceiling was never large. At four jobs `dom` runs about 7.9 files/s,
    so a file costs ~0.5s and process start is well under a tenth of that.
    Nothing batching could have recovered was worth this.

    This is §B15.12a's lesson arriving a fourth time, and it is the same shape
    every time: an optimisation reasoned from what the *code* looks like rather
    than from a measurement. The rule that keeps being relearned is that the
    rule against building what no page asked for applies to performance too.
    """
    rel, base, timeout, mem_mb, grants, script_seconds = args
    url = base + rel
    started = time.monotonic()
    try:
        proc = subprocess.run(
            capped(
                [str(BINARY), *ENGINE_ARGS, "open", "--script", "--json",
                 "--max-snapshot-lines", "1",
                 # A conformance file is allowed to be slow. The engine's
                 # default script ceiling exists to stop a runaway page, and
                 # `html/dom/idlharness` sits exactly on it — so its 1,896
                 # passing subtests appeared or vanished with machine load,
                 # which is a score that depends on the other processes on the
                 # box. See `--script-seconds`.
                 "--script-seconds", str(int(script_seconds)),
                 # WebIDL member decoration: enumerable members and the brand
                 # check that makes an accessor reached on a prototype throw.
                 # `idlharness` checks both on every member of every interface
                 # and no page does, so the engine installs it only when asked
                 # — it cost 15 ms of the 83 ms a script realm took, on every
                 # page. Asked for here, so this number means what it meant.
                 "--webidl-conformance",
                 *grants, url],
                mem_mb,
            ),
            capture_output=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return {"test": rel, "outcome": "engine_timeout",
                "elapsed": time.monotonic() - started}

    elapsed = time.monotonic() - started
    if proc.returncode != 0 and not proc.stdout:
        return {"test": rel, "outcome": "engine_crash", "elapsed": elapsed,
                "detail": panic_reason(proc.stderr, proc.returncode)}

    try:
        payload = json.loads(proc.stdout.decode("utf8", "replace"))
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        return {"test": rel, "outcome": "engine_crash", "elapsed": elapsed,
                "detail": str(exc)}

    return _score(rel, payload, elapsed)


def _score(rel, payload, elapsed):
    """Turn one page's JSON into the outcome record the report is built from."""
    # A page the engine could not open at all carries `ok: false` instead of a
    # snapshot. That is the engine reporting a refusal or a load failure, which
    # is a measured outcome and not a crash.
    if payload.get("ok") is False:
        return {"test": rel, "outcome": "no_report", "elapsed": elapsed,
                "unsupported": {}, "detail": str(payload.get("error", ""))[:300]}

    unsupported = {u["api"]: u["calls"] for u in payload.get("unsupported", [])}

    report = None
    for line in payload.get("console", []):
        text = line.get("text", "")
        index = text.find(MARKER)
        if index != -1:
            try:
                report = json.loads(text[index + len(MARKER):])
            except json.JSONDecodeError:
                pass
            break

    if report is None:
        errors = [
            line.get("text", "")
            for line in payload.get("console", [])
            if line.get("level") == "error"
        ]
        return {
            "test": rel, "outcome": "no_report", "elapsed": elapsed,
            "unsupported": unsupported,
            "detail": errors[0][:300] if errors else "",
        }

    counts = {}
    failures = []
    for sub in report.get("tests", []):
        label = SUBTEST_STATUS.get(sub.get("status"), "UNKNOWN")
        counts[label] = counts.get(label, 0) + 1
        if label not in ("PASS",) and len(failures) < MAX_FAILURES:
            failures.append({"name": sub.get("name", "")[:200],
                             "status": label,
                             "message": (sub.get("message") or "")[:300]})

    harness = HARNESS_STATUS.get(report.get("status"), "UNKNOWN")
    outcome = {"OK": "ok", "ERROR": "harness_error",
               "TIMEOUT": "harness_timeout"}.get(harness, "harness_error")
    return {
        "test": rel, "outcome": outcome, "elapsed": elapsed,
        "harness": harness, "subtests": counts, "failures": failures,
        "unsupported": unsupported,
        "detail": (report.get("message") or "")[:300],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--wpt", default=os.environ.get("WPT_ROOT", os.path.expanduser("~/Dev/wpt")))
    parser.add_argument("--dirs", nargs="*", default=["dom"])
    parser.add_argument("--all", action="store_true", help="every directory")
    parser.add_argument("--jobs", type=int, default=8)
    # A margin over the slowest legitimate file, not a workaround for slowness.
    # This was briefly raised to 120 because `html/dom`'s reflection files took
    # forty seconds — which turned out to be testharness rendering one DOM row
    # per subtest, not the engine. With that output turned off (see serve.py)
    # the same directory finishes in 26 seconds total, so the generous timeout
    # was treating a harness cost as an engine cost.
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--out", default=None)
    parser.add_argument("--mem-mb", type=int, default=1200,
                        help="address-space cap per test process")
    # Comfortably above the engine's own 20s default, so a heavy conformance
    # file finishes rather than landing on the ceiling. Still finite: a file
    # that needs more than this is a file the engine cannot run, and the
    # per-test `--timeout` is the outer bound either way.
    parser.add_argument("--script-seconds", type=int, default=60,
                        help="per-page script ceiling handed to the engine")
    # The other server. See `wptserve.py` for what it buys, what it costs, and
    # why the https variants stay out.
    parser.add_argument("--wptserve", action="store_true",
                        help="run against WPT's own server (real subdomains, "
                             ".py handlers) instead of the static one")
    parser.add_argument("--keep-overlay", action="store_true",
                        help="with --wptserve, leave our reporter installed in "
                             "the checkout afterwards (for debugging)")
    opts = parser.parse_args()

    if not BINARY.exists():
        sys.exit(f"no binary at {BINARY}; cargo build --release -p h5i")

    root = Path(opts.wpt).expanduser()
    serve.WPT_ROOT = str(root)
    dirs = None if opts.all else opts.dirs
    tests, generated, unscoreable = find_tests(root, dirs, opts.limit)
    if not tests:
        sys.exit("no tests found")

    httpd = process = None
    if opts.wptserve:
        import wptserve as wptserve_backend

        # The https and worker variants are dropped by name rather than run and
        # failed: they fail on a certificate this engine cannot be told to
        # trust, and a trust decision recorded as a conformance result is the
        # kind of plausible-wrong number this whole harness exists to avoid.
        before = len(tests)
        tests = [t for t in tests if wptserve_backend.reachable(t)]
        dropped = before - len(tests)
        wptserve_backend.install_overlay(root)
        process = wptserve_backend.start(root)
        base, grants = wptserve_backend.BASE, []
        for origin in wptserve_backend.GRANTS:
            grants += ["--allow", origin]
        print(f"  {dropped} https/worker variant(s) left out: no way to trust "
              f"WPT's certificate authority (see wptserve.py)", flush=True)
    else:
        httpd, port = serve.start()
        base = f"http://127.0.0.1:{port}/"
        # The static server hands out 127.0.0.x as distinct origins, and
        # loopback is reachable by default, so nothing has to be granted.
        grants = []

    print(f"{len(tests)} testharness files, {opts.jobs} jobs | skipped: "
          f"{generated} generated endpoints, {unscoreable} files that load no testharness",
          flush=True)

    results = []
    started = time.monotonic()
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=opts.jobs) as pool:
            work = [(t, base, opts.timeout, opts.mem_mb, grants, opts.script_seconds)
                    for t in tests]
            for i, result in enumerate(pool.map(run_one, work), 1):
                results.append(result)
                if i % 50 == 0 or i == len(tests):
                    passed = sum(r.get("subtests", {}).get("PASS", 0) for r in results)
                    rate = i / (time.monotonic() - started)
                    print(f"  {i}/{len(tests)}  {passed} subtests passing  {rate:.1f} files/s",
                          flush=True)
    finally:
        # The checkout is put back whatever happened, Ctrl-C included. A run
        # that dies leaving our reporter in `resources/` turns the next
        # `git status` in that tree into a mystery.
        if httpd is not None:
            httpd.shutdown()
        if opts.wptserve:
            import wptserve as wptserve_backend

            wptserve_backend.stop(process)
            if not opts.keep_overlay:
                wptserve_backend.restore(root)

    summary = summarise(results, generated, unscoreable, time.monotonic() - started)
    report(summary, results)
    if opts.out:
        Path(opts.out).parent.mkdir(parents=True, exist_ok=True)
        Path(opts.out).write_text(json.dumps(
            {"summary": summary, "results": results}, indent=1))
        print(f"\nwrote {opts.out}")
    return 0


def summarise(results, generated, unscoreable, elapsed):
    outcomes, subtests, unsupported = {}, {}, {}
    for r in results:
        outcomes[r["outcome"]] = outcomes.get(r["outcome"], 0) + 1
        for label, n in r.get("subtests", {}).items():
            subtests[label] = subtests.get(label, 0) + n
        for api, n in r.get("unsupported", {}).items():
            unsupported[api] = unsupported.get(api, 0) + n
    measured = sum(outcomes.get(k, 0) for k in ("ok", "harness_error", "harness_timeout"))
    return {
        "files": len(results),
        "files_measured": measured,
        "files_unmeasured": len(results) - measured,
        "generated_endpoints_skipped": generated,
        "unscoreable_files_skipped": unscoreable,
        "outcomes": outcomes,
        "subtests": subtests,
        "subtests_total": sum(subtests.values()),
        "subtests_passing": subtests.get("PASS", 0),
        "unsupported": dict(sorted(unsupported.items(), key=lambda kv: -kv[1])),
        "elapsed_s": round(elapsed, 1),
    }


def report(summary, results):
    passing = summary["subtests_passing"]
    total = summary["subtests_total"]
    pct = (100.0 * passing / total) if total else 0.0
    print(f"\n{'=' * 62}\nsubtests passing: {passing} of {total} scored ({pct:.1f}%)")
    print(f"files: {summary['files']}  measured {summary['files_measured']}  "
          f"unmeasured {summary['files_unmeasured']}")
    print(f"\noutcomes:")
    for name, n in sorted(summary["outcomes"].items(), key=lambda kv: -kv[1]):
        print(f"  {n:6d}  {name}")
    if summary["subtests"]:
        print(f"\nsubtest results:")
        for name, n in sorted(summary["subtests"].items(), key=lambda kv: -kv[1]):
            print(f"  {n:6d}  {name}")

    missing = summary["unsupported"]
    if missing:
        print(f"\nAPIs the tests asked for and this engine does not have"
              f" ({len(missing)} distinct, top 25):")
        for api, n in list(missing.items())[:25]:
            print(f"  {n:6d}  {api}")

    stuck = [r for r in results if r["outcome"] == "no_report"]
    if stuck:
        print(f"\n{len(stuck)} files where the harness never reported. Top errors:")
        tally = {}
        for r in stuck:
            key = re.sub(r"\d+", "N", (r.get("detail") or "(silent)"))[:120]
            tally[key] = tally.get(key, 0) + 1
        for detail, n in sorted(tally.items(), key=lambda kv: -kv[1])[:15]:
            print(f"  {n:6d}  {detail}")
    print(f"\n{summary['elapsed_s']}s")


if __name__ == "__main__":
    sys.exit(main())
