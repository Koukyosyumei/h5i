use base64::prelude::*;
use console::style;
use git2::{Blob, Repository};
use git2::{Commit, ObjectType, Oid, Signature};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use yrs::updates::decoder::Decode;
use yrs::{GetString, Text, Transact};

use crate::blame::{AncestryEntry, BlameMode, BlameResult};
use crate::metadata::Decision;
use crate::delta_store::{sha256_hash, DeltaStore};
use crate::error::H5iError;
use chrono::{TimeZone, Utc};

use crate::metadata::{
    AiMetadata, CommitSummary, H5iCommitRecord, IntegrityLevel, IntentEdge, IntentGraph,
    IntentNode, IntegrityReport, PendingContext, TestMetrics, TestSource, TokenUsage,
};
use crate::LocalSession;

/// Git ref used to store all h5i commit metadata (AI provenance, test results,
/// AST hashes, causal links). Using a custom `refs/h5i/` namespace keeps h5i
/// data clearly separated from standard `refs/notes/commits` and lets a single
/// `h5i push` sync everything under `refs/h5i/*` in one refspec.
pub const H5I_NOTES_REF: &str = "refs/h5i/notes";
pub const H5I_AST_REF: &str = "refs/h5i/ast";

pub struct H5iRepository {
    git_repo: Repository,
    pub h5i_root: PathBuf,
}

// ============================================================
// Repository lifecycle
// ============================================================

impl H5iRepository {
    /// Opens or initializes an `h5i` context for an existing Git repository.
    ///
    /// This function discovers the Git repository starting from the given path
    /// and ensures that the `.h5i` metadata directory exists inside the
    /// repository root.
    ///
    /// If the `.h5i` directory does not exist, it will be created along with
    /// several subdirectories used by the system:
    ///
    /// - `ast/` – stores hashed AST representations for tracked files
    /// - `metadata/` – stores commit-related metadata (e.g., AI provenance)
    /// - `crdt/` – stores CRDT state or collaboration data
    ///
    /// # Parameters
    ///
    /// - `path`: A path inside the target Git repository (or the repository root).
    ///
    /// # Returns
    ///
    /// Returns a [`H5iRepository`] instance containing:
    ///
    /// - the discovered Git repository handle
    /// - the `.h5i` root directory path
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - a Git repository cannot be discovered from the given path
    /// - the repository root directory cannot be determined
    /// - the `.h5i` directories cannot be created
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, H5iError> {
        let git_repo = Repository::discover(path)?;
        let h5i_root = git_repo
            .path()
            .parent()
            .ok_or_else(|| {
                H5iError::InvalidPath(
                    "Could not find the parent directory of the repository".to_string(),
                )
            })?
            .join(".git/.h5i");

        if !h5i_root.exists() {
            fs::create_dir_all(&h5i_root)?;
            fs::create_dir_all(h5i_root.join("metadata"))?;
            fs::create_dir_all(h5i_root.join("crdt"))?;
        }

        Ok(H5iRepository { git_repo, h5i_root })
    }
}

// ============================================================
// Core operations
// ============================================================

impl H5iRepository {
    /// Creates a Git commit and atomically associates it with h5i extended metadata.
    ///
    /// This function performs a standard Git commit while collecting and storing
    /// additional `h5i` sidecar data. The extra metadata may include:
    ///
    /// - **AI provenance metadata** describing AI-assisted code generation
    /// - **AST hashes** derived from source files using an optional parser
    /// - **Test provenance metrics** extracted from staged test files
    ///
    /// The collected metadata is stored separately in the `.h5i` directory
    /// and linked to the Git commit via the commit OID.
    ///
    /// The operation proceeds in three phases:
    ///
    /// 1. **Pre-processing staged files**
    ///    - Optionally generate AST representations using the provided parser.
    ///    - Optionally extract test-related metrics.
    ///
    /// 2. **Git commit creation**
    ///    - Uses the `git2` API to write the index tree and create a commit.
    ///
    /// 3. **Sidecar metadata persistence**
    ///    - A corresponding `H5iCommitRecord` is created and stored under `.h5i`.
    ///
    /// # Parameters
    ///
    /// - `message` – Commit message.
    /// - `author` – Git author signature.
    /// - `committer` – Git committer signature.
    /// - `ai_meta` – Optional AI provenance metadata associated with the commit.
    /// - `enable_test_tracking` – Enables automatic test provenance detection.
    /// - `ast_parser` – Optional externally injected parser that converts a file
    ///   into an AST S-expression representation.
    ///
    /// # Returns
    ///
    /// Returns the [`Oid`] of the newly created Git commit.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the Git index cannot be accessed or written
    /// - the commit cannot be created
    /// - AST sidecar data cannot be persisted
    /// - the `h5i` metadata record cannot be stored
    ///
    /// # Notes
    ///
    /// The AST parser is injected as a function pointer to keep the repository
    /// layer language-agnostic. This allows external tools to supply parsers
    /// for different programming languages without modifying the core system.
    pub fn commit(
        &self,
        message: &str,
        author: &Signature,
        committer: &Signature,
        ai_meta: Option<AiMetadata>,
        test_source: TestSource,
        ast_parser: Option<&dyn Fn(&Path) -> Option<String>>, // Optional externally injected parser
        caused_by: Vec<String>,
        decisions: Vec<Decision>,
    ) -> Result<Oid, H5iError> {
        let mut index = self.git_repo.index()?;

        // 1. Prepare optional features
        let mut ast_hashes = None;
        let mut crdt_states = HashMap::new();

        // For ScanMarkers we look for the marker block in staged files (first hit wins).
        let mut scanned_metrics: Option<TestMetrics> = None;

        // Scan staged files
        for entry in index.iter() {
            let path_bytes = &entry.path;
            let path_str = std::str::from_utf8(path_bytes).unwrap();
            let full_path = self.git_repo.workdir().unwrap().join(path_str);

            // Harvest AST (Optional)
            if let Some(parser) = ast_parser {
                let hashes = ast_hashes.get_or_insert_with(HashMap::new);
                if let Some(sexp) = parser(&full_path) {
                    let hash = self.save_ast_to_sidecar(path_str, &sexp)?;
                    hashes.insert(path_str.to_string(), hash);
                }
            }

            // HARVEST: Read the latest local CRDT state managed by the Watcher
            if let Ok(state_b64) = self.load_local_crdt_state_as_base64(path_str) {
                crdt_states.insert(path_str.to_string(), state_b64);
            }

            // Scan for test markers only when requested and not yet found
            if matches!(test_source, TestSource::ScanMarkers) && scanned_metrics.is_none() {
                scanned_metrics = self.scan_test_block(&full_path);
            }
        }

        // Resolve final test_metrics from the chosen source
        let test_metrics = match test_source {
            TestSource::None => None,
            TestSource::ScanMarkers => scanned_metrics,
            TestSource::Provided(metrics) => Some(metrics),
        };

        // Validate and resolve caused_by OIDs (supports abbreviated OIDs)
        let mut resolved_caused_by = Vec::with_capacity(caused_by.len());
        for oid_str in &caused_by {
            let commit = self
                .git_repo
                .revparse_single(oid_str)
                .and_then(|o| o.peel_to_commit())
                .map_err(|_| {
                    H5iError::Git(git2::Error::from_str(&format!(
                        "caused_by OID not found in repository: {}",
                        oid_str
                    )))
                })?;
            resolved_caused_by.push(commit.id().to_string());
        }

        // 2. Create the standard Git commit (using the git2-rs API)
        let tree_id = index.write_tree()?;
        let tree = self.git_repo.find_tree(tree_id)?;
        let parent_commit = self.get_head_commit().ok();
        let mut parents = Vec::new();
        if let Some(ref p) = parent_commit {
            parents.push(p);
        }

        let commit_oid =
            self.git_repo
                .commit(Some("HEAD"), author, committer, message, &tree, &parents)?;

        // 3. Persist the h5i sidecar record
        let record = H5iCommitRecord {
            git_oid: commit_oid.to_string(),
            parent_oid: parent_commit.map(|p| p.id().to_string()),
            ai_metadata: ai_meta,
            test_metrics,
            ast_hashes,
            crdt_states: if crdt_states.is_empty() {
                None
            } else {
                Some(crdt_states)
            },
            timestamp: chrono::Utc::now(),
            caused_by: resolved_caused_by.clone(),
            causes: vec![],
            decisions,
        };
        let metadata_json = serde_json::to_string(&record)?;
        self.git_repo
            .note(author, committer, Some(H5I_NOTES_REF), commit_oid, &metadata_json, true)?;

        // 4. For each commit listed in caused_by, push this commit's OID into
        //    that commit's `causes` list so the inverse link is always stored.
        for cause_oid_str in &resolved_caused_by {
            if let Ok(cause_oid) = git2::Oid::from_str(cause_oid_str) {
                if let Ok(mut cause_record) = self.load_h5i_record(cause_oid) {
                    if !cause_record.causes.contains(&commit_oid.to_string()) {
                        cause_record.causes.push(commit_oid.to_string());
                        let updated_json = serde_json::to_string(&cause_record)?;
                        self.git_repo.note(
                            author,
                            committer,
                            Some(H5I_NOTES_REF),
                            cause_oid,
                            &updated_json,
                            true,
                        )?;
                    }
                }
            }
        }

        Ok(commit_oid)
    }

    /// Reads local binary deltas from .git/h5i/delta and encodes them for the Note.
    fn load_local_crdt_state_as_base64(&self, file_path: &str) -> Result<String, H5iError> {
        let file_hash = crate::delta_store::sha256_hash(file_path);
        let delta_path = self
            .h5i_root
            .join("delta")
            .join(format!("{}.bin", file_hash));

        if !delta_path.exists() {
            return Err(H5iError::RecordNotFound(file_path.to_string()));
        }

        let binary_data = fs::read(&delta_path)?;
        // Use standard base64 encoding (requires base64 crate)
        Ok(BASE64_STANDARD.encode(binary_data))
    }

    fn count_tokens_internal(&self, text: &str, model: &str) -> usize {
        use tiktoken_rs::get_bpe_from_model;
        if let Ok(bpe) = get_bpe_from_model(model) {
            bpe.encode_with_special_tokens(text).len()
        } else {
            text.split_whitespace().count()
        }
    }

    /*
    pub fn commit_with_stats(
        &self,
        prompt: &str,
        model_name: &str,
        agent_id: &str,
        file_path: &str,
        sig: &Signature,
    ) -> crate::error::Result<Oid> {
        // 1. 現在の HEAD の内容（コンテキスト）を取得してトークンを数える
        // 初回コミットなどで HEAD がない場合は空文字として扱う
        let context_content = self.get_content_at_head(file_path).unwrap_or_default();

        let prompt_tokens = self.count_tokens_internal(prompt, model_name);
        let content_tokens = self.count_tokens_internal(&context_content, model_name);

        let usage = TokenUsage {
            prompt_tokens,
            content_tokens,
            total_tokens: prompt_tokens + content_tokens,
            model: model_name.to_string(),
        };

        // 2. メタデータオブジェクトを構築 (プロンプトをそのまま保存)
        let ai_meta = AiMetadata {
            model_name: model_name.to_string(),
            agent_id: agent_id.to_string(),
            prompt: prompt.to_string(),
            usage: Some(usage),
        };

        // 3. 通常の Git コミットを実行
        // コミットメッセージにはプロンプトの要約などを使う運用が一般的です
        let commit_oid = self.commit(prompt, sig, sig, None, TestSource::None, None, vec![])?;

        // 4. メタデータを .h5i/metadata/{oid}.json に保存
        self.save_ai_metadata(commit_oid, &ai_meta)?;

        Ok(commit_oid)
    }*/
}

// ============================================================
// Log API
// ============================================================

