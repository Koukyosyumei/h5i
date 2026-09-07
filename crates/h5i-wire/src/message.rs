//! What a capture store holds about one message: the bytes, the headers, and
//! an explicit state for every body it does not have.
//!
//! Data only, and the counterpart to [`crate::record`]: a receipt row counts
//! cookies, this counts nothing and keeps the values. That is why the store is
//! owner-only and never exported unless the caller names it
//! (design-websec.md W5). The writer stays with the engine in
//! `h5i_browser::capture`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum body stored without truncation.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Maximum total body storage per session.
pub const MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;

/// The file a body hash names inside a store, or `None` when it is not a hash.
///
/// A boxed session's store is on a filesystem the boxed code can write to, so a
/// `..` in that field would name a path outside it. Hex and length are the
/// whole check. Public so h5i's own reader shares the rule rather than drifting.
pub fn body_file(store: &Path, sha256: &str) -> Option<PathBuf> {
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(store.join("bodies").join(sha256))
}

/// Why a body is not in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Skip {
    /// A media type from the skip list.
    Media,
    /// The session store is full. Oversized individual bodies are truncated.
    StoreFull,
    /// The engine did not read the body.
    NotRead,
    /// Storage failed; the evidence gap remains visible.
    Failed,
}

/// Where a message's body went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Body {
    /// There was no body. A GET's request body, or a 204.
    Empty,
    /// In the store, under `sha256`, which is also its file name.
    Stored {
        sha256: String,
        /// How many bytes are in the store.
        bytes: u64,
        /// How many bytes there were, when that is a larger number.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        of_bytes: Option<u64>,
        /// Set when only the head was kept.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// Not stored, and why.
    Skipped {
        reason: Skip,
        /// How large it was, when the engine had it in hand to measure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
    },
}

/// One request as sent, including client- and cookie-added headers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRequest {
    pub seq: u64,
    pub at: String,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// One response, as the engine received it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredResponse {
    pub seq: u64,
    pub at: String,
    /// URL for this redirect hop.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    /// Wire encoding; stored bodies are decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    /// What crossed the wire, when that is a different number from the body's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_bytes: Option<u64>,
    pub body: Body,
    /// Bytes the connection carried after this response ended.
    ///
    /// Empty for every ordinary fetch, because one request gets one response.
    /// A raw send that desynchronised a proxy from its backend gets two, and
    /// the second is the smuggled request's answer — the evidence the attack
    /// worked. Kept beside the response rather than merged into its body,
    /// because it is not this response's body; it is a different message that
    /// arrived on the same socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing: Option<Body>,
}

/// Store counters, including evidence gaps in `errors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// Message files written, both phases.
    pub messages: u64,
    /// Bytes of body held.
    pub bytes: u64,
    /// Messages this store could not write.
    pub errors: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_name_that_is_not_a_hash_names_nothing() {
        let store = Path::new("/sessions/s1/messages");
        // Hex and length are the whole check, and they have to be, because a
        // boxed session's store is writable by boxed code: a traversal in this
        // field would name a file outside the store.
        assert!(body_file(store, "../../../etc/passwd").is_none());
        assert!(body_file(store, "").is_none());
        assert!(body_file(store, &"g".repeat(64)).is_none());

        let sha = "a".repeat(64);
        assert_eq!(
            body_file(store, &sha).expect("a real hash names its file"),
            store.join("bodies").join(&sha)
        );
    }

    #[test]
    fn a_skipped_body_keeps_the_reason_across_a_round_trip() {
        // An absent body is a state, never an empty one: "the store was full"
        // and "there was nothing to store" are different evidence.
        let full = Body::Skipped {
            reason: Skip::StoreFull,
            bytes: Some(9),
        };
        let json = serde_json::to_string(&full).expect("serialise");
        assert_eq!(
            serde_json::from_str::<Body>(&json).expect("round trip"),
            full
        );
        assert!(json.contains("store-full"), "{json}");
        assert_ne!(full, Body::Empty);
    }
}
