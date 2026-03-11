use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::H5iError;

/// Computes the SHA-256 hash of a string and returns the hexadecimal representation.
///
/// This helper function is used to deterministically map file paths to
/// delta log filenames inside the `.h5i/delta` directory.
///
/// # Parameters
///
/// - `input`: The input string to hash.
///
/// # Returns
///
/// A lowercase hexadecimal string representing the SHA-256 digest.
pub fn sha256_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

/// Persistent storage for CRDT update logs.
///
/// `DeltaStore` maintains an append-only binary log containing
/// serialized CRDT updates for a single source file.
///
/// Each source file maps to a unique log file under:
///
/// `.h5i/delta/<sha256(file_path)>.bin`
///
/// The log format is:
///
/// `[length: u32][update bytes]`
pub struct DeltaStore {
    pub log_path: PathBuf,
}

impl DeltaStore {
    pub fn new(repo_root: PathBuf, file_path: &str) -> Self {
        let hash = sha256_hash(file_path); // Hash the file path to generate a stable log filename
        let log_path = repo_root.join(".h5i/delta").join(format!("{}.bin", hash));
        Self { log_path }
    }
}

impl DeltaStore {
    /// Returns the path to the active (uncommitted) delta log.
    /// This corresponds to the current working session's changes.
    pub fn active_path(&self) -> &PathBuf {
        &self.log_path
    }

    /// Returns the path to the current session's snapshot.
    pub fn snapshot_path(&self) -> PathBuf {
        self.log_path.with_extension("snapshot")
    }

    /// Returns the path to a committed delta log associated with a specific OID.
    /// This follows the structure: .h5i/deltas/<oid>/<file_hash>.bin
    pub fn committed_path(repo_root: &Path, oid: &str, file_path: &str) -> PathBuf {
        let hash = sha256_hash(file_path);
        repo_root
            .join(".h5i/deltas")
            .join(oid)
            .join(format!("{}.bin", hash))
    }
}

impl DeltaStore {
    /// Appends a CRDT update to the delta log.
    ///
    /// Updates are stored in a binary format:
    ///
    /// `[data_length (u32)][binary update data]`
    ///
    /// This method ensures that the parent directory exists
    /// before writing the update.
    ///
    /// # Parameters
    ///
    /// - `data`: Serialized CRDT update bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created
    /// or if writing to the log file fails.
    pub fn append_update(&self, data: &[u8]) -> Result<(), H5iError> {
        // Ensure the parent directory (.h5i/delta) exists
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| H5iError::Io(e))?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        let len = data.len() as u32;
        file.write_all(&len.to_le_bytes())?;
        file.write_all(data)?;
        Ok(())
    }

    /// Saves a full CRDT snapshot and resets the delta log.
    ///
    /// Snapshotting stores the complete document state and
    /// removes previous incremental updates. This prevents
    /// the log from growing indefinitely.
    ///
    /// Snapshot files are stored alongside the delta log
    /// using the `.snapshot` extension.
    ///
    /// # Parameters
    ///
    /// - `state_v1`: Serialized CRDT document state.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot be written
    /// or if the existing log cannot be removed.
    pub fn save_snapshot(&self, state_v1: &[u8]) -> Result<(), H5iError> {
        let snapshot_path = self.log_path.with_extension("snapshot");
        fs::write(&snapshot_path, state_v1).map_err(|e| H5iError::Io(e))?;

        // After snapshotting, clear the old delta log
        if self.log_path.exists() {
            fs::remove_file(&self.log_path).map_err(|e| H5iError::Io(e))?;
        }
        Ok(())
    }

    /// Compacts multiple CRDT updates into a single merged update.
    ///
    /// Compaction reduces disk usage by merging several small
    /// updates into one larger update using Yrs' merge functionality.
    ///
    /// If fewer than two updates exist, compaction is skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if updates cannot be read, merged,
    /// or written back to disk.
    pub fn compact(&self) -> Result<(), H5iError> {
        let updates = self.read_all_updates()?;
        if updates.len() < 2 {
            return Ok(());
        }

        let merged = yrs::merge_updates_v1(&updates).map_err(|e| H5iError::Crdt(e.to_string()))?;

        // Replace the log with the merged update
        fs::remove_file(&self.log_path).ok();
        self.append_update(&merged)?;

        Ok(())
    }
}
impl DeltaStore {
    /// Reads all updates from the delta log.
    ///
    /// This method parses the append-only log and returns
    /// each stored update as a byte vector.
    ///
    /// If the log file does not exist, an empty vector is returned.
    ///
    /// # Returns
    ///
    /// A vector containing all serialized CRDT updates.
    ///
    /// # Errors
    ///
    /// Returns an error if the log file cannot be read.
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

