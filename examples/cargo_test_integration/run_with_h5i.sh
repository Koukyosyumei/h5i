#!/usr/bin/env bash
# Demonstrate all three ways to attach cargo-test results to an h5i commit.
#
# Run from the REPO ROOT:
#   bash examples/cargo_test_integration/run_with_h5i.sh [option-a|option-b|option-c]
#
# Prerequisites:
#   cargo, h5i (in PATH or target/debug/h5i), git identity configured
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
EXAMPLE_DIR="$REPO_ROOT/examples/cargo_test_integration"
ADAPTER="$REPO_ROOT/script/h5i-cargo-test-adapter.sh"
H5I="${REPO_ROOT}/target/debug/h5i"

# Fall back to PATH if the binary isn't built yet
command -v h5i &>/dev/null && H5I="h5i"

OPTION="${1:-option-a}"

# Ensure the example project's lib is staged so h5i commit has something to commit
git -C "$REPO_ROOT" add "$EXAMPLE_DIR/src/lib.rs" "$EXAMPLE_DIR/tests/integration.rs" 2>/dev/null || true

case "$OPTION" in
# ── Option A: explicit adapter → results file ────────────────────────────────
option-a)
  echo "=== Option A: explicit adapter → --test-results file ==="
  RESULTS_FILE="$(mktemp /tmp/h5i-cargo-results-XXXXXX.json)"

  # Run the adapter inside the example sub-project
  (cd "$EXAMPLE_DIR" && bash "$ADAPTER") > "$RESULTS_FILE"
  echo "Adapter output:"
  cat "$RESULTS_FILE"
  echo

  "$H5I" commit \
    -m "example(cargo): option-a — test results from file" \
    --test-results "$RESULTS_FILE"

  rm -f "$RESULTS_FILE"
  ;;

# ── Option B: inline --test-cmd ───────────────────────────────────────────────
option-b)
  echo "=== Option B: inline --test-cmd ==="
  "$H5I" commit \
    -m "example(cargo): option-b — inline test-cmd" \
    --test-cmd "cd '$EXAMPLE_DIR' && bash '$ADAPTER'"
  ;;

# ── Option C: env var handoff (CI-friendly) ───────────────────────────────────
option-c)
  echo "=== Option C: H5I_TEST_RESULTS env var ==="
  RESULTS_FILE="$(mktemp /tmp/h5i-cargo-results-XXXXXX.json)"

  (cd "$EXAMPLE_DIR" && bash "$ADAPTER") > "$RESULTS_FILE"
  echo "Adapter output:"
  cat "$RESULTS_FILE"
  echo

  H5I_TEST_RESULTS="$RESULTS_FILE" "$H5I" commit \
    -m "example(cargo): option-c — results via env var"

  rm -f "$RESULTS_FILE"
  ;;

*)
  echo "Usage: $0 [option-a|option-b|option-c]"
  exit 1
  ;;
esac

echo
echo "=== h5i log (last 1 commit) ==="
"$H5I" log --limit 1
