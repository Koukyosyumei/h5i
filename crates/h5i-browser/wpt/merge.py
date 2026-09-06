#!/usr/bin/env python3
"""Total up every per-directory result file into one honest score.

Honest here means the three numbers stay separate and all three get printed:
what was scored, what could not be scored because this engine could not get the
file to report, and what was never on the table because a static server cannot
serve it. A single percentage hides which of the three moved.

Two things were added for roadmap-history.md §B19 and both are about turning a
number into something a reader can check:

* **Tiers** (§B19.2). `tiers.list` declares what a published number counts,
  by capability and with the reason on each line, and the table it produces
  prints the encoding block as its own row — so §B13.3's caveat travels with
  the headline by construction rather than by somebody remembering it.
* **The triage rollup** (§B19.4). Per-run, `run.py` already groups what a
  directory asked for and could not have; the rollup does it across a whole
  sweep and adds the failure *messages*, which is where the cheap structural
  causes hide. §B12.2's lesson was that one missing binding cost twenty files
  their entire score, and that an hour reading actual failure text beats a week
  implementing what a count seemed to ask for.
"""

import argparse
import collections
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / "results"
sys.path.insert(0, str(HERE))
import tiers as tiers_module  # noqa: E402

# What makes two failure messages "the same failure".
#
# A message carries the specifics that make it useless for grouping: the
# property being reflected, the value that came back, the line number. Stripped,
# a hundred messages collapse to the handful of causes behind them, which is the
# form §B12.2 found four bugs in.
NOISE = [
    (re.compile(r'"[^"]*"'), '"X"'),
    (re.compile(r"'[^']*'"), "'X'"),
    (re.compile(r"\b\d+\b"), "N"),
    (re.compile(r"`[^`]*`"), "`X`"),
    (re.compile(r"\bat line N(:N)?"), "at line N"),
]


def signature(message):
    """One failure message, reduced to its shape."""
    text = (message or "").strip()
    for pattern, replacement in NOISE:
        text = pattern.sub(replacement, text)
    return text[:160]


def _tier_blob(every_result):
    try:
        return tiers_module.summarise(every_result, tiers_module.load())
    except (OSError, ValueError):
        return {}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", default=str(RESULTS))
    opts = parser.parse_args()
    results_dir = Path(opts.results)
    files = sorted(p for p in results_dir.glob("*.json") if p.name != "merged.json")
    if not files:
        sys.exit(f"no result files in {results_dir}; run wpt/sweep.sh first")

    subtests = collections.Counter()
    outcomes = collections.Counter()
    unsupported = collections.Counter()
    per_dir, totals = {}, collections.Counter()
    # The rollup: failure shapes, and the silent files that produced no report
    # at all. The second is the bucket §B12.2 says is worth chasing, because
    # one gap there stops a file before it can say what it failed.
    failure_shapes = collections.Counter()
    failure_example = {}
    silent_shapes = collections.Counter()
    # Every result, kept for the tier fold. Tiers are computed per *test*
    # because a directory can straddle them: `css` holds `css/cssom` (core) and
    # `css/css-animations` (excluded by capability) in one result file.
    every_result = []

    for path in files:
        blob = json.loads(path.read_text())
        summary = blob["summary"]
        subtests.update(summary["subtests"])
        outcomes.update(summary["outcomes"])
        unsupported.update(summary["unsupported"])
        for key in ("files", "files_measured", "files_unmeasured",
                    "generated_endpoints_skipped", "unscoreable_files_skipped"):
            totals[key] += summary.get(key, 0)
        per_dir[path.stem] = (summary["subtests_passing"],
                              summary["subtests_total"],
                              summary["files"])

        for result in blob.get("results", []):
            every_result.append(
                {"test": result.get("test", ""), "subtests": result.get("subtests", {})}
            )
            if result.get("outcome") in ("no_report", "engine_crash", "engine_timeout"):
                shape = signature(result.get("detail", "")) or result["outcome"]
                key = f"{result['outcome']}: {shape}"
                silent_shapes[key] += 1
                failure_example.setdefault(key, result["test"])
            for failure in result.get("failures", []):
                shape = signature(failure.get("message", ""))
                if not shape:
                    continue
                failure_shapes[shape] += 1
                failure_example.setdefault(shape, result["test"])

    passing = subtests["PASS"]
    scored = sum(subtests.values())
    print("=" * 66)
    print(f"WPT subtests passing: {passing}  of {scored} scored")
    print(f"files run {totals['files']}  "
          f"(reported {totals['files_measured']}, silent {totals['files_unmeasured']})")
    print(f"not run: {totals['generated_endpoints_skipped']} generated endpoints, "
          f"{totals['unscoreable_files_skipped']} files with no testharness")
    print()
    print("outcomes:", dict(outcomes.most_common()))
    print("subtests:", dict(subtests.most_common()))
    print()
    print("top directories by passing subtests:")
    for name, (p, t, f) in sorted(per_dir.items(), key=lambda kv: -kv[1][0])[:20]:
        print(f"  {p:7d} / {t:<7d} {f:5d} files  {name}")
    print()
    print("most-wanted missing APIs:")
    for api, n in unsupported.most_common(30):
        print(f"  {n:6d}  {api}")

    # The tier table. Printed after the raw totals rather than instead of them,
    # because the totals are what a regression gate reads and the tiers are
    # what a claim is made from.
    print()
    print("=" * 66)
    try:
        rules = tiers_module.load()
        print(tiers_module.render(tiers_module.summarise(every_result, rules)))
    except (OSError, ValueError) as exc:
        print(f"tiers.list could not be read ({exc}); no tier table.")

    if silent_shapes:
        print()
        print("=" * 66)
        print("files that reported nothing, grouped by cause:")
        print("  (one gap here stops a whole file before it can say what it failed)")
        for shape, n in silent_shapes.most_common(20):
            print(f"  {n:5d}  {shape}")
            print(f"         e.g. {failure_example.get(shape, '')}")

    if failure_shapes:
        print()
        print("=" * 66)
        print("failing subtests, grouped by message shape:")
        for shape, n in failure_shapes.most_common(25):
            print(f"  {n:6d}  {shape}")
            print(f"          e.g. {failure_example.get(shape, '')}")

    (results_dir / "merged.json").write_text(json.dumps({
        "subtests_passing": passing,
        "subtests_scored": scored,
        "subtests": dict(subtests),
        "outcomes": dict(outcomes),
        "files": dict(totals),
        "per_dir": per_dir,
        "unsupported": dict(unsupported.most_common(200)),
        "tiers": _tier_blob(every_result),
        "failure_shapes": dict(failure_shapes.most_common(200)),
        "silent_shapes": dict(silent_shapes.most_common(200)),
        "examples": failure_example,
    }, indent=1))
    print(f"\nwrote {results_dir / 'merged.json'}")


if __name__ == "__main__":
    main()
