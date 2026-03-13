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

use crate::blame::{BlameMode, BlameResult};
use crate::delta_store::{sha256_hash, DeltaStore};
use crate::error::H5iError;
use crate::metadata::{
    AiMetadata, H5iCommitRecord, IntegrityLevel, IntegrityReport, TestMetrics, TokenUsage,
};

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
            .join(".h5i");

        if !h5i_root.exists() {
            fs::create_dir_all(&h5i_root)?;
            fs::create_dir_all(h5i_root.join("ast"))?;
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
        enable_test_tracking: bool,
        ast_parser: Option<&dyn Fn(&Path) -> Option<String>>, // Optional externally injected parser
    ) -> Result<Oid, H5iError> {
        let mut index = self.git_repo.index()?;

        // 1. Prepare optional features
        let mut ast_hashes = None;
        let mut test_metrics = None;

        // Scan staged files
        for entry in index.iter() {
            let path_bytes = &entry.path;
            let path_str = std::str::from_utf8(path_bytes).unwrap();
            let full_path = self.git_repo.workdir().unwrap().join(path_str);

            // A. AST generation (optional)
            if let Some(parser) = ast_parser {
                let hashes = ast_hashes.get_or_insert_with(HashMap::new);
                if let Some(sexp) = parser(&full_path) {
                    let hash = self.save_ast_to_sidecar(path_str, &sexp)?;
                    hashes.insert(path_str.to_string(), hash);
                }
            }

            // B. Extract test provenance (optional)
            if enable_test_tracking && test_metrics.is_none() {
                test_metrics = self.scan_test_block(&full_path);
            }
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
            timestamp: chrono::Utc::now(),
        };
        let metadata_json = serde_json::to_string(&record)?;
        self.git_repo
            .note(author, committer, None, commit_oid, &metadata_json, false)?;

        //self.persist_h5i_record(record)?;

        Ok(commit_oid)
    }

    fn count_tokens_internal(&self, text: &str, model: &str) -> usize {
        use tiktoken_rs::get_bpe_from_model;
        if let Ok(bpe) = get_bpe_from_model(model) {
            bpe.encode_with_special_tokens(text).len()
        } else {
            text.split_whitespace().count()
        }
    }

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
        let commit_oid = self.commit(prompt, sig, sig, None, false, None)?;

        // 4. メタデータを .h5i/metadata/{oid}.json に保存
        self.save_ai_metadata(commit_oid, &ai_meta)?;

        Ok(commit_oid)
    }
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
                    let color = if tm.coverage > 80.0 {
                        console::Color::Green
                    } else {
                        console::Color::Yellow
                    };
                    println!(
                        "{:<10} {} {}%",
                        style("Tests:").dim(),
                        style("✔").fg(color),
                        style(format!("{:.1}", tm.coverage)).fg(color)
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
            }
            println!("\n    {}\n", style(commit.message().unwrap_or("")).bold());
            println!("{}", style("─".repeat(60)).dim());
        }
        Ok(())
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
            let agent_info = record
                .as_ref()
                .and_then(|r| r.ai_metadata.as_ref())
                .map(|a| format!("AI:{}", a.agent_id))
                .unwrap_or_else(|| "Human".to_string());
            let test_passed = record
                .as_ref()
                .and_then(|r| r.test_metrics.as_ref())
                .map(|tm| tm.coverage > 0.0);

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
                    });
                }
            }
        }
        Ok(results)
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
    /// This method reads the corresponding JSON file stored in the
    /// `.h5i/metadata` directory and deserializes it into an
    /// [`H5iCommitRecord`].
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
    /// - the file cannot be read
    /// - the JSON data cannot be deserialized
    pub fn load_h5i_record(&self, oid: git2::Oid) -> Result<H5iCommitRecord, H5iError> {
        // Attempt to find the note attached to the commit OID.
        // Passing None uses the default notes reference (refs/notes/commits).
        let note = match self.git_repo.find_note(None, oid) {
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

    /// Saves commit provenance metadata associated with a Git commit.
    ///
    /// This method stores a [`CommitProvenance`] structure as a JSON file
    /// under `.h5i/metadata/`. The filename corresponds to the commit OID
    /// contained in the provenance record.
    ///
    /// Compared to [`persist_h5i_record`], this function focuses specifically
    /// on provenance metadata and may be used by external tools that only
    /// need to record commit provenance information.
    ///
    /// # Parameters
    ///
    /// - `provenance` – A [`CommitProvenance`] object containing metadata
    ///   describing the origin and context of the commit.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata cannot be serialized or written.
    pub fn save_metadata(
        &self,
        provenance: crate::metadata::CommitProvenance,
    ) -> Result<(), H5iError> {
        let path = self
            .h5i_path()
            .join("metadata")
            .join(format!("{}.json", provenance.commit_oid));
        let data = serde_json::to_string_pretty(&provenance)?;
        fs::write(path, data)?;
        Ok(())
    }

    pub fn save_ai_metadata(&self, commit_oid: Oid, metadata: &AiMetadata) -> Result<(), H5iError> {
        // 1. 保存先ディレクトリの準備 (.h5i/metadata)
        let metadata_dir = self.h5i_root.join("metadata");
        if !metadata_dir.exists() {
            fs::create_dir_all(&metadata_dir)?;
        }

        // 2. JSON シリアライズ
        let json_data = serde_json::to_string_pretty(metadata)
            .map_err(|e| H5iError::Metadata(format!("Failed to serialize metadata: {}", e)))?;

        // 3. コミットOIDをファイル名にして書き出し
        let file_path = metadata_dir.join(format!("{}.json", commit_oid));
        let mut file = fs::File::create(file_path)?;
        file.write_all(json_data.as_bytes())?;

        Ok(())
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
        let base_oid = self.git_repo.merge_base(our_oid, their_oid)?;

        // 1. Reconstruct the full state of the common ancestor (base)
        // by replaying all updates from the beginning up to the merge base.
        let mut doc = yrs::Doc::new();
        let text_ref = doc.get_or_insert_text("code");

        // Apply the entire history up to the base commit
        self.apply_all_updates_up_to(base_oid, file_path, &mut doc)?;

        // 2. Retrieve and merge updates from OURS and THEIRS
        // Apply only the incremental updates from each branch to the same document.
        self.apply_updates_between(base_oid, our_oid, file_path, &mut doc)?;
        self.apply_updates_between(base_oid, their_oid, file_path, &mut doc)?;

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

        // 1. HEAD を取得してコミットまで解決
        let head = repo.head()?;
        let head_commit = head.peel_to_commit()?;

        // 2. コミットからツリー（ファイル構造の木）を取得
        let tree = head_commit.tree()?;

        // 3. ツリー内からパスを辿って Blob (データの塊) を取得
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

            Some(TestMetrics {
                test_suite_hash: format!("{:x}", hasher.finalize()),
                coverage: 0.0,
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

        // Determine the storage location (.h5i/ast/<hash>.sexp)
        let ast_dir = self.h5i_root.join("ast");
        if !ast_dir.exists() {
            fs::create_dir_all(&ast_dir).map_err(|e| H5iError::Io(e))?;
        }

        let target_path = ast_dir.join(format!("{}.sexp", hash));

        // Write the AST only if it does not already exist
        if !target_path.exists() {
            fs::write(&target_path, sexp).map_err(|e| H5iError::Io(e))?;
        }

        Ok(hash)
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
        let content = std::fs::read_to_string(path).ok()?;
        let start_tag = "// h5_i_test_start";
        let end_tag = "// h5_i_test_end";

        if let (Some(s), Some(e)) = (content.find(start_tag), content.find(end_tag)) {
            let test_code = &content[s + start_tag.len()..e];
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            hasher.update(test_code.trim());
            let hash = format!("{:x}", hasher.finalize());

            Some(TestMetrics {
                test_suite_hash: hash,
                coverage: 0.0,
            })
        } else {
            None
        }
    }
}

// src/repository.rs

impl H5iRepository {
    /// Detailed integrity check comparing intent (prompt/message) vs actual diff.
    /// Priority: Prompt > Commit Message.
    pub fn verify_integrity(
        &self,
        prompt: Option<&str>,
        message: &str,
    ) -> Result<IntegrityReport, H5iError> {
        let mut findings = Vec::new();
        let mut penalty: f32 = 0.0;

        // 1. Identify Primary Intent
        // If a prompt exists, it's our "contract". Otherwise, use the message.
        let primary_intent = prompt.unwrap_or(message).to_lowercase();

        // 2. Extract staged diff statistics
        let diff = self.get_staged_diff()?;
        let stats = diff.stats()?;
        let deletions = stats.deletions();
        let insertions = stats.insertions();

        // Check A: Unintended Deletions
        // High penalty if the AI deletes significant code without "delete/remove/cleanup" keywords.
        let deletion_keywords = ["delete", "remove", "cleanup", "rm", "drop", "refactor"];
        let has_deletion_intent = deletion_keywords
            .iter()
            .any(|&k| primary_intent.contains(k));

        if deletions > 5 && !has_deletion_intent {
            findings.push(format!(
                "High Risk: {} lines deleted, but no deletion intent found in prompt/message.",
                deletions
            ));
            penalty += 0.5;
        }

        // Check B: Scope Mismatch (File Path check)
        // If the prompt mentions a specific file, check if changes leaked into other files.
        let changed_files = self.get_changed_file_names(&diff)?;
        for path in &changed_files {
            let file_name = Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Heuristic: If prompt mentions "main.rs" but we see changes in "auth.rs"
            if changed_files.len() > 1 && !primary_intent.contains(&file_name) {
                // Low penalty per file to account for shared headers/imports
                findings.push(format!(
                    "Scope Warning: File '{}' modified but not explicitly mentioned in intent.",
                    path
                ));
                penalty += 0.1;
            }

            if path.ends_with("Cargo.toml") && !primary_intent.contains("dependency") {
                findings.push(format!(
                    "Scope Warning: File '{}' modified but dependency update is not explicitly mentioned in intent.",
                    path
                ));
                penalty += 0.2;
            }
        }

        // Check C: "Refactor" Hallucination
        // "Refactor" or "Comment" should generally have a balanced Ins/Del ratio.
        if (primary_intent.contains("refactor") || primary_intent.contains("comment"))
            && insertions > 100
            && deletions < 10
        {
            findings.push(
                "Anomaly: Large insertion detected for a 'refactor' or 'comment' task.".to_string(),
            );
            penalty += 0.3;
        }

        // --- Scoring ---
        let score = (1.0 - penalty).max(0.0);
        let level = if score < 0.4 {
            IntegrityLevel::Violation
        } else if score < 0.8 {
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

    /// Helper to get diff between Git index and HEAD.
    fn get_staged_diff(&'_ self) -> Result<git2::Diff<'_>, H5iError> {
        let head_tree = self.get_head_commit()?.tree()?;
        let index = self.git_repo.index()?;
        let mut opts = git2::DiffOptions::new();

        let diff =
            self.git_repo
                .diff_tree_to_index(Some(&head_tree), Some(&index), Some(&mut opts))?;
        Ok(diff)
    }

    /// Helper to extract file names from a diff object
    fn get_changed_file_names(&self, diff: &git2::Diff) -> Result<Vec<String>, H5iError> {
        let mut files = Vec::new();
        diff.print(git2::DiffFormat::NameOnly, |_delta, _hunk, line| {
            let path = std::str::from_utf8(line.content()).unwrap_or("").trim();
            if !path.is_empty() {
                files.push(path.to_string());
            }
            true
        })?;
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Repository, Signature};
    use std::fs;
    use tempfile::tempdir;
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
        assert!(repo.h5i_root.join("ast").exists());
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
            false, // enable_test_tracking
            None,  // ast_parser
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
        let ast_file = h5i_repo.h5i_root.join("ast").join(format!("{}.sexp", hash));

        assert!(ast_file.exists());
        assert_eq!(fs::read_to_string(ast_file).unwrap(), sexp);
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

        let base_content = "def main():\n    pass";
        let base_oid = create_commit(git_repo, "base", file_path, base_content, &[]);

        let base_update = {
            let doc = Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            text.push(&mut txn, base_content);
            txn.encode_update_v1()
        };
        h5i_repo.persist_delta_for_commit(base_oid, file_path, &base_update)?;

        // --- 2. OURS ---
        let (our_oid, our_update) = {
            let doc = Doc::new();
            let text = doc.get_or_insert_text("code");

            let mut txn = doc.transact_mut();
            txn.apply_update(Update::decode_v1(&base_update)?)?;

            text.insert(&mut txn, 0, "# OURS COMMENT\n");
            let update = txn.encode_update_v1();

            let base_commit = git_repo.find_commit(base_oid)?;
            let oid = create_commit(
                git_repo,
                "ours",
                file_path,
                &text.get_string(&txn),
                &[&base_commit],
            );
            (oid, update)
        };
        h5i_repo.persist_delta_for_commit(our_oid, file_path, &our_update)?;

        // --- 3. THEIRS ---
        git_repo.set_head_detached(base_oid)?;
        let (their_oid, their_update) = {
            let doc = Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            txn.apply_update(Update::decode_v1(&base_update)?)?;

            text.push(&mut txn, "\nprint('done')");
            let update = txn.encode_update_v1();

            let base_commit = git_repo.find_commit(base_oid)?;
            let oid = create_commit(
                git_repo,
                "theirs",
                file_path,
                &text.get_string(&txn),
                &[&base_commit],
            );
            (oid, update)
        };
        h5i_repo.persist_delta_for_commit(their_oid, file_path, &their_update)?;

        // --- 4. Merge ---
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
    use crate::repository::H5iRepository;
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
            None,  // ai_meta
            false, // tests
            None,  // ast
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

        // --- PHASE 1: Base Commit ---
        // Start from an empty state to ensure the first insertion is recorded as a delta
        fs::write(&full_path, "")?;
        let mut session_ours = LocalSession::new(h5i_repo.h5i_root.clone(), full_path.clone(), 1)?;

        // Initial code: 20 characters long
        let base_content = "def main():\n    pass";
        session_ours.apply_local_edit(0, base_content)?;

        let mut index = git_repo.index()?;
        index.add_path(std::path::Path::new(file_path))?;
        let base_oid = h5i_repo.commit("base", &sig, &sig, None, false, None)?;
        let base_commit = git_repo.find_commit(base_oid)?;

        // Capture BASE state for later diffing
        let base_delta = yrs::merge_updates_v1(&session_ours.delta_store.read_all_updates()?)
            .map_err(|e| crate::error::H5iError::Crdt(e.to_string()))?;
        let base_sv = session_ours.doc.transact().state_vector();
        h5i_repo.persist_delta_for_commit(base_oid, file_path, &base_delta)?;

        // --- PHASE 2: Branch OURS ---
        // OURS adds a header at the very beginning (Index 0)
        session_ours.apply_local_edit(0, "# Header\n")?;

        // Save incremental delta for OURS
        // For testing, we use encode_state_as_update or diff to get ONLY the new parts
        let our_oid = h5i_repo.commit("ours", &sig, &sig, None, false, None)?;
        let ours_diff: Vec<u8> = session_ours.doc.transact().encode_diff_v1(&base_sv);
        h5i_repo.persist_delta_for_commit(our_oid, file_path, &ours_diff)?;

        // --- PHASE 3: Branch THEIRS ---
        // Switch "context" back to base
        git_repo.set_head_detached(base_oid)?;

        // CRITICAL: We create a new doc and APPLY the base_delta
        // to ensure character IDs match exactly.
        let doc_theirs = yrs::Doc::with_options(yrs::Options {
            client_id: 2,
            ..Default::default()
        });
        let text_theirs = doc_theirs.get_or_insert_text("code");
        {
            let mut txn = doc_theirs.transact_mut();
            txn.apply_update(yrs::Update::decode_v1(&base_delta)?)
                .map_err(|e| crate::error::H5iError::Crdt(e.to_string()))?;
        }

        // Simulate a second session branching from the SAME state
        let mut session_theirs = LocalSession {
            doc: doc_theirs,
            text_ref: text_theirs,
            delta_store: DeltaStore::new(dir.path().to_path_buf(), "theirs_temp"),
            target_fs_path: full_path.clone(),
            update_count: 0,
            last_read_offset: 0,
        };

        // THEIRS adds print at the end of "def main():\n    pass" (index 18)
        session_theirs.apply_local_edit(20, "\nprint('end')")?;

        let tree = git_repo.find_tree(git_repo.index()?.write_tree()?)?;
        let their_oid =
            git_repo.commit(Some("HEAD"), &sig, &sig, "theirs", &tree, &[&base_commit])?;

        let theirs_diff = session_theirs.doc.transact().encode_diff_v1(&base_sv);
        h5i_repo.persist_delta_for_commit(their_oid, file_path, &theirs_diff)?;

        // --- PHASE 4: Semantic Merge ---
        let merged_text = h5i_repo.merge_h5i_logic(our_oid, their_oid, file_path)?;

        // Final Assertions
        assert!(merged_text.contains("# Header"), "OURS missing");
        assert!(merged_text.contains("print('end')"), "THEIRS missing");
        assert!(merged_text.contains("def main():"), "BASE missing");

        // Ensure no weird interleaving
        assert!(merged_text.contains("def main():\n    pass\nprint('end')"));

        Ok(())
    }
}
