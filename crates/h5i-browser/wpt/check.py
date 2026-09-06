#!/usr/bin/env python3
"""Compare a WPT run against the committed baseline and fail on a regression.

Why a gate at all: a coverage number with nothing defending it decays. Every
number in roadmap-history.md §B12 was paid for by a specific change, and any of
them can be given back silently by an unrelated one — the settle-loop rewrite in
this branch cost 3,142 subtests in `html` before anyone looked.

Why it gates on *passing* and not on a percentage: the denominator moves when
tests are added upstream or when the harness learns to reach more of them, and a
percentage that falls because the denominator grew is not a regression. The pass
count only falls when the engine got worse.

Directions are not symmetric. Going *down* by more than the tolerance fails.
Going up is fine and prints a reminder to re-baseline, because a baseline that
lags reality stops catching small regressions.
"""

import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASELINE = HERE / "baseline.json"

# Room for genuine run-to-run movement — a test that times out under load, a
# machine slower than the one the baseline came from — without letting a real
# loss through. Per directory, so a small directory cannot hide behind a large
# one's slack.
TOLERANCE = 0.01


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--results", default=str(HERE / "results"))
    parser.add_argument("--baseline", default=str(BASELINE))
    parser.add_argument("--write", action="store_true",
                        help="record the current run as the new baseline")
    opts = parser.parse_args()

    results = {}
    for path in Path(opts.results).glob("*.json"):
        if path.name in ("merged.json", "baseline.json"):
            continue
        summary = json.loads(path.read_text())["summary"]
        results[path.stem] = summary["subtests_passing"]
    if not results:
        sys.exit(f"no results in {opts.results}; run wpt/gate.sh first")

    if opts.write:
        Path(opts.baseline).write_text(json.dumps(
            {"note": "Committed floor for the WPT gate. Raise with wpt/check.py --write.",
             "total": sum(results.values()),
             "per_dir": dict(sorted(results.items()))},
            indent=1) + "\n")
        print(f"wrote {opts.baseline}: {sum(results.values())} passing")
        return 0

    baseline = json.loads(Path(opts.baseline).read_text())
    expected = baseline["per_dir"]

    failures, gains = [], []
    for name, floor in sorted(expected.items()):
        got = results.get(name)
        if got is None:
            failures.append(f"  {name}: not in this run at all (baseline {floor})")
            continue
        allowed = floor - max(1, int(floor * TOLERANCE))
        if got < allowed:
            failures.append(f"  {name}: {got} passing, baseline {floor} (floor {allowed})")
        elif got > floor:
            gains.append(f"  {name}: {got} passing, baseline {floor}  (+{got - floor})")

    total = sum(results.values())
    print(f"WPT gate: {total} subtests passing, baseline {baseline['total']}")
    if gains:
        print("\nabove baseline — re-baseline with `wpt/check.py --write` to keep the gate tight:")
        print("\n".join(gains))
    if failures:
        print("\nREGRESSION:")
        print("\n".join(failures))
        return 1
    print("\nno regression.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
