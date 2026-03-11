use git2::{Blob, Repository};
use git2::{Commit, ObjectType, Oid, Signature};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use yrs::updates::decoder::Decode;
use yrs::{GetString, Text, Transact};

use crate::blame::{BlameMode, BlameResult};
use crate::delta_store::{sha256_hash, DeltaStore};
use crate::error::H5iError;
use crate::metadata::{AiMetadata, H5iCommitRecord, TestMetrics};

pub struct H5iRepository {
    git_repo: Repository,
    pub h5i_root: PathBuf,
}

// ============================================================
// Repository lifecycle
// ============================================================

impl H5iRepository {
    /// 既存のGitリポジトリからh5iコンテキストを初期化/開く
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
    /// Gitコミットを実行し、h5i拡張データをアトミックに紐付ける
    pub fn commit(
        &self,
        message: &str,
        author: &Signature,
        committer: &Signature,
        ai_meta: Option<AiMetadata>,
        enable_test_tracking: bool,
        ast_parser: Option<&dyn Fn(&Path) -> Option<String>>, // 外部注入のオプショナルパーサー
    ) -> Result<Oid, H5iError> {
        let mut index = self.git_repo.index()?;

        // 1. オプショナル機能の実行準備
        let mut ast_hashes = None;
        let mut test_metrics = None;

        // ステージングされたファイルを走査
        for entry in index.iter() {
            let path_bytes = &entry.path;
            let path_str = std::str::from_utf8(path_bytes).unwrap();
            let full_path = self.git_repo.workdir().unwrap().join(path_str);

            // A. AST生成 (オプショナル)
            if let Some(parser) = ast_parser {
                let hashes = ast_hashes.get_or_insert_with(HashMap::new);
                if let Some(sexp) = parser(&full_path) {
                    let hash = self.save_ast_to_sidecar(path_str, &sexp)?;
                    hashes.insert(path_str.to_string(), hash);
                }
            }

            // B. テストプロビナンスの取得 (オプショナル)
            if enable_test_tracking && test_metrics.is_none() {
                test_metrics = self.scan_test_block(&full_path);
            }
        }

        // 2. 標準 Git コミットの作成 (git2-rs API を利用)
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

        // 3. h5i サイドカーレコードの保存
        let record = H5iCommitRecord {
            git_oid: commit_oid.to_string(),
            parent_oid: parent_commit.map(|p| p.id().to_string()),
            ai_metadata: ai_meta,
            test_metrics,
            ast_hashes,
            timestamp: chrono::Utc::now(),
        };

        self.persist_h5i_record(record)?;

        Ok(commit_oid)
    }
}

// ============================================================
// Log API
// ============================================================

impl H5iRepository {
    /// AI情報を含む拡張コミットログを取得する
    pub fn get_log(&self, limit: usize) -> Result<Vec<H5iCommitRecord>, H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        let mut records = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            // .h5i/metadata/<oid>.json を読み取る。存在しない場合は最小限のGit情報を返す
            let record = self
                .load_h5i_record(oid)
                .unwrap_or_else(|_| H5iCommitRecord::minimal_from_git(&self.git_repo, oid));
            records.push(record);
        }
        Ok(records)
    }

    /// AI メタデータを含む拡張ログの取得
    pub fn h5i_log(&self, limit: usize) -> Result<Vec<H5iCommitRecord>, H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?; // HEAD から遡る

        let mut logs = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            // サイドカーデータを読み取る。なければ Git 情報から最小構成を作成
            let record = self
                .load_h5i_record(oid)
                .unwrap_or_else(|_| H5iCommitRecord::minimal_from_git(&self.git_repo, oid));
            logs.push(record);
        }
        Ok(logs)
    }

    pub fn print_log(&self, limit: usize) -> anyhow::Result<()> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push_head()?;

        for oid in revwalk.take(limit) {
            let oid = oid?;
            let commit = self.git_repo.find_commit(oid)?;
            let record = self.load_h5i_record(oid).ok(); // オプショナルに読み込み

            println!("commit {}", oid);
            println!("Author: {}", commit.author());

            if let Some(r) = record {
                if let Some(ai) = r.ai_metadata {
                    println!("Agent:  {} (Model: {})", ai.agent_id, ai.model_name);
                    println!("Prompt: [hash: {}]", ai.prompt_hash);
                }
                if let Some(tm) = r.test_metrics {
                    println!(
                        "Tests:  Hash: {}, Coverage: {}%",
                        tm.test_suite_hash, tm.coverage
                    );
                }
                let ast_count = r.ast_hashes.map(|m| m.len()).unwrap_or(0);
                println!("AST:    {} files tracked", ast_count);
            }
            println!("Message: {}\n", commit.message().unwrap_or(""));
        }
        Ok(())
    }
}

