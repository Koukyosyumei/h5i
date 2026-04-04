use chrono::{TimeZone, Utc};
use git2::Oid;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tiktoken_rs::get_bpe_from_model;

/// A structured record of one design decision made during an AI coding session.
///
/// Stored alongside the commit so future agents and reviewers can understand
/// *why* the code looks the way it does — including the alternatives that were
/// considered and rejected.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Decision {
    /// Where in the code this decision applies.
    /// Free-form: "src/model.py:fit()", "tree.py:87", "architecture".
    pub location: String,
    /// What was actually implemented / chosen.
    pub choice: String,
    /// Alternatives that were considered but rejected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
    /// The reasoning behind the choice.
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct H5iCommitRecord {
    pub git_oid: String,
    pub parent_oid: Option<String>,
    pub ai_metadata: Option<AiMetadata>,
    pub test_metrics: Option<TestMetrics>,
    /// File path -> hash of the externally provided AST (S-expression)
    pub ast_hashes: Option<HashMap<String, String>>,
    /// Maps file path -> Base64 encoded CRDT state (v1 update)
    pub crdt_states: Option<HashMap<String, String>>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// OIDs of commits that causally triggered this commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caused_by: Vec<String>,
    /// OIDs of commits that this commit causally triggered.
    /// Inverse of `caused_by` — populated automatically at commit time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    /// Structured design decisions recorded at commit time via `--decisions`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub content_tokens: usize,
    pub total_tokens: usize,
    pub model: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AiMetadata {
    pub model_name: String,
    pub prompt: String,
    pub agent_id: String,
    pub usage: Option<TokenUsage>,
}

/// Rich test metrics stored in h5i commit notes.
///
/// All fields except `test_suite_hash` and `coverage` default to zero/None so
/// that old records (which only contain those two fields) can still be read.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TestMetrics {
    /// SHA-256 of the extracted test code block (from `// h5_i_test_start/end` markers).
    /// Empty string when metrics come from an external adapter rather than marker scanning.
    #[serde(default)]
    pub test_suite_hash: String,
    /// Number of tests that passed.
    #[serde(default)]
    pub passed: u64,
    /// Number of tests that failed.
    #[serde(default)]
    pub failed: u64,
    /// Number of tests that were skipped / ignored.
    #[serde(default)]
    pub skipped: u64,
    /// Total tests collected (may be > passed+failed+skipped when errors occurred).
    #[serde(default)]
    pub total: u64,
    /// Wall-clock run time in seconds.
    #[serde(default)]
    pub duration_secs: f64,
    /// Line / branch coverage percentage (0–100). 0.0 means unknown.
    #[serde(default)]
    pub coverage: f64,
    /// Name of the test tool that produced these metrics (e.g. "pytest", "cargo-test").
    #[serde(default)]
    pub tool: Option<String>,
    /// Process exit code returned by the test runner. `Some(0)` means all passed.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Human-readable one-line summary (e.g. "10 passed, 2 skipped in 1.23s").
    #[serde(default)]
    pub summary: Option<String>,
}

impl TestMetrics {
    /// Returns `true` when the test run is considered successful (no failures).
    pub fn is_passing(&self) -> bool {
        if let Some(code) = self.exit_code {
            return code == 0;
        }
        if self.total > 0 {
            return self.failed == 0;
        }
        // Legacy records: rely on coverage heuristic
        self.coverage > 0.0
    }
}

/// Universal input format produced by h5i test-tool adapters.
///
/// Write a JSON file matching this schema and pass it via
/// `--test-results <file>` or the `H5I_TEST_RESULTS` environment variable.
/// All fields are optional; h5i fills in defaults and computes missing totals.
///
/// # Minimal example
/// ```json
/// { "tool": "pytest", "passed": 42, "failed": 0, "duration_secs": 1.5 }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TestResultInput {
    /// Name of the test tool (e.g. `"pytest"`, `"cargo-test"`, `"jest"`).
    pub tool: Option<String>,
    /// Tests that passed.
    pub passed: Option<u64>,
    /// Tests that failed.
    pub failed: Option<u64>,
    /// Tests that were skipped.
    pub skipped: Option<u64>,
    /// Total tests collected (computed from passed+failed+skipped if absent).
    pub total: Option<u64>,
    /// Wall-clock run time in seconds.
    pub duration_secs: Option<f64>,
    /// Line/branch coverage percentage (0–100).
    pub coverage: Option<f64>,
    /// Exit code returned by the test runner.
    pub exit_code: Option<i32>,
    /// Human-readable summary line.
    pub summary: Option<String>,
}

