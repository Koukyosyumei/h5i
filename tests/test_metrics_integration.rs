//! Integration tests for h5i test-metrics ingestion.
//!
//! These tests exercise the full path from producing a `TestResultInput` JSON
//! file (as any adapter would) through committing with `TestSource::Provided`
//! and reading the stored metrics back from the Git Note.
//!
//! Run with:
//!   cargo test --test test_metrics_integration -- --nocapture

use git2::{Repository, Signature};
use h5i_core::metadata::{TestMetrics, TestResultInput, TestSource};
use h5i_core::repository::H5iRepository;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

// ─── helpers ────────────────────────────────────────────────────────────────

fn setup_repo(dir: &Path) -> H5iRepository {
    let git_repo = Repository::init(dir).expect("git init failed");

    // git requires a user identity for commits
    let mut config = git_repo.config().unwrap();
    config.set_str("user.name", "H5i Test").unwrap();
    config.set_str("user.email", "test@h5i.io").unwrap();

    H5iRepository::open(dir).expect("h5i open failed")
}

/// Stage a file so the commit has at least one entry in the index.
fn stage_file(repo: &H5iRepository, name: &str, content: &str) {
    let workdir = repo.git().workdir().unwrap().to_path_buf();
    fs::write(workdir.join(name), content).unwrap();
    let mut index = repo.git().index().unwrap();
    index.add_path(Path::new(name)).unwrap();
    index.write().unwrap();
}

fn sig(repo: &H5iRepository) -> Signature<'static> {
    repo.git().signature().unwrap()
}

// ─── TestResultInput / TestMetrics unit tests ────────────────────────────────

#[test]
fn test_result_input_into_metrics_full() {
    let input = TestResultInput {
        tool: Some("pytest".into()),
        passed: Some(42),
        failed: Some(0),
        skipped: Some(3),
        total: Some(45),
        duration_secs: Some(1.234),
        coverage: Some(87.5),
        exit_code: Some(0),
        summary: Some("42 passed, 3 skipped in 1.23s".into()),
    };

    let m = input.into_metrics("abc123".into());
    assert_eq!(m.passed, 42);
    assert_eq!(m.failed, 0);
    assert_eq!(m.skipped, 3);
    assert_eq!(m.total, 45);
    assert!((m.duration_secs - 1.234).abs() < 0.001);
    assert!((m.coverage - 87.5).abs() < 0.01);
    assert_eq!(m.exit_code, Some(0));
    assert_eq!(m.tool.as_deref(), Some("pytest"));
    assert_eq!(m.test_suite_hash, "abc123");
    assert!(m.is_passing());
}

#[test]
fn test_result_input_into_metrics_partial_computes_total() {
    let input = TestResultInput {
        passed: Some(5),
        failed: Some(1),
        skipped: Some(2),
        ..Default::default()
    };
    let m = input.into_metrics(String::new());
    assert_eq!(m.total, 8); // computed from passed+failed+skipped
    assert!(!m.is_passing()); // failed > 0 and no exit_code
}

#[test]
fn test_is_passing_exit_code_zero() {
    let m = TestMetrics {
        exit_code: Some(0),
        failed: 5, // exit_code takes precedence
        ..Default::default()
    };
    assert!(m.is_passing());
}

#[test]
fn test_is_passing_nonzero_exit_code() {
    let m = TestMetrics {
        exit_code: Some(1),
        passed: 10,
        failed: 0,
        total: 10,
        ..Default::default()
    };
    assert!(!m.is_passing());
}

#[test]
fn test_is_passing_legacy_coverage_heuristic() {
    // Old records have total==0, rely on coverage > 0
    let m = TestMetrics {
        coverage: 75.0,
        ..Default::default()
    };
    assert!(m.is_passing());

    let m_zero = TestMetrics {
        coverage: 0.0,
        ..Default::default()
    };
    assert!(!m_zero.is_passing());
}