// ============================================================
// Blame API
// ============================================================

impl H5iRepository {
    pub fn blame(
        &self,
        path: &std::path::Path,
        mode: BlameMode,
    ) -> anyhow::Result<Vec<BlameResult>> {
        match mode {
            BlameMode::Line => self.blame_by_line(path),
            BlameMode::Ast => self.blame_by_ast(path),
        }
    }

    pub fn get_blame(
        &self,
        path: &Path,
        use_ast: bool,
    ) -> Result<Vec<crate::blame::BlameResult>, H5iError> {
        if use_ast {
            // 1. 最新のレコードから AST ハッシュ履歴を取得
            // 2. 構造が変わったコミットを特定
            self.compute_semantic_blame(path)
        } else {
            // 標準の git2 blame を実行し、メタデータを Join する
            self.compute_line_blame(path)
        }
    }

    /// 行ベースの Blame (Git 標準 + AI メタデータ)
    fn blame_by_line(&self, path: &std::path::Path) -> anyhow::Result<Vec<BlameResult>> {
        let blame = self.git_repo.blame_file(path, None)?;
        let mut results = Vec::new();

        // ファイル内容を読み込み
        let blob = self.get_blob_at_head(path)?;
        let lines: Vec<&str> = std::str::from_utf8(blob.content())?.lines().collect();

        for hunk in blame.iter() {
            let commit_id = hunk.final_commit_id();
            let record = self.load_h5i_record(commit_id).ok();
            let agent_info = record
                .and_then(|r| r.ai_metadata)
                .map(|a| format!("AI:{}", a.agent_id))
                .unwrap_or_else(|| "Human".to_string());

            for i in 0..hunk.lines_in_hunk() {
                let line_idx = hunk.final_start_line() + i - 1;
                results.push(BlameResult {
                    line_content: lines[line_idx].to_string(),
                    commit_id: commit_id.to_string(),
                    agent_info: agent_info.clone(),
                    is_semantic_change: false,
                    line_number: todo!(),
                    test_passed: todo!(),
                });
            }
        }
        Ok(results)
    }

