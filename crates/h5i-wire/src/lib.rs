//! The shapes h5i writes about an HTTP message: the receipt row ([`record`],
//! counts, exportable) and the stored message ([`message`], bytes and
//! credentials, owner-only).
//!
//! Separate from the engine so the plugins can read both without linking one
//! (design-websec.md W21). The writers stay in `h5i-browser`.

pub mod message;
pub mod record;

pub use message::{
    Body, Health, MAX_BODY_BYTES, MAX_STORE_BYTES, Skip, StoredRequest, StoredResponse, body_file, message_file,
};
pub use record::{Initiator, Phase, RequestRecord, now_rfc3339};