// ─── JSON round-trip ─────────────────────────────────────────────────────────

#[test]
fn test_result_input_json_round_trip() {
    let input = TestResultInput {
        tool: Some("cargo-test".into()),
        passed: Some(10),
        failed: Some(0),
        skipped: Some(1),
        total: Some(11),
        duration_secs: Some(0.5),
        coverage: Some(0.0),
        exit_code: Some(0),
        summary: Some("10 passed, 1 ignored in 0.50s".into()),
    };

    let json = serde_json::to_string(&input).unwrap();
    let decoded: TestResultInput = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.tool.as_deref(), Some("cargo-test"));
    assert_eq!(decoded.passed, Some(10));
}

#[test]
fn test_partial_json_deserialization() {
    // Adapters may omit fields — all optional
    let json = r#"{ "tool": "jest", "passed": 7, "exit_code": 0 }"#;
    let input: TestResultInput = serde_json::from_str(json).unwrap();
    assert_eq!(input.tool.as_deref(), Some("jest"));
    assert_eq!(input.passed, Some(7));
    assert_eq!(input.failed, None);
    assert_eq!(input.exit_code, Some(0));
}

#[test]
fn test_legacy_test_metrics_backward_compatible() {
    // Old git note JSON only had test_suite_hash + coverage
    let legacy = r#"{ "test_suite_hash": "deadbeef", "coverage": 88.5 }"#;
    let m: TestMetrics = serde_json::from_str(legacy).unwrap();
    assert_eq!(m.test_suite_hash, "deadbeef");
    assert!((m.coverage - 88.5).abs() < 0.01);
    // New fields default
    assert_eq!(m.passed, 0);
    assert_eq!(m.failed, 0);
    assert_eq!(m.tool, None);
}

// ─── load_test_results_from_file integration ─────────────────────────────────

#[test]
fn test_load_test_results_from_file() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    let results_path = dir.path().join("results.json");
    let json = serde_json::json!({
        "tool": "pytest",
        "passed": 15,
        "failed": 0,
        "skipped": 2,
        "total": 17,
        "duration_secs": 2.5,
        "coverage": 91.0,
        "exit_code": 0,
        "summary": "15 passed, 2 skipped in 2.50s"
    });
    fs::write(&results_path, json.to_string()).unwrap();

    let m = repo.load_test_results_from_file(&results_path).unwrap();
    assert_eq!(m.passed, 15);
    assert_eq!(m.failed, 0);
    assert_eq!(m.skipped, 2);
    assert_eq!(m.total, 17);
    assert!((m.coverage - 91.0).abs() < 0.01);
    assert_eq!(m.tool.as_deref(), Some("pytest"));
    assert!(m.is_passing());
}

#[test]
fn test_load_test_results_invalid_json_returns_error() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());
    let bad_path = dir.path().join("bad.json");
    fs::write(&bad_path, "not json at all").unwrap();
    assert!(repo.load_test_results_from_file(&bad_path).is_err());
}

#[test]
fn test_load_test_results_missing_file_returns_error() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());
    let missing = dir.path().join("nonexistent.json");
    assert!(repo.load_test_results_from_file(&missing).is_err());
}

// ─── commit with TestSource::Provided ────────────────────────────────────────

#[test]
fn test_commit_with_provided_test_metrics() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    stage_file(&repo, "lib.rs", "fn answer() -> i32 { 42 }");

    let metrics = TestMetrics {
        tool: Some("cargo-test".into()),
        passed: 8,
        failed: 0,
        skipped: 1,
        total: 9,
        duration_secs: 0.3,
        coverage: 95.0,
        exit_code: Some(0),
        summary: Some("8 passed, 1 ignored in 0.30s".into()),
        ..Default::default()
    };

    let s = sig(&repo);
    let oid = repo
        .commit(
            "feat: add answer function",
            &s,
            &s,
            None,
            TestSource::Provided(metrics),
            None,
        )
        .expect("commit failed");

    // Read back the h5i record from the Git Note
    let record = repo.load_h5i_record(oid).expect("load_h5i_record failed");
    let tm = record.test_metrics.expect("test_metrics should be Some");
    assert_eq!(tm.passed, 8);
    assert_eq!(tm.failed, 0);
    assert_eq!(tm.total, 9);
    assert!((tm.coverage - 95.0).abs() < 0.01);
    assert_eq!(tm.tool.as_deref(), Some("cargo-test"));
    assert_eq!(tm.exit_code, Some(0));
    assert!(tm.is_passing());
}