impl TestResultInput {
    /// Convert into a `TestMetrics` record, optionally attaching a suite hash
    /// (pass an empty string when no marker scanning was performed).
    pub fn into_metrics(self, suite_hash: String) -> TestMetrics {
        let passed = self.passed.unwrap_or(0);
        let failed = self.failed.unwrap_or(0);
        let skipped = self.skipped.unwrap_or(0);
        let total = self.total.unwrap_or(passed + failed + skipped);
        TestMetrics {
            test_suite_hash: suite_hash,
            passed,
            failed,
            skipped,
            total,
            duration_secs: self.duration_secs.unwrap_or(0.0),
            coverage: self.coverage.unwrap_or(0.0),
            tool: self.tool,
            exit_code: self.exit_code,
            summary: self.summary,
        }
    }
}

/// Describes how `h5i commit` should collect test metrics.
#[derive(Debug, Clone)]
pub enum TestSource {
    /// Do not record any test metrics.
    None,
    /// Scan staged source files for `// h5_i_test_start` / `// h5_i_test_end`
    /// markers and hash the extracted code block.
    ScanMarkers,
    /// Use pre-computed metrics — e.g. loaded from a `--test-results` JSON file
    /// or produced by running `--test-cmd`.
    Provided(TestMetrics),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitProvenance {
    pub commit_oid: String,
    pub ai_metadata: Option<AiMetadata>,
    pub test_metrics: Option<TestMetrics>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Severity of a single rule finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Commit should be blocked unless `--force` is used.
    Violation,
    /// Human review strongly recommended.
    Warning,
    /// Informational — no action required.
    Info,
}

/// A single rule trigger produced by the integrity checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleFinding {
    /// Short machine-readable identifier (e.g. `"CREDENTIAL_LEAK"`).
    pub rule_id: String,
    pub severity: Severity,
    /// Human-readable explanation of what was detected.
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum IntegrityLevel {
    /// No significant mismatch detected.
    Valid,
    /// Minor deviations found. Human review suggested.
    Warning,
    /// Critical mismatch. Action significantly differs from intent.
    Violation,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IntegrityReport {
    pub level: IntegrityLevel,
    /// 1.0 = clean, 0.0 = maximum penalty.
    pub score: f32,
    pub findings: Vec<RuleFinding>,
}

impl H5iCommitRecord {
    /// Constructs a minimal [`H5iCommitRecord`] from standard Git commit metadata.
    ///
    /// This function extracts basic information directly from a Git repository
    /// without relying on `.h5i` metadata files. It is primarily used as a
    /// fallback when visualizing or processing historical commits that were
    /// created before `.h5i` metadata tracking was introduced.
    ///
    /// The resulting record contains:
    ///
    /// - The commit OID
    /// - The first parent commit OID (if any)
    /// - The commit timestamp converted to `chrono::DateTime<Utc>`
    ///
    /// Fields related to AI generation, testing, and AST hashes are left as `None`
    /// because this information is not available in standard Git commits.
    ///
    /// # Parameters
    ///
    /// * `repo` - The Git repository containing the commit.
    /// * `oid` - The object ID of the commit to inspect.
    ///
    /// # Panics
    ///
    /// This function panics if the commit cannot be found in the repository.
    /// In production environments, it is recommended to propagate errors
    /// instead of panicking.
    pub fn minimal_from_git(repo: &Repository, oid: Oid) -> Self {
        // Retrieve the commit object
        // In practice, find_commit may fail (e.g., shallow clones).
        // Ideally the caller should handle Result, but we simplify here.
        let commit = repo.find_commit(oid).expect("Commit not found");

        // Obtain the parent commit OID (only the first parent is considered)
        let parent_oid = if commit.parent_count() > 0 {
            Some(commit.parent_id(0).unwrap_or(Oid::zero()).to_string())
        } else {
            None
        };

        // Convert Git's timestamp into chrono::DateTime<Utc>
        let time = commit.time();
        let timestamp = Utc
            .timestamp_opt(time.seconds(), 0)
            .single()
            .unwrap_or_else(Utc::now);

        H5iCommitRecord {
            git_oid: oid.to_string(),
            parent_oid,
            ai_metadata: None,
            test_metrics: None,
            ast_hashes: None,
            crdt_states: None,
            timestamp,
            caused_by: vec![],
            causes: vec![],
            decisions: vec![],
        }
    }
}

/// A compact view of a commit used for intent-based search and rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitSummary {
    pub oid: String,
    pub message: String,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub agent_id: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// A single node in the intent graph, representing one commit.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IntentNode {
    pub oid: String,
    pub short_oid: String,
    /// Original commit message.
    pub message: String,
    /// Human-readable intent: stored prompt (mode=prompt) or LLM-generated (mode=analyze).
    pub intent: String,
    /// How the intent was derived: `"analyzed"`, `"prompt"`, or `"message"`.
    pub intent_source: String,
    pub author: String,
    pub timestamp: String,
    pub is_ai: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
}

/// A directed relationship between two commits in the intent graph.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IntentEdge {
    /// Source commit OID.
    pub from: String,
    /// Target commit OID.
    pub to: String,
    /// `"parent"` for sequential history, `"causal"` for explicit `caused_by` links.
    pub kind: String,
}

