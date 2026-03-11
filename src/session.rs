use std::fs;
use std::path::{Path, PathBuf};
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, Text, TextRef, Transact, Update};

use crate::delta_store::DeltaStore;
use crate::error::H5iError;

/// Represents a local editing session backed by a CRDT document.
///
/// `LocalSession` manages a Yrs (Y-CRDT) document synchronized with:
///
/// - a shared append-only update log stored in `.h5i/delta`
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
    /// persistent delta log stored under `.h5i/delta`.
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
    pub fn new(repo_root: PathBuf, target_path: PathBuf) -> Result<Self, crate::error::H5iError> {
        // 1. The ACTUAL source code must exist to start a session
        if !target_path.exists() {
            return Err(H5iError::InvalidPath(format!(
                "Source file not found: {:?}",
                target_path
            )));
        }

        let doc = Doc::new();
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
    /// This method should typically be called before committing changes.
    ///
    /// Steps performed:
    ///
    /// 1. Encode the latest CRDT update.
    /// 2. Append the update to the shared binary delta log.
    /// 3. Optionally perform log compaction or snapshotting.
    /// 4. Write the merged CRDT text to the actual filesystem file.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the delta log or filesystem fails.
    pub fn flush_and_sync_file(&mut self) -> Result<(), crate::error::H5iError> {
        // Encode the current document update
        let txn = self.doc.transact_mut();
        let update = txn.encode_update_v1();

        // Append to the shared binary log (.h5i/delta/...)
        self.delta_store.append_update(&update)?;
        self.update_count += 1;

        // Example strategy: compact every 10 updates, snapshot every 50
        if self.update_count % 50 == 0 {
            // let state = txn.encode_state_as_update_v1(sv);
            // self.delta_store.save_snapshot(&state)?;
        } else if self.update_count % 10 == 0 {
            self.delta_store.compact()?;
        }

        // Write the latest merged text to the actual file
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
        let mut txn = self.doc.transact_mut();

        // Capture the state vector before editing (useful for delta extraction)
        self.text_ref.insert(&mut txn, offset, content);

        // Extract and store the CRDT update
        let update = txn.encode_update_v1();
        self.delta_store.append_update(&update)?;

        // Map the CRDT result back to the real source file
        let merged_text = self.text_ref.get_string(&txn);
        fs::write(&self.target_fs_path, merged_text)?;

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
        let updates = self.delta_store.read_all_updates()?;

        if updates.is_empty() {
            // Case 1: No updates exist — load the file contents
            let content = fs::read_to_string(target_path).map_err(H5iError::Io)?;
            let mut txn = self.doc.transact_mut();
            self.text_ref.push(&mut txn, &content);
        } else {
            // Case 2: Updates exist — replay the update log
            let mut txn = self.doc.transact_mut();
            for data in updates {
                let update = Update::decode_v1(&data)?;
                txn.apply_update(update)?;
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
        let mut session = LocalSession::new(repo_root.clone(), file_path.clone())?;

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
        let repo_root = dir.path().to_path_buf();
        let file_path = repo_root.join("code.rs");

        // Create the source file beforehand
        fs::write(&file_path, "fn main() {}")?;

        // 2. セッションの作成 (引数の順番を new(repo_root, file_path) に合わせる)
        let session = LocalSession::new(repo_root, file_path)?;

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

        let result = LocalSession::new(non_existent, delta);

        // Verify that our H5iError::Io or InvalidPath is returned
        assert!(result.is_err());
    }
}
