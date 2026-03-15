#!/usr/bin/env bash
# h5i cargo-test adapter — runs `cargo test` and prints h5i-compatible JSON to stdout.
#
# Usage (standalone):
#   ./script/h5i-cargo-test-adapter.sh [cargo-test-args...]
#
# Usage (pipe into h5i commit):
#   ./script/h5i-cargo-test-adapter.sh > /tmp/h5i-results.json
#   h5i commit -m "..." --test-results /tmp/h5i-results.json
#
# Usage (one-liner with --test-cmd):
#   h5i commit -m "..." \
#     --test-cmd "./script/h5i-cargo-test-adapter.sh"
#
# The script exits with cargo test's own exit code so CI can gate on it.
set -euo pipefail

CARGO_ARGS=("$@")

# Run cargo test capturing both stdout and stderr
TMPOUT=$(mktemp)
TMPERR=$(mktemp)
trap 'rm -f "$TMPOUT" "$TMPERR"' EXIT

START_NS=$(date +%s%N 2>/dev/null || echo 0)
cargo test "${CARGO_ARGS[@]}" >"$TMPOUT" 2>"$TMPERR" || CARGO_EXIT=$?
END_NS=$(date +%s%N 2>/dev/null || echo 0)
CARGO_EXIT=${CARGO_EXIT:-0}

# Compute duration
if [[ "$START_NS" != "0" && "$END_NS" != "0" ]]; then
  DURATION=$(awk "BEGIN { printf \"%.3f\", ($END_NS - $START_NS) / 1000000000 }")
else
  DURATION="0.0"
fi

STDOUT=$(cat "$TMPOUT")
STDERR=$(cat "$TMPERR")
COMBINED="$STDOUT
$STDERR"

# Parse "test result: ok. N passed; M failed; K ignored;" lines.
# cargo emits one such line per test binary (lib, integration, doc-tests).
# We SUM across all of them so the total reflects the full test run.
PASSED=0
FAILED=0
SKIPPED=0

while IFS= read -r line; do
  if [[ "$line" =~ ^test\ result: ]]; then
    if [[ "$line" =~ ([0-9]+)\ passed ]]; then
      PASSED=$((PASSED + BASH_REMATCH[1]))
    fi
    if [[ "$line" =~ ([0-9]+)\ failed ]]; then
      FAILED=$((FAILED + BASH_REMATCH[1]))
    fi
    if [[ "$line" =~ ([0-9]+)\ ignored ]]; then
      SKIPPED=$((SKIPPED + BASH_REMATCH[1]))
    fi
  fi
done <<< "$COMBINED"

TOTAL=$((PASSED + FAILED + SKIPPED))

# Human-readable summary built from the accumulated totals
SUMMARY="$PASSED passed"
[[ $FAILED -gt 0 ]]  && SUMMARY="$SUMMARY, $FAILED failed"
[[ $SKIPPED -gt 0 ]] && SUMMARY="$SUMMARY, $SKIPPED ignored"

# Emit h5i TestResultInput JSON
python3 - <<PYEOF
import json, sys
data = {
    "tool": "cargo-test",
    "passed": $PASSED,
    "failed": $FAILED,
    "skipped": $SKIPPED,
    "total": $TOTAL,
    "duration_secs": $DURATION,
    "coverage": 0.0,
    "exit_code": $CARGO_EXIT,
    "summary": $(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$SUMMARY"),
}
print(json.dumps(data, indent=2))
PYEOF

exit $CARGO_EXIT
