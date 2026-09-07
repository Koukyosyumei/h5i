//! Reconnaissance: what an application exposes, and how h5i knows.
//!
//! The ledger ([`ledger`]) is the whole of it. Everything else here fills the
//! ledger from evidence a session already holds, and nothing here sends a
//! request: recon's own fetches go through the engine's broker like every
//! other fetch, so policy decides and the receipt is written first
//! (design-recon.md N13).
//!
//! The discipline this crate exists to enforce is in [`ledger::State`]: a URL
//! read out of a bundle and a URL that answered are different states, and no
//! amount of convenience is allowed to collapse them.

pub mod extract;
pub mod ingest;
pub mod js;
pub mod ledger;
pub mod store;

pub use extract::{Found, from_headers, from_html, from_json};
pub use ingest::{Ingested, from_receipts};
pub use ledger::{
    Endpoint, Inventory, Ledger, Observation, Param, Progress, Source, State, Where,
};
