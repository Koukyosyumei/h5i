"""
conftest.py — h5i pytest integration hook.

Place this file at the root of your test suite.  It writes test results to
a temporary JSON file after each session and sets H5I_TEST_RESULTS so that
a subsequent `h5i commit` picks them up automatically.

Enable the hook by passing --h5i on the pytest command line:

  pytest --h5i

or by setting the environment variable H5I_RESULTS_FILE:

  H5I_RESULTS_FILE=/tmp/h5i-results.json pytest
"""

import json
import os
import time

import pytest


def pytest_addoption(parser):
    parser.addoption(
        "--h5i",
        action="store_true",
        default=False,
        help="Write h5i test-result JSON after the session (sets H5I_TEST_RESULTS).",
    )


def pytest_sessionfinish(session, exitstatus):
    """Called after the entire test session finishes."""
    if not (
        session.config.getoption("--h5i", default=False)
        or os.environ.get("H5I_RESULTS_FILE")
    ):
        return

    # Collect counts from the terminalreporter plugin
    tr = session.config.pluginmanager.get_plugin("terminalreporter")
    passed  = len(tr.stats.get("passed",  []))
    failed  = len(tr.stats.get("failed",  [])) + len(tr.stats.get("error", []))
    skipped = len(tr.stats.get("skipped", []))
    total   = passed + failed + skipped

    duration = time.time() - session._setupstate._finalizers[0].__self__._start_time \
        if hasattr(session, "_setupstate") else 0.0

    parts = []
    if passed:
        parts.append(f"{passed} passed")
    if failed:
        parts.append(f"{failed} failed")
    if skipped:
        parts.append(f"{skipped} skipped")

    result = {
        "tool": "pytest",
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "total": total,
        "duration_secs": round(duration, 3),
        "coverage": 0.0,
        "exit_code": int(exitstatus),
        "summary": ", ".join(parts) if parts else "no tests collected",
    }

    out_path = os.environ.get("H5I_RESULTS_FILE") or "/tmp/h5i-pytest-results.json"
    with open(out_path, "w") as f:
        json.dump(result, f, indent=2)

    # Expose for the next shell command (only works in the same process — use
    # H5I_RESULTS_FILE env var in CI for cross-process handoff)
    os.environ["H5I_TEST_RESULTS"] = out_path
    print(f"\n[h5i] Test results written to {out_path}")