    /// AST ベースの Blame (構造ハッシュの変化を追跡)
    fn blame_by_ast(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<Vec<crate::blame::BlameResult>> {
        // 1. 最新のレコードから対象ファイルの AST ハッシュを取得
        // 2. 履歴を遡り、そのハッシュが「最後に変化した」コミットを特定
        // 3. そのコミットの AI 情報を取得
        // 注意: 外部ツールが提供した AST が不正確な場合は、Line ベースにフォールバック表示
        println!("Note: Semantic tracking depends on externally provided AST hashes.");
        self.blame_by_line(path) // プロトタイプでは Line ベースで結果を表示しつつ、AST情報を付与
    }

    /// 従来の行ベース (1D) の Blame を高度化して実行
    pub fn compute_line_blame(&self, path: &Path) -> Result<Vec<BlameResult>, H5iError> {
        let blame = self.git_repo.blame_file(path, None)?;
        let blob = self.get_blob_at_head(path)?;
        let content = std::str::from_utf8(blob.content())
            .map_err(|_| H5iError::Ast("File content is not valid UTF-8".to_string()))?;
        let lines: Vec<&str> = content.lines().collect();

        let mut results = Vec::new();

        for hunk in blame.iter() {
            let commit_id = hunk.final_commit_id();
            // サイドカーデータをロード（なければ Git から最小構成を作成）
            let record = self.load_h5i_record(commit_id)?;

            let agent_info = record
                .ai_metadata
                .map(|ai| format!("AI:{}", ai.model_name))
                .unwrap_or_else(|| "Human".to_string());

            let test_passed = record.test_metrics.map(|tm| tm.coverage > 0.0); // 簡易判定

            for i in 0..hunk.lines_in_hunk() {
                let line_idx = hunk.final_start_line() + i - 1;
                if line_idx < lines.len() {
                    results.push(BlameResult {
                        line_number: line_idx + 1,
                        line_content: lines[line_idx].to_string(),
                        commit_id: commit_id.to_string(),
                        agent_info: agent_info.clone(),
                        is_semantic_change: false, // Lineモードでは一律 false
                        test_passed,
                    });
                }
            }
        }

        Ok(results)
    }

    /// ASTハッシュの変化 (Structural Dimension) に基づく Blame
    /// 行が移動していても、ロジックが本質的に変更された瞬間を追跡する
    pub fn compute_semantic_blame(&self, path: &Path) -> Result<Vec<BlameResult>, H5iError> {
        // 基本的な行情報は Git Blame から取得
        let mut line_results = self.compute_line_blame(path)?;
        let path_str = path
            .to_str()
            .ok_or_else(|| H5iError::InvalidPath("Invalid path encoding".to_string()))?;

        for result in &mut line_results {
            let oid = git2::Oid::from_str(&result.commit_id)?;
            let record = self.load_h5i_record(oid)?;

            // 1. このコミットに AST ハッシュが含まれているか確認
            if let Some(hashes) = record.ast_hashes {
                if let Some(current_ast_hash) = hashes.get(path_str) {
                    // 2. 親コミットの AST ハッシュと比較
                    if let Some(parent_oid_str) = record.parent_oid {
                        let parent_oid = git2::Oid::from_str(&parent_oid_str)?;
                        if let Ok(parent_record) = self.load_h5i_record(parent_oid) {
                            let parent_ast_hash = parent_record
                                .ast_hashes
                                .and_then(|h| h.get(path_str).cloned());

                            // 親とハッシュが異なる場合、このコミットは「セマンティックな変更」
                            if Some(current_ast_hash.clone()) != parent_ast_hash {
                                result.is_semantic_change = true;
                            }
                        }
                    } else {
                        // 親がいない（最初のコミット）で AST があれば、それはセマンティックな誕生
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
    /// H5iCommitRecord を JSON 形式でサイドカーディレクトリに永続化する。
    /// ファイル名は Git のコミットハッシュ (<oid>.json) となる。
    pub fn persist_h5i_record(&self, record: H5iCommitRecord) -> Result<(), H5iError> {
        // 1. 保存先ディレクトリ (.h5i/metadata) のパスを確定
        let metadata_dir = self.h5i_root.join("metadata");

        // 2. ディレクトリが存在しない場合は作成
        if !metadata_dir.exists() {
            fs::create_dir_all(&metadata_dir).map_err(|e| H5iError::Io(e))?;
        }

        // 3. ファイルパスの決定 (<git_oid>.json)
        let file_path = metadata_dir.join(format!("{}.json", record.git_oid));

        // 4. JSON へのシリアライズ
        // 実戦での可読性とデバッグ性を考慮し、pretty-print 形式を採用
        let json_data = serde_json::to_string_pretty(&record)?;

        // 5. ファイルの書き込み
        // 書き込み失敗時は H5iError::io を通じて詳細なパス情報を付与
        fs::write(&file_path, json_data).map_err(|e| H5iError::Io(e))?;

        Ok(())
    }

    /// 指定された OID に紐づく h5i レコードを読み込む (log や blame で使用)
    pub fn load_h5i_record(&self, oid: git2::Oid) -> Result<H5iCommitRecord, H5iError> {
        let file_path = self.h5i_root.join("metadata").join(format!("{}.json", oid));

        if !file_path.exists() {
            return Err(H5iError::RecordNotFound(oid.to_string()));
        }

        let data = fs::read_to_string(&file_path).map_err(|e| H5iError::Io(e))?;
        let record: H5iCommitRecord = serde_json::from_str(&data)?;

        Ok(record)
    }

    /// コミットに紐づくメタデータを保存する
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
}

// ============================================================
// Resolve Conflict
// ============================================================

impl H5iRepository {
    /// 二つのブランチ（またはコミット）間のCRDT操作を統合し、コンフリクトなしのテキストを生成する
    pub fn merge_h5i_logic(
        &self,
        our_oid: Oid,
        their_oid: Oid,
        file_path: &str,
    ) -> Result<String, H5iError> {
        let base_oid = self.git_repo.merge_base(our_oid, their_oid)?;

        // 1. 共通祖先 (Base) の完全な状態を復元する
        // (ここでは操作ログを最初からマージベースまで再生して Doc を作る)
        let mut doc = yrs::Doc::new();
        let text_ref = doc.get_or_insert_text("code");

        // 起点となるベースまでの全履歴を適用
        self.apply_all_updates_up_to(base_oid, file_path, &mut doc)?;

        // 2. OURS と THEIRS の差分（Update）を取得してマージ
        // 状態を分岐させず、同じ Doc に対して両方のブランチの「差分のみ」を適用する
        self.apply_updates_between(base_oid, our_oid, file_path, &mut doc)?;
        self.apply_updates_between(base_oid, their_oid, file_path, &mut doc)?;

        let txn = doc.transact();
        Ok(text_ref.get_string(&txn))
    }

    /// 特定の範囲のコミットに紐づくデルタログをすべて適用する補助関数
    fn apply_updates_between(
        &self,
        base: Oid,
        tip: Oid,
        file_path: &str,
        doc: &mut yrs::Doc,
    ) -> Result<(), H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.push(tip)?;
        revwalk.hide(base)?;

        for oid_res in revwalk {
            let oid = oid_res?;
            // 【重要】そのコミット固有のデルタ（Update）をロードする
            // 以前実装した「h5i commit」で、コミット時にこのUpdateをサイドカーに保存しておく設計が必要
            if let Ok(update_data) = self.load_specific_delta_for_commit(oid, file_path) {
                let mut txn = doc.transact_mut();
                txn.apply_update(yrs::Update::decode_v1(&update_data)?);
            }
        }
        Ok(())
    }

    /// 履歴の最初から指定した base_oid まで、すべての差分を順番に適用して Doc を構築する
    pub fn apply_all_updates_up_to(
        &self,
        base_oid: Oid,
        file_path: &str,
        doc: &mut yrs::Doc,
    ) -> Result<(), H5iError> {
        let mut revwalk = self.git_repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?; // 古い順に歩く
        revwalk.push(base_oid)?;

        for oid_res in revwalk {
            let oid = oid_res?;
            if let Ok(update_data) = self.load_specific_delta_for_commit(oid, file_path) {
                let mut txn = doc.transact_mut();
                txn.apply_update(yrs::Update::decode_v1(&update_data)?);
            } else {
                // サイドカーにデルタがない（通常の人間によるコミットなど）場合のフォールバック
                // その時点のファイル内容を「まるごと挿入」として扱う
                self.fallback_ingest_content(oid, file_path, doc)?;
            }
        }
        Ok(())
    }

    /// 特定のコミット時に保存された、そのファイル固有の Update バイナリをロードする
    pub fn load_specific_delta_for_commit(
        &self,
        oid: Oid,
        file_path: &str,
    ) -> Result<Vec<u8>, H5iError> {
        // .h5i/deltas/<oid>/<file_hash>.bin という構造を想定
        let file_hash = sha256_hash(file_path);
        let delta_path = self
            .h5i_root
            .join("deltas")
            .join(oid.to_string())
            .join(format!("{}.bin", file_hash));

        if !delta_path.exists() {
            return Err(H5iError::Internal("Delta not found for this commit".into()));
        }

        let mut file = std::fs::File::open(&delta_path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        Ok(buffer)
    }

    /// サイドカーがないコミットの場合、Git から内容を読み取って CRDT に「全置換」として注入する
    fn fallback_ingest_content(
        &self,
        oid: Oid,
        file_path: &str,
        doc: &mut yrs::Doc,
    ) -> Result<(), H5iError> {
        let content = self.get_content_at_oid(oid, std::path::Path::new(file_path))?;
        let text_ref = doc.get_or_insert_text("code");
        let mut txn = doc.transact_mut();

        // 既存の内容を消して、新しい内容を書き込む
        let len = text_ref.len(&txn);
        text_ref.remove_range(&mut txn, 0, len);
        text_ref.push(&mut txn, &content);
        Ok(())
    }

    /// そのコミットで発生した差分（Update）を、OID付きのサイドカーとして永続化する
    pub fn persist_delta_for_commit(
        &self,
        oid: Oid,
        file_path: &str,
        update_data: &[u8],
    ) -> Result<(), H5iError> {
        let file_hash = sha256_hash(file_path);
        let delta_dir = self.h5i_root.join("deltas").join(oid.to_string());

        // ディレクトリ作成
        std::fs::create_dir_all(&delta_dir).map_err(|e| H5iError::Io(e))?;

        let delta_path = delta_dir.join(format!("{}.bin", file_hash));

        // 差分バイナリを書き込み
        std::fs::write(&delta_path, update_data).map_err(|e| H5iError::Io(e))?;

        Ok(())
    }
}

// ============================================================
// Internal helpers
// ============================================================

impl H5iRepository {
    pub fn git(&self) -> &Repository {
        &self.git_repo
    }

    pub fn h5i_path(&self) -> &Path {
        &self.h5i_root
    }

    fn get_head_commit(&self) -> Result<Commit<'_>, git2::Error> {
        let obj = self.git_repo.head()?.resolve()?.peel(ObjectType::Commit)?;
        obj.into_commit()
            .map_err(|_| git2::Error::from_str("Not a commit"))
    }

    /// HEAD コミットから指定されたパスの Blob (ファイルの実体) を取得する。
    pub fn get_blob_at_head(&self, path: &Path) -> Result<Blob<'_>, H5iError> {
        // 1. HEAD リファレンスを取得し、コミットまで解決する
        let head_commit = self.get_head_commit()?;

        // 2. コミットからツリー（ファイル構造のスナップショット）を取得
        let tree = head_commit.tree()?;

        // 3. ツリー内から指定されたパスののエントリを探す
        let entry = tree
            .get_path(path)
            .map_err(|_| H5iError::RecordNotFound(format!("Path not found in HEAD: {:?}", path)))?;

        // 4. エントリが Blob (ファイル) であることを確認
        if entry.kind() != Some(ObjectType::Blob) {
            return Err(H5iError::Ast(format!(
                "Path is not a file (blob): {:?}",
                path
            )));
        }

        // 5. OID を使用して実際の Blob オブジェクトを検索して返す
        let blob = self.git_repo.find_blob(entry.id())?;
        Ok(blob)
    }

    /// 指定された OID (コミット等) における特定のパスの Blob を取得する
    pub fn get_blob_at_oid(&self, oid: Oid, path: &Path) -> Result<Blob, H5iError> {
        // 1. OID からコミットオブジェクトを探す
        let commit = self
            .git_repo
            .find_commit(oid)
            .map_err(|e| H5iError::Internal(format!("Commit not found {}: {}", oid, e)))?;

        // 2. コミットに紐づくツリー（ディレクトリ構造）を取得
        let tree = commit.tree().map_err(|e| {
            H5iError::Internal(format!("Failed to get tree for commit {}: {}", oid, e))
        })?;

        // 3. ツリーの中から指定されたパスのエントリを探す
        let entry = tree.get_path(path).map_err(|_| {
            H5iError::InvalidPath(format!("Path {:?} not found in commit {}", path, oid))
        })?;

        // 4. エントリの ID から実際のデータ（Blob）を取得
        let blob = self.git_repo.find_blob(entry.id()).map_err(|e| {
            H5iError::Internal(format!("Failed to find blob for path {:?}: {}", path, e))
        })?;

        Ok(blob)
    }

    /// (便利関数) 指定された OID の内容を String として取得する
    pub fn get_content_at_oid(&self, oid: Oid, path: &Path) -> Result<String, H5iError> {
        let blob = self.get_blob_at_oid(oid, path)?;

        // UTF-8 チェックを行いながら文字列に変換
        let content = std::str::from_utf8(blob.content())
            .map_err(|_| H5iError::Internal(format!("File at {:?} is not valid UTF-8", path)))?;

        Ok(content.to_string())
    }

    /// // h5_i_test_start ～ // h5_i_test_end を抽出してハッシュ化
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
                coverage: 0.0, // ここには CI 等の外部結果を入れる想定
            })
        } else {
            None
        }
    }

