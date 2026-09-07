//! Reading a session's stored messages, without the engine that wrote them.
//!
//! This is what the `h5i-wire` extraction bought: a plugin can read the bytes a
//! session captured without linking a browser (design-websec.md W21). The store
//! is opt-in, owner-only, and holds credentials, so nothing here copies it
//! anywhere; it reads, bounded, and hands back text to scan.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use h5i_wire::message::{Body, StoredRequest, StoredResponse, body_file, message_file};

/// The most body text one extractor pass will read from a single message.
///
/// A stored body can be 8 MiB, and a document that size discloses no more than
/// its first megabyte does.
pub const MAX_SCAN_BYTES: usize = 1024 * 1024;

/// One captured message, as far as recon needs it.
pub struct Message {
    pub seq: u64,
    pub request: StoredRequest,
    pub response: StoredResponse,
}

impl Message {
    /// The header value, case-insensitively, as the response carried it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }
}

/// Every sequence number the store holds, in order.
pub fn sequences(store: &Path) -> Vec<u64> {
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(store) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some((seq, _)) = name.split_once('.')
                && let Ok(seq) = seq.parse::<u64>()
            {
                seen.insert(seq);
            }
        }
    }
    seen.into_iter().collect()
}

/// One message's two halves, or `None` when the store has only one of them.
pub fn read(store: &Path, seq: u64) -> Option<Message> {
    let request: StoredRequest = read_json(&message_file(store, seq, "request"))?;
    let response: StoredResponse = read_json(&message_file(store, seq, "response"))?;
    Some(Message {
        seq,
        request,
        response,
    })
}

/// A response body as text to scan, bounded and lossy.
///
/// Lossy on purpose: an extractor looks for URL shapes, and one invalid byte in
/// a mixed body is no reason to stop looking. It is never the authority on what
/// the body was; that is the store, and `h5i websec show` reads it exactly.
pub fn body_text(store: &Path, body: &Body) -> Option<String> {
    let Body::Stored { sha256, .. } = body else {
        return None;
    };
    let path = body_file(store, sha256)?;
    let bytes = std::fs::read(&path).ok()?;
    let head = &bytes[..bytes.len().min(MAX_SCAN_BYTES)];
    Some(String::from_utf8_lossy(head).into_owned())
}

fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &PathBuf) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}
