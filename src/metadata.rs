use chrono::{TimeZone, Utc};
use git2::Oid;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct H5iCommitRecord {
    pub git_oid: String,
    pub parent_oid: Option<String>,
    pub ai_metadata: Option<AiMetadata>,
    pub test_metrics: Option<TestMetrics>,
    /// File path -> hash of the externally provided AST (S-expression)
    pub ast_hashes: Option<HashMap<String, String>>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AiMetadata {
    pub model_name: String,
    pub prompt_hash: String,
    pub agent_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TestMetrics {
    pub test_suite_hash: String,
    pub coverage: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitProvenance {
    pub commit_oid: String,
    pub ai_metadata: Option<AiMetadata>,
    pub test_metrics: Option<TestMetrics>,
    pub created_at: chrono::DateTime<chrono::Utc>,
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
            ai_metadata: None,  // Standard Git commits do not contain AI metadata
            test_metrics: None, // Standard Git commits do not contain testing metrics
            ast_hashes: None,   // Standard Git commits do not contain AST hashes
            timestamp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
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