    /// Reads only newly appended updates starting from a given offset.
    ///
    /// This method enables incremental synchronization by reading
    /// only updates that were appended after the last read position.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset indicating where reading should begin.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    ///
    /// - `Vec<Vec<u8>>`: Newly discovered updates
    /// - `u64`: The next offset for subsequent reads
    ///
    /// # Errors
    ///
    /// Returns an error if the log file cannot be accessed or read.
    pub fn read_new_updates(&self, mut offset: u64) -> Result<(Vec<Vec<u8>>, u64), H5iError> {
        if !self.log_path.exists() {
            return Ok((vec![], 0));
        }

        let mut file = File::open(&self.log_path).map_err(H5iError::Io)?;

        // Seek to the requested offset
        file.seek(SeekFrom::Start(offset)).map_err(H5iError::Io)?;

        let mut new_updates = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            if file.read_exact(&mut len_buf).is_err() {
                break; // EOF
            }

            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];

            if file.read_exact(&mut data).is_err() {
                break; // Incomplete data (likely mid-write)
            }

            offset += 4 + len as u64;
            new_updates.push(data);
        }

        Ok((new_updates, offset))
    }
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

    /// 1. Snapshot test: verifies that the delta log is removed
    /// and a snapshot file is created successfully.
    #[test]
    fn test_save_snapshot_clears_delta_log() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();
        let store = DeltaStore::new(repo_root, "test.rs");

        // Write multiple updates to the delta log
        store.append_update(&[1, 2, 3])?;
        store.append_update(&[4, 5, 6])?;
        assert!(store.log_path.exists());

        // Save a snapshot
        let dummy_state = vec![0xDE, 0xAD, 0xBE, 0xEF];
        store.save_snapshot(&dummy_state)?;

        // Verify that the delta log (.bin) is removed and the snapshot (.snapshot) exists
        assert!(
            !store.log_path.exists(),
            "Delta log should be removed after snapshot"
        );
        let snapshot_path = store.log_path.with_extension("snapshot");
        assert!(snapshot_path.exists(), "Snapshot file should be created");

        // Verify that the snapshot content is correct
        let saved_data = std::fs::read(snapshot_path)?;
        assert_eq!(saved_data, dummy_state);

        Ok(())
    }

    /// 2. Compaction test: verifies that multiple updates are merged into
    /// a single update while preserving the CRDT state.
    #[test]
    fn test_compact_integrates_multiple_updates() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let repo_root = dir.path().to_path_buf();
        let store = DeltaStore::new(repo_root, "compaction_test.rs");

        // Generate meaningful CRDT updates using yrs
        let doc = Doc::new();
        let text = doc.get_or_insert_text("code");

        let mut updates = Vec::new();

        // Operation 1: insert "Hello "
        {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 0, "Hello ");
            updates.push(txn.encode_update_v1());
        }
        // Operation 2: insert "World"
        {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 6, "World");
            updates.push(txn.encode_update_v1());
        }

        // Store the updates in the delta store
        for u in &updates {
            store.append_update(u)?;
        }

        // Run compaction
        store.compact()?;

        // Verify that the log now contains only a single update
        let read_updates = store.read_all_updates()?;
        assert_eq!(
            read_updates.len(),
            1,
            "Should be compacted into a single update"
        );

        // Semantic verification:
        // Apply the merged update to a new document and check if the result is "Hello World"
        let new_doc = Doc::new();
        let new_text = new_doc.get_or_insert_text("code");
        {
            let mut txn = new_doc.transact_mut();
            txn.apply_update(Update::decode_v1(&read_updates[0])?)?;
            assert_eq!(new_text.get_string(&txn), "Hello World");
        }

        Ok(())
    }

    /// 3. Edge case test: compaction should be a no-op
    /// when only a single update exists.
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
