//! Calibration and clustering, as recon uses them (design-recon.md N11).
//!
//! The fold itself lives in `h5i-wire`, because the workbench's experiment
//! folds responses too and two copies of "these look alike" would drift apart.
//! What recon keeps is the question it asks: [`By::Shape`], which puts two
//! renderings of one template in one row.

pub use h5i_wire::triage::{
    Baseline, By, Cluster, Fingerprint, MAX_NAMES_SHOWN, SIZE_TOLERANCE, Sample, cluster,
    directory_of, text_digest,
};
