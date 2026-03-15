//! Integration tests for `math_utils`.
//!
//! These live in `tests/` so they are compiled as a separate crate by cargo,
//! giving a realistic picture of how external callers use the library.
//! `cargo test` runs both the unit tests in `src/lib.rs` and these.

use math_utils::{add, divide, factorial, is_prime, multiply, subtract};

#[test]
fn test_add_large_numbers() {
    assert_eq!(add(i64::MAX - 1, 1), i64::MAX);
}

#[test]
fn test_subtract_to_min() {
    assert_eq!(subtract(i64::MIN + 1, 1), i64::MIN);
}

#[test]
fn test_multiply_commutative() {
    assert_eq!(multiply(7, 13), multiply(13, 7));
}

#[test]
fn test_divide_truncates_toward_zero() {
    assert_eq!(divide(7, 2), Ok(3));
    assert_eq!(divide(-7, 2), Ok(-3));
}

#[test]
fn test_divide_by_zero_integration() {
    let result = divide(100, 0);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "division by zero");
}

#[test]
fn test_factorial_sequence() {
    let expected = [1u64, 1, 2, 6, 24, 120, 720, 5040, 40320, 362880, 3628800];
    for (n, &exp) in expected.iter().enumerate() {
        assert_eq!(factorial(n as u64), exp, "factorial({n}) mismatch");
    }
}

#[test]
fn test_prime_sieve_consistency() {
    // Compare is_prime against a simple trial-division sieve for [0..50]
    let primes_under_50: Vec<u64> = (0..50).filter(|&n| is_prime(n)).collect();
    assert_eq!(
        primes_under_50,
        vec![2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47]
    );
}
