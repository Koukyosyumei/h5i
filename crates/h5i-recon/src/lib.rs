//! Reconnaissance: what an application exposes, and how h5i knows.
//!
//! Nothing here sends a request; the plugin does that through `h5i browser`
//! (design-recon.md N13). The discipline is [`ledger::State`]: a URL read out
//! of a bundle and a URL that answered never collapse into one state.

pub mod crawl;
pub mod extract;
pub mod import;
pub mod ingest;
pub mod jobs;
pub mod js;
pub mod known;
pub mod ledger;
pub mod paths;
pub mod store;
pub mod triage;

pub use extract::{Found, from_headers, from_html, from_json};
pub use ingest::{Ingested, from_receipts};
pub use ledger::{
    Endpoint, Inventory, Ledger, Observation, Param, Progress, Source, State, Where,
};
