use yrs::{Doc, GetString, Transact};

use crate::error::H5iError;

pub struct CrdtSession {
    doc: Doc,
    file_id: String,
}

impl CrdtSession {
    pub fn new(file_id: &str) -> Self {
        CrdtSession {
            doc: Doc::new(),
            file_id: file_id.to_string(),
        }
    }

    /// 現在のテキスト状態を取得
    pub fn get_content(&self) -> String {
        let text = self.doc.get_or_insert_text("content");
        text.get_string(&self.doc.transact())
    }

    /// 外部からの更新（他のAgentや人間）をマージ
    pub fn apply_update(&mut self, update: Vec<u8>) -> Result<(), H5iError> {
        use yrs::updates::decoder::Decode;
        use yrs::Update;

        let mut txn = self.doc.transact_mut();
        let update = Update::decode_v1(&update)?;
        txn.apply_update(update)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::LocalAgentSession;
    use std::fs;
    use tempfile::tempdir;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, Text, Transact};

    #[test]
    fn test_session_initialization_and_flush() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let temp_path = dir.path();
        let repo_root = temp_path.to_path_buf();
        let file_path = repo_root.join("test_file.py");

        // Setup: Source file MUST exist
        fs::write(&file_path, "print('hello')")?;

        // Fix: Pass the existing code path and the repo root
        let mut session = LocalAgentSession::new(repo_root.clone(), file_path.clone())?;

        // Edit and Flush
        {
            let mut txn = session.doc.transact_mut();
            session.text_ref.insert(&mut txn, 14, "\nprint('world')");
        }
        session.flush_and_sync_file()?;

        let updated_content = fs::read_to_string(&file_path)?;
        assert!(updated_content.contains("hello"));
        assert!(updated_content.contains("world"));

        let delta_path = repo_root.join(".h5i/delta");
        assert!(delta_path.exists());
        /*
        assert!(delta_path.exists());
        let delta_size = fs::metadata(&delta_path)?.len();
        assert!(
            delta_size > 0,
            "Delta store should have recorded the update"
        );*/

        Ok(())
    }

    #[test]
    fn test_concurrent_conflict_resolution_simulation() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf(); // 第一引数：リポジトリルート
        let file_path = repo_root.join("code.rs"); // 第二引数：ファイルパス

        // 1. 事前にソースファイルを作成しておく
        fs::write(&file_path, "fn main() {}")?;

        // 2. セッションの作成 (引数の順番を new(repo_root, file_path) に合わせる)
        let mut session = LocalAgentSession::new(repo_root, file_path)?;

        // 3. 外部エージェントの更新をシミュレート
        let remote_doc = Doc::new();
        // 重要: LocalAgentSession 内の識別子 "code" と合わせる必要があります
        let remote_text = remote_doc.get_or_insert_text("code");

        let remote_update = {
            let mut txn = remote_doc.transact_mut();
            remote_text.insert(&mut txn, 0, "// Agent Alpha\n");
            txn.encode_update_v1()
        };

        // 4. ローカルセッションにリモートの更新を適用
        {
            let mut txn = session.doc.transact_mut();
            // yrs の Update::decode_v1 は Result を返すため ? で処理
            txn.apply_update(yrs::Update::decode_v1(&remote_update)?);
        }

        // 5. マージ結果の検証
        let final_text = session.get_current_text();

        // Agent Alpha のコメントが先頭にあり、かつ元の fn main も残っていることを確認
        assert!(
            final_text.contains("// Agent Alpha"),
            "Should contain remote comment. Current text: {}",
            final_text
        );
        assert!(
            final_text.contains("fn main()"),
            "Should preserve local code. Current text: {}",
            final_text
        );

        Ok(())
    }

    #[test]
    fn test_error_on_missing_file() {
        let dir = tempdir().unwrap();
        let non_existent = dir.path().join("ghost.txt");
        let delta = dir.path().join("delta.bin");

        let result = LocalAgentSession::new(non_existent, delta);

        // Verify that our H5iError::Io or InvalidPath is returned
        assert!(result.is_err());
    }
}