impl H5iRepository {
    /// Retrieves an extended commit log that includes AI provenance metadata.
    ///
    /// This function traverses the Git commit history starting from `HEAD`
    /// and attempts to load the corresponding `h5i` sidecar metadata for
    /// each commit.
    ///
    /// If a sidecar metadata file does not exist for a given commit,
    /// the function falls back to constructing a minimal record using
    /// only the information available in the Git commit object.
    ///
    /// # Parameters
    ///
    /// - `limit` – Maximum number of commits to return.
    ///
    /// # Returns
    ///
    /// Returns a vector of [`H5iCommitRecord`] entries representing the
    /// most recent commits, enriched with `h5i` metadata when available.
    ///
    /// # Errors
    ///
    /// Returns an error if the Git revision walker cannot be created
    /// or if the repository history cannot be traversed.
    pub fn get_log(&self, limit: usize) -> Result<Vec<H5iCommitRecord>, H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        let mut records = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            // Read `.h5i/metadata/<oid>.json`. If it does not exist,
            // return a minimal record derived from Git.
            let record = self
                .load_h5i_record(oid)
                .unwrap_or_else(|_| H5iCommitRecord::minimal_from_git(&self.git_repo, oid));
            records.push(record);
        }
        Ok(records)
    }

    /// Retrieves the extended `h5i` commit log including AI metadata.
    ///
    /// This method behaves similarly to `get_log`, but is intended as the
    /// primary API for accessing commit history enriched with `h5i`
    /// provenance data such as:
    ///
    /// - AI generation metadata
    /// - test provenance metrics
    /// - AST hash tracking
    ///
    /// The history traversal begins at `HEAD` and proceeds backwards.
    ///
    /// # Parameters
    ///
    /// - `limit` – Maximum number of commits to retrieve.
    ///
    /// # Returns
    ///
    /// Returns a vector of [`H5iCommitRecord`] values representing the
    /// extended commit history.
    ///
    /// # Errors
    ///
    /// Returns an error if the Git revision walker fails to initialize
    /// or if history traversal encounters an issue.
    pub fn h5i_log(&self, limit: usize) -> Result<Vec<H5iCommitRecord>, H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?; // Traverse history starting from HEAD

        let mut logs = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            // Load sidecar metadata. If unavailable, construct a minimal record from Git data.
            let record = self
                .load_h5i_record(oid)
                .unwrap_or_else(|_| H5iCommitRecord::minimal_from_git(&self.git_repo, oid));
            logs.push(record);
        }
        Ok(logs)
    }

    /// Prints a human-readable commit log enriched with `h5i` metadata.
    ///
    /// This function traverses the Git history starting from `HEAD` and
    /// prints commit information similar to `git log`, augmented with
    /// additional `h5i` metadata when available.
    ///
    /// The output may include:
    ///
    /// - Commit identifier and author
    /// - AI agent metadata (agent ID, model name, prompt hash)
    /// - Test provenance metrics (test suite hash and coverage)
    /// - Number of tracked AST hashes
    /// - Commit message
    ///
    /// Missing metadata is handled gracefully; commits without sidecar
    /// records are displayed using only the standard Git information.
    ///
    /// # Parameters
    ///
    /// - `limit` – Maximum number of commits to display.
    ///
    /// # Errors
    ///
    /// Returns an error if the repository history cannot be traversed
    /// or if commit objects cannot be retrieved.
    pub fn print_log(&self, limit: usize) -> anyhow::Result<()> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        for oid in revwalk.take(limit) {
            let oid = oid?;
            let commit = self.git_repo.find_commit(oid)?;
            let record = self.load_h5i_record(oid).ok();

            println!(
                "{} {}",
                style("commit").yellow(),
                style(oid).magenta().bold()
            );
            println!("{:<10} {}", style("Author:").dim(), commit.author());

            if let Some(r) = record {
                if let Some(ai) = r.ai_metadata {
                    println!(
                        "{:<10} {} {} {}",
                        style("Agent:").dim(),
                        style(&ai.agent_id).cyan().bold(),
                        style(format!("({})", ai.model_name)).dim(),
                        if ai.usage.is_some() {
                            style("󱐋").yellow()
                        } else {
                            style("")
                        }
                    );

                    if !ai.prompt.is_empty() {
                        println!(
                            "{:<10} {}",
                            style("Prompt:").dim(),
                            style(format!("\"{}\"", ai.prompt)).italic()
                        );
                    }

                    if let Some(usage) = ai.usage {
                        println!(
                            "{:<10} {} {} {}",
                            style("Usage:").dim(),
                            style(format!("+{} tokens", usage.total_tokens)).green(),
                            style("|").dim(),
                            style(format!("model: {}", usage.model)).dim()
                        );
                    }
                }

                if let Some(tm) = r.test_metrics {
                    let passing = tm.is_passing();
                    let color = if passing {
                        console::Color::Green
                    } else {
                        console::Color::Red
                    };
                    let icon = if passing { "✔" } else { "✖" };

                    // Prefer an explicit summary; fall back to building one from counts.
                    let detail = if let Some(ref s) = tm.summary {
                        s.clone()
                    } else if tm.total > 0 {
                        let mut parts = vec![format!("{} passed", tm.passed)];
                        if tm.failed > 0 {
                            parts.push(format!("{} failed", tm.failed));
                        }
                        if tm.skipped > 0 {
                            parts.push(format!("{} skipped", tm.skipped));
                        }
                        if tm.duration_secs > 0.0 {
                            parts.push(format!("{:.2}s", tm.duration_secs));
                        }
                        if tm.coverage > 0.0 {
                            parts.push(format!("{:.1}% cov", tm.coverage));
                        }
                        parts.join(", ")
                    } else {
                        // Legacy record with only coverage
                        format!("{:.1}% coverage", tm.coverage)
                    };

                    let tool_label = tm
                        .tool
                        .as_deref()
                        .map(|t| format!(" [{}]", t))
                        .unwrap_or_default();

                    println!(
                        "{:<10} {} {}{}",
                        style("Tests:").dim(),
                        style(icon).fg(color),
                        style(detail).fg(color),
                        style(tool_label).dim()
                    );
                }

                let ast_count = r.ast_hashes.map(|m| m.len()).unwrap_or(0);
                if ast_count > 0 {
                    println!(
                        "{:<10} {} {} files",
                        style("AST:").dim(),
                        style("🧬").cyan(),
                        ast_count
                    );
                }

                if !r.caused_by.is_empty() {
                    for cause_oid_str in &r.caused_by {
                        // Try to get the short message of the cause commit
                        let cause_msg = git2::Oid::from_str(cause_oid_str)
                            .ok()
                            .and_then(|o| self.git_repo.find_commit(o).ok())
                            .and_then(|c| c.summary().map(|s| s.to_string()))
                            .unwrap_or_default();
                        let short = &cause_oid_str[..8.min(cause_oid_str.len())];
                        println!(
                            "{:<10} {} {}",
                            style("Caused by:").dim(),
                            style(short).magenta(),
                            style(format!("\"{}\"", cause_msg)).dim().italic()
                        );
                    }
                }

                if !r.decisions.is_empty() {
                    println!("{:<10}", style("Decisions:").dim());
                    for d in &r.decisions {
                        println!(
                            "  {} {}  {}  {}",
                            style("◆").cyan(),
                            style(&d.location).dim(),
                            style(&d.choice).bold(),
                            if !d.alternatives.is_empty() {
                                style(format!(
                                    "(considered: {})",
                                    d.alternatives.join(", ")
                                ))
                                .dim()
                            } else {
                                style(String::new()).dim()
                            }
                        );
                        if !d.reason.is_empty() {
                            println!("             {}", style(&d.reason).italic());
                        }
                    }
                }
            }
            println!("\n    {}\n", style(commit.message().unwrap_or("")).bold());
            println!("{}", style("─".repeat(60)).dim());
        }
        Ok(())
    }
}

// ============================================================
// Causal chain API
// ============================================================

impl H5iRepository {
    /// Follows `caused_by` links backward from `start_oid`, returning
    /// `(oid, short_message)` pairs in traversal order (BFS).
    pub fn causal_ancestors(&self, start_oid: git2::Oid) -> Vec<(git2::Oid, String)> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut result = Vec::new();

        if let Ok(record) = self.load_h5i_record(start_oid) {
            for oid_str in record.caused_by {
                if let Ok(oid) = git2::Oid::from_str(&oid_str) {
                    if visited.insert(oid) {
                        queue.push_back(oid);
                    }
                }
            }
        }

        while let Some(oid) = queue.pop_front() {
            let msg = self.git_repo.find_commit(oid)
                .ok()
                .and_then(|c| c.summary().map(|s| s.to_string()))
                .unwrap_or_default();
            result.push((oid, msg));

            if let Ok(record) = self.load_h5i_record(oid) {
                for oid_str in record.caused_by {
                    if let Ok(o) = git2::Oid::from_str(&oid_str) {
                        if visited.insert(o) {
                            queue.push_back(o);
                        }
                    }
                }
            }
        }
        result
    }

    /// Scans up to `limit` recent commits for any whose `caused_by` list
    /// includes `target_oid`. Returns `(oid, short_message)` pairs.
    pub fn causal_dependents(
        &self,
        target_oid: git2::Oid,
        limit: usize,
    ) -> Vec<(git2::Oid, String)> {
        let target_str = target_oid.to_string();
        let mut result = Vec::new();
        let mut revwalk = match self.git_repo.revwalk() {
            Ok(r) => r,
            Err(_) => return result,
        };
        if revwalk.push_head().is_err() {
            return result;
        }
        for oid in revwalk.take(limit).flatten() {
            if oid == target_oid {
                continue;
            }
            if let Ok(record) = self.load_h5i_record(oid) {
                if record.caused_by.iter().any(|s| s.starts_with(&target_str[..8.min(target_str.len())]) || *s == target_str) {
                    let msg = self.git_repo.find_commit(oid)
                        .ok()
                        .and_then(|c| c.summary().map(|s| s.to_string()))
                        .unwrap_or_default();
                    result.push((oid, msg));
                }
            }
        }
        result
    }
}

// ============================================================
// Blame API
// ============================================================

impl H5iRepository {
    /// Computes blame information for a file using the specified mode.
    ///
    /// This function acts as a dispatcher that selects the appropriate
    /// blame algorithm based on the provided [`BlameMode`].
    ///
    /// # Modes
    ///
    /// - `BlameMode::Line` – Standard line-based blame using Git history.
    /// - `BlameMode::Ast` – Semantic blame based on AST structure changes.
    ///
    /// # Parameters
    ///
    /// - `path` – Path to the target file within the repository.
    /// - `mode` – The blame computation strategy.
    ///
    /// # Returns
    ///
    /// Returns a vector of [`BlameResult`] entries describing the origin
    /// of each line (or semantic unit) in the file.
    pub fn blame(
        &self,
        path: &std::path::Path,
        mode: BlameMode,
    ) -> Result<Vec<BlameResult>, H5iError> {
        match mode {
            BlameMode::Line => self.blame_by_line(path),
            BlameMode::Ast => self.blame_by_ast(path),
        }
    }

    /// Performs line-based blame (Git standard + AI metadata).
    ///
    /// This method uses the native Git blame algorithm and enriches
    /// the results with `h5i` metadata, including AI provenance
    /// information when available.
    ///
    /// Each line in the file is mapped to the commit that last
    /// modified it.
    fn blame_by_line(&self, path: &std::path::Path) -> Result<Vec<BlameResult>, H5iError> {
        let blame = self.git_repo.blame_file(path, None)?;
        let mut results = Vec::new();

        // Load the file content at HEAD
        let blob = self.get_blob_at_head(path)?;
        let content = std::str::from_utf8(blob.content())
            .map_err(|_| H5iError::Ast("File content is not valid UTF-8".to_string()))?;
        let lines: Vec<&str> = content.lines().collect();

        for hunk in blame.iter() {
            let commit_id = hunk.final_commit_id();
            let record = self.load_h5i_record(commit_id).ok();
            let ai = record.as_ref().and_then(|r| r.ai_metadata.as_ref());
            let agent_info = ai
                .map(|a| format!("AI:{}", a.agent_id))
                .unwrap_or_else(|| "Human".to_string());
            let prompt = ai.map(|a| a.prompt.clone()).filter(|p| !p.is_empty());
            let test_passed = record
                .as_ref()
                .and_then(|r| r.test_metrics.as_ref())
                .map(|tm| tm.is_passing());

            for i in 0..hunk.lines_in_hunk() {
                let line_idx = hunk.final_start_line() + i - 1;
                if line_idx < lines.len() {
                    results.push(BlameResult {
                        line_content: lines[line_idx].to_string(),
                        commit_id: commit_id.to_string(),
                        agent_info: agent_info.clone(),
                        is_semantic_change: false,
                        line_number: line_idx + 1,
                        test_passed,
                        prompt: prompt.clone(),
                    });
                }
            }
        }
        Ok(results)
    }

    // ── Prompt Ancestry ───────────────────────────────────────────────────────

    /// Returns the full prompt ancestry chain for a specific line in a file.
    ///
    /// Starting from HEAD, this method walks backwards through the commit history
    /// following the line as it moves through edits.  At each commit that touched
    /// the line it records the commit OID, author, timestamp, and — critically —
    /// the human prompt that triggered the change (from h5i AI metadata).
    ///
    /// The result is in *reverse-chronological* order (most-recent first), i.e.
    /// the direct cause of the current content is at index 0.
    ///
    /// # Arguments
    /// * `path`        – repo-relative path to the file
    /// * `line_number` – 1-indexed line number in the current HEAD version
    pub fn blame_ancestry(
        &self,
        path: &Path,
        line_number: usize,
    ) -> Result<Vec<AncestryEntry>, H5iError> {
        if line_number == 0 {
            return Err(H5iError::InvalidPath(
                "line_number must be ≥ 1".to_string(),
            ));
        }

        let mut ancestry: Vec<AncestryEntry> = Vec::new();
        // current_commit is where we evaluate blame; line_in_commit is the
        // 1-indexed target line *in that commit's version of the file*.
        let mut current_commit = self.git_repo.head()?.peel_to_commit()?;
        let mut line_in_commit = line_number;
        // Guard against infinite loops in pathological repos.
        const MAX_DEPTH: usize = 500;

        for _ in 0..MAX_DEPTH {
            // ── 1. Blame the file at current_commit ──────────────────────────
            let mut opts = git2::BlameOptions::new();
            opts.newest_commit(current_commit.id());
            let blame = match self.git_repo.blame_file(path, Some(&mut opts)) {
                Ok(b) => b,
                Err(_) => break, // file may not exist yet in this commit
            };

            let hunk = match blame.get_line(line_in_commit) {
                Some(h) => h,
                None => break,
            };
            let responsible_oid = hunk.final_commit_id();
            let responsible = match self.git_repo.find_commit(responsible_oid) {
                Ok(c) => c,
                Err(_) => break,
            };

            // ── 2. Load h5i record for that commit ───────────────────────────
            let record = self.load_h5i_record(responsible_oid).ok();
            let ai = record.as_ref().and_then(|r| r.ai_metadata.as_ref());

            // ── 3. Resolve line content in that commit ────────────────────────
            let line_content = self
                .get_file_line_at_commit(responsible_oid, path, line_in_commit)
                .unwrap_or_default();

            let ts = chrono::DateTime::from_timestamp(responsible.time().seconds(), 0)
                .unwrap_or_default();

            ancestry.push(AncestryEntry {
                commit_id: responsible_oid.to_string(),
                author: responsible
                    .author()
                    .name()
                    .unwrap_or("unknown")
                    .to_string(),
                timestamp: ts,
                prompt: ai.map(|a| a.prompt.clone()).filter(|p| !p.is_empty()),
                agent: ai.map(|a| a.agent_id.clone()),
                line_content,
            });

            // ── 4. Find the parent of the responsible commit ──────────────────
            if responsible.parent_count() == 0 {
                break; // reached root
            }
            let parent = match responsible.parent(0) {
                Ok(p) => p,
                Err(_) => break,
            };

            // ── 5. Map line_in_commit through the diff to the parent ──────────
            let parent_tree = parent.tree().ok();
            let commit_tree = match responsible.tree() {
                Ok(t) => t,
                Err(_) => break,
            };
            match self.map_line_to_parent(
                parent_tree.as_ref(),
                &commit_tree,
                path,
                line_in_commit,
            ) {
                Ok(Some(parent_line)) => {
                    line_in_commit = parent_line;
                    current_commit = parent;
                }
                _ => break, // line was introduced in this commit — ancestry complete
            }
        }

        Ok(ancestry)
    }

    /// Given line `line_in_new` (1-indexed) in the diff from `parent_tree → commit_tree`
    /// for `path`, return the corresponding line number in the parent (old) file.
    ///
    /// Returns `Ok(None)` when the line was *added* in this commit (no ancestor line).
    fn map_line_to_parent(
        &self,
        parent_tree: Option<&git2::Tree>,
        commit_tree: &git2::Tree,
        path: &Path,
        line_in_new: usize,
    ) -> Result<Option<usize>, H5iError> {
        let mut diff_opts = git2::DiffOptions::new();
        if let Some(s) = path.to_str() {
            diff_opts.pathspec(s);
        }
        let diff = self
            .git_repo
            .diff_tree_to_tree(parent_tree, Some(commit_tree), Some(&mut diff_opts))?;

        // No deltas for this file → the file was unchanged; line maps 1-to-1.
        if diff.deltas().count() == 0 {
            return Ok(Some(line_in_new));
        }

        // Walk the first (and only) patch for our file.
        let patch = git2::Patch::from_diff(&diff, 0)?;
        let patch = match patch {
            Some(p) => p,
            None => return Ok(Some(line_in_new)),
        };

        // Cumulative offset applied to lines that fall *before* each hunk.
        let mut cumulative_offset: i64 = 0;

        for hunk_idx in 0..patch.num_hunks() {
            let (hunk, _) = patch.hunk(hunk_idx)?;

            let new_start = hunk.new_start() as usize; // 1-indexed
            let new_count = hunk.new_lines() as usize;
            let old_start = hunk.old_start() as usize;
            let old_count = hunk.old_lines() as usize;

            if line_in_new < new_start {
                // The target line is before this hunk; apply offset from earlier hunks.
                let mapped = line_in_new as i64 + cumulative_offset;
                return Ok(if mapped > 0 { Some(mapped as usize) } else { None });
            }

            if line_in_new < new_start + new_count {
                // The target line is *inside* this hunk.  Walk line-by-line to find
                // the exact correspondence.
                let mut new_cursor = new_start;
                let mut old_cursor = old_start;
                for line_idx in 0..patch.num_lines_in_hunk(hunk_idx)? {
                    let dl = patch.line_in_hunk(hunk_idx, line_idx)?;
                    match dl.origin() {
                        '+' => {
                            // Added line — exists only in new.
                            if new_cursor == line_in_new {
                                return Ok(None); // introduced here
                            }
                            new_cursor += 1;
                        }
                        '-' => {
                            // Removed line — exists only in old.
                            old_cursor += 1;
                        }
                        _ => {
                            // Context line — present in both.
                            if new_cursor == line_in_new {
                                return Ok(Some(old_cursor));
                            }
                            new_cursor += 1;
                            old_cursor += 1;
                        }
                    }
                }
                // Shouldn't be reached if hunk metadata is correct.
                return Ok(None);
            }

            // Line is after this hunk; accumulate offset.
            cumulative_offset += old_count as i64 - new_count as i64;
        }

        // Line is after all hunks.
        let mapped = line_in_new as i64 + cumulative_offset;
        Ok(if mapped > 0 { Some(mapped as usize) } else { None })
    }

