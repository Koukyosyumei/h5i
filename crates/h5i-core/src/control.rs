//! The browser control lock.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::H5iError;

/// Who is driving the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Holder {
    Agent,
    Human,
}

impl Holder {
    pub fn as_str(self) -> &'static str {
        match self {
            Holder::Agent => "agent",
            Holder::Human => "human",
        }
    }
}

/// The lock as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Control {
    pub holder: Holder,
    /// RFC3339 of the last transition.
    pub since: String,
    /// True when control has changed hands since the agent last snapshotted, so
    /// its `@ref` handles are stale. Cleared by [`snapshotted`].
    #[serde(default)]
    pub needs_resnapshot: bool,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            holder: Holder::Agent,
            since: now(),
            needs_resnapshot: false,
        }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Where the lock is written. Public for the same reason [`read`] is: a watcher
/// notices a handover by this file changing.
pub const CONTROL_JSON: &str = "control.json";

fn path(env_dir: &Path) -> PathBuf {
    env_dir.join(CONTROL_JSON)
}

/// Current state. A box that has never had a viewer attached is agent-held.
pub fn read(env_dir: &Path) -> Control {
    std::fs::read(path(env_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Write the control file atomically: temp file plus rename.
///
/// `pump_input` re-reads this on every input frame, so a truncate-then-write
/// left a window where a reader saw a torn file, fell back to `Holder::Agent`,
/// and silently dropped a human's input mid-drag. `write_user_allow` in env.rs
/// already uses this shape for the same reason.
fn write(env_dir: &Path, c: &Control) -> Result<(), H5iError> {
    let p = path(env_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| H5iError::with_path(e, parent))?;
    }
    let tmp = p.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(c)?).map_err(|e| H5iError::with_path(e, &tmp))?;
    std::fs::rename(&tmp, &p).map_err(|e| H5iError::with_path(e, &p))
}

/// A human takes control. Always succeeds: someone at the viewer wants the
/// pointer now, and the agent is a program that can wait.
pub fn take(env_dir: &Path) -> Result<Control, H5iError> {
    let c = Control {
        holder: Holder::Human,
        since: now(),
        // Set on the *take*, not the release: the agent's handles go stale the
        // moment a human touches the page, and a session that never returns
        // control should still leave the flag set for the next reader.
        needs_resnapshot: true,
    };
    write(env_dir, &c)?;
    Ok(c)
}

/// The human hands control back. The stale-handle flag survives until the agent
/// actually re-snapshots.
pub fn release(env_dir: &Path) -> Result<Control, H5iError> {
    let prev = read(env_dir);
    // `needs_resnapshot` only ever latches true and is cleared by the agent's
    // own re-snapshot, so a read-modify-write racing a concurrent `take` can
    // only lose the *holder*. Releasing to Agent when someone just took control
    // would hand the pointer back under them, so keep Human if that is what the
    // file says now.
    let c = Control {
        holder: Holder::Agent,
        since: now(),
        needs_resnapshot: prev.needs_resnapshot || prev.holder == Holder::Human,
    };
    write(env_dir, &c)?;
    Ok(c)
}

/// The agent took a fresh snapshot, so its handles are current again.
pub fn snapshotted(env_dir: &Path) -> Result<Control, H5iError> {
    let mut c = read(env_dir);
    // Same read-modify-write hazard `release` guards: this rewrites the whole
    // record, so a takeover landing between the read above and the write below
    // would be erased. The file would say the agent holds control while a
    // human is driving the page. Since the mediator now calls this on every
    // forwarded snapshot (constantly), re-read and refuse to clear anything if
    // a human has taken the wheel in the meantime.
    if read(env_dir).holder == Holder::Human {
        return Ok(read(env_dir));
    }
    c.needs_resnapshot = false;
    write(env_dir, &c)?;
    Ok(c)
}

/// What the agent is allowed to do right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Go ahead.
    Allowed,
    /// A human is driving. The agent waits rather than fighting for the pointer.
    HeldByHuman,
    /// Control came back but the page moved underneath: snapshot first.
    NeedsResnapshot,
}

