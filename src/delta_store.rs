use crate::error::H5iError;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

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
}

fn sha256_hash(input: &str) -> String {
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
}
