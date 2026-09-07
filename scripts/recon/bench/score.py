#!/usr/bin/env python3
"""Score one recon run against a target's declared truth.

    score.py <origin> <truth-json> <endpoints-json>

Reads what the target says is real and what recon confirmed, and prints the
four numbers the design asks for: confirmed, missed, false confirmations, and
requests spent per confirmed endpoint (docs/design/design-recon.md N15).
"""
import json
import sys


def main():
    truth = set(json.load(open(sys.argv[2])))
    report = json.load(open(sys.argv[3]))
    requests = int(sys.argv[4])

    confirmed = {e["path"] for e in report["endpoints"] if e["state"] == "confirmed"}
    # `/` and `/index.html` are the same page to a person; the ledger keys on
    # the path asked for, so treat a trailing slash as equal.
    def norm(path):
        return path.rstrip("/") or "/"

    truth_n = {norm(p) for p in truth}
    confirmed_n = {norm(p) for p in confirmed}
    found = truth_n & confirmed_n
    missed = truth_n - confirmed_n
    false = confirmed_n - truth_n

    print(json.dumps({
        "real": len(truth_n),
        "confirmed": len(found),
        "missed": sorted(missed),
        "false_confirmations": sorted(false),
        "requests": requests,
        "requests_per_confirmed": round(requests / len(found), 1) if found else None,
    }, indent=2))


if __name__ == "__main__":
    main()
