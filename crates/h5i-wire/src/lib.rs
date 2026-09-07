//! The shapes h5i writes about an HTTP message.
//!
//! Two artifacts, and the difference between them is the whole reason this
//! crate is separate from the engine as well as from the reader:
//!
//! - a **receipt row** ([`record`]) is the decision and the outcome, counts
//!   rather than values, safe to export;
//! - a **stored message** ([`message`]) is the bytes, headers and credentials,
//!   owner-only and never exported unless the caller names it.
//!
//! Both are read by code that has no business linking a browser: the websec
//! plugin (design-websec.md W21) and recon's ledger (design-recon.md N5). The
//! writers stay in `h5i-browser`, because writing a receipt is the engine's
//! fail-closed duty and not a shape anyone else may take on.

pub mod message;
pub mod record;

pub use message::{
    Body, Health, MAX_BODY_BYTES, MAX_STORE_BYTES, Skip, StoredRequest, StoredResponse, body_file, message_file,
};
pub use record::{Initiator, Phase, RequestRecord, now_rfc3339};