    /// Return the content of a single line (1-indexed) in `path` at `commit_oid`.
    fn get_file_line_at_commit(
        &self,
        commit_oid: git2::Oid,
        path: &Path,
        line_number: usize,
    ) -> Option<String> {
        let commit = self.git_repo.find_commit(commit_oid).ok()?;
        let tree = commit.tree().ok()?;
        let entry = tree.get_path(path).ok()?;
        let blob = self.git_repo.find_blob(entry.id()).ok()?;
        let content = std::str::from_utf8(blob.content()).ok()?;
        content.lines().nth(line_number.saturating_sub(1)).map(|s| s.to_string())
    }

    /// Performs semantic blame based on AST hash changes (structural dimension).
    ///
    /// Unlike traditional blame, which tracks line modifications,
    /// semantic blame identifies the commit where the logical structure
    /// of the code last changed.
    ///
    /// This allows the system to detect meaningful code modifications
    /// even when lines are moved or reformatted.
    ///
    /// # Algorithm
    ///
    /// 1. Compute standard line-based blame results.
    /// 2. Retrieve AST hashes associated with each commit.
    /// 3. Compare AST hashes with the parent commit.
    /// 4. Mark the commit as a semantic change if the hash differs.
    ///
    /// # Returns
    ///
    /// Returns blame results annotated with the `is_semantic_change` flag.
    pub fn blame_by_ast(&self, path: &Path) -> Result<Vec<BlameResult>, H5iError> {
        // Base line information from Git blame
        let mut line_results = self.blame_by_line(path)?;
        let path_str = path
            .to_str()
            .ok_or_else(|| H5iError::InvalidPath("Invalid path encoding".to_string()))?;

        for result in &mut line_results {
            let oid = git2::Oid::from_str(&result.commit_id)?;
            let record = self.load_h5i_record(oid)?;

            // 1. Check if this commit contains an AST hash
            if let Some(hashes) = record.ast_hashes {
                if let Some(current_ast_hash) = hashes.get(path_str) {
                    // 2. Compare with the parent commit's AST hash
                    if let Some(parent_oid_str) = record.parent_oid {
                        let parent_oid = git2::Oid::from_str(&parent_oid_str)?;
                        if let Ok(parent_record) = self.load_h5i_record(parent_oid) {
                            let parent_ast_hash = parent_record
                                .ast_hashes
                                .and_then(|h| h.get(path_str).cloned());

                            // If hashes differ, this commit represents a semantic change
                            if Some(current_ast_hash.clone()) != parent_ast_hash {
                                result.is_semantic_change = true;
                            }
                        }
                    } else {
                        // No parent (initial commit): the AST introduction is semantic
                        result.is_semantic_change = true;
                    }
                }
            }
        }

        Ok(line_results)
    }
}

// ============================================================
// Metadata
// ============================================================

impl H5iRepository {
    /// Loads the `h5i` metadata record associated with a specific commit OID.
    ///
    /// This method reads the corresponding Note it into an [`H5iCommitRecord`].
    ///
    /// The function is primarily used by higher-level APIs such as
    /// `log`, `blame`, and other history inspection tools.
    ///
    /// # Parameters
    ///
    /// - `oid` – The Git commit [`Oid`] whose metadata should be loaded.
    ///
    /// # Returns
    ///
    /// Returns the corresponding [`H5iCommitRecord`] if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the metadata file does not exist
    /// - Note is not found
    pub fn load_h5i_record(&self, oid: git2::Oid) -> Result<H5iCommitRecord, H5iError> {
        // Attempt to find the note attached to the commit OID.
        let note = match self.git_repo.find_note(Some(H5I_NOTES_REF), oid) {
            Ok(n) => n,
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                return Err(H5iError::RecordNotFound(oid.to_string()));
            }
            Err(e) => return Err(H5iError::Git(e)),
        };

        // Extract the JSON string from the note
        let data = note
            .message()
            .ok_or_else(|| H5iError::Metadata(format!("Empty note found for commit {}", oid)))?;

        // Deserialize the JSON content into the H5iCommitRecord struct
        let record: H5iCommitRecord = serde_json::from_str(data)?;

        Ok(record)
    }
}

// ============================================================
// Resolve Conflict
// ============================================================

impl H5iRepository {
    /// Merges CRDT operations from two branches (or commits) and produces
    /// a conflict-free text representation.
    ///
    /// Unlike traditional Git merges that operate on text diffs, this method
    /// reconstructs the document state using CRDT updates and merges the
    /// operations from both branches.
    ///
    /// # Algorithm
    ///
    /// 1. Identify the merge base between `our_oid` and `their_oid`.
    /// 2. Reconstruct the base document state by replaying all CRDT updates
    ///    up to the merge base.
    /// 3. Apply updates from the `ours` branch.
    /// 4. Apply updates from the `theirs` branch.
    /// 5. Extract the resulting text from the merged CRDT state.
    ///
    /// Because CRDT operations are commutative and conflict-free,
    /// the resulting document state does not require manual conflict resolution.
    ///
    /// # Parameters
    ///
    /// - `our_oid` – The commit OID representing the current branch.
    /// - `their_oid` – The commit OID representing the incoming branch.
    /// - `file_path` – Path of the file being merged.
    ///
    /// # Returns
    ///
    /// Returns the merged text content produced by the CRDT document.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the merge base cannot be determined
    /// - CRDT updates cannot be loaded or applied
    /// - the repository history cannot be traversed
    pub fn merge_h5i_logic(
        &self,
        our_oid: Oid,
        their_oid: Oid,
        file_path: &str,
    ) -> Result<String, H5iError> {
        // 1. Load mathematical context from Git Notes
        let our_record = self.load_h5i_record(our_oid)?;
        let their_record = self.load_h5i_record(their_oid)?;

        // 2. Initialize a clean CRDT document
        let doc = yrs::Doc::new();
        let text_ref = doc.get_or_insert_text("code");

        // 3. Apply state from OURS
        if let Some(states) = our_record.crdt_states {
            if let Some(b64) = states.get(file_path) {
                let data = BASE64_STANDARD
                    .decode(b64)
                    .map_err(|e| H5iError::Crdt(e.to_string()))?;
                let mut txn = doc.transact_mut();
                txn.apply_update(yrs::Update::decode_v1(&data)?)?;
            }
        }

        // 4. Apply state from THEIRS (The "Magic" automatic merge)
        if let Some(states) = their_record.crdt_states {
            if let Some(b64) = states.get(file_path) {
                let data = BASE64_STANDARD
                    .decode(b64)
                    .map_err(|e| H5iError::Crdt(e.to_string()))?;
                let mut txn = doc.transact_mut();
                // CRDT math ensures this is conflict-free and commutative
                txn.apply_update(yrs::Update::decode_v1(&data)?)?;
            }
        }

        // 5. Extract and return the unified text
        let txn = doc.transact();
        Ok(text_ref.get_string(&txn))
    }

    /// Applies all CRDT updates associated with commits between `base` and `tip`.
    ///
    /// This helper function traverses the commit history from `tip` down to
    /// (but excluding) `base` and applies the CRDT updates stored for each
    /// commit.
    ///
    /// The function assumes that each commit may have an associated CRDT
    /// delta stored in the `.h5i` sidecar storage.
    ///
    /// # Parameters
    ///
    /// - `base` – The base commit where traversal should stop.
    /// - `tip` – The commit representing the tip of the branch.
    /// - `file_path` – Path of the file whose updates should be applied.
    /// - `doc` – The CRDT document being reconstructed.
    ///
    /// # Errors
    ///
    /// Returns an error if update decoding or application fails.
    fn apply_updates_between(
        &self,
        base: Oid,
        tip: Oid,
        file_path: &str,
        doc: &mut yrs::Doc,
    ) -> Result<(), H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
        revwalk.push(tip)?;
        revwalk.hide(base)?;

        for oid_res in revwalk {
            let oid = oid_res?;
            // IMPORTANT:
            // Load the commit-specific CRDT delta.
            // The design assumes that the "h5i commit" process persists
            // these updates as sidecar metadata.
            if let Ok(update_data) = self.load_specific_delta_for_commit(oid, file_path) {
                let mut txn = doc.transact_mut();
                txn.apply_update(yrs::Update::decode_v1(&update_data)?)?;
            }
        }
        Ok(())
    }

    /// Reconstructs the document state by applying all updates from the
    /// beginning of history up to `base_oid`.
    ///
    /// This function walks the commit history in chronological order
    /// and sequentially applies all CRDT updates associated with the file.
    ///
    /// If a commit does not have a CRDT sidecar delta (e.g., a regular
    /// human-created Git commit), the function falls back to ingesting
    /// the full file content at that commit.
    ///
    /// # Parameters
    ///
    /// - `base_oid` – The commit up to which updates should be applied.
    /// - `file_path` – The file being reconstructed.
    /// - `doc` – The CRDT document being updated.
    pub fn apply_all_updates_up_to(
        &self,
        base_oid: Oid,
        file_path: &str,
        doc: &mut yrs::Doc,
    ) -> Result<(), H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?; // Walk in chronological order
        revwalk.push(base_oid)?;

        for oid_res in revwalk {
            let oid = oid_res?;
            if let Ok(update_data) = self.load_specific_delta_for_commit(oid, file_path) {
                let mut txn = doc.transact_mut();
                txn.apply_update(yrs::Update::decode_v1(&update_data)?)?;
            } else {
                // Fallback for commits without CRDT sidecar data
                // (e.g., normal Git commits created by humans).
                // In this case, the entire file content is ingested
                // as a full replacement.
                self.fallback_ingest_content(oid, file_path, doc)?;
            }
        }
        Ok(())
    }

    /// Loads the CRDT update binary associated with a specific commit and file.
    ///
    /// The implementation assumes the following storage layout:
    ///
    /// ```text
    /// .h5i/
    ///   deltas/
    ///     <commit_oid>/
    ///       <file_hash>.bin
    /// ```
    ///
    /// # Parameters
    ///
    /// - `oid` – Commit OID.
    /// - `file_path` – File path used to derive the hash identifier.
    ///
    /// # Returns
    ///
    /// Returns the raw CRDT update bytes for the given commit and file.
    pub fn load_specific_delta_for_commit(
        &self,
        oid: Oid,
        file_path: &str,
    ) -> Result<Vec<u8>, H5iError> {
        let delta_path = DeltaStore::committed_path(
            &self.h5i_root.parent().unwrap(),
            &oid.to_string(),
            file_path,
        );

        if !delta_path.exists() {
            return Err(H5iError::Internal("Delta not found for this commit".into()));
        }

        let mut file = std::fs::File::open(&delta_path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        Ok(buffer)
    }

    /// Ingests file content from Git when CRDT sidecar data is unavailable.
    ///
    /// This fallback mechanism is used for commits that do not contain
    /// CRDT deltas (e.g., regular Git commits).
    ///
    /// The current CRDT document content is cleared and replaced with
    /// the file content retrieved from the specified commit.
    fn fallback_ingest_content(
        &self,
        oid: Oid,
        file_path: &str,
        doc: &mut yrs::Doc,
    ) -> Result<(), H5iError> {
        let content = self.get_content_at_oid(oid, std::path::Path::new(file_path))?;
        let text_ref = doc.get_or_insert_text("code");
        let mut txn = doc.transact_mut();

        // Remove the existing content and insert the new content
        let len = text_ref.len(&txn);
        text_ref.remove_range(&mut txn, 0, len);
        text_ref.push(&mut txn, &content);
        Ok(())
    }

    /// Persists a CRDT delta associated with a specific commit.
    ///
    /// Each delta represents the document update produced during
    /// the commit and is stored in the `.h5i` sidecar directory.
    ///
    /// # Storage layout
    ///
    /// ```text
    /// .h5i/delta/<commit_oid>/<file_hash>.bin
    /// ```
    ///
    /// # Parameters
    ///
    /// - `oid` – Commit OID.
    /// - `file_path` – File path used to derive the hash identifier.
    /// - `update_data` – Binary CRDT update data.
    pub fn persist_delta_for_commit(
        &self,
        oid: Oid,
        file_path: &str,
        update_data: &[u8],
    ) -> Result<(), H5iError> {
        let file_hash = sha256_hash(file_path);
        let delta_dir = self.h5i_root.join("delta").join(oid.to_string());

        // Create directory if necessary
        std::fs::create_dir_all(&delta_dir).map_err(|e| H5iError::Io(e))?;

        let delta_path = delta_dir.join(format!("{}.bin", file_hash));

        // Write the delta binary
        std::fs::write(&delta_path, update_data).map_err(|e| H5iError::Io(e))?;

        Ok(())
    }

    fn rewrite_note_oid(
        &self,
        old_oid: git2::Oid,
        new_oid: git2::Oid,
    ) -> Result<(), H5iError> {
        let note = match self.git_repo.find_note(Some(H5I_NOTES_REF), old_oid) {
            Ok(n) => n,
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(()),
            Err(e) => return Err(H5iError::Git(e)),
        };

        let raw = note
            .message()
            .ok_or_else(|| H5iError::Metadata(format!("Empty note on {}", old_oid)))?;

        let mut record: H5iCommitRecord = serde_json::from_str(raw)?;
        record.git_oid = new_oid.to_string();
        let updated_json = serde_json::to_string(&record)?;

        let sig = self
            .git_repo
            .signature()
            .unwrap_or_else(|_| git2::Signature::now("h5i", "h5i@local").unwrap());

        self.git_repo.note(
            &sig,
            &sig,
            Some(H5I_NOTES_REF),
            new_oid,
            &updated_json,
            true,
        )?;

        self.git_repo
            .note_delete(old_oid, Some(H5I_NOTES_REF), &sig, &sig)?;

        Ok(())
    }

    fn delete_h5i_note(&self, oid: git2::Oid) -> Result<(), H5iError> {
        let sig = self
            .git_repo
            .signature()
            .unwrap_or_else(|_| git2::Signature::now("h5i", "h5i@local").unwrap());

        match self.git_repo.note_delete(oid, Some(H5I_NOTES_REF), &sig, &sig) {
            Ok(_) => Ok(()),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(e) => Err(H5iError::Git(e)),
        }
    }

}

