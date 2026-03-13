use std::fs;
use std::path::{Path, PathBuf};
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, Text, TextRef, Transact};
use yrs::{ReadTxn, StateVector};

use crate::delta_store::DeltaStore;
use crate::error::H5iError;

/// Represents a local editing session backed by a CRDT document.
///
/// `LocalSession` manages a Yrs (Y-CRDT) document synchronized with:
///
/// - a shared append-only update log stored in `.git/.h5i/delta`
/// - the actual source file on disk
///
/// The CRDT log enables multiple agents or editors to concurrently
/// modify the same file while preserving strong eventual consistency.
///
/// ### Responsibilities:
///
/// - maintain an in-memory CRDT document
/// - append incremental updates to the shared delta log
/// - apply updates from other agents
/// - synchronize the final merged state to the filesystem
pub struct LocalSession {
    pub doc: Doc,
    pub text_ref: TextRef,
    pub delta_store: DeltaStore,
    pub target_fs_path: PathBuf,
    pub update_count: usize,
    pub last_read_offset: u64,
}

impl LocalSession {
    /// Creates a new `LocalSession`.
    ///
    /// The session initializes a Yrs CRDT document and connects it to a
    /// persistent delta log stored under `.git/.h5i/delta`.
    ///
    /// During initialization:
    ///
    /// 1. The target source file must already exist.
    /// 2. A CRDT document (`Doc`) and text reference are created.
    /// 3. The delta store is initialized.
    /// 4. Existing updates from disk are replayed to reconstruct the latest state.
    ///
    /// # Parameters
    ///
    /// - `repo_root`: Root directory of the repository.
    /// - `target_path`: Path to the source file being collaboratively edited.
    ///
    /// # Errors
    ///
    /// Returns an error if the target file does not exist or if
    /// synchronization from disk fails.
    pub fn new(
        repo_root: PathBuf,
        target_path: PathBuf,
        client_id: u64,
    ) -> Result<Self, crate::error::H5iError> {
        // 1. The ACTUAL source code must exist to start a session
        if !target_path.exists() {
            return Err(H5iError::InvalidPath(format!(
                "Source file not found: {:?}",
                target_path
            )));
        }

        let doc = yrs::Doc::with_options(yrs::Options::with_client_id(client_id));
        let text_ref = doc.get_or_insert_text("code");
        let delta_store = DeltaStore::new(repo_root, target_path.to_str().unwrap());

        let mut session = Self {
            doc,
            text_ref,
            delta_store,
            target_fs_path: target_path.clone(),
            update_count: 0,
            last_read_offset: 0,
        };

        // At startup, apply all existing operation logs to reconstruct the latest state
        session.sync_from_disk(&target_path)?;
        Ok(session)
    }
}

impl LocalSession {
    /// Flushes pending CRDT updates and synchronizes the file on disk.
    ///
    /// This method writes the merged CRDT text to the actual filesystem file.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the delta log or filesystem fails.
    pub fn flush_and_sync_file(&mut self) -> Result<(), crate::error::H5iError> {
        let txn = self.doc.transact_mut();
        let final_text = self.text_ref.get_string(&txn);
        std::fs::write(&self.target_fs_path, final_text)?;

        Ok(())
    }
}

impl LocalSession {
    pub fn get_current_text(&self) -> String {
        let txn = self.doc.transact();
        self.text_ref.get_string(&txn)
    }

    /// Applies a local edit and immediately persists the update.
    ///
    /// This function performs three steps:
    ///
    /// 1. Apply the edit to the CRDT document.
    /// 2. Extract and append the resulting update to the shared delta log.
    /// 3. Write the merged result to the actual source file.
    ///
    /// # Parameters
    ///
    /// - `offset`: Character position where the insertion occurs.
    /// - `content`: Text to insert.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the delta store or filesystem fails.
    pub fn apply_local_edit(
        &mut self,
        offset: u32,
        content: &str,
    ) -> Result<(), crate::error::H5iError> {
        // Apply edit in the Yrs CRDT
        let mut txn: yrs::TransactionMut<'_> = self.doc.transact_mut();

        // Capture the state vector before editing (useful for delta extraction)
        self.text_ref.insert(&mut txn, offset, content);

        // Extract and store the CRDT update
        let update = txn.encode_update_v1();
        self.delta_store.append_update(&update)?;

        // Trigger maintenance logic so tests can pass
        self.update_count += 1;
        if self.update_count % 50 == 0 {
            //let state = txn.encode_;
            let full_state = txn.encode_diff_v1(&StateVector::default());
            self.delta_store.save_snapshot(&full_state)?;
        } else if self.update_count % 10 == 0 {
            self.delta_store.compact()?;
        }

        // Map the CRDT result back to the real source file
        let merged_text = self.text_ref.get_string(&txn);
        fs::write(&self.target_fs_path, merged_text)?;

        Ok(())
    }