    /// 外部から提供された S式 (AST) をサイドカーに保存し、そのハッシュを返す。
    /// AST はオプショナルな機能であるため、提供された場合のみこの処理が呼ばれる。
    pub fn save_ast_to_sidecar(&self, _file_path: &str, sexp: &str) -> Result<String, H5iError> {
        // 1. S式のコンテンツハッシュを計算
        // これにより、内容が同じであれば同じファイルとして扱われる（デデュープ）
        let mut hasher = Sha256::new();
        hasher.update(sexp.as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        // 2. 保存先パスの決定 (.h5i/ast/<hash>.sexp)
        let ast_dir = self.h5i_root.join("ast");
        if !ast_dir.exists() {
            fs::create_dir_all(&ast_dir).map_err(|e| H5iError::Io(e))?;
        }

        let target_path = ast_dir.join(format!("{}.sexp", hash));

        // 3. ファイルの書き込み
        // すでに存在する場合は、コンテンツアドレス指定のため書き込みをスキップしてもよいが、
        // 確実性のために常に書き込むか、存在チェックを行う
        if !target_path.exists() {
            fs::write(&target_path, sexp).map_err(|e| H5iError::Io(e))?;
        }

        // 4. ハッシュを返す (これが H5iCommitRecord の ast_hashes に格納される)
        Ok(hash)
    }

    /// // h5_i_test_start 間のコードを抽出してハッシュ化
    fn scan_test_metrics(&self, path: &std::path::Path) -> Option<TestMetrics> {
        let content = std::fs::read_to_string(path).ok()?;
        let start_tag = "// h5_i_test_start";
        let end_tag = "// h5_i_test_end";

        if let (Some(s), Some(e)) = (content.find(start_tag), content.find(end_tag)) {
            let test_code = &content[s + start_tag.len()..e];
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            hasher.update(test_code.trim());
            let hash = format!("{:x}", hasher.finalize());

            // 実際の運用ではここで直近のテスト実行結果(coverage)をJSON等から取得する
            Some(TestMetrics {
                test_suite_hash: hash,
                coverage: 0.0, // 後で結合
                               //runtime_ms: 0,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Repository, Signature};
    use std::fs;
    use tempfile::tempdir;
    use yrs::{Doc, Text, Transact, Update};

    // --- テスト用ヘルパー ---

    /// テスト用の Git リポジトリを初期化し、H5iRepository を返す
    fn setup_test_repo(root: &std::path::Path) -> H5iRepository {
        let repo = Repository::init(root).unwrap();
        let h5i_root = root.join(".h5i");
        fs::create_dir_all(h5i_root.join("metadata")).unwrap();
        fs::create_dir_all(h5i_root.join("delta")).unwrap();

        H5iRepository {
            git_repo: repo,
            h5i_root,
        }
    }

    /// Git コミットを作成するヘルパー
    fn create_commit(
        repo: &Repository,
        message: &str,
        file_path: &str,
        content: &str,
        parents: &[&git2::Commit],
    ) -> Oid {
        let mut index = repo.index().unwrap();
        let path = std::path::Path::new(file_path);

        // ファイルを物理的に書き込んでインデックスに追加
        fs::write(repo.workdir().unwrap().join(path), content).unwrap();
        index.add_path(path).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        let sig = Signature::now("test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, parents)
            .unwrap()
    }

    // --- テストケース ---

    #[test]
    fn test_get_content_at_oid() {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let git_repo = &h5i_repo.git_repo;

        // 1. コミットを作成
        let oid = create_commit(git_repo, "initial", "hello.txt", "hello world", &[]);

        // 2. 取得検証
        let content = h5i_repo
            .get_content_at_oid(oid, std::path::Path::new("hello.txt"))
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_merge_h5i_logic_with_proper_deltas() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir().unwrap();
        let h5i_repo = setup_test_repo(dir.path());
        let git_repo = &h5i_repo.git_repo;
        let file_path = "main.py";

        // --- 1. Base (共通祖先) ---
        let base_content = "def main():\n    pass";
        let base_oid = create_commit(git_repo, "base", file_path, base_content, &[]);
        // ベース時点のデルタも保存（空の状態からの挿入として記録）
        let base_update = {
            let doc = Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            text.push(&mut txn, base_content);
            txn.encode_update_v1()
        };
        h5i_repo.persist_delta_for_commit(base_oid, file_path, &base_update)?;

        // --- 2. OURS (自分側の変更) ---
        let (our_oid, our_update) = {
            let doc = Doc::new();
            let text = doc.get_or_insert_text("code");
            // ベースを再現
            let mut txn = doc.transact_mut();
            txn.apply_update(Update::decode_v1(&base_update)?);
            // 変更を加える
            text.insert(&mut txn, 0, "# OURS COMMENT\n");
            let update = txn.encode_update_v1(); // ここでは「差分」ではなく「全状態」として一旦扱う（簡易化のため）

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

        // --- 3. THEIRS (相手側の変更) ---
        git_repo.set_head_detached(base_oid)?;
        let (their_oid, their_update) = {
            let doc = Doc::new();
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            txn.apply_update(Update::decode_v1(&base_update)?);
            // 変更を加える
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

        // --- 4. Merge 実行 ---
        let merged_text = h5i_repo.merge_h5i_logic(our_oid, their_oid, file_path)?;

        // --- 5. 検証 ---
        println!("Final Merged Text:\n{}", merged_text);
        assert!(merged_text.contains("# OURS COMMENT"));
        assert!(merged_text.contains("print('done')"));
        assert!(merged_text.contains("def main():"));

        Ok(())
    }
}