#[test]
fn test_commit_with_test_source_none_stores_no_metrics() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    stage_file(&repo, "hello.rs", "fn main() {}");

    let s = sig(&repo);
    let oid = repo
        .commit("chore: empty", &s, &s, None, TestSource::None, None)
        .expect("commit failed");

    let record = repo.load_h5i_record(oid).expect("load_h5i_record failed");
    assert!(record.test_metrics.is_none());
}

#[test]
fn test_commit_with_scan_markers() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    stage_file(
        &repo,
        "tests.rs",
        r#"
// h5_i_test_start
fn test_add() { assert_eq!(2 + 2, 4); }
// h5_i_test_end
"#,
    );

    let s = sig(&repo);
    let oid = repo
        .commit("test: add basic test", &s, &s, None, TestSource::ScanMarkers, None)
        .expect("commit failed");

    let record = repo.load_h5i_record(oid).expect("load_h5i_record failed");
    let tm = record.test_metrics.expect("test_metrics should be Some");
    assert!(!tm.test_suite_hash.is_empty());
    assert_eq!(tm.tool.as_deref(), Some("marker-scan"));
}

#[test]
fn test_commit_provided_metrics_from_file() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    stage_file(&repo, "main.py", "print('hello')");

    // Simulate what a pytest adapter would write
    let results_path = dir.path().join("pytest-results.json");
    let json = serde_json::json!({
        "tool": "pytest",
        "passed": 3,
        "failed": 1,
        "skipped": 0,
        "total": 4,
        "duration_secs": 0.42,
        "exit_code": 1,
        "summary": "3 passed, 1 failed in 0.42s"
    });
    fs::write(&results_path, json.to_string()).unwrap();

    let metrics = repo
        .load_test_results_from_file(&results_path)
        .expect("load failed");
    assert!(!metrics.is_passing()); // 1 failure

    let s = sig(&repo);
    let oid = repo
        .commit(
            "fix: partial fix",
            &s,
            &s,
            None,
            TestSource::Provided(metrics),
            None,
        )
        .expect("commit failed");

    let record = repo.load_h5i_record(oid).unwrap();
    let tm = record.test_metrics.unwrap();
    assert_eq!(tm.failed, 1);
    assert!(!tm.is_passing());
}

// ─── run_test_command integration ────────────────────────────────────────────

#[test]
fn test_run_test_command_exit_code_captured() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    // Use a command guaranteed to succeed
    let m = repo.run_test_command("true").expect("run_test_command failed");
    assert_eq!(m.exit_code, Some(0));
}

#[test]
fn test_run_test_command_failing_exit_code() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    let m = repo
        .run_test_command("false")
        .expect("run_test_command failed");
    assert_ne!(m.exit_code, Some(0));
    assert!(!m.is_passing());
}

#[test]
fn test_run_test_command_json_stdout_parsed() {
    let dir = tempdir().unwrap();
    let repo = setup_repo(dir.path());

    // Echo valid TestResultInput JSON from the command
    let cmd = r#"printf '{"tool":"echo-test","passed":7,"failed":0,"exit_code":0}'"#;
    let m = repo.run_test_command(cmd).expect("run_test_command failed");
    assert_eq!(m.tool.as_deref(), Some("echo-test"));
    assert_eq!(m.passed, 7);
    assert_eq!(m.failed, 0);
    assert!(m.is_passing());
}