    pub fn ingest_diff_from_disk(&mut self) -> Result<(), crate::error::H5iError> {
        let path = self.target_fs_path.clone();

        // 1. リトライロジック付きでディスクから読み込み
        let mut content = None;
        for attempt in 0..4 {
            match fs::read_to_string(&path) {
                Ok(s) => {
                    content = Some(s);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1)));
                    continue;
                }
                Err(e) => return Err(crate::error::H5iError::Io(e)),
            }
        }
        let new_text = content.ok_or_else(|| {
            crate::error::H5iError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "File missing",
            ))
        })?;

        // 2. 現在のメモリ上のテキストを取得
        let old_text = self.get_current_text();
        if old_text == new_text {
            return Ok(());
        }

        // 3. 最小限の編集範囲を特定 (Prefix/Suffix アルゴリズム)
        let old_chars: Vec<char> = old_text.chars().collect();
        let new_chars: Vec<char> = new_text.chars().collect();

        let mut prefix_len = 0;
        while prefix_len < old_chars.len()
            && prefix_len < new_chars.len()
            && old_chars[prefix_len] == new_chars[prefix_len]
        {
            prefix_len += 1;
        }

        let mut suffix_len = 0;
        while suffix_len < (old_chars.len() - prefix_len)
            && suffix_len < (new_chars.len() - prefix_len)
            && old_chars[old_chars.len() - 1 - suffix_len]
                == new_chars[new_chars.len() - 1 - suffix_len]
        {
            suffix_len += 1;
        }

        // 4. トランザクション内で最小限の操作を適用
        {
            let mut txn = self.doc.transact_mut();

            // 古いテキストの中間部分を削除
            let remove_len = old_chars.len() - prefix_len - suffix_len;
            if remove_len > 0 {
                self.text_ref
                    .remove_range(&mut txn, prefix_len as u32, remove_len as u32);
            }

            // 新しいテキストの中間部分を挿入
            let insert_text: String = new_chars[prefix_len..(new_chars.len() - suffix_len)]
                .iter()
                .collect();
            if !insert_text.is_empty() {
                self.text_ref
                    .insert(&mut txn, prefix_len as u32, &insert_text);
            }
        }

        // 5. 差分を保存
        self.save_current_state_to_delta()?;
        Ok(())
    }

    /// Persists the current in-memory CRDT state to the local sidecar delta store.
    ///
    /// This method is called to ensure that local edits are not lost and can be
    /// reconstructed by `sync_from_disk` in future sessions.
    pub fn save_current_state_to_delta(&mut self) -> Result<(), crate::error::H5iError> {
        // We create a read transaction to encode the current state.
        // Using an empty StateVector ensures we capture the full state
        // as a single, restorable update block.
        let update_data = {
            let txn = self.doc.transact();
            // Pointed out previously: encode_state_as_update_v1 requires a &StateVector
            txn.encode_state_as_update_v1(&yrs::StateVector::default())
        };

        // Persist to the .git/.h5i/delta directory via DeltaStore
        self.delta_store.append_update(&update_data)?;

        // Increment the internal update counter for tracking session activity
        self.update_count += 1;

        Ok(())
    }

    /// Synchronizes the CRDT state from the full delta log on disk.
    ///
    /// This method reconstructs the current document state by either:
    ///
    /// - loading the initial file contents (if no updates exist), or
    /// - replaying all stored CRDT updates.
    ///
    /// # Parameters
    ///
    /// - `target_path`: Path to the source file used as the initial state.
    ///
    /// # Errors
    ///
    /// Returns an error if updates cannot be decoded or applied.
    pub fn sync_from_disk(&mut self, target_path: &Path) -> Result<(), crate::error::H5iError> {
        let mut history_applied = false;

        // 1. Try Snapshot
        let snapshot_path = self.delta_store.snapshot_path();
        if snapshot_path.exists() {
            let data = fs::read(&snapshot_path)?;
            let mut txn = self.doc.transact_mut();
            txn.apply_update(yrs::Update::decode_v1(&data)?)
                .map_err(|e| H5iError::Crdt(e.to_string()))?;
            history_applied = true;
        }

        // 2. Apply incremental updates
        let updates = self.delta_store.read_all_updates()?;
        if !updates.is_empty() {
            let mut txn = self.doc.transact_mut();
            for data in updates {
                txn.apply_update(yrs::Update::decode_v1(&data)?)
                    .map_err(|e| H5iError::Crdt(e.to_string()))?;
            }
            history_applied = true;
        }

        // 3. Fallback to raw file ONLY if no CRDT history exists and Doc is empty
        if !history_applied {
            let content = fs::read_to_string(target_path).map_err(H5iError::Io)?;
            let mut txn = self.doc.transact_mut();
            // Check if text is already populated to prevent "print()print()"
            if self.text_ref.len(&txn) == 0 {
                self.text_ref.push(&mut txn, &content);
            }
        }

        Ok(())
    }

    /// Synchronizes only new updates from the shared delta log.
    ///
    /// This function reads updates starting from `last_read_offset`
    /// and merges them into the local CRDT document.
    ///
    /// This method is useful when multiple agents concurrently append
    /// updates to the same shared log.
    ///
    /// # Errors
    ///
    /// Returns an error if updates cannot be decoded or applied.
    pub fn sync_from_shared_log(&mut self) -> Result<(), crate::error::H5iError> {
        // Start reading from the previous offset
        let (new_updates, next_offset) =
            self.delta_store.read_new_updates(self.last_read_offset)?;

        if !new_updates.is_empty() {
            let mut txn = self.doc.transact_mut();
            for data in new_updates {
                let update = yrs::Update::decode_v1(&data)?;
                txn.apply_update(update)?;
            }

            // Advance the offset only for successfully applied updates
            self.last_read_offset = next_offset;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::tempdir;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, Text, Transact};

    use crate::session::LocalSession;

    #[test]
    fn test_session_initialization_and_flush() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let temp_path = dir.path();
        let repo_root = temp_path.to_path_buf();
        let file_path = repo_root.join("test_file.py");

        // Setup: Source file MUST exist
        fs::write(&file_path, "print('hello')")?;

        // Fix: Pass the existing code path and the repo root
        let mut session = LocalSession::new(repo_root.clone(), file_path.clone(), 0)?;

        // Edit and Flush
        session.apply_local_edit(14, "\nprint('world')")?;
        session.flush_and_sync_file()?;

        let updated_content = fs::read_to_string(&file_path)?;
        assert!(updated_content.contains("hello"));
        assert!(updated_content.contains("world"));

        let delta_path = repo_root.join(".git/.h5i/delta");
        assert!(delta_path.exists());

        Ok(())
    }

    #[test]
    fn test_concurrent_conflict_resolution_simulation() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let file_path = repo_root.join("code.rs");

        // Create the source file beforehand
        fs::write(&file_path, "fn main() {}")?;

        let session = LocalSession::new(repo_root, file_path, 0)?;

        // Simulate an external agent update
        let remote_doc = Doc::new();
        let remote_text = remote_doc.get_or_insert_text("code");

        let remote_update = {
            let mut txn = remote_doc.transact_mut();
            remote_text.insert(&mut txn, 0, "// Agent Alpha\n");
            txn.encode_update_v1()
        };

        // Apply remote update locally
        {
            let mut txn = session.doc.transact_mut();
            txn.apply_update(yrs::Update::decode_v1(&remote_update)?)?;
        }

        // Verify merged result
        let final_text = session.get_current_text();

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

        let result = LocalSession::new(non_existent, delta, 0);

        // Verify that our H5iError::Io or InvalidPath is returned
        assert!(result.is_err());
    }

    #[test]
    fn test_session_compaction_logic() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let file_path = repo_root.join("compact_test.txt");

        // Initial setup: File must exist
        fs::write(&file_path, "initial content")?;

        let mut session = LocalSession::new(repo_root.clone(), file_path.clone(), 0)?;

        // 1. Perform 9 edits (compaction threshold is 10)
        for i in 0..9 {
            session.apply_local_edit(0, &format!("edit{} ", i))?;
        }

        // Verify that we have multiple updates in the log before compaction
        let updates_before = session.delta_store.read_all_updates()?;
        assert!(updates_before.len() == 9);

        // 2. Perform the 10th edit to trigger compaction
        // flush_and_sync_file will evaluate session.update_count % 10 == 0
        session.apply_local_edit(0, "compaction_trigger ")?;
        session.flush_and_sync_file()?;

        // 3. Verification: The delta log should now be merged into a single entry
        let updates_after = session.delta_store.read_all_updates()?;
        assert_eq!(
            updates_after.len(),
            1,
            "Delta log should be compacted into exactly 1 binary entry"
        );

        // 4. Content Integrity: Ensure data is still correct after compaction
        let content = fs::read_to_string(&file_path)?;
        assert!(content.contains("compaction_trigger"));
        assert!(content.contains("initial content"));
        assert!(content.contains("edit0"));
        assert!(content.contains("edit8"));

        Ok(())
    }

    #[test]
    fn test_session_snapshot_logic() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let file_path = repo_root.join("snapshot_test.txt");

        // Initial setup
        fs::write(&file_path, "baseline")?;

        let mut session = LocalSession::new(repo_root.clone(), file_path.clone(), 0)?;

        // 1. Perform 50 edits to trigger the snapshot threshold
        for _ in 0..50 {
            session.apply_local_edit(0, "x")?;
        }
        session.flush_and_sync_file()?;

        // 2. Verification: The .snapshot file should be created
        let snapshot_path = session.delta_store.log_path.with_extension("snapshot");
        assert!(
            snapshot_path.exists(),
            "Snapshot file should be created at the 50th update"
        );

        // 3. Verification: The incremental delta log (.bin) should be cleared (deleted) after snapshot
        // based on the logic in DeltaStore::save_snapshot
        assert!(
            !session.delta_store.log_path.exists(),
            "Incremental log should be removed to save space after snapshotting"
        );

        // 4. Restoration: Re-initialize a session to ensure it can hydrate from the snapshot
        // Note: This requires sync_from_disk to be updated to look for .snapshot files
        let new_session = LocalSession::new(repo_root, file_path, 0)?;
        let final_text = new_session.get_current_text();
        assert!(final_text.contains("baseline"));
        assert!(final_text.contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxbaseline"));

        Ok(())
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use yrs::{Doc, Text, Transact};

    fn get_canonical_path(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn test_sync_from_disk_cold_start() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("app.py");
        let content = "print('hello world')";
        fs::write(&file_path, content)?;

        // Initialize session - should trigger sync_from_disk fallback to raw file
        let mut session = LocalSession::new(dir.path().to_path_buf(), file_path.clone(), 1)?;

        // Use the method explicitly to verify it works as intended
        session.sync_from_disk(&file_path)?;

        assert_eq!(
            session.get_current_text(),
            content,
            "Should load raw file when no CRDT history exists"
        );
        Ok(())
    }

    #[test]
    fn test_sync_from_disk_with_incremental_updates() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let file_path = repo_root.join("logic.rs");
        fs::write(&file_path, "Original")?;

        {
            let mut session_1 = LocalSession::new(repo_root.clone(), file_path.clone(), 1)?;
            let base_update = session_1
                .doc
                .transact()
                .encode_state_as_update_v1(&yrs::StateVector::default());
            session_1.delta_store.append_update(&base_update)?;
            session_1.apply_local_edit(8, " + Update")?;
        }

        let session_2 = LocalSession::new(repo_root, file_path, 2)?;

        assert_eq!(session_2.get_current_text(), "Original + Update");
        Ok(())
    }

    #[test]
    fn test_sync_from_disk_with_snapshot_priority() -> crate::error::Result<()> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let file_path = repo_root.join("data.txt");

        // Physical file exists but is different
        fs::write(&file_path, "File Content")?;

        // Manually setup a snapshot in the .git/.h5i/metadata dir (or wherever DeltaStore points)
        let doc = Doc::new();
        let text = doc.get_or_insert_text("code");
        {
            let mut txn = doc.transact_mut();
            text.push(&mut txn, "Snapshot Content");
        }
        let snapshot_data = doc
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());

        // We use the same name formatting as DeltaStore expects
        let delta_store = DeltaStore::new(repo_root.clone(), &get_canonical_path(&file_path));
        fs::create_dir_all(repo_root.join(".git/.h5i/delta"))?;
        delta_store.save_snapshot(&snapshot_data)?;

        // New session should load "Snapshot Content" and ignore "File Content"
        let session = LocalSession::new(repo_root, file_path, 1)?;

        assert_eq!(session.get_current_text(), "Snapshot Content");
        Ok(())
    }
}
