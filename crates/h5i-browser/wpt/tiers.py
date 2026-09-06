#!/usr/bin/env python3
"""Read `tiers.list` and sort test paths into the tiers it declares.

roadmap-history.md §B19.2. The file next door is the source of truth for what a
published number counts; this is the twenty lines that apply it. Kept apart so
the rules can be read, diffed and argued with by somebody who does not read Python.
"""

import re
from pathlib import Path

HERE = Path(__file__).resolve().parent
TIERS_FILE = HERE / "tiers.list"

# The order a report prints them in. `remainder` is not in the file: it is
# where anything matching no rule lands, and it is printed as a count rather
# than as a percentage, because a denominator nobody scoped is not a score.
ORDER = ["core", "encoding", "relevant", "exclude", "remainder"]


def load(path=TIERS_FILE):
    """The rules, in file order. First match wins, so order is preserved."""
    rules = []
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 2:
            raise ValueError(f"{path}:{number}: expected '<tier> <prefix>', got {line!r}")
        tier, prefix = parts
        if tier not in ("core", "relevant", "encoding", "exclude"):
            raise ValueError(f"{path}:{number}: unknown tier {tier!r}")
        rules.append((tier, prefix.strip("/")))
    return rules


def tier_of(test_path, rules):
    """Which tier a test belongs to. First matching rule wins."""
    # Result files are named after the directory with `/` folded to `_`
    # (`css_cssom`), and test paths inside them keep their slashes. Both are
    # accepted so a caller can classify either.
    path = test_path.replace("\\", "/").strip("/")
    for tier, prefix in rules:
        if path == prefix or path.startswith(prefix + "/"):
            return tier
    return "remainder"


def summarise(results, rules):
    """Fold individual test results into per-tier totals.

    **Per test, never per directory**, and the first attempt at this got it
    wrong in a way worth recording. A per-directory fold looked simpler and was
    incorrect for the largest directory in the suite: the sweep produces one
    `css.json` holding 86,000 subtests, and `css` straddles three tiers —
    `css/cssom` is core, `css/css-animations` is excluded by capability, and
    the rest is unscoped. Classified as a directory it matched no rule at all
    and the whole of it fell into the remainder, which made the remainder the
    second-largest row in a table whose entire purpose is that the remainder is
    small and named.

    `results` is the flat list `run.py` writes, each entry carrying its own
    `test` path and `subtests` counts. Returns `{tier: {...}}` with every tier
    in `ORDER` present even when empty, so a report never silently omits a row.
    """
    out = {
        tier: {"passing": 0, "total": 0, "files": 0, "areas": {}} for tier in ORDER
    }
    for result in results:
        tier = tier_of(result.get("test", ""), rules)
        bucket = out[tier]
        counts = result.get("subtests", {})
        bucket["passing"] += counts.get("PASS", 0)
        bucket["total"] += sum(counts.values())
        bucket["files"] += 1
        # The top path component, so the remainder row can name what is in it
        # rather than only how big it is.
        area = result.get("test", "").split("/", 1)[0]
        entry = bucket["areas"].setdefault(area, [0, 0])
        entry[0] += counts.get("PASS", 0)
        entry[1] += sum(counts.values())
    return out


def render(summary):
    """The table, with the two framings §B13.3 says have to travel together."""
    lines = []
    lines.append("tier         subtests passing        of scored   files   dirs")
    lines.append("-" * 66)
    for tier in ORDER:
        bucket = summary[tier]
        if not bucket["areas"] and tier == "remainder":
            continue
        share = (
            f"{100 * bucket['passing'] / bucket['total']:.1f}%"
            if bucket["total"]
            else "—"
        )
        lines.append(
            f"{tier:<12} {bucket['passing']:>10}  {share:>7}  "
            f"{bucket['total']:>11}  {bucket['files']:>6}  {len(bucket['areas']):>4}"
        )

    core = summary["core"]
    enc = summary["encoding"]
    lines.append("")
    # Both halves of §B13.3's sentence, printed together and by construction
    # rather than by somebody remembering to add the caveat.
    lines.append(
        f"headline (core + encoding): {core['passing'] + enc['passing']} passing"
    )
    lines.append(
        f"  of which the encoding tier is {enc['passing']}"
        + (
            f" ({100 * enc['passing'] / (core['passing'] + enc['passing']):.0f}%)"
            if core["passing"] + enc["passing"]
            else ""
        )
    )
    lines.append(f"  core alone: {core['passing']} passing")
    rem = summary["remainder"]
    if rem["areas"]:
        # A count, never a percentage. See ORDER.
        lines.append(
            f"\nunscoped remainder: {rem['passing']} passing of {rem['total']} "
            f"scored, across {len(rem['areas'])} areas not named in tiers.list. "
            f"Name them or leave them out of every claim. Largest first:"
        )
        biggest = sorted(rem["areas"].items(), key=lambda kv: -kv[1][1])[:12]
        for area, (passing, total) in biggest:
            lines.append(f"  {passing:>8} / {total:<8} {area}")
    return "\n".join(lines)


if __name__ == "__main__":
    import json
    import sys

    merged = HERE / "results" / "merged.json"
    if not merged.exists():
        sys.exit(f"no {merged}; run wpt/merge.py first")
    blob = json.loads(merged.read_text())
    print(render(summarise(blob.get("results", []), load())))
