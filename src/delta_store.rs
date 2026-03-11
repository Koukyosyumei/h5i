use crate::error::H5iError;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::RwLock;
use yrs::Update;

pub struct DeltaStore {
    log_path: PathBuf,
}

impl DeltaStore {
    pub fn new(repo_root: PathBuf, file_path: &str) -> Self {
        let hash = sha256_hash(file_path); // ファイルパスをハッシュ化してファイル名に
        let log_path = repo_root.join(".h5i/delta").join(format!("{}.bin", hash));
        Self { log_path }
    }

    /// 自分の更新分を追記する
    pub fn append_update(&self, data: &[u8]) -> Result<(), H5iError> {
        // 親ディレクトリ (.h5i/delta) が存在することを確認
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| H5iError::Io(e))?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        // [データ長(u32)][バイナリデータ] の形式で保存
        let len = data.len() as u32;
        file.write_all(&len.to_le_bytes())?;
        file.write_all(data)?;
        Ok(())
    }

    /// 全ての操作ログを読み出す
    pub fn read_all_updates(&self) -> Result<Vec<Vec<u8>>, H5iError> {
        if !self.log_path.exists() {
            return Ok(vec![]);
        }
        let mut file = File::open(&self.log_path)?;
        let mut updates = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            if file.read_exact(&mut len_buf).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];
            file.read_exact(&mut data)?;
            updates.push(data);
        }
        Ok(updates)
    }

    /// 1. Snapshotting: 現在の完全な状態を保存し、それ以前のログを実質的にリセットする
    pub fn save_snapshot(&self, state_v1: &[u8]) -> Result<(), H5iError> {
        let snapshot_path = self.log_path.with_extension("snapshot");
        fs::write(&snapshot_path, state_v1).map_err(|e| H5iError::Io(e))?;

        // スナップショットが取れたら、古いデルタログをクリア（またはリネームして退避）
        if self.log_path.exists() {
            fs::remove_file(&self.log_path).map_err(|e| H5iError::Io(e))?;
        }
        Ok(())
    }

    /// 2. Compaction: 複数の小さいUpdateを1つの大きなUpdateにマージしてディスクを節約
    pub fn compact(&self) -> Result<(), H5iError> {
        let updates = self.read_all_updates()?;
        if updates.len() < 2 {
            return Ok(());
        }

        // yrs のマージ機能を使用
        let merged = yrs::merge_updates_v1(&updates).map_err(|e| H5iError::Crdt(e.to_string()))?;

        // ログを一度消して、マージされた1つのデータで書き直す
        fs::remove_file(&self.log_path).ok();
        self.append_update(&merged)?;

        println!(
            "📦 Compaction complete: {} updates -> 1 merged update",
            updates.len()
        );
        Ok(())
    }

    /// 指定されたオフセットから新しく追記された更新分だけを読み出す。
    /// 返り値: (新規更新データのリスト, 次回読み出し用のオフセット)
    /// ロックを一旦排して、シンプルに差分読み込みを行う
    pub fn read_new_updates(&self, mut offset: u64) -> Result<(Vec<Vec<u8>>, u64), H5iError> {
        if !self.log_path.exists() {
            return Ok((vec![], 0));
        }

        // ロックを使わず、個別のファイルハンドルを作成して mut で扱う
        let mut file = File::open(&self.log_path).map_err(H5iError::Io)?;

        // 指定されたオフセットまで移動 (mut が必要)
        file.seek(SeekFrom::Start(offset)).map_err(H5iError::Io)?;

        let mut new_updates = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            // 長さ情報の読み込み
            if file.read_exact(&mut len_buf).is_err() {
                break; // EOF
            }

            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];

            if file.read_exact(&mut data).is_err() {
                break; // 書き込み途中の不完全なデータ
            }

            offset += 4 + len as u64;
            new_updates.push(data);
        }

        Ok((new_updates, offset))
    }
}

