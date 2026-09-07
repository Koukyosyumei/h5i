// ─── public surface ─────────────────────────────────────────────────────────
// The domain: an environment is a confined worktree with a pinned policy, and
// a receipt is the record of what actually ran inside it. (`error` stays public
// because `H5iError` appears in the signatures of most of them.)
pub mod browser;
pub mod browser_events;
pub mod browser_frames;
pub mod browser_sandbox;
pub mod browser_session;
pub mod browser_proxy;
pub mod cache;
pub mod control;
pub mod env;
pub mod export;
/// Where a box runs: the trait the lifecycle engine uses to place one on
/// another machine, with no transport in it (design-runner.md R1, R7).
pub mod placement;
/// Taking a tree from a machine we agreed might be compromised
/// (design-runner.md R9).
pub mod quarantine;
pub mod receipt;
pub mod redact;
pub mod refstore;
// The box console (`h5i ui`). A read-only HTTP view of the fleet. Optional:
// `--no-default-features` drops it, and with it axum, tokio, the embedded
// bundle, and the build script's dependency on Node.
#[cfg(feature = "web")]
pub mod server;
// What the console shows about browser sessions, and which of them wants a
// human. Not feature-gated: `h5i box watch` and the TUI want the same answer.
pub mod session_view;
// Reading `share.json` for everything below `h5i-share`. See the module note:
// there were three hand-rolled probes here and they did not agree with the
// crate that writes the file.
pub mod share_record;
pub mod skill;
pub mod source;
pub mod storage;
pub mod termview;
pub mod token;
pub mod ui;
pub mod view;
// The error type lives in its own leaf crate (`h5i-error`) so extracted crates
// can depend on it without depending on `h5i-core`. Re-exported as
// `crate::error` so every existing `crate::error::*` path resolves unchanged.
pub use h5i_error as error;

// The confinement layer lives in its own crate (`h5i-sandbox`). Re-exported so
// every `crate::sandbox::*` / `crate::container::*` / … path resolves inside
// this crate unchanged.
pub use h5i_sandbox::{
    auth_proxy, cgroup, container, sandbox, sandbox_policy, secrets, secrets_broker, supervisor,
};
// `seccomp_notify` is Linux+x86_64/aarch64 only (its whole module is cfg'd out
// elsewhere), so the re-export must carry the same gate or it fails to resolve
// on macOS/other targets in the cross-check job.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub use h5i_sandbox::seccomp_notify;
/// The kernel tiers' effective-config dump (design-policy.md §P1). Linux only.
#[cfg(target_os = "linux")]
pub use h5i_sandbox::effective;
/// The filesystem-authority validator (design-policy.md §P2).
pub use h5i_sandbox::fs_authority;
/// The macOS Seatbelt backend. Compiled on every Unix target so its pure
/// SBPL generator is testable from the Linux job.
#[cfg(unix)]
pub use h5i_sandbox::seatbelt;
/// The runtime-detection lane (design-detect.md D1–D14). Re-exported
/// unconditionally, every build has to be able to read a receipt written by one
/// that had the collector, while the collector itself is behind this crate's
/// `bpf` feature.
pub use h5i_bpf as bpf;