// ============================================================
// Internal helpers
// ============================================================

impl H5iRepository {
    /// Returns a reference to the underlying Git repository.
    ///
    /// This provides direct access to the `git2::Repository` instance
    /// used internally by `H5iRepository`.
    pub fn git(&self) -> &Repository {
        &self.git_repo
    }

    /// Returns the root directory of the `.h5i` sidecar storage.
    ///
    /// The `.h5i` directory contains auxiliary metadata used by H5i,
    /// such as:
    ///
    /// - AST sidecar files
    /// - CRDT deltas
    /// - commit metadata
    pub fn h5i_path(&self) -> &Path {
        &self.h5i_root
    }

    /// Reads the pending AI context written by a Claude Code hook.
    ///
    /// Returns `None` if no pending context file exists.
    /// Returns a closure that parses a source file into an s-expression string.
    ///
    /// Language detection is based on file extension. The appropriate parser
    /// script is discovered by searching, in order:
    ///   1. `$H5I_PARSER_DIR`
    ///   2. `<repo_workdir>/script/`
    ///   3. Directory containing the current executable (`../script/`)
    ///
    /// Currently supported extensions: `.py` (via `h5i-py-parser.py`).
    pub fn make_ast_parser(&self) -> Box<dyn Fn(&std::path::Path) -> Option<String>> {
        let workdir = self.git_repo.workdir().map(|p| p.to_path_buf());

        Box::new(move |path: &std::path::Path| {
            let ext = path.extension()?.to_str()?;

            // Resolve the script path for the detected language.
            let script_name = match ext {
                "py" => "h5i-py-parser.py",
                _ => return None,
            };

            let script_path = find_parser_script(script_name, workdir.as_deref())?;

            let output = std::process::Command::new("python3")
                .arg(&script_path)
                .arg(path)
                .output()
                .ok()?;

            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        })
    }

    /// Computes the structural (AST-level) diff for `path` between two versions.
    ///
    /// - `from_oid`: the "old" commit (defaults to `HEAD`).
    /// - `to_oid`:   the "new" commit (defaults to the working-tree file).
    ///
    /// For each version, the method first tries the stored AST sidecar, then
    /// falls back to parsing the file content on-the-fly via `make_ast_parser`.
    pub fn diff_ast(
        &self,
        path: &std::path::Path,
        from_oid: Option<Oid>,
        to_oid: Option<Oid>,
    ) -> Result<crate::ast::AstDiff, H5iError> {
        use crate::ast::SemanticAst;

        let parser = self.make_ast_parser();

        let from_sexp = {
            let oid = match from_oid {
                Some(o) => o,
                None => self.get_head_commit()?.id(),
            };
            self.load_ast_at_commit(oid, path, &*parser)?
        };

        let to_sexp = match to_oid {
            Some(oid) => self.load_ast_at_commit(oid, path, &*parser)?,
            None => {
                // Parse the working-tree file directly.
                let abs = self
                    .git_repo
                    .workdir()
                    .ok_or_else(|| H5iError::InvalidPath("bare repository".into()))?
                    .join(path);
                parser(&abs).ok_or_else(|| {
                    H5iError::Ast(format!(
                        "No parser available for '{}'. \
                         Ensure python3 and the parser script are accessible.",
                        path.display()
                    ))
                })?
            }
        };

        let base = SemanticAst::from_sexp(&from_sexp);
        let head = SemanticAst::from_sexp(&to_sexp);
        Ok(base.diff(&head))
    }

    /// Retrieves the s-expression for `path` at `oid`.
    ///
    /// Lookup order:
    ///   1. Stored AST sidecar (if the commit was made with `--ast`)
    ///   2. On-the-fly parse of the blob content via `parser`
    fn load_ast_at_commit(
        &self,
        oid: Oid,
        path: &std::path::Path,
        parser: &dyn Fn(&std::path::Path) -> Option<String>,
    ) -> Result<String, H5iError> {
        let path_str = path.to_str().unwrap_or("");

        // Fast path: use the stored AST if available.
        if let Ok(record) = self.load_h5i_record(oid) {
            if let Some(hashes) = &record.ast_hashes {
                if let Some(hash) = hashes.get(path_str) {
                    // Primary: Git-object store (refs/h5i/ast)
                    if let Some(sexp) = self.load_ast_blob(hash) {
                        return Ok(sexp);
                    }
                    // Fallback: legacy filesystem sidecar (.git/.h5i/ast/)
                    let sidecar = self.h5i_root.join("ast").join(format!("{}.sexp", hash));
                    if let Ok(sexp) = fs::read_to_string(&sidecar) {
                        return Ok(sexp);
                    }
                }
            }
        }

        // Slow path: extract blob content and parse on-the-fly.
        let content = self.get_content_at_oid(oid, path)?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
        let tmp = self.h5i_root.join(format!("_tmp_ast.{}", ext));
        fs::write(&tmp, &content)?;
        let result = parser(&tmp);
        let _ = fs::remove_file(&tmp);

        result.ok_or_else(|| {
            H5iError::Ast(format!(
                "No parser available for '{}' at commit {}",
                path.display(),
                oid
            ))
        })
    }

    pub fn read_pending_context(&self) -> Result<Option<PendingContext>, H5iError> {
        let path = self.h5i_root.join("pending_context.json");
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        let ctx: PendingContext = serde_json::from_str(&raw).map_err(|e| {
            H5iError::Metadata(format!("Failed to parse pending_context.json: {e}"))
        })?;
        Ok(Some(ctx))
    }