/// The full intent graph returned by `build_intent_graph`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct IntentGraph {
    pub nodes: Vec<IntentNode>,
    pub edges: Vec<IntentEdge>,
}

/// Context written by an AI agent hook before making changes.
/// Stored in `.git/.h5i/pending_context.json` and consumed at commit time.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PendingContext {
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
}

pub fn count_tokens(text: &str, model: &str) -> Result<usize, String> {
    let bpe = get_bpe_from_model(model)
        .map_err(|e| format!("Failed to get tokenizer for {}: {}", model, e))?;
    Ok(bpe.encode_with_special_tokens(text).len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use git2::{Oid, Repository, Signature};
    use tempfile::tempdir;

    /// Creates a temporary Git repository for testing purposes.
    ///
    /// Returns the temporary directory and the initialized repository.
    /// The directory is automatically deleted when the test finishes.
    fn setup_git_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempdir().expect("Failed to create temp dir");
        let repo = Repository::init(dir.path()).expect("Failed to init repo");
        (dir, repo)
    }

    /// Creates a dummy commit in the provided repository.
    ///
    /// This helper is used by tests to quickly construct commits
    /// with optional parent commits.
    ///
    /// # Parameters
    ///
    /// * `repo` - Target repository
    /// * `message` - Commit message
    /// * `parents` - List of parent commits
    ///
    /// # Returns
    ///
    /// The `Oid` of the newly created commit.
    fn create_dummy_commit(repo: &Repository, message: &str, parents: &[&git2::Commit]) -> Oid {
        let sig = Signature::now("H5i Test", "test@h5i.io").expect("Failed to create signature");
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, parents)
            .expect("Failed to create commit")
    }

    #[test]
    fn test_minimal_from_git_root_commit() {
        let (_dir, repo) = setup_git_repo();

        // 1. Create the root commit
        let root_oid = create_dummy_commit(&repo, "Initial commit", &[]);

        // 2. Execute the function under test
        let record = H5iCommitRecord::minimal_from_git(&repo, root_oid);

        // 3. Verify results
        assert_eq!(record.git_oid, root_oid.to_string());
        assert_eq!(
            record.parent_oid, None,
            "Root commit should not have a parent"
        );
        assert!(record.ai_metadata.is_none());
        assert!(record.test_metrics.is_none());
        assert!(record.ast_hashes.is_none());

        // Ensure the timestamp is not extremely far from the current time
        // (allowing a few seconds of tolerance)
        let now = Utc::now().timestamp();
        assert!((record.timestamp.timestamp() - now).abs() < 5);
    }

    #[test]
    fn test_minimal_from_git_child_commit() {
        let (_dir, repo) = setup_git_repo();

        // 1. Create the root commit
        let root_oid = create_dummy_commit(&repo, "Root", &[]);
        let root_commit = repo.find_commit(root_oid).unwrap();

        // 2. Create a child commit using the root as its parent
        let child_oid = create_dummy_commit(&repo, "Child", &[&root_commit]);

        // 3. Execute the function under test
        let record = H5iCommitRecord::minimal_from_git(&repo, child_oid);

        // 4. Verify results
        assert_eq!(record.git_oid, child_oid.to_string());
        assert_eq!(
            record.parent_oid,
            Some(root_oid.to_string()),
            "Child should correctly identify its first parent OID"
        );
    }

    // ── TestMetrics::is_passing ───────────────────────────────────────────────

    #[test]
    fn test_is_passing_exit_code_zero() {
        let m = TestMetrics { exit_code: Some(0), ..Default::default() };
        assert!(m.is_passing());
    }

    #[test]
    fn test_is_passing_exit_code_nonzero() {
        let m = TestMetrics { exit_code: Some(1), ..Default::default() };
        assert!(!m.is_passing());
    }

    #[test]
    fn test_is_passing_no_exit_code_no_failures() {
        let m = TestMetrics { total: 10, failed: 0, ..Default::default() };
        assert!(m.is_passing());
    }

    #[test]
    fn test_is_passing_no_exit_code_with_failures() {
        let m = TestMetrics { total: 10, failed: 2, ..Default::default() };
        assert!(!m.is_passing());
    }

    #[test]
    fn test_is_passing_legacy_coverage_fallback() {
        // total == 0, no exit_code → falls back to coverage heuristic
        let m = TestMetrics { coverage: 85.0, ..Default::default() };
        assert!(m.is_passing());
    }

    #[test]
    fn test_is_passing_legacy_zero_coverage_fails() {
        let m = TestMetrics { coverage: 0.0, ..Default::default() };
        assert!(!m.is_passing());
    }

    // ── TestResultInput::into_metrics ─────────────────────────────────────────

    #[test]
    fn test_into_metrics_computes_total_from_components() {
        let input = TestResultInput {
            passed: Some(8),
            failed: Some(2),
            skipped: Some(1),
            ..Default::default()
        };
        let m = input.into_metrics(String::new());
        assert_eq!(m.total, 11); // 8 + 2 + 1
        assert_eq!(m.passed, 8);
        assert_eq!(m.failed, 2);
        assert_eq!(m.skipped, 1);
    }

    #[test]
    fn test_into_metrics_explicit_total_wins() {
        let input = TestResultInput {
            passed: Some(5),
            failed: Some(0),
            total: Some(100), // explicitly provided
            ..Default::default()
        };
        let m = input.into_metrics(String::new());
        assert_eq!(m.total, 100); // explicit value preserved
    }

    #[test]
    fn test_into_metrics_defaults_all_zeroes() {
        let m = TestResultInput::default().into_metrics("abc".to_string());
        assert_eq!(m.passed, 0);
        assert_eq!(m.failed, 0);
        assert_eq!(m.skipped, 0);
        assert_eq!(m.total, 0);
        assert_eq!(m.duration_secs, 0.0);
        assert_eq!(m.test_suite_hash, "abc");
    }

    #[test]
    fn test_into_metrics_propagates_tool_and_summary() {
        let input = TestResultInput {
            tool: Some("pytest".to_string()),
            summary: Some("10 passed in 1.2s".to_string()),
            exit_code: Some(0),
            ..Default::default()
        };
        let m = input.into_metrics(String::new());
        assert_eq!(m.tool.as_deref(), Some("pytest"));
        assert_eq!(m.summary.as_deref(), Some("10 passed in 1.2s"));
        assert_eq!(m.exit_code, Some(0));
    }

    #[test]
    fn test_timestamp_conversion_precision() {
        let (_dir, repo) = setup_git_repo();

        // Create a commit with a specific timestamp
        let fixed_time = 1700000000; // 2023-11-14頃
        let sig = Signature::new("Test", "test@h5i.io", &git2::Time::new(fixed_time, 0)).unwrap();

        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let oid = repo
            .commit(None, &sig, &sig, "Fixed time commit", &tree, &[])
            .unwrap();

        let record = H5iCommitRecord::minimal_from_git(&repo, oid);

        // Verify that the chrono conversion preserves the timestamp accurately
        assert_eq!(record.timestamp.timestamp(), fixed_time);
    }
}
