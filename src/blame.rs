use crate::metadata::{AiMetadata, TestMetrics};

pub enum BlameMode {
    Line, // 従来の行ベース (標準)
    Ast,  // AST ハッシュベース (Optional)
}

pub struct BlameEntry {
    pub commit_oid: String,
    pub author_name: String,
    pub ai_metadata: Option<AiMetadata>,   // AIが書いた場合
    pub test_metrics: Option<TestMetrics>, // その時のテスト結果
}

pub struct H5iBlameEntry {
    pub line_number: usize,
    pub commit_id: String,
    pub ai_metadata: Option<AiMetadata>, // AIが関与した場合
    pub test_passed: Option<bool>,       // その時のテスト状態
    pub is_semantic: bool,               // ASTベースでの特定か
}

#[derive(Debug)]
pub struct BlameResult {
    pub line_number: usize,
    pub line_content: String,
    pub commit_id: String,
    pub agent_info: String,       // "Human" または "AI:ModelName"
    pub is_semantic_change: bool, // ASTレベルでの変更があったか
    pub test_passed: Option<bool>,
}