    /// Deletes the pending context file after it has been consumed by a commit.
    pub fn clear_pending_context(&self) -> Result<(), H5iError> {
        let path = self.h5i_root.join("pending_context.json");
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Returns a list of commits enriched with h5i AI metadata, suitable for
    /// intent-based search. Commits without h5i records are included but will
    /// have `None` for prompt/model/agent_id.
    pub fn list_ai_commits(&self, limit: usize) -> Result<Vec<CommitSummary>, H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        let mut results = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            let commit = self.git_repo.find_commit(oid)?;
            let message = commit.message().unwrap_or("").to_string();

            let record = self.load_h5i_record(oid).ok();

            let (prompt, model, agent_id) =
                match record.as_ref().and_then(|r| r.ai_metadata.as_ref()) {
                    Some(ai) => (
                        Some(ai.prompt.clone()).filter(|p| !p.is_empty()),
                        Some(ai.model_name.clone()).filter(|m| !m.is_empty()),
                        Some(ai.agent_id.clone()).filter(|a| !a.is_empty()),
                    ),
                    None => (None, None, None),
                };

            let timestamp = record.map(|r| r.timestamp).unwrap_or_else(|| {
                Utc.timestamp_opt(commit.time().seconds(), 0)
                    .single()
                    .unwrap_or_else(Utc::now)
            });

            results.push(CommitSummary {
                oid: oid.to_string(),
                message,
                prompt,
                model,
                agent_id,
                timestamp,
            });
        }
        Ok(results)
    }

    /// Builds an [`IntentGraph`] for the most recent `limit` commits.
    ///
    /// Each node carries a human-readable *intent*:
    /// - `analyze = false` — uses the stored AI prompt when available, falling back to the
    ///   commit message.
    /// - `analyze = true`  — calls Claude to generate a concise (≤12-word) intent sentence
    ///   for every commit. Falls back to the prompt-mode logic when the API key is absent.
    ///
    /// Edges represent two kinds of relationship:
    /// - `"parent"` — the standard Git parent/child link between adjacent commits.
    /// - `"causal"` — an explicit `caused_by` declaration stored in the h5i record.
    ///
    /// Edges whose endpoints are outside the `limit` window are silently dropped.
    pub fn build_intent_graph(
        &self,
        limit: usize,
        analyze: bool,
    ) -> Result<IntentGraph, H5iError> {
        use crate::claude::AnthropicClient;
        let client = if analyze {
            AnthropicClient::from_env()
        } else {
            None
        };

        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        let mut nodes: Vec<IntentNode> = Vec::new();
        let mut edges: Vec<IntentEdge> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for raw_oid in revwalk.take(limit) {
            let oid = raw_oid?;
            let oid_str = oid.to_string();
            let commit = self.git_repo.find_commit(oid)?;

            let record = self
                .load_h5i_record(oid)
                .unwrap_or_else(|_| H5iCommitRecord::minimal_from_git(&self.git_repo, oid));

            let message = commit.message().unwrap_or("").trim().to_string();
            let author = commit.author().name().unwrap_or("Unknown").to_string();
            let short_oid = oid_str[..8.min(oid_str.len())].to_string();
            let timestamp = record.timestamp.to_rfc3339();

            let is_ai = record.ai_metadata.is_some();
            let agent = record
                .ai_metadata
                .as_ref()
                .map(|a| a.agent_id.clone())
                .filter(|s| !s.is_empty());
            let model = record
                .ai_metadata
                .as_ref()
                .map(|a| a.model_name.clone())
                .filter(|s| !s.is_empty());
            let stored_prompt: Option<String> = record
                .ai_metadata
                .as_ref()
                .map(|a| a.prompt.clone())
                .filter(|s| !s.is_empty());

            // Determine intent label and track its source
            let (intent, intent_source) = if analyze {
                match client {
                    Some(ref c) => {
                        match c.generate_intent(&short_oid, &message, stored_prompt.as_deref()) {
                            Ok(generated) => (generated, "analyzed".to_string()),
                            Err(e) => {
                                eprintln!(
                                    "  [intent-graph] Claude call failed for {}: {e}",
                                    &short_oid
                                );
                                let fallback = stored_prompt
                                    .clone()
                                    .unwrap_or_else(|| message.clone());
                                let src = if stored_prompt.is_some() { "prompt" } else { "message" };
                                (fallback, src.to_string())
                            }
                        }
                    }
                    None => {
                        let fallback = stored_prompt.clone().unwrap_or_else(|| message.clone());
                        let src = if stored_prompt.is_some() { "prompt" } else { "message" };
                        (fallback, src.to_string())
                    }
                }
            } else {
                let fallback = stored_prompt.clone().unwrap_or_else(|| message.clone());
                let src = if stored_prompt.is_some() { "prompt" } else { "message" };
                (fallback, src.to_string())
            };

            // Causal edges (explicit h5i caused_by)
            for cause_oid in &record.caused_by {
                edges.push(IntentEdge {
                    from: cause_oid.clone(),
                    to: oid_str.clone(),
                    kind: "causal".to_string(),
                });
            }

            // Parent edge (sequential Git history)
            if let Some(ref parent_oid) = record.parent_oid {
                edges.push(IntentEdge {
                    from: parent_oid.clone(),
                    to: oid_str.clone(),
                    kind: "parent".to_string(),
                });
            }

            seen.insert(oid_str.clone());
            nodes.push(IntentNode {
                oid: oid_str,
                short_oid,
                message,
                intent,
                intent_source,
                author,
                timestamp,
                is_ai,
                agent,
                model,
            });
        }

        // Drop edges whose endpoints are outside the loaded window
        edges.retain(|e| seen.contains(&e.from) && seen.contains(&e.to));

        Ok(IntentGraph { nodes, edges })
    }

    /// Prints an ASCII intent graph to stdout.
    pub fn print_intent_graph(&self, limit: usize, analyze: bool) -> anyhow::Result<()> {
        let graph = self.build_intent_graph(limit, analyze)?;

        // Map OID → set of causes (for annotation)
        let mut causes_of: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for e in &graph.edges {
            if e.kind == "causal" {
                causes_of
                    .entry(e.to.as_str())
                    .or_default()
                    .push(e.from.as_str());
            }
        }

        // Warn when analyze mode couldn't use Claude
        if analyze {
            let analyzed_count = graph.nodes.iter().filter(|n| n.intent_source == "analyzed").count();
            if analyzed_count == 0 {
                eprintln!(
                    "  [intent-graph] ANTHROPIC_API_KEY not set or no commits processed — \
                     intents are stored prompts / commit messages. \
                     Set ANTHROPIC_API_KEY to enable Claude analysis."
                );
            } else {
                let fallback_count = graph.nodes.len() - analyzed_count;
                if fallback_count > 0 {
                    eprintln!(
                        "  [intent-graph] {}/{} intents generated by Claude ({} fell back to stored data).",
                        analyzed_count,
                        graph.nodes.len(),
                        fallback_count
                    );
                }
            }
        }

        let mode_label = if analyze { "analyze (Claude)" } else { "prompt" };
        println!(
            "{}",
            style(format!(
                "Intent Graph ─ {} commits, mode: {} ──────────────────────────",
                graph.nodes.len(),
                mode_label
            ))
            .bold()
        );

        for node in &graph.nodes {
            let oid_s = if node.is_ai {
                style(&node.short_oid).magenta().bold()
            } else {
                style(&node.short_oid).blue().bold()
            };
            let intent_s = match node.intent_source.as_str() {
                "analyzed" => style(format!("\"{}\"", &node.intent)).green().italic(),
                "prompt"   => style(format!("\"{}\"", &node.intent)).cyan().italic(),
                _          => style(format!("\"{}\"", &node.intent)).dim().italic(),
            };
            let src_tag = match node.intent_source.as_str() {
                "analyzed" => style("[Claude]").green().dim(),
                "prompt"   => style("[prompt]").cyan().dim(),
                _          => style("[msg]").dim(),
            };
            println!("\n  {} {} {}", oid_s, src_tag, intent_s);
            println!("     {}", style(&node.message).dim());

            if let Some(causes) = causes_of.get(node.oid.as_str()) {
                let shorts: Vec<String> = causes
                    .iter()
                    .map(|c| c[..8.min(c.len())].to_string())
                    .collect();
                println!(
                    "     {} {}",
                    style("↤ caused by:").yellow(),
                    style(shorts.join(", ")).yellow().bold()
                );
            }
            if let Some(ref a) = node.agent {
                println!("     {}", style(format!("agent: {a}")).dim());
            }
        }

        println!("\n{}", style("─".repeat(60)).dim());
        let causal_count = graph.edges.iter().filter(|e| e.kind == "causal").count();
        let ai_count = graph.nodes.iter().filter(|n| n.is_ai).count();
        let analyzed_count = graph.nodes.iter().filter(|n| n.intent_source == "analyzed").count();
        print!("{} AI commits, {} causal link{}", ai_count, causal_count,
            if causal_count == 1 { "" } else { "s" });
        if analyze && analyzed_count > 0 {
            print!(", {} Claude-generated intent{}", analyzed_count,
                if analyzed_count == 1 { "" } else { "s" });
        }
        println!();
        Ok(())
    }

    /// Creates a revert commit for the given OID using `git revert --no-edit`.
    /// Returns the OID of the newly created revert commit.
    pub fn revert_commit(&self, oid: Oid) -> Result<Oid, H5iError> {
        let workdir = self
            .git_repo
            .workdir()
            .ok_or_else(|| H5iError::InvalidPath("Cannot revert in a bare repository".into()))?;

        let output = std::process::Command::new("git")
            .args(["revert", "--no-edit", &oid.to_string()])
            .current_dir(workdir)
            .output()
            .map_err(H5iError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(H5iError::Git(git2::Error::from_str(&format!(
                "git revert failed: {stderr}"
            ))));
        }

        Ok(self.git_repo.head()?.peel_to_commit()?.id())
    }

    // Provides opportunity to edit the message of a previously made commit.
    // Returns the OID of the newly edited commit and a map of all old → new OIDs
    // for every commit that was rewritten (including descendants of the target).
    // Callers that hold OIDs to downstream commits must remap them using this map.
    pub fn edit_commit_message(
        &self,
        oid: Oid,
        new_message: &str,
    ) -> Result<(Oid, HashMap<git2::Oid, git2::Oid>), H5iError> {
        // ── Phase 1: BFS from HEAD to find the target ────────────────────
        // `path` is ordered [target, ..., HEAD].

        let head_commit = self.get_head_commit()?;

        let path: Vec<git2::Oid> = if head_commit.id() == oid {
            vec![oid]
        } else {
            let mut found: Option<Vec<git2::Oid>> = None;
            let mut queue: std::collections::VecDeque<(git2::Oid, Vec<git2::Oid>)> =
                std::collections::VecDeque::new();
            let mut visited: std::collections::HashSet<git2::Oid> =
                std::collections::HashSet::new();

            queue.push_back((head_commit.id(), vec![head_commit.id()]));
            visited.insert(head_commit.id());

            'bfs: while let Some((current_oid, current_path)) = queue.pop_front() {
                let commit = self.git_repo.find_commit(current_oid)?;
                for i in 0..commit.parent_count() {
                    let parent_oid = commit.parent_id(i)?;
                    if parent_oid == oid {
                        let mut p = current_path.clone();
                        p.push(parent_oid);
                        found = Some(p);
                        break 'bfs;
                    }
                    if visited.insert(parent_oid) {
                        let mut p = current_path.clone();
                        p.push(parent_oid);
                        queue.push_back((parent_oid, p));
                    }
                }
            }

            match found {
                Some(mut p) => {
                    p.reverse(); // now [target, ..., HEAD]
                    p
                }
                None => {
                    return Err(H5iError::Git(git2::Error::from_str(&format!(
                        "Commit {} is not reachable from HEAD",
                        oid
                    ))));
                }
            }
        };

        // ── Phase 2: Rewrite the target commit ───────────────────────────

        let target_commit = self.git_repo.find_commit(oid)?;

        let parent_commits: Vec<git2::Commit> = (0..target_commit.parent_count())
            .map(|i| target_commit.parent(i).map_err(H5iError::Git))
            .collect::<Result<Vec<_>, _>>()?;
        let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();

        let new_target_oid = self.git_repo.commit(
            None,
            &target_commit.author(),
            &target_commit.committer(),
            new_message,
            &target_commit.tree()?,
            &parent_refs,
        )?;

        self.rewrite_note_oid(oid, new_target_oid)?;

        // ── Phase 3: Rewrite each ancestor back up to HEAD ───────────────

        let mut oid_map: std::collections::HashMap<git2::Oid, git2::Oid> =
            std::collections::HashMap::new();
        oid_map.insert(oid, new_target_oid);

        let mut previous_new_oid = new_target_oid;

        for &old_oid in path.iter().skip(1) {
            let old_commit = self.git_repo.find_commit(old_oid)?;

            let new_parents: Vec<git2::Commit> = (0..old_commit.parent_count())
                .map(|i| {
                    let p_oid = old_commit.parent_id(i)?;
                    let remapped = oid_map.get(&p_oid).copied().unwrap_or(p_oid);
                    self.git_repo.find_commit(remapped)
                })
                .collect::<Result<Vec<_>, git2::Error>>()?;
            let new_parent_refs: Vec<&git2::Commit> = new_parents.iter().collect();

            let new_oid = self.git_repo.commit(
                None,
                &old_commit.author(),
                &old_commit.committer(),
                old_commit.message().unwrap_or(""),
                &old_commit.tree()?,
                &new_parent_refs,
            )?;

            oid_map.insert(old_oid, new_oid);
            self.rewrite_note_oid(old_oid, new_oid)?;

            previous_new_oid = new_oid;
        }

        // ── Phase 4: Fix up all causal metadata using the completed oid_map ──
        //
        // Now that every rewritten OID is known, do a single pass over every
        // rewritten commit's note and:
        //   a) update `caused_by` entries that reference any old OID
        //   b) update `causes` entries that reference any old OID
        //   c) update the records of commits outside `path` whose `caused_by`
        //      referenced any old OID (using the `causes` field to find them)

        let sig = self
            .git_repo
            .signature()
            .unwrap_or_else(|_| git2::Signature::now("h5i", "h5i@local").unwrap());

        // Collect the set of new OIDs so we know which notes to patch.
        let new_oids: Vec<git2::Oid> = oid_map.values().copied().collect();

        // a+b) Patch caused_by and causes inside every rewritten note.
        for &new_oid in &new_oids {
            if let Ok(mut record) = self.load_h5i_record(new_oid) {
                let mut changed = false;

                for entry in record.caused_by.iter_mut() {
                    if let Ok(e_oid) = git2::Oid::from_str(entry) {
                        if let Some(&mapped) = oid_map.get(&e_oid) {
                            *entry = mapped.to_string();
                            changed = true;
                        }
                    }
                }

                for entry in record.causes.iter_mut() {
                    if let Ok(e_oid) = git2::Oid::from_str(entry) {
                        if let Some(&mapped) = oid_map.get(&e_oid) {
                            *entry = mapped.to_string();
                            changed = true;
                        }
                    }
                }

                if changed {
                    let updated_json = serde_json::to_string(&record)?;
                    self.git_repo.note(
                        &sig,
                        &sig,
                        Some(H5I_NOTES_REF),
                        new_oid,
                        &updated_json,
                        true,
                    )?;
                }
            }
        }

        // c) For every old OID that was rewritten, find all commits outside
        //    `path` whose `caused_by` referenced that old OID, and update them.
        //    These commits are listed in the `causes` field of the rewritten note.
        for &new_oid in &new_oids {
            if let Ok(record) = self.load_h5i_record(new_oid) {
                // Find the old OID this new_oid was rewritten from.
                let old_oid = match oid_map.iter().find(|(_, &v)| v == new_oid) {
                    Some((&k, _)) => k,
                    None => continue,
                };
                let old_oid_str = old_oid.to_string();
                let new_oid_str = new_oid.to_string();

                for dep_oid_str in &record.causes {
                    if let Ok(dep_oid) = git2::Oid::from_str(dep_oid_str) {
                        // Skip if this dependent was itself rewritten (handled above).
                        if oid_map.contains_key(&dep_oid) {
                            continue;
                        }
                        if let Ok(mut dep_record) = self.load_h5i_record(dep_oid) {
                            let mut changed = false;
                            for entry in dep_record.caused_by.iter_mut() {
                                if *entry == old_oid_str {
                                    *entry = new_oid_str.clone();
                                    changed = true;
                                }
                            }
                            if changed {
                                let updated_json = serde_json::to_string(&dep_record)?;
                                self.git_repo.note(
                                    &sig,
                                    &sig,
                                    Some(H5I_NOTES_REF),
                                    dep_oid,
                                    &updated_json,
                                    true,
                                )?;
                            }
                        }
                    }
                }
            }
        }

        // ── Phase 5: Advance HEAD to the new tip ─────────────────────────

        let new_head_oid = previous_new_oid;
        let head_ref = self.git_repo.head()?;

        if head_ref.is_branch() {
            let branch_name = head_ref
                .shorthand()
                .ok_or_else(|| H5iError::Git(git2::Error::from_str("Cannot read branch name")))?;
            let full_ref = format!("refs/heads/{}", branch_name);
            let new_head_commit = self.git_repo.find_commit(new_head_oid)?;
            self.git_repo.reference(
                &full_ref,
                new_head_oid,
                true,
                &format!("h5i: rewrite message of {}", &oid.to_string()[..8]),
            )?;
            self.git_repo
                .checkout_tree(new_head_commit.as_object(), None)?;
            self.git_repo.set_head(&full_ref)?;
        } else {
            self.git_repo.set_head_detached(new_head_oid)?;
        }

        Ok((new_target_oid, oid_map))
    }

    /// Resolves the current `HEAD` reference and returns the associated commit.
    ///
    /// This method resolves symbolic references and ensures that the
    /// resulting object is a commit.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - `HEAD` cannot be resolved
    /// - the resolved object is not a commit
    fn get_head_commit(&self) -> Result<Commit<'_>, git2::Error> {
        let obj = self.git_repo.head()?.resolve()?.peel(ObjectType::Commit)?;
        obj.into_commit()
            .map_err(|_| git2::Error::from_str("Not a commit"))
    }

    /// Retrieves the `Blob` (file object) for a given path from the `HEAD` commit.
    ///
    /// # Parameters
    ///
    /// - `path` – Path to the file within the repository.
    ///
    /// # Returns
    ///
    /// Returns the Git blob representing the file contents at `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the path does not exist in `HEAD`
    /// - the path does not correspond to a file
    /// - the blob cannot be retrieved from the repository
    pub fn get_blob_at_head(&self, path: &Path) -> Result<Blob<'_>, H5iError> {
        // 1. Resolve the HEAD reference to a commit
        let head_commit = self.get_head_commit()?;

        // 2. Retrieve the tree (snapshot of the file structure)
        let tree = head_commit.tree()?;

        // 3. Locate the entry corresponding to the specified path
        let entry = tree
            .get_path(path)
            .map_err(|_| H5iError::RecordNotFound(format!("Path not found in HEAD: {:?}", path)))?;

        // 4. Ensure that the entry is a Blob (file)
        if entry.kind() != Some(ObjectType::Blob) {
            return Err(H5iError::Ast(format!(
                "Path is not a file (blob): {:?}",
                path
            )));
        }

        // 5. Retrieve the actual Blob object using its OID
        let blob = self.git_repo.find_blob(entry.id())?;
        Ok(blob)
    }

    /// Retrieves the `Blob` associated with a given path at a specific commit.
    ///
    /// # Parameters
    ///
    /// - `oid` – Commit OID.
    /// - `path` – File path within the repository.
    ///
    /// # Returns
    ///
    /// Returns the Git blob representing the file contents at the specified commit.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the commit cannot be found
    /// - the path does not exist in the commit tree
    /// - the blob object cannot be retrieved
    pub fn get_blob_at_oid(&'_ self, oid: Oid, path: &Path) -> Result<Blob<'_>, H5iError> {
        // 1. Locate the commit object from the OID
        let commit = self
            .git_repo
            .find_commit(oid)
            .map_err(|e| H5iError::Internal(format!("Commit not found {}: {}", oid, e)))?;

        // 2. Retrieve the tree associated with the commit
        let tree = commit.tree().map_err(|e| {
            H5iError::Internal(format!("Failed to get tree for commit {}: {}", oid, e))
        })?;

        // 3. Find the entry corresponding to the specified path
        let entry = tree.get_path(path).map_err(|_| {
            H5iError::InvalidPath(format!("Path {:?} not found in commit {}", path, oid))
        })?;

        // 4. Retrieve the Blob object from its ID
        let blob = self.git_repo.find_blob(entry.id()).map_err(|e| {
            H5iError::Internal(format!("Failed to find blob for path {:?}: {}", path, e))
        })?;

        Ok(blob)
    }

    /// Convenience helper that retrieves file content at a specific commit
    /// and returns it as a UTF-8 string.
    ///
    /// # Parameters
    ///
    /// - `oid` – Commit OID.
    /// - `path` – File path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the file cannot be retrieved
    /// - the file content is not valid UTF-8
    pub fn get_content_at_oid(&self, oid: Oid, path: &Path) -> Result<String, H5iError> {
        let blob = self.get_blob_at_oid(oid, path)?;
        let content = std::str::from_utf8(blob.content())
            .map_err(|_| H5iError::Internal(format!("File at {:?} is not valid UTF-8", path)))?;

        Ok(content.to_string())
    }

    pub fn get_content_at_head(&self, file_path: &str) -> Result<String, H5iError> {
        let repo = &self.git_repo;

        let head = repo.head()?;
        let head_commit = head.peel_to_commit()?;

        let tree = head_commit.tree()?;

        let entry = tree.get_path(Path::new(file_path))?;
        let object = entry.to_object(repo)?;
        let blob = object.as_blob().ok_or_else(|| {
            H5iError::Internal(format!(
                "Path {} exists but is not a file (blob)",
                file_path
            ))
        })?;

        let content = std::str::from_utf8(blob.content())
            .map_err(|e| H5iError::Internal(format!("Content is not valid UTF-8: {}", e)))?;

        Ok(content.to_string())
    }

    /// Extracts the code block between
    /// `// h5_i_test_start` and `// h5_i_test_end` and computes its hash.
    ///
    /// This method is used to identify the logical content of a test suite.
    /// The resulting hash can be stored in commit metadata to track
    /// changes to tests independently of the main source code.
    fn scan_test_block(&self, path: &Path) -> Option<TestMetrics> {
        let content = std::fs::read_to_string(path).ok()?;
        let start = "// h5_i_test_start";
        let end = "// h5_i_test_end";

        if let (Some(s_idx), Some(e_idx)) = (content.find(start), content.find(end)) {
            let test_code = &content[s_idx + start.len()..e_idx];
            let mut hasher = sha2::Sha256::new();
            use sha2::Digest;
            hasher.update(test_code.trim().as_bytes());
            let suite_hash = format!("{:x}", hasher.finalize());

            Some(TestMetrics {
                test_suite_hash: suite_hash,
                tool: Some("marker-scan".into()),
                summary: Some(format!(
                    "marker block detected in {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                )),
                ..Default::default()
            })
        } else {
            None
        }
    }

    /// Stores an externally provided S-expression (AST) into the `.h5i` sidecar.
    ///
    /// The AST is stored using **content-addressed storage**.
    /// If the same AST content already exists, it will share the same hash.
    ///
    /// # Storage Layout
    ///
    /// ```text
    /// .h5i/ast/<hash>.sexp
    /// ```
    ///
    /// # Parameters
    ///
    /// - `_file_path` – Source file path (currently unused but reserved for future indexing).
    /// - `sexp` – Serialized AST represented as an S-expression.
    ///
    /// # Returns
    ///
    /// Returns the content hash of the stored AST.
    pub fn save_ast_to_sidecar(&self, _file_path: &str, sexp: &str) -> Result<String, H5iError> {
        // Compute the content hash of the S-expression
        let mut hasher = Sha256::new();
        hasher.update(sexp.as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        let filename = format!("{}.sexp", hash);

        // Check if already stored (dedup).
        if let Ok(r) = self.git_repo.find_reference(H5I_AST_REF) {
            if let Ok(commit) = r.peel_to_commit() {
                if commit.tree().map(|t| t.get_name(&filename).is_some()).unwrap_or(false) {
                    return Ok(hash);
                }
            }
        }

        // Create a blob for the S-expression content.
        let blob_oid = self.git_repo.blob(sexp.as_bytes())?;

        // Build a new tree: start from the existing tree (if any) and insert the new blob.
        let parent_commit = self.git_repo
            .find_reference(H5I_AST_REF)
            .ok()
            .and_then(|r| r.peel_to_commit().ok());

        let base_tree = parent_commit.as_ref().and_then(|c| c.tree().ok());
        let mut builder = self.git_repo.treebuilder(base_tree.as_ref())?;
        builder.insert(&filename, blob_oid, 0o100644)?;
        let tree_oid = builder.write()?;
        let tree = self.git_repo.find_tree(tree_oid)?;

        let sig = self.git_repo.signature().unwrap_or_else(|_| {
            git2::Signature::now("h5i", "h5i@local").unwrap()
        });
        let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
        self.git_repo.commit(
            Some(H5I_AST_REF),
            &sig,
            &sig,
            &format!("ast: store {}", &hash[..12]),
            &tree,
            &parents,
        )?;

        Ok(hash)
    }

    /// Load an AST s-expression by its content hash from `refs/h5i/ast`.
    fn load_ast_blob(&self, hash: &str) -> Option<String> {
        let filename = format!("{}.sexp", hash);
        let r = self.git_repo.find_reference(H5I_AST_REF).ok()?;
        let commit = r.peel_to_commit().ok()?;
        let tree = commit.tree().ok()?;
        let entry = tree.get_name(&filename)?;
        let obj = entry.to_object(&self.git_repo).ok()?;
        let blob = obj.as_blob()?;
        std::str::from_utf8(blob.content()).ok().map(|s| s.to_string())
    }

    /// Extracts test code between
    /// `// h5_i_test_start` and `// h5_i_test_end`
    /// and produces test-related metrics.
    ///
    /// The extracted code is hashed to detect logical changes in the
    /// test suite across commits.
    ///
    /// In production usage, coverage and runtime metrics may be
    /// integrated from external CI systems.
    pub fn scan_test_metrics(&self, path: &std::path::Path) -> Option<TestMetrics> {
        self.scan_test_block(path)
    }

    /// Load a [`TestMetrics`] record from a JSON file written by any test adapter.
    ///
    /// The file must contain a JSON object matching the [`TestResultInput`] schema.
    /// Missing fields default to zero / `None`.
    ///
    /// # Example adapter output
    /// ```json
    /// { "tool": "pytest", "passed": 10, "failed": 0, "duration_secs": 1.23 }
    /// ```
    pub fn load_test_results_from_file(&self, path: &Path) -> Result<TestMetrics, H5iError> {
        use crate::metadata::TestResultInput;
        let raw = fs::read_to_string(path)
            .map_err(|e| H5iError::Internal(format!("Cannot read test results file: {e}")))?;
        let input: TestResultInput = serde_json::from_str(&raw)
            .map_err(|e| H5iError::Internal(format!("Invalid test results JSON: {e}")))?;
        Ok(input.into_metrics(String::new()))
    }

    /// Run an arbitrary shell command and return [`TestMetrics`].
    ///
    /// The command's **stdout** is parsed as a [`TestResultInput`] JSON object
    /// when it is valid JSON.  If parsing fails, only the exit code is captured,
    /// making this useful even for test tools that produce no structured output.
    ///
    /// # Example
    /// ```rust,ignore
    /// let metrics = repo.run_test_command("cargo test 2>&1 | h5i-cargo-test-adapter")?;
    /// ```
    pub fn run_test_command(&self, cmd: &str) -> Result<TestMetrics, H5iError> {
        use crate::metadata::TestResultInput;
        use std::process::Command;

        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .map_err(|e| H5iError::Internal(format!("Failed to run test command: {e}")))?;

        let exit_code = output.status.code();
        let stdout = String::from_utf8_lossy(&output.stdout);

        // Try to parse stdout as TestResultInput JSON
        if let Ok(input) = serde_json::from_str::<TestResultInput>(stdout.trim()) {
            let mut metrics = input.into_metrics(String::new());
            // The exit code from the actual process takes precedence
            if exit_code.is_some() {
                metrics.exit_code = exit_code;
            }
            return Ok(metrics);
        }

        // Fallback: capture exit code and a brief summary from combined output
        let combined = format!("{}{}", stdout, String::from_utf8_lossy(&output.stderr));
        let summary_line = combined
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("(no output)")
            .to_string();

        Ok(TestMetrics {
            exit_code,
            summary: Some(summary_line),
            tool: Some(cmd.split_whitespace().next().unwrap_or(cmd).to_string()),
            ..Default::default()
        })
    }
}

impl H5iRepository {
    /// Runs all integrity rules against the staged diff and returns a report.
    ///
    /// Priority for "intent": prompt (if supplied) > commit message.
    /// Scoring: each Violation costs −0.4, each Warning −0.15; score is clamped to [0, 1].
    pub fn verify_integrity(
        &self,
        prompt: Option<&str>,
        message: &str,
    ) -> Result<IntegrityReport, H5iError> {
        use crate::metadata::Severity;
        use crate::rules::run_all_rules;
        use crate::rules::DiffContext;

        let primary_intent = prompt.unwrap_or(message).to_string();

        let diff = self.get_staged_diff()?;
        let stats = diff.stats()?;
        let ctx =
            DiffContext::from_diff(&diff, primary_intent, stats.insertions(), stats.deletions())?;

        let findings = run_all_rules(&ctx);

        let violations = findings
            .iter()
            .filter(|f| f.severity == Severity::Violation)
            .count();
        let warnings = findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count();

        let penalty = (violations as f32 * 0.4 + warnings as f32 * 0.15).min(1.0);
        let score = 1.0 - penalty;

        let level = if violations > 0 {
            IntegrityLevel::Violation
        } else if warnings > 0 {
            IntegrityLevel::Warning
        } else {
            IntegrityLevel::Valid
        };

        Ok(IntegrityReport {
            level,
            score,
            findings,
        })
    }

    /// Run integrity rules against a *historical* commit's own diff (parent→commit).
    ///
    /// Unlike [`verify_integrity`], this does not touch the staging area; it
    /// reconstructs the diff from Git objects so it works on any committed OID.
    pub fn verify_commit_integrity(&self, oid: git2::Oid) -> Result<IntegrityReport, H5iError> {
        use crate::metadata::{IntegrityLevel, Severity};
        use crate::rules::{run_all_rules, DiffContext};

        let commit = self.git_repo.find_commit(oid)?;
        let message = commit.message().unwrap_or("").to_string();

        // Prefer the stored h5i prompt; fall back to commit message as intent.
        let record = self.load_h5i_record(oid).ok();
        let prompt_owned: Option<String> = record
            .as_ref()
            .and_then(|r| r.ai_metadata.as_ref())
            .map(|a| a.prompt.clone())
            .filter(|p| !p.is_empty());
        let primary_intent = prompt_owned.clone().unwrap_or_else(|| message.clone());

        // Build the diff: parent tree → commit tree (root commits diff to empty).
        let commit_tree = commit.tree()?;
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let diff =
            self.git_repo
                .diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), None)?;

        let stats = diff.stats()?;
        let ctx =
            DiffContext::from_diff(&diff, primary_intent, stats.insertions(), stats.deletions())?;

        let findings = run_all_rules(&ctx);

        let violations = findings
            .iter()
            .filter(|f| f.severity == Severity::Violation)
            .count();
        let warnings = findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count();

        let penalty = (violations as f32 * 0.4 + warnings as f32 * 0.15).min(1.0);
        let score = 1.0 - penalty;

        let level = if violations > 0 {
            IntegrityLevel::Violation
        } else if warnings > 0 {
            IntegrityLevel::Warning
        } else {
            IntegrityLevel::Valid
        };

        Ok(IntegrityReport {
            level,
            score,
            findings,
        })
    }

    fn get_staged_diff(&'_ self) -> Result<git2::Diff<'_>, H5iError> {
        let head_tree = self.get_head_commit()?.tree()?;
        let index = self.git_repo.index()?;
        let mut opts = git2::DiffOptions::new();
        let diff =
            self.git_repo
                .diff_tree_to_index(Some(&head_tree), Some(&index), Some(&mut opts))?;
        Ok(diff)
    }

    // ── Suggested Review Points ───────────────────────────────────────────────

    /// Scans recent commits and returns those that warrant human review, ranked
    /// by review priority.
    ///
    /// Each commit is scored against a set of deterministic, language-agnostic
    /// rules.  Only commits whose aggregate score is ≥ `min_score` are returned.
    /// Pass `crate::review::REVIEW_THRESHOLD` as a sensible default.
    ///
    /// Rules applied (all are purely structural / metric-based, no AI required):
    ///
    /// | Rule ID           | Signal                                            |
    /// |-------------------|---------------------------------------------------|
    /// | LARGE_DIFF        | Many lines changed (>50 / >200 / >500)           |
    /// | WIDE_IMPACT       | Many files changed (>5 / >10 / >20)              |
    /// | CROSS_CUTTING     | Changes span many top-level directories (>3 / >5)|
    /// | TEST_REGRESSION   | Test failures increased or coverage dropped       |
    /// | UNTESTED_CHANGE   | Large diff with no test metrics recorded          |
    /// | AI_NO_PROMPT      | AI commit with blank prompt (provenance gap)      |
    /// | BURST_AFTER_GAP   | First commit after a quiet period (>3 / >7 days) |
    /// | POLYGLOT_CHANGE   | More than 4 distinct file extensions changed      |
    /// | BINARY_FILE       | Binary file(s) modified                           |
    /// | MASS_DELETION     | >80 % of the diff is deletions (>100 lines)      |
    /// | BLIND_EDIT        | File(s) edited with no prior Read in the session  |
    pub fn suggest_review_points(
        &self,
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<crate::review::ReviewPoint>, H5iError> {
        use crate::review::{ReviewPoint, ReviewTrigger};
        use std::collections::HashSet;

        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        let mut results: Vec<ReviewPoint> = Vec::new();

        for oid_result in revwalk.take(limit) {
            let oid = oid_result?;
            let commit = self.git_repo.find_commit(oid)?;

            let message = commit.message().unwrap_or("").trim().to_string();
            let author = commit.author().name().unwrap_or("Unknown").to_string();
            let record = self.load_h5i_record(oid).ok();

            let timestamp = record.as_ref().map(|r| r.timestamp).unwrap_or_else(|| {
                chrono::Utc
                    .timestamp_opt(commit.time().seconds(), 0)
                    .single()
                    .unwrap_or_else(chrono::Utc::now)
            });

            let mut triggers: Vec<ReviewTrigger> = Vec::new();

            // ── Diff stats ────────────────────────────────────────────────────
            let commit_tree = commit.tree()?;
            let parent_tree = if commit.parent_count() > 0 {
                Some(commit.parent(0)?.tree()?)
            } else {
                None
            };
            let diff = self.git_repo.diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&commit_tree),
                None,
            )?;
            let stats = diff.stats()?;
            let files_changed = stats.files_changed();
            let insertions = stats.insertions();
            let deletions = stats.deletions();
            let lines_changed = insertions + deletions;

            // Collect file paths and binary file count from the diff.
            // Auto-generated / build-artifact paths are excluded from all counts so
            // they don't inflate risk scores with noise.
            let mut file_paths: Vec<String> = Vec::new();
            let mut binary_count: usize = 0;
            for delta in diff.deltas() {
                let path = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .and_then(|p| p.to_str())
                    .map(|s| s.to_string());
                // Skip auto-generated / build-artifact files entirely.
                if path.as_deref().map(is_artifact_path).unwrap_or(false) {
                    continue;
                }
                if let Some(ref p) = path {
                    file_paths.push(p.clone());
                }
                if delta.flags().contains(git2::DiffFlags::BINARY) {
                    binary_count += 1;
                }
            }

            // R1 — LARGE_DIFF
            if lines_changed > 500 {
                triggers.push(ReviewTrigger {
                    rule_id: "LARGE_DIFF".into(),
                    weight: 0.40,
                    detail: format!("{lines_changed} lines changed (>500)"),
                });
            } else if lines_changed > 200 {
                triggers.push(ReviewTrigger {
                    rule_id: "LARGE_DIFF".into(),
                    weight: 0.25,
                    detail: format!("{lines_changed} lines changed (>200)"),
                });
            } else if lines_changed > 50 {
                triggers.push(ReviewTrigger {
                    rule_id: "LARGE_DIFF".into(),
                    weight: 0.10,
                    detail: format!("{lines_changed} lines changed (>50)"),
                });
            }

            // R2 — WIDE_IMPACT
            if files_changed > 20 {
                triggers.push(ReviewTrigger {
                    rule_id: "WIDE_IMPACT".into(),
                    weight: 0.35,
                    detail: format!("{files_changed} files changed (>20)"),
                });
            } else if files_changed > 10 {
                triggers.push(ReviewTrigger {
                    rule_id: "WIDE_IMPACT".into(),
                    weight: 0.20,
                    detail: format!("{files_changed} files changed (>10)"),
                });
            } else if files_changed > 5 {
                triggers.push(ReviewTrigger {
                    rule_id: "WIDE_IMPACT".into(),
                    weight: 0.10,
                    detail: format!("{files_changed} files changed (>5)"),
                });
            }

            // R3 — CROSS_CUTTING: distinct top-level directory components
            let distinct_dirs: HashSet<&str> = file_paths
                .iter()
                .filter_map(|p| p.split('/').next())
                .collect();
            let dir_count = distinct_dirs.len();
            if dir_count > 5 {
                triggers.push(ReviewTrigger {
                    rule_id: "CROSS_CUTTING".into(),
                    weight: 0.25,
                    detail: format!("changes span {dir_count} top-level directories (>5)"),
                });
            } else if dir_count > 3 {
                triggers.push(ReviewTrigger {
                    rule_id: "CROSS_CUTTING".into(),
                    weight: 0.15,
                    detail: format!("changes span {dir_count} top-level directories (>3)"),
                });
            }

            // R4 — TEST_REGRESSION: compare metrics to parent commit
            if let Some(ref rec) = record {
                if let Some(ref current_tm) = rec.test_metrics {
                    let parent_tm = rec
                        .parent_oid
                        .as_ref()
                        .and_then(|p| git2::Oid::from_str(p).ok())
                        .and_then(|p| self.load_h5i_record(p).ok())
                        .and_then(|r| r.test_metrics);

                    if let Some(ref prev_tm) = parent_tm {
                        let was_passing = prev_tm.is_passing();
                        let is_passing = current_tm.is_passing();

                        if was_passing && !is_passing {
                            triggers.push(ReviewTrigger {
                                rule_id: "TEST_REGRESSION".into(),
                                weight: 0.50,
                                detail: "tests were passing but now failing".into(),
                            });
                        } else if current_tm.failed > prev_tm.failed {
                            let new_fails = current_tm.failed - prev_tm.failed;
                            triggers.push(ReviewTrigger {
                                rule_id: "TEST_REGRESSION".into(),
                                weight: 0.40,
                                detail: format!("{new_fails} new test failure(s) since parent"),
                            });
                        }

                        if prev_tm.coverage > 0.0 && current_tm.coverage > 0.0 {
                            let drop = prev_tm.coverage - current_tm.coverage;
                            if drop > 10.0 {
                                triggers.push(ReviewTrigger {
                                    rule_id: "TEST_REGRESSION".into(),
                                    weight: 0.35,
                                    detail: format!("coverage dropped {drop:.1}% (>10%)"),
                                });
                            } else if drop > 5.0 {
                                triggers.push(ReviewTrigger {
                                    rule_id: "TEST_REGRESSION".into(),
                                    weight: 0.20,
                                    detail: format!("coverage dropped {drop:.1}% (>5%)"),
                                });
                            }
                        }
                    }
                }
            }

            // R5 — UNTESTED_CHANGE: significant diff without any test metrics
            if lines_changed > 100 {
                let has_tests = record
                    .as_ref()
                    .map(|r| r.test_metrics.is_some())
                    .unwrap_or(false);
                if !has_tests {
                    triggers.push(ReviewTrigger {
                        rule_id: "UNTESTED_CHANGE".into(),
                        weight: 0.20,
                        detail: format!("{lines_changed} lines changed with no test metrics recorded"),
                    });
                }
            }

            // R6 — AI_NO_PROMPT: AI commit without a recorded prompt
            if let Some(ref rec) = record {
                if let Some(ref ai) = rec.ai_metadata {
                    if ai.prompt.trim().is_empty() {
                        triggers.push(ReviewTrigger {
                            rule_id: "AI_NO_PROMPT".into(),
                            weight: 0.15,
                            detail: "AI-generated commit with no prompt recorded (provenance gap)".into(),
                        });
                    }
                }
            }

            // R7 — BURST_AFTER_GAP: large time gap between this commit and its parent
            if commit.parent_count() > 0 {
                if let Ok(parent_commit) = commit.parent(0) {
                    let gap_secs = commit.time().seconds() - parent_commit.time().seconds();
                    if gap_secs > 7 * 24 * 3600 {
                        let days = gap_secs / (24 * 3600);
                        triggers.push(ReviewTrigger {
                            rule_id: "BURST_AFTER_GAP".into(),
                            weight: 0.25,
                            detail: format!("first commit after a {days}-day gap (>7 days)"),
                        });
                    } else if gap_secs > 3 * 24 * 3600 {
                        let days = gap_secs / (24 * 3600);
                        triggers.push(ReviewTrigger {
                            rule_id: "BURST_AFTER_GAP".into(),
                            weight: 0.15,
                            detail: format!("first commit after a {days}-day gap (>3 days)"),
                        });
                    }
                }
            }

            // R8 — POLYGLOT_CHANGE: many distinct file extensions
            let extensions: HashSet<&str> = file_paths
                .iter()
                .filter_map(|p| std::path::Path::new(p).extension()?.to_str())
                .collect();
            if extensions.len() > 4 {
                triggers.push(ReviewTrigger {
                    rule_id: "POLYGLOT_CHANGE".into(),
                    weight: 0.15,
                    detail: format!(
                        "{} distinct file type(s) changed (harder to review holistically)",
                        extensions.len()
                    ),
                });
            }

            // R9 — BINARY_FILE: opaque binary changes
            if binary_count > 0 {
                triggers.push(ReviewTrigger {
                    rule_id: "BINARY_FILE".into(),
                    weight: 0.20,
                    detail: format!("{binary_count} binary file(s) modified"),
                });
            }

            // R10 — MASS_DELETION: bulk removal without matching insertions
            if deletions > 100 && lines_changed > 0 {
                let deletion_ratio = deletions as f32 / lines_changed as f32;
                if deletion_ratio > 0.80 {
                    triggers.push(ReviewTrigger {
                        rule_id: "MASS_DELETION".into(),
                        weight: 0.15,
                        detail: format!(
                            "{deletions} lines deleted ({:.0}% of total changes)",
                            deletion_ratio * 100.0
                        ),
                    });
                }
            }

            // R11 — BLIND_EDIT: files edited without a prior Read in the session
            if let Ok(Some(analysis)) =
                crate::session_log::load_analysis(&self.h5i_root, &oid.to_string())
            {
                let blind_files: Vec<&str> = analysis
                    .coverage
                    .iter()
                    .filter(|c| c.blind_edit_count > 0)
                    .map(|c| c.file.as_str())
                    .collect();
                if !blind_files.is_empty() {
                    let count = blind_files.len();
                    let examples = blind_files[..3.min(count)].join(", ");
                    let suffix = if count > 3 {
                        format!(" (and {} more)", count - 3)
                    } else {
                        String::new()
                    };
                    triggers.push(ReviewTrigger {
                        rule_id: "BLIND_EDIT".into(),
                        weight: (0.10 * count as f32).min(0.30),
                        detail: format!(
                            "{count} file(s) edited without a prior Read: {examples}{suffix}"
                        ),
                    });
                }
            }

            // ── Aggregate & filter ────────────────────────────────────────────
            if triggers.is_empty() {
                continue;
            }
            let score: f32 = triggers.iter().map(|t| t.weight).sum::<f32>().min(1.0);
            if score >= min_score {
                results.push(ReviewPoint {
                    commit_oid: oid.to_string(),
                    short_oid: oid.to_string()[..8].to_string(),
                    message,
                    author,
                    timestamp,
                    score,
                    triggers,
                });
            }
        }

        // Sort highest priority first
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok(results)
    }
}

// ── Artifact path filter ──────────────────────────────────────────────────────

/// Returns `true` when `path` is a well-known build artifact or auto-generated
/// file that should be excluded from review risk scoring.
///
/// Covers the most common ecosystems:
/// - Python: `__pycache__/`, `*.pyc`, `*.pyo`, `.pytest_cache/`, `*.egg-info/`
/// - JavaScript/TypeScript: `node_modules/`, `dist/`, `*.min.js`, `.next/`
/// - Java/Kotlin: `*.class`, `*.jar`, `build/`, `target/`
/// - Rust: `target/`
/// - Go: vendor artefacts
/// - General: `.DS_Store`, `Thumbs.db`, `*.lock` lock-file binaries
fn is_artifact_path(path: &str) -> bool {
    // Check path components (any segment matching these is an artifact dir)
    const ARTIFACT_DIRS: &[&str] = &[
        "__pycache__",
        ".pytest_cache",
        "node_modules",
        ".next",
        ".nuxt",
        "dist",
        ".eggs",
        ".tox",
        ".mypy_cache",
        ".ruff_cache",
    ];

    // Suffix-based checks
    const ARTIFACT_EXTENSIONS: &[&str] = &[
        ".pyc",
        ".pyo",
        ".class",
        ".jar",
        ".war",
        ".ear",
        ".min.js",
        ".min.css",
        ".map",       // JS source maps
    ];

    // Exact filename matches
    const ARTIFACT_FILENAMES: &[&str] = &[
        ".DS_Store",
        "Thumbs.db",
        "desktop.ini",
    ];

    // Check directory segments
    for segment in path.split('/') {
        if ARTIFACT_DIRS.contains(&segment) {
            return true;
        }
        // *.egg-info directories
        if segment.ends_with(".egg-info") || segment.ends_with(".dist-info") {
            return true;
        }
    }

    // Check extension
    let lower = path.to_ascii_lowercase();
    for ext in ARTIFACT_EXTENSIONS {
        if lower.ends_with(ext) {
            return true;
        }
    }

    // Check filename
    if let Some(filename) = path.split('/').last() {
        if ARTIFACT_FILENAMES.contains(&filename) {
            return true;
        }
    }

    false
}

// ── Parser script discovery ───────────────────────────────────────────────────

/// Searches for `script_name` in the standard locations and returns the first
/// path that exists.
fn find_parser_script(
    script_name: &str,
    workdir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    // 1. Explicit override via environment variable.
    if let Ok(dir) = std::env::var("H5I_PARSER_DIR") {
        let p = std::path::Path::new(&dir).join(script_name);
        if p.exists() {
            return Some(p);
        }
    }

    // 2. `script/` inside the repository working directory.
    if let Some(wd) = workdir {
        let p = wd.join("script").join(script_name);
        if p.exists() {
            return Some(p);
        }
    }

    // 3. Relative to the h5i binary (`<bin_dir>/../script/` for development builds,
    //    `<bin_dir>/script/` for flat installs).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            for candidate in &[
                bin_dir.join("script").join(script_name),
                bin_dir.join("..").join("script").join(script_name),
            ] {
                if candidate.exists() {
                    return Some(candidate.clone());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Repository, Signature};
    use std::fs;
    use tempfile::tempdir;
    use yrs::ReadTxn;
    use yrs::{Doc, Text, Transact, Update};

    fn setup_test_repo(root: &std::path::Path) -> H5iRepository {
        let _repo = Repository::init(root).unwrap();
        H5iRepository::open(root).expect("Failed to open repo")
    }

    fn create_commit(
        repo: &Repository,
        message: &str,
        file_path: &str,
        content: &str,
        parents: &[&git2::Commit],
    ) -> Oid {
        let mut index = repo.index().unwrap();
        let path = std::path::Path::new(file_path);

        fs::write(repo.workdir().unwrap().join(path), content).unwrap();
        index.add_path(path).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        let sig = Signature::now("test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, parents)
            .unwrap()
    }

    // --- 1. Lifecycle & Basic Info ---

    #[test]
    fn test_repository_open_initializes_directories() {
        let dir = tempdir().unwrap();
        let repo = setup_test_repo(dir.path());

        // Ensure .h5i subdirectories are created
        assert!(repo.h5i_root.join("metadata").exists());
        assert!(repo.h5i_root.join("crdt").exists());
        assert_eq!(repo.h5i_path(), &repo.h5i_root);
    }

    // --- 2. Commit & Metadata Persistence ---

    #[test]
    fn test_commit_with_ai_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let sig = Signature::now("ai_agent", "bot@h5i.io")?;

        let ai_meta = Some(AiMetadata {
            model_name: "h5i-alpha-01".to_string(),
            prompt: "abc123hash".to_string(),
            agent_id: "agent_7".to_string(),
            usage: None,
        });

        // Prepare a staged file
        fs::write(dir.path().join("logic.py"), "print('hello')")?;
        let mut index = h5i_repo.git().index()?;
        index.add_path(Path::new("logic.py"))?;
        index.write()?;

        let oid = h5i_repo.commit(
            "AI generated commit",
            &sig,
            &sig,
            ai_meta,
            TestSource::None,
            None, // ast_parser
            vec![],
            vec![],
        )?;

        // Verify standard git commit
        let commit = h5i_repo.git().find_commit(oid)?;
        assert_eq!(commit.message(), Some("AI generated commit"));

        // Verify h5i sidecar record
        let record = h5i_repo.load_h5i_record(oid)?;
        assert_eq!(record.ai_metadata.unwrap().agent_id, "agent_7");
        Ok(())
    }

    #[test]
    fn test_load_h5i_record_fallback_to_git() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());

        // Create a commit without using h5i_repo.commit (no sidecar)
        let oid = create_commit(
            h5i_repo.git(),
            "legacy commit",
            "legacy.txt",
            "old data",
            &vec![],
        );

        // h5i_log should fallback to minimal record
        let logs = h5i_repo.h5i_log(1).unwrap();
        assert_eq!(logs[0].git_oid, oid.to_string());
        assert!(logs[0].ai_metadata.is_none());
    }

    // --- 3. Blame & AST tracking ---

    #[test]
    fn test_blame_line_mode() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let path = Path::new("README.md");

        create_commit(
            h5i_repo.git(),
            "initial",
            "README.md",
            "Line 1\nLine 2",
            &vec![],
        );

        let results = h5i_repo.blame(path, BlameMode::Line).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].line_content, "Line 1");
    }

    #[test]
    fn test_ast_sidecar_storage() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let sexp = "(module (fn main))";

        let hash = h5i_repo.save_ast_to_sidecar("main.rs", sexp).unwrap();

        // Verify content is stored in refs/h5i/ast (Git object store).
        let loaded = h5i_repo.load_ast_blob(&hash);
        assert!(loaded.is_some(), "AST blob should be in refs/h5i/ast");
        assert_eq!(loaded.unwrap(), sexp);

        // Idempotent: storing the same content returns the same hash.
        let hash2 = h5i_repo.save_ast_to_sidecar("other.rs", sexp).unwrap();
        assert_eq!(hash, hash2);
    }

    // --- 4. Merge & CRDT Delta Logic ---

    #[test]
    fn test_persist_and_load_delta_for_commit() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let oid = Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
        let delta_data = vec![1, 2, 3, 4, 5];

        h5i_repo
            .persist_delta_for_commit(oid, "test.txt", &delta_data)
            .unwrap();
        let loaded = h5i_repo
            .load_specific_delta_for_commit(oid, "test.txt")
            .unwrap();

        assert_eq!(loaded, delta_data);
    }

    #[test]
    fn test_get_content_at_oid() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let git_repo = &h5i_repo.git_repo;

        let oid = create_commit(git_repo, "initial", "hello.txt", "hello world", &[]);

        let content = h5i_repo
            .get_content_at_oid(oid, std::path::Path::new("hello.txt"))
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_scan_test_metrics_detection() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let path = dir.path().join("test_file.rs");
        let content = "
            // h5_i_test_start
            fn test_logic() { assert!(true); }
            // h5_i_test_end
        ";
        fs::write(&path, content).unwrap();

        let metrics = h5i_repo.scan_test_metrics(&path).unwrap();
        assert!(!metrics.test_suite_hash.is_empty());
    }

    #[test]
    fn test_merge_h5i_logic_with_proper_deltas() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let git_repo = &h5i_repo.git_repo;
        let file_path = "main.py";
        let sig = git_repo.signature()?;

        // Helper to attach metadata to a commit via Git Notes
        let attach_metadata =
            |oid: Oid, update: Vec<u8>| -> Result<(), Box<dyn std::error::Error>> {
                let mut crdt_states = std::collections::HashMap::new();
                crdt_states.insert(file_path.to_string(), base64::encode(update));

                let record = H5iCommitRecord {
                    git_oid: oid.to_string(),
                    parent_oid: None,
                    ai_metadata: None,
                    test_metrics: None,
                    ast_hashes: None,
                    crdt_states: Some(crdt_states),
                    timestamp: chrono::Utc::now(),
                    caused_by: vec![],
                    causes: vec![],
                    decisions: vec![],
                };

                let json = serde_json::to_string(&record)?;
                git_repo.note(&sig, &sig, Some(H5I_NOTES_REF), oid, &json, true)?;
                Ok(())
            };

        // --- 1. BASE ---
        let base_content = "def main():\n    pass";
        let base_oid = create_commit(git_repo, "base", file_path, base_content, &[]);

        let base_update = {
            let doc = yrs::Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            text.push(&mut txn, base_content);
            txn.encode_state_as_update_v1(&yrs::StateVector::default())
        };
        attach_metadata(base_oid, base_update.clone())?;

        // --- 2. OURS ---
        let (our_oid, _our_update) = {
            let doc = yrs::Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            txn.apply_update(yrs::Update::decode_v1(&base_update)?)?;

            text.insert(&mut txn, 0, "# OURS COMMENT\n");
            let full_state = txn.encode_state_as_update_v1(&yrs::StateVector::default());

            let base_commit = git_repo.find_commit(base_oid)?;
            let oid = create_commit(
                git_repo,
                "ours",
                file_path,
                &text.get_string(&txn),
                &[&base_commit],
            );
            (oid, full_state)
        };
        attach_metadata(our_oid, _our_update)?;

        // --- 3. THEIRS ---
        git_repo.set_head_detached(base_oid)?;
        let (their_oid, _their_update) = {
            let doc = yrs::Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            txn.apply_update(yrs::Update::decode_v1(&base_update)?)?;

            text.push(&mut txn, "\nprint('done')");
            let full_state = txn.encode_state_as_update_v1(&yrs::StateVector::default());

            let base_commit = git_repo.find_commit(base_oid)?;
            let oid = create_commit(
                git_repo,
                "theirs",
                file_path,
                &text.get_string(&txn),
                &[&base_commit],
            );
            (oid, full_state)
        };
        attach_metadata(their_oid, _their_update)?;

        // --- 4. Merge ---
        // The merge logic now pulls context from the Notes we attached above
        let merged_text = h5i_repo.merge_h5i_logic(our_oid, their_oid, file_path)?;

        // --- 5. Verify ---
        assert!(merged_text.contains("# OURS COMMENT"));
        assert!(merged_text.contains("print('done')"));
        assert!(merged_text.contains("def main():"));

        Ok(())
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::delta_store::DeltaStore;
    use crate::metadata::TestSource;
    use crate::repository::{H5iRepository, H5I_NOTES_REF};
    use crate::session::LocalSession;
    use git2::{Repository, Signature};
    use std::fs;
    use tempfile::tempdir;
    use yrs::updates::decoder::Decode;
    use yrs::ReadTxn;
    use yrs::Transact;

    /// Helper to setup both Git and H5i repositories in a temp directory.
    fn setup_integration_context(root: &std::path::Path) -> H5iRepository {
        // First, initialize a standard Git repository
        Repository::init(root).expect("Failed to init git repo");
        // Then, open it as an H5i repository (which creates .h5i/ folders)
        H5iRepository::open(root).expect("Failed to open h5i repo")
    }

    #[test]
    fn test_full_session_to_repository_commit_flow() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_path = dir.path();

        // 1. Initialize Context
        let h5i_repo = setup_integration_context(repo_path);
        let git_repo = h5i_repo.git();
        let file_path = "logic.rs";
        let full_file_path = repo_path.join(file_path);

        // Initial physical file for Git tracking
        fs::write(&full_file_path, "// Initial content\n")?;

        // 2. Start a LocalSession (Simulation of 'h5i start')
        let mut session = LocalSession::new(h5i_repo.h5i_root.clone(), full_file_path.clone(), 0)?;

        // 3. Apply edits via Session
        session.apply_local_edit(0, "// AI Optimized\n")?;
        let session_text = session.get_current_text();

        // 4. Prepare Git Commit
        let sig = Signature::now("h5i-integration-test", "test@h5i.io")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new(file_path))?;
        index.write()?;

        let oid = h5i_repo.commit(
            "Integrated commit with CRDT",
            &sig,
            &sig,
            None, // ai_meta
            TestSource::None,
            None, // ast
            vec![],
            vec![],
        )?;

        // 5. BRIDGE: Transition Active Delta -> Committed Delta
        // This simulates the 'h5i commit' logic where current session work is frozen.
        let active_updates = session.delta_store.read_all_updates()?;
        let merged_delta = yrs::merge_updates_v1(&active_updates)
            .map_err(|e| crate::error::H5iError::Crdt(e.to_string()))?;

        h5i_repo.persist_delta_for_commit(oid, file_path, &merged_delta)?;

        // 6. VERIFICATION: Does the Repository OID match the Session State?
        let content_from_git = h5i_repo.get_content_at_oid(oid, std::path::Path::new(file_path))?;
        assert_eq!(
            content_from_git, session_text,
            "Content at OID must match the final CRDT session text"
        );

        Ok(())
    }

    #[test]
    fn test_cross_branch_merge_using_session_history() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_integration_context(dir.path());
        let git_repo = h5i_repo.git();
        let file_path = "app.py";
        let full_path = dir.path().join(file_path);
        let sig = git2::Signature::now("h5i-tester", "test@h5i.io")?;

        // Helper to bundle and attach CRDT state to a commit via Git Notes
        let attach_h5i_note = |oid: git2::Oid, doc: &yrs::Doc| -> crate::error::Result<()> {
            let mut crdt_states = std::collections::HashMap::new();
            // Capture the FULL state at the time of commit
            let state = doc
                .transact()
                .encode_state_as_update_v1(&yrs::StateVector::default());
            crdt_states.insert(file_path.to_string(), base64::encode(state));

            let record = crate::metadata::H5iCommitRecord {
                git_oid: oid.to_string(),
                parent_oid: None,
                ai_metadata: None,
                test_metrics: None,
                ast_hashes: None,
                crdt_states: Some(crdt_states),
                timestamp: chrono::Utc::now(),
                caused_by: vec![],
                causes: vec![],
                decisions: vec![],
            };

            let metadata_json = serde_json::to_string(&record).unwrap();
            git_repo.note(&sig, &sig, Some(H5I_NOTES_REF), oid, &metadata_json, true)?;
            Ok(())
        };

        // --- PHASE 1: Base Commit ---
        fs::write(&full_path, "")?;
        let mut session_ours = LocalSession::new(h5i_repo.h5i_root.clone(), full_path.clone(), 1)?;

        let base_content = "def main():\n    pass";
        session_ours.apply_local_edit(0, base_content)?;

        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new(file_path))?;
        let base_oid = h5i_repo.commit("base", &sig, &sig, None, TestSource::None, None, vec![], vec![])?;
        let base_commit = git_repo.find_commit(base_oid)?;

        // Attach mathematical state to the BASE commit note
        attach_h5i_note(base_oid, &session_ours.doc)?;

        // --- PHASE 2: Branch OURS ---
        session_ours.apply_local_edit(0, "# Header\n")?;

        let our_oid = h5i_repo.commit("ours", &sig, &sig, None, TestSource::None, None, vec![], vec![])?;
        // Attach mathematical state to the OURS commit note
        attach_h5i_note(our_oid, &session_ours.doc)?;

        // --- PHASE 3: Branch THEIRS ---
        // Move back to base and simulate a different user/client
        git_repo.set_head_detached(base_oid)?;

        let doc_theirs = yrs::Doc::with_options(yrs::Options {
            client_id: 2,
            ..Default::default()
        });
        let text_theirs = doc_theirs.get_or_insert_text("code");

        // Initialize THEIRS with the BASE state to ensure ID continuity
        {
            let base_record = h5i_repo.load_h5i_record(base_oid)?;
            let base_state_b64 = base_record
                .crdt_states
                .unwrap()
                .get(file_path)
                .unwrap()
                .clone();
            let base_state = base64::decode(base_state_b64).unwrap();
            let mut txn = doc_theirs.transact_mut();
            txn.apply_update(yrs::Update::decode_v1(&base_state)?)?;
        }

        let mut session_theirs = LocalSession {
            doc: doc_theirs,
            text_ref: text_theirs,
            delta_store: crate::delta_store::DeltaStore::new(
                dir.path().to_path_buf(),
                "theirs_temp",
            ),
            target_fs_path: full_path.clone(),
            update_count: 0,
            last_read_offset: 0,
        };

        session_theirs.apply_local_edit(20, "\nprint('end')")?;

        let tree = git_repo.find_tree(git_repo.index()?.write_tree()?)?;
        let their_oid =
            git_repo.commit(Some("HEAD"), &sig, &sig, "theirs", &tree, &[&base_commit])?;

        // Attach mathematical state to the THEIRS commit note
        attach_h5i_note(their_oid, &session_theirs.doc)?;

        // --- PHASE 4: Semantic Merge ---
        // merge_h5i_logic now fetches the notes for our_oid and their_oid
        let merged_text = h5i_repo.merge_h5i_logic(our_oid, their_oid, file_path)?;

        // Final Assertions
        assert!(merged_text.contains("# Header"), "OURS content missing");
        assert!(
            merged_text.contains("print('end')"),
            "THEIRS content missing"
        );
        assert!(merged_text.contains("def main():"), "BASE content missing");
        assert!(merged_text.contains("def main():\n    pass\nprint('end')"));

        Ok(())
    }

    // Testcase to rewrite message of commit without dependent commits
    #[test]
    fn test_edit_commit_message() {
        let dir = tempdir().unwrap();
        let repo = setup_integration_context(dir.path());
        let git_repo = repo.git();
        let sig = Signature::now("h5i-test", "test@h5i.io").unwrap();

        let file_path = dir.path().join("foo.rs");
        fs::write(&file_path, "fn foo() {}").unwrap();
        let mut index = git_repo.index().unwrap();
        index.add_path(std::path::Path::new("foo.rs")).unwrap();
        index.write().unwrap();

        let oid = repo
            .commit("original message", &sig, &sig, None, TestSource::None, None, vec![], vec![])
            .expect("commit failed");

        let (new_oid, _oid_map) = repo
            .edit_commit_message(oid, "rewritten message")
            .expect("edit_commit_message failed");

        let updated_commit = git_repo.find_commit(new_oid).unwrap();
        assert_eq!(updated_commit.message().unwrap().trim(), "rewritten message");
    }

    // Testcase to rewrite message of commit with dependent commits
    #[test]
    fn test_edit_commit_message_non_head() {
        let dir = tempdir().unwrap();
        let repo = setup_integration_context(dir.path());
        let git_repo = repo.git();
        let sig = Signature::now("h5i-test", "test@h5i.io").unwrap();

        let file_path = dir.path().join("foo.rs");
        fs::write(&file_path, "fn foo() {}").unwrap();
        let mut index = git_repo.index().unwrap();
        index.add_path(std::path::Path::new("foo.rs")).unwrap();
        index.write().unwrap();

        let first_oid = repo
            .commit("original message", &sig, &sig, None, TestSource::None, None, vec![], vec![])
            .expect("first commit failed");

        fs::write(&file_path, "fn foo() {} fn bar() {}").unwrap();
        let mut index = git_repo.index().unwrap();
        index.add_path(std::path::Path::new("foo.rs")).unwrap();
        index.write().unwrap();

        repo.commit("second commit", &sig, &sig, None, TestSource::None, None, vec![], vec![])
            .expect("second commit failed");

        let (new_oid, _oid_map) = repo
            .edit_commit_message(first_oid, "rewritten first message")
            .expect("edit_commit_message failed");

        let updated_commit = git_repo.find_commit(new_oid).unwrap();
        assert_eq!(updated_commit.message().unwrap().trim(), "rewritten first message");
    }

    // Testcase: caused_by populates causes on the referenced commit
    #[test]
    fn test_caused_by_populates_causes() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo = setup_integration_context(dir.path());
        let git_repo = repo.git();
        let sig = Signature::now("h5i-test", "test@h5i.io")?;

        // First commit
        let file_path = dir.path().join("foo.rs");
        fs::write(&file_path, "fn foo() {}")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new("foo.rs"))?;
        index.write()?;
        let first_oid = repo.commit(
            "first commit",
            &sig,
            &sig,
            None,
            crate::metadata::TestSource::None,
            None,
            vec![],
            vec![],
        )?;

        // Second commit caused by first
        fs::write(&file_path, "fn foo() {} fn bar() {}")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new("foo.rs"))?;
        index.write()?;
        let second_oid = repo.commit(
            "second commit",
            &sig,
            &sig,
            None,
            crate::metadata::TestSource::None,
            None,
            vec![first_oid.to_string()],
            vec![],
        )?;

        // The first commit's `causes` should now contain the second commit's OID
        let first_record = repo.load_h5i_record(first_oid)?;
        assert!(
            first_record.causes.contains(&second_oid.to_string()),
            "first commit's causes should contain second commit OID"
        );

        // The second commit's `caused_by` should contain the first commit's OID
        let second_record = repo.load_h5i_record(second_oid)?;
        assert!(
            second_record.caused_by.contains(&first_oid.to_string()),
            "second commit's caused_by should contain first commit OID"
        );

        Ok(())
    }

    // Testcase: edit_commit_message updates caused_by on dependent commits
    #[test]
    fn test_edit_commit_message_updates_causal_metadata() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo = setup_integration_context(dir.path());
        let git_repo = repo.git();
        let sig = Signature::now("h5i-test", "test@h5i.io")?;

        // First commit
        let file_path = dir.path().join("foo.rs");
        fs::write(&file_path, "fn foo() {}")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new("foo.rs"))?;
        index.write()?;
        let first_oid = repo.commit(
            "first commit",
            &sig,
            &sig,
            None,
            crate::metadata::TestSource::None,
            None,
            vec![],
            vec![],
        )?;

        // Second commit caused by first
        fs::write(&file_path, "fn foo() {} fn bar() {}")?;
        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new("foo.rs"))?;
        index.write()?;
        let second_oid = repo.commit(
            "second commit",
            &sig,
            &sig,
            None,
            crate::metadata::TestSource::None,
            None,
            vec![first_oid.to_string()],
            vec![],
        )?;

        // Rewrite first commit's message — its OID changes, and second's OID changes too
        let (new_first_oid, oid_map) = repo.edit_commit_message(first_oid, "rewritten first commit")?;

        // second_oid was also rewritten because its parent changed; get the new OID from the map
        let new_second_oid_from_map = *oid_map.get(&second_oid).expect("second_oid must be in oid_map");

        // The second commit's caused_by should now reference the new OID
        let second_record = repo.load_h5i_record(new_second_oid_from_map)?;
        assert!(
            second_record.caused_by.contains(&new_first_oid.to_string()),
            "second commit's caused_by should be updated to new first OID"
        );
        assert!(
            !second_record.caused_by.contains(&first_oid.to_string()),
            "second commit's caused_by should not contain old first OID"
        );

        // The rewritten first commit's causes should reference the new second commit OID
        let new_first_record = repo.load_h5i_record(new_first_oid)?;
        assert!(
            new_first_record.causes.contains(&new_second_oid_from_map.to_string()),
            "rewritten first commit's causes should reference the new second commit OID"
        );

        Ok(())
    }

}
