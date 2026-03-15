"""
Example test suite for the h5i pytest integration.

Run the full flow with:

  # Option A — explicit adapter invocation
  python ../../script/h5i-pytest-adapter.py -v > /tmp/h5i-results.json
  h5i commit -m "feat: add math helpers" --test-results /tmp/h5i-results.json

  # Option B — inline via --test-cmd
  h5i commit -m "feat: add math helpers" \
    --test-cmd "python ../../script/h5i-pytest-adapter.py -q"

  # Option C — env var (useful in CI / shell hooks)
  python ../../script/h5i-pytest-adapter.py -q > /tmp/h5i-results.json
  H5I_TEST_RESULTS=/tmp/h5i-results.json h5i commit -m "feat: add math helpers"

After the commit, inspect the stored metrics with:
  h5i log --limit 1
"""


# ── module under test (inline for the example) ──────────────────────────────

def add(a, b):
    return a + b


def subtract(a, b):
    return a - b


def divide(a, b):
    if b == 0:
        raise ValueError("Division by zero")
    return a / b


# ── tests ────────────────────────────────────────────────────────────────────

def test_add_positive():
    assert add(2, 3) == 5


def test_add_negative():
    assert add(-1, -1) == -2


def test_subtract():
    assert subtract(10, 4) == 6


def test_divide_normal():
    assert divide(10, 2) == 5.0


def test_divide_by_zero_raises():
    import pytest
    with pytest.raises(ValueError, match="Division by zero"):
        divide(1, 0)


import pytest


@pytest.mark.skip(reason="pending implementation")
def test_multiply():
    pass
