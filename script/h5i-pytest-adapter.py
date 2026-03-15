#!/usr/bin/env python3
"""
h5i pytest adapter — runs pytest and writes h5i-compatible test results JSON.

Usage (standalone):
    python script/h5i-pytest-adapter.py [pytest-args...]

Usage (pipe into h5i commit):
    python script/h5i-pytest-adapter.py -q > /tmp/h5i-results.json
    h5i commit -m "..." --test-results /tmp/h5i-results.json

Usage (one-liner with --test-cmd):
    h5i commit -m "..." --test-cmd "python script/h5i-pytest-adapter.py -q"

Environment variables:
    H5I_TEST_OUTPUT   Path where the JSON is written (default: stdout).
    PYTEST_ARGS       Extra arguments forwarded to pytest.

The script exits with pytest's own exit code, so CI pipelines can gate on it.
"""

import json
import os
import subprocess
import sys
import time


def run_pytest(extra_args):
    """Run pytest with JSON report plugin and return (returncode, report_path)."""
    import tempfile

    report_fd, report_path = tempfile.mkstemp(suffix=".json", prefix="h5i-pytest-")
    os.close(report_fd)

    cmd = [
        sys.executable, "-m", "pytest",
        f"--json-report",
        f"--json-report-file={report_path}",
        "--tb=short",
        "-q",
    ] + extra_args

    start = time.perf_counter()
    result = subprocess.run(cmd, capture_output=True, text=True)
    duration = time.perf_counter() - start

    return result.returncode, report_path, duration, result.stdout, result.stderr


def parse_json_report(path):
    """Parse pytest-json-report output into h5i TestResultInput fields."""
    try:
        with open(path) as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}

    summary = data.get("summary", {})
    passed  = summary.get("passed", 0)
    failed  = summary.get("failed", 0)
    error   = summary.get("error", 0)
    skipped = summary.get("skipped", 0)
    total   = summary.get("total", passed + failed + error + skipped)

    # Coverage (requires pytest-cov)
    coverage = 0.0
    collectors = data.get("collectors", [])
    for c in collectors:
        if "coverage" in str(c).lower():
            coverage = float(c.get("percent", 0.0))
            break

    duration = data.get("duration", 0.0)

    return {
        "passed": passed,
        "failed": failed + error,
        "skipped": skipped,
        "total": total,
        "duration_secs": round(duration, 3),
        "coverage": coverage,
    }


def fallback_parse(stdout, stderr, duration):
    """
    Parse pytest's plain-text output when pytest-json-report is unavailable.

    Looks for a line like:  "5 passed, 1 failed in 0.43s"
    """
    combined = stdout + stderr
    passed = failed = skipped = 0
    for line in combined.splitlines():
        line = line.strip()
        if not line:
            continue
        # Typical pytest summary line: "N passed" / "N failed" / "N skipped"
        import re
        m = re.search(
            r"(\d+)\s+passed"
            r"(?:,\s*(\d+)\s+failed)?"
            r"(?:,\s*(\d+)\s+(?:skipped|warning))?",
            line,
        )
        if m:
            passed  = int(m.group(1) or 0)
            failed  = int(m.group(2) or 0)
            skipped = int(m.group(3) or 0)
            break

    return {
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "total": passed + failed + skipped,
        "duration_secs": round(duration, 3),
        "coverage": 0.0,
    }


def build_summary(fields):
    parts = []
    if fields.get("passed"):
        parts.append(f"{fields['passed']} passed")
    if fields.get("failed"):
        parts.append(f"{fields['failed']} failed")
    if fields.get("skipped"):
        parts.append(f"{fields['skipped']} skipped")
    if fields.get("duration_secs"):
        parts.append(f"{fields['duration_secs']:.2f}s")
    return ", ".join(parts) if parts else "no tests collected"


def main():
    extra_args = sys.argv[1:]

    # Check whether pytest-json-report is available
    try:
        import pytest_jsonreport  # noqa: F401
        use_json_report = True
    except ImportError:
        use_json_report = False

    if use_json_report:
        returncode, report_path, duration, stdout, stderr = run_pytest(extra_args)
        fields = parse_json_report(report_path)
        try:
            os.unlink(report_path)
        except OSError:
            pass
        if not fields:
            fields = fallback_parse(stdout, stderr, duration)
    else:
        # Plain pytest run — parse text output
        import tempfile
        start = time.perf_counter()
        cmd = [sys.executable, "-m", "pytest", "-q"] + extra_args
        result = subprocess.run(cmd, capture_output=True, text=True)
        duration = time.perf_counter() - start
        returncode = result.returncode
        fields = fallback_parse(result.stdout, result.stderr, duration)

    output = {
        "tool": "pytest",
        "exit_code": returncode,
        "summary": build_summary(fields),
        **fields,
    }

    out_path = os.environ.get("H5I_TEST_OUTPUT")
    if out_path:
        with open(out_path, "w") as f:
            json.dump(output, f, indent=2)
    else:
        print(json.dumps(output, indent=2))

    sys.exit(returncode)


if __name__ == "__main__":
    main()