pub fn sha256_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use yrs::updates::decoder::Decode;
    use yrs::GetString;
    use yrs::{Doc, Text, Transact, Update};

    #[test]
    fn test_delta_store_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        // 1. Setup: Use a temporary directory for isolation
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();

        // Ensure the directory structure h5i expects exists
        fs::create_dir_all(repo_root.join(".h5i/delta"))?;

        let file_path = "src/main.rs";
        let store = DeltaStore::new(repo_root.clone(), file_path);

        // 2. Define sample binary updates (simulating yrs updates)
        let update_1 = vec![0x01, 0x02, 0x03];
        let update_2 = vec![0xFF, 0xEE, 0xDD, 0xCC];
        let update_3 = vec![0x00];

        // 3. Append updates
        store.append_update(&update_1)?;
        store.append_update(&update_2)?;
        store.append_update(&update_3)?;

        // 4. Read back and verify
        let results = store.read_all_updates()?;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], update_1);
        assert_eq!(results[1], update_2);
        assert_eq!(results[2], update_3);

        Ok(())
    }

    #[test]
    fn test_empty_log_returns_empty_vec() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let store = DeltaStore::new(dir.path().to_path_buf(), "non_existent.rs");

        let results = store.read_all_updates()?;
        assert!(
            results.is_empty(),
            "Reading a non-existent log should return an empty Vec"
        );

        Ok(())
    }

    #[test]
    fn test_persistence_across_instances() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();
        fs::create_dir_all(repo_root.join(".h5i/delta"))?;

        let file_path = "lib.rs";
        let payload = vec![0xAA, 0xBB, 0xCC];

        // Instance 1: Write
        {
            let store = DeltaStore::new(repo_root.clone(), file_path);
            store.append_update(&payload)?;
        }

        // Instance 2: Read from the same file path
        {
            let store = DeltaStore::new(repo_root, file_path);
            let results = store.read_all_updates()?;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0], payload);
        }

        Ok(())
    }

    #[test]
    fn test_large_payload_integrity() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();
        fs::create_dir_all(repo_root.join(".h5i/delta"))?;

        let store = DeltaStore::new(repo_root, "large_file.bin");

        // Create a 1MB payload
        let large_data = vec![0u8; 1_024 * 1_024];
        store.append_update(&large_data)?;

        let results = store.read_all_updates()?;
        assert_eq!(results[0].len(), 1_024 * 1_024);

        Ok(())
    }

    /// 1. Snapshotのテスト: デルタログが消去され、スナップショットが生成されるか
    #[test]
    fn test_save_snapshot_clears_delta_log() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();
        let store = DeltaStore::new(repo_root, "test.rs");

        // 複数のアップデートを書き込む
        store.append_update(&[1, 2, 3])?;
        store.append_update(&[4, 5, 6])?;
        assert!(store.log_path.exists());

        // スナップショットを保存
        let dummy_state = vec![0xDE, 0xAD, 0xBE, 0xEF];
        store.save_snapshot(&dummy_state)?;

        // 検証: デルタログ (.bin) が消え、スナップショット (.snapshot) が存在すること
        assert!(
            !store.log_path.exists(),
            "Delta log should be removed after snapshot"
        );
        let snapshot_path = store.log_path.with_extension("snapshot");
        assert!(snapshot_path.exists(), "Snapshot file should be created");

        // 検証: 内容が正しいこと
        let saved_data = std::fs::read(snapshot_path)?;
        assert_eq!(saved_data, dummy_state);

        Ok(())
    }

    /// 2. Compactionのテスト: 複数のUpdateが1つに統合され、CRDT状態が維持されるか
    #[test]
    fn test_compact_integrates_multiple_updates() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();
        let store = DeltaStore::new(repo_root, "compaction_test.rs");

        // 実際に yrs を使って意味のある Update を生成
        let doc = Doc::new();
        let text = doc.get_or_insert_text("code");

        let mut updates = Vec::new();

        // 操作1: "Hello " を挿入
        {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 0, "Hello ");
            updates.push(txn.encode_update_v1());
        }
        // 操作2: "World" を挿入
        {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 6, "World");
            updates.push(txn.encode_update_v1());
        }

        // デルタストアに保存
        for u in &updates {
            store.append_update(u)?;
        }

        // Compaction 実行
        store.compact()?;

        // 検証: ログ内のエントリー数が 1 になっていること
        let read_updates = store.read_all_updates()?;
        assert_eq!(
            read_updates.len(),
            1,
            "Should be compacted into a single update"
        );

        // セマンティック検証: マージされたデータを新しい Doc に適用して "Hello World" になるか
        let new_doc = Doc::new();
        let new_text = new_doc.get_or_insert_text("code");
        {
            let mut txn = new_doc.transact_mut();
            txn.apply_update(Update::decode_v1(&read_updates[0])?);
            assert_eq!(new_text.get_string(&txn), "Hello World");
        }

        Ok(())
    }

    /// 3. エッジケース: 更新が1つしかない場合の Compaction は何もしない
    #[test]
    fn test_compact_noop_for_single_update() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let store = DeltaStore::new(dir.path().to_path_buf(), "noop.rs");

        store.append_update(&[1, 2, 3])?;
        store.compact()?;

        let updates = store.read_all_updates()?;
        assert_eq!(updates.len(), 1);

        Ok(())
    }
}
