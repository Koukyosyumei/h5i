use chrono::{DateTime, TimeZone, Utc};
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
    /// ファイルパス -> 外部から提供された AST (S式) のハッシュ
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
    /// Git の標準情報から最小限のレコードを作成する。
    /// .h5i メタデータが存在しない古いコミットを表示する際のフォールバックとして使用。
    pub fn minimal_from_git(repo: &Repository, oid: Oid) -> Self {
        // コミットオブジェクトを取得
        // 実戦では find_commit が失敗する可能性（浅いクローン等）も考慮し、
        // 呼び出し元で Result を扱う設計にするのが理想的ですが、ここでは簡略化しています。
        let commit = repo.find_commit(oid).expect("Commit not found");

        // 親コミットの OID を取得 (最初の親のみを対象とする)
        let parent_oid = if commit.parent_count() > 0 {
            Some(commit.parent_id(0).unwrap_or(Oid::zero()).to_string())
        } else {
            None
        };

        // Git のタイムスタンプを chrono::DateTime<Utc> に変換
        let time = commit.time();
        let timestamp = Utc
            .timestamp_opt(time.seconds(), 0)
            .single()
            .unwrap_or_else(Utc::now);

        H5iCommitRecord {
            git_oid: oid.to_string(),
            parent_oid,
            ai_metadata: None,  // Git 標準コミットには AI 情報はない
            test_metrics: None, // Git 標準コミットには品質データはない
            ast_hashes: None,   // Git 標準コミットには AST ハッシュはない
            timestamp,
        }
    }
}
