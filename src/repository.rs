use git2::Repository;
use git2::{Commit, Index, ObjectType, Oid, Signature};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::blame::{BlameMode, BlameResult};
use crate::error::H5iError;
use crate::metadata::{AiMetadata, H5iCommitRecord, TestMetrics};

pub struct H5iRepository {
    git_repo: Repository,
    h5i_root: PathBuf,
}

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
    pub fn save_ast_to_sidecar(&self, file_path: &str, sexp: &str) -> Result<String, H5iError> {
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
}

impl H5iRepository {
    fn get_head_commit(&self) -> Result<Commit, git2::Error> {
        let obj = self.git_repo.head()?.resolve()?.peel(ObjectType::Commit)?;
        obj.into_commit()
            .map_err(|_| git2::Error::from_str("Not a commit"))
    }
}

// src/repository.rs (内部メソッド)

impl H5iRepository {
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

    pub fn git(&self) -> &Repository {
        &self.git_repo
    }

    pub fn h5i_path(&self) -> &Path {
        &self.h5i_root
    }
}

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
}

// src/repository.rs

impl H5iRepository {
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
                .unwrap_or_else(|_| H5iCommitRecord::from_git_only(&self.git_repo, oid));
            logs.push(record);
        }
        Ok(logs)
    }
}

// src/repository.rs の一部
impl H5iRepository {
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
                    is_semantic_match: false,
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
}

impl H5iRepository {
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

impl H5iRepository {
    pub fn get_blame(
        &self,
        path: &Path,
        use_ast: bool,
    ) -> Result<Vec<crate::blame::H5iBlameEntry>, H5iError> {
        if use_ast {
            // 1. 最新のレコードから AST ハッシュ履歴を取得
            // 2. 構造が変わったコミットを特定
            self.compute_semantic_blame(path)
        } else {
            // 標準の git2 blame を実行し、メタデータを Join する
            self.compute_line_blame(path)
        }
    }
}
