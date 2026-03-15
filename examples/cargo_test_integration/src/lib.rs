//! Small math-utilities library used to demonstrate h5i cargo-test integration.
//!
//! The unit tests below are picked up by `cargo test` and fed to
//! `script/h5i-cargo-test-adapter.sh`, which converts cargo's text output
//! into the h5i [`TestResultInput`] JSON schema before `h5i commit` stores
//! the results as a Git Note.
//!
//! Workflow (run from the repo root):
//!
//! ```bash
//! # Option A — explicit adapter
//! cd examples/cargo_test_integration
//! ../../script/h5i-cargo-test-adapter.sh > /tmp/h5i-results.json
//! cd ../..
//! git add examples/cargo_test_integration/src/lib.rs
//! h5i commit -m "feat: add math utils" --test-results /tmp/h5i-results.json
//!
//! # Option B — inline via --test-cmd (run from repo root)
//! git add examples/cargo_test_integration/src/lib.rs
//! h5i commit -m "feat: add math utils" \
//!   --test-cmd "cd examples/cargo_test_integration && ../../script/h5i-cargo-test-adapter.sh"
//!
//! # Option C — env var (CI-friendly)
//! cd examples/cargo_test_integration
//! ../../script/h5i-cargo-test-adapter.sh > /tmp/h5i-results.json
//! cd ../..
//! H5I_TEST_RESULTS=/tmp/h5i-results.json h5i commit -m "feat: add math utils"
//! ```

/// Returns the sum of `a` and `b`.
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

/// Returns the difference `a - b`.
pub fn subtract(a: i64, b: i64) -> i64 {
    a - b
}

/// Returns `a * b`.
pub fn multiply(a: i64, b: i64) -> i64 {
    a * b
}

/// Divides `a` by `b`.
///
/// # Errors
/// Returns `Err` when `b` is zero.
pub fn divide(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        return Err("division by zero");
    }
    Ok(a / b)
}

/// Returns the factorial of `n`.
///
/// # Panics
/// Panics for `n > 20` (overflow).
pub fn factorial(n: u64) -> u64 {
    assert!(n <= 20, "factorial: input too large");
    (1..=n).product()
}

/// Returns `true` when `n` is prime.
pub fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n == 2 {
        return true;
    }
    if n % 2 == 0 {
        return false;
    }
    let mut i = 3u64;
    while i * i <= n {
        if n % i == 0 {
            return false;
        }
        i += 2;
    }
    true
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // add
    #[test]
    fn test_add_positive() {
        assert_eq!(add(3, 4), 7);
    }

    #[test]
    fn test_add_negative() {
        assert_eq!(add(-5, 3), -2);
    }

    #[test]
    fn test_add_zero() {
        assert_eq!(add(0, 0), 0);
    }

    // subtract
    #[test]
    fn test_subtract() {
        assert_eq!(subtract(10, 4), 6);
    }

    #[test]
    fn test_subtract_negative_result() {
        assert_eq!(subtract(1, 9), -8);
    }

    // multiply
    #[test]
    fn test_multiply() {
        assert_eq!(multiply(6, 7), 42);
    }

    #[test]
    fn test_multiply_by_zero() {
        assert_eq!(multiply(99, 0), 0);
    }

    // divide
    #[test]
    fn test_divide_even() {
        assert_eq!(divide(20, 4), Ok(5));
    }

    #[test]
    fn test_divide_by_zero_returns_err() {
        assert!(divide(1, 0).is_err());
    }

    // factorial
    #[test]
    fn test_factorial_base_cases() {
        assert_eq!(factorial(0), 1);
        assert_eq!(factorial(1), 1);
    }

    #[test]
    fn test_factorial_small() {
        assert_eq!(factorial(5), 120);
        assert_eq!(factorial(10), 3_628_800);
    }

    // is_prime
    #[test]
    fn test_is_prime_small_primes() {
        for &p in &[2u64, 3, 5, 7, 11, 13, 17, 19, 23] {
            assert!(is_prime(p), "{p} should be prime");
        }
    }

    #[test]
    fn test_is_prime_composites() {
        for &c in &[0u64, 1, 4, 6, 8, 9, 10, 15, 25] {
            assert!(!is_prime(c), "{c} should not be prime");
        }
    }

    #[test]
    #[ignore = "slow — only run explicitly"]
    fn test_is_prime_large() {
        assert!(is_prime(104_729)); // known prime
        assert!(!is_prime(104_730));
    }
}