impl Verdict {
    /// The message an agent should see. Says what happened and what to do, so a
    /// refusal never reads as a malfunction.
    pub fn explain(&self) -> Option<String> {
        match self {
            Verdict::Allowed => None,
            Verdict::HeldByHuman => Some(
                "browser control is held by a human — automation is paused. \
                 Wait, or ask them to hand it back (`h5i browser status` shows who holds it)."
                    .into(),
            ),
            // One command, not a list of engines: every verb an agent sends now
            // goes through `h5i browser`, whatever renders the page, so the
            // advice no longer has to guess which engine a box was pinned to.
            Verdict::NeedsResnapshot => Some(
                "control was handed back after a human used the browser, so the page may have \
                 moved: every @ref from your last snapshot is stale. Re-snapshot before acting \
                 (`h5i browser snapshot <session>`)."
                    .into(),
            ),
        }
    }
}

/// May the agent act? `mutating` is false for read-only verbs (`snapshot`,
/// `screenshot`, `console`), which stay available while a human drives: watching
/// is never the thing that collides.
pub fn check(env_dir: &Path, mutating: bool) -> Verdict {
    let c = read(env_dir);
    if c.holder == Holder::Human && mutating {
        return Verdict::HeldByHuman;
    }
    if c.needs_resnapshot && mutating {
        return Verdict::NeedsResnapshot;
    }
    Verdict::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_fresh_box_is_agent_held() {
        let td = TempDir::new().unwrap();
        assert_eq!(read(td.path()).holder, Holder::Agent);
        assert_eq!(check(td.path(), true), Verdict::Allowed);
    }

    #[test]
    fn a_human_take_pauses_the_agents_mutating_verbs_only() {
        let td = TempDir::new().unwrap();
        take(td.path()).unwrap();

        assert_eq!(check(td.path(), true), Verdict::HeldByHuman);
        // Watching never collides, so reads stay available.
        assert_eq!(check(td.path(), false), Verdict::Allowed);
        assert!(check(td.path(), true).explain().unwrap().contains("human"));
    }

    #[test]
    fn handing_control_back_forces_a_resnapshot() {
        let td = TempDir::new().unwrap();
        take(td.path()).unwrap();
        let c = release(td.path()).unwrap();

        assert_eq!(c.holder, Holder::Agent);
        assert!(c.needs_resnapshot, "the page moved under the agent");
        // Acting is refused with an actionable message, not a mis-click.
        let v = check(td.path(), true);
        assert_eq!(v, Verdict::NeedsResnapshot);
        assert!(v.explain().unwrap().contains("snapshot"));

        // A read is still fine, and taking a snapshot clears it.
        assert_eq!(check(td.path(), false), Verdict::Allowed);
        snapshotted(td.path()).unwrap();
        assert_eq!(check(td.path(), true), Verdict::Allowed);
    }

    #[test]
    fn the_stale_flag_survives_a_session_that_never_hands_back() {
        let td = TempDir::new().unwrap();
        take(td.path()).unwrap();
        // No release: the human closed the viewer. The next reader still learns
        // the handles are stale.
        assert!(read(td.path()).needs_resnapshot);
    }

    #[test]
    fn state_round_trips_through_disk() {
        let td = TempDir::new().unwrap();
        take(td.path()).unwrap();
        let reread = read(td.path());
        assert_eq!(reread.holder, Holder::Human);
        assert!(!reread.since.is_empty());
    }

    #[test]
    fn a_corrupt_file_falls_back_to_agent_held_rather_than_failing() {
        let td = TempDir::new().unwrap();
        std::fs::write(path(td.path()), b"{ not json").unwrap();
        // Losing the lock file must not brick the box: the safe default is the
        // one a box starts in.
        assert_eq!(read(td.path()).holder, Holder::Agent);
    }
}
