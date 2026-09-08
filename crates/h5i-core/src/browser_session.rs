//! The host-owned registry of browser sessions.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::H5iError;

/// Exit status for a verb addressed to a session that is not live.
///
/// `sysexits.h`'s `EX_UNAVAILABLE`. It is a distinct code rather than the
/// generic failure because the whole point is that an agent can tell "the
/// session is gone" from "the click did not work", and retry logic that cannot
/// tell them apart is retry logic that silently starts a second browser.
pub const EXIT_SESSION_GONE: i32 = 69;

/// Directory name under the state root that holds one directory per session.
const SESSIONS: &str = "sessions";

/// The record file inside a session directory.
/// The record itself. Public because a reader that watches for changes has to
/// name the same file the writer does.
pub const RECORD: &str = "session.json";

/// Holds the id of the session a verb acts on when nobody says which.
///
/// A pointer rather than a convention like "the newest live one", because the
/// convention silently moves under an agent the moment a second session is
/// opened, and an agent that has been quietly redirected to a different page is
/// the failure this whole module is arranged to prevent.
const DEFAULT_POINTER: &str = "default";

/// Where the engine advertises its control port, inside a session directory.
pub const CONTROL_FILE: &str = "control";

/// Where the engine advertises its viewer stream port.
pub const STREAM_FILE: &str = "stream";

/// The engine's request log: one JSON object per line, written before the wire.
pub const RECEIPTS_FILE: &str = "requests.jsonl";

/// The verbs an agent asked for, as the session recorded them.
pub const ACTIONS_FILE: &str = "actions.jsonl";

/// Outside programs h5i ran on this session's behalf, one JSON object per line.
///
/// Written by h5i, not by the engine, and that is the whole reason it is a
/// separate file rather than a row in the action log. A helper is a second
/// program with its own network, so the engine's log cannot account for it and
/// must not appear to: mixing the two would turn "a request that is not in
/// `requests` did not happen" from a claim the engine can keep into one it
/// cannot.
pub const HELPERS_FILE: &str = "helpers.jsonl";

/// The session's cookie jar, mirrored by the engine and read by `--restore`.
///
/// The one file in a session directory that is *credential material*, and the
/// only one h5i copies from a session to its successor. It is written `0600`,
/// no verb returns what is in it, and it exists so a login a human performed
/// once does not have to be performed again on every session, which is what
/// `--restore` promised before there was anything to restore (roadmap-history.md §B19.6).
pub const COOKIES_FILE: &str = "cookies.json";

/// The handover journal: one line per `take` or `release`.
///
/// Separate from `control.json`, which holds only *who holds it now*. A current
/// holder cannot answer "was a human driving when that form was submitted",
/// and that is the question an audit is for.
pub const CONTROL_JOURNAL: &str = "control.jsonl";

/// The session's stored messages: headers and bodies, both directions.
///
/// Written only when a session was opened with `--capture`, and the one
/// directory here that is *evidence* rather than account. It holds session
/// cookies and `Authorization` headers in full, which is exactly what the
/// request log refuses to hold, so it is `0700`, it is never copied by
/// `--restore`, and no export includes it unless someone names it. See
/// `h5i-browser`'s `capture` module and `docs/design/design-websec.md`.
pub const MESSAGES_DIR: &str = "messages";

/// Where files this session produced are collected. Host-named, always: see
/// [`crate::browser_session::artifact_path`].
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Which engine renders the page.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// h5i's own engine. Every request is policy-checked and recorded before it
    /// reaches the wire, and the record is fail-closed.
    H5iLight,
    /// A Chromium driven through `agent-browser`. Its request lane is
    /// best-effort: attach races and buffer limits leave gaps.
    Chromium,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::H5iLight => "h5i-light",
            Engine::Chromium => "chromium",
        }
    }
}

/// Where the engine process runs.
///
/// This is the only thing `--in` changes, and it changes nothing an agent
/// types: the id resolves the same way and every verb has the same name and
/// the same answer. What it changes is what the record can honestly claim
/// about the network lane ([`Session::lane`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Placement {
    /// On this machine, in this user's ordinary process space. No containment
    /// beyond the engine itself.
    Host,
    /// Inside a box. The engine's loopback is the box's loopback, so verbs are
    /// carried in rather than dialled directly (`h5i box run`).
    Box {
        /// The box's name, as `h5i box list` prints it.
        name: String,
    },
}

impl Placement {
    pub fn box_name(&self) -> Option<&str> {
        match self {
            Placement::Host => None,
            Placement::Box { name } => Some(name),
        }
    }

    /// What to print for `isolation` in a one-line status.
    pub fn as_str(&self) -> &str {
        match self {
            Placement::Host => "none",
            Placement::Box { .. } => "box",
        }
    }
}

/// Who observed the session's network activity.
///
/// The same split [`crate::browser_events::Lane`] carries per row, recorded
/// once for the session so a reader does not have to infer it from placement.
/// A host session's requests are the engine's own account of what it fetched:
/// fail-closed and complete, and still the engine's account. A boxed session's
/// requests are additionally seen at the box's boundary, which is outside the
/// thing being described.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Lane {
    /// The engine said so, fail-closed.
    EngineClaimed,
    /// h5i saw it from outside the box as well.
    HostObserved,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::EngineClaimed => "engine-claimed",
            Lane::HostObserved => "host-observed",
        }
    }
}

/// What survives the session.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Storage {
    /// Cookies and page state die with the session.
    Ephemeral,
    /// Cookies are written into the session directory and can seed a later
    /// session through `--restore`.
    Persistent,
}

/// Where a session is in its life.
///
/// `Live` is the only state a verb may act on. The other four are all endings,
/// kept apart because they are different facts about the run and a receipt that
/// merged them would be a receipt that cannot say whether the record is
/// complete.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Started, and the engine answered the last time anyone looked.
    Live,
    /// Ended by `h5i browser close`. The record is complete.
    Closed,
    /// The engine stopped without being asked. Whatever it was doing when it
    /// stopped is not in the record, and the record says so.
    Died,
    /// Outlived `expires_at`. An ending like any other, written as an event
    /// rather than by the directory quietly disappearing.
    Expired,
    /// The box holding the engine was removed. Distinct from `Died` because the
    /// cause is on this side of the boundary and is therefore known.
    Evicted,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Closed => "closed",
            State::Died => "died",
            State::Expired => "expired",
            State::Evicted => "evicted",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, State::Live)
    }

    /// The ending as a clause, so a message reads as English rather than as a
    /// field name pasted into a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            State::Live => "is live",
            State::Closed => "was closed",
            State::Died => "died",
            State::Expired => "expired",
            State::Evicted => "was evicted",
        }
    }
}

/// How a verb reaches the engine.
///
/// Two, because neither works everywhere. A loopback port is the simple case and
/// needs no path short enough to be a socket address. A Unix socket is the only
/// thing that works anywhere a network namespace is in play: a box's netns may
/// have no usable loopback at all, and every `h5i box run` gets a fresh one, so
/// a port bound in one is unreachable from the next. A path survives both,
/// because the box's filesystem is one filesystem across every run in it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    /// A file holding a loopback port number.
    #[default]
    Port,
    /// A Unix domain socket.
    Socket,
}

impl Channel {
    /// The flag the engine's CLI takes for this channel.
    pub fn flag(self) -> &'static str {
        match self {
            Channel::Port => "--control-file",
            Channel::Socket => "--control-socket",
        }
    }
}

/// How to reach the engine, when it is reachable at all.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Control {
    /// Which of the two channels [`Control::file`] names.
    #[serde(default)]
    pub channel: Channel,
    /// Where the engine listens, as the engine sees it.
    ///
    /// Two different things behind one field, because a caller only ever hands
    /// it straight back to the engine. For a host session it is the file the
    /// engine wrote its control port into, inside the session directory. For a
    /// boxed one it is a Unix socket in the box's own `/tmp`. A path rather
    /// than a port, because each `h5i box run` gets its own network namespace
    /// and a port bound in one is unreachable from the next.
    pub file: Option<PathBuf>,
    /// The same file as this machine sees it, when this machine can see it
    /// at all. `None` on an image-backed tier, whose `/tmp` lives in the image
    /// and is not on the host's filesystem. There, liveness is not knowable
    /// from outside and [`Session::probe`] says so by not guessing.
    pub witness: Option<PathBuf>,
    /// The process h5i spawned. On the host that is the engine; for a boxed
    /// session it is the `box run` that carries it, which is a host process and
    /// lives exactly as long as the engine inside does.
    pub pid: Option<u32>,
}

/// One browser session, as the host records it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Session {
    /// The opaque, durable reference. In the record, in `--json`, in receipts.
    /// Not what an agent types: see the module docs.
    pub id: String,
    /// The name a person gave this session with `--session`, if any. Unnamed
    /// sessions are the ordinary case and are reached through the default
    /// pointer instead.
    ///
    /// A name is not an identity: it can be reused once the session it named
    /// has ended, which is exactly what makes it comfortable to type. The id
    /// cannot, which is why the id is what gets written down.
    #[serde(default)]
    pub name: Option<String>,
    pub engine: Engine,
    pub placement: Placement,
    pub lane: Lane,
    /// The URL this session was last told to open: the one `open` was given,
    /// whether it made the session or navigated it.
    ///
    /// Deliberately not "the current URL". That is page state: a redirect, a
    /// script or a human at the viewer moves it, and asking the session is the
    /// only way to know it. Recording *that* here would be a second answer that
    /// goes stale. What is recorded is an instruction, which does not.
    pub url: String,
    pub started_at: String,
    pub expires_at: Option<String>,
    pub storage: Storage,
    /// The policy this session runs under, digested. Two sessions with the same
    /// digest were allowed the same things.
    pub policy_digest: String,
    /// Who this session presented itself as, and the digest of everything that identity
    /// declared.
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub identity_digest: String,
    /// The session whose storage seeded this one, if any. A restore is a new
    /// session with a new id and an inheritance recorded, never a resurrection.
    pub restored_from: Option<String>,
    pub state: State,
    pub ended_at: Option<String>,
    /// One line on how it ended, for the states that have something to say.
    pub end_reason: Option<String>,
    /// What is holding the engine process.
    ///
    /// A third axis, and genuinely a third question. `placement` says *where*
    /// the session runs, `enclosing_box` says what h5i was standing in when it
    /// opened one, and this says what confines the engine there. A host session
    /// can be confined; a boxed one is confined by its box.
    #[serde(default)]
    pub confinement: crate::browser_sandbox::Confinement,
    /// The box h5i itself was running inside when this session was opened, if any.
    #[serde(default)]
    pub enclosing_box: Option<String>,
    pub control: Control,
    /// Where this machine can read the session's own logs, when it can.
    #[serde(default)]
    pub logs: Logs,
    /// Whether pages here may send credentials cross-origin (`--permissive-cors`).
    ///
    /// On the record and not only in the digest, which says two sessions differ
    /// without saying how: a result gathered under this means something else.
    #[serde(default)]
    pub permissive_cors: bool,
}

/// The engine's two logs, as this machine sees them.
///
/// Recorded at start rather than derived at read time, for the reason
/// [`Control::witness`] exists: a boxed session's logs live in the box's
/// `/tmp`, and re-deriving that path later means re-deriving a mapping that has
/// since been rewritten. `None` means this machine cannot read that log at all,
/// which an audit reports as *unavailable* rather than as an empty list. An
/// empty list looks like a quiet session; unavailable looks like what it is.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Logs {
    pub actions: Option<PathBuf>,
    pub requests: Option<PathBuf>,
}

impl Session {
    /// Where this session ran, in one clause, with nothing claimed that cannot
    /// be checked from wherever h5i was standing.
    pub fn where_it_ran(&self) -> String {
        use crate::browser_sandbox::Confinement;
        match (&self.placement, &self.enclosing_box) {
            (Placement::Box { name }, _) => format!("in box `{name}`"),
            // h5i was in the box too. Name it; claim nothing about it.
            (Placement::Host, Some(id)) => {
                format!("on this machine, which is box `{id}` — its policy is not readable here")
            }
            (Placement::Host, None) => match &self.confinement {
                Confinement::Process => {
                    // Named precisely, because the two things it does not do are
                    // the two a reader would otherwise assume it does.
                    "on this machine, in a process-tier sandbox (its files and its \
                     environment; not its network)"
                        .to_string()
                }
                Confinement::None { why } => {
                    format!("on this machine, unconfined — {why}")
                }
            },
        }
    }

    /// The lane a placement can honestly claim.
    pub fn lane_for(placement: &Placement, boundary_enforced: bool) -> Lane {
        match placement {
            Placement::Host => Lane::EngineClaimed,
            Placement::Box { .. } if boundary_enforced => Lane::HostObserved,
            Placement::Box { .. } => Lane::EngineClaimed,
        }
    }

    /// Is the engine still there?
    pub fn probe(&self) -> bool {
        if !self.state.is_live() {
            return false;
        }
        if let Some(pid) = self.control.pid {
            return process_alive(pid);
        }
        match &self.control.witness {
            Some(path) => path.exists(),
            None => true,
        }
    }
}

/// The state directory the registry lives in.
///
/// `$H5I_BROWSER_HOME` wins so a test, or a user who wants two independent
/// fleets, can say where. Otherwise the XDG state directory, which is the
/// correct place for "state that should persist between restarts but is not
/// important enough for the data directory".
pub fn root() -> Result<PathBuf, H5iError> {
    if let Some(explicit) = std::env::var_os("H5I_BROWSER_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    // Inside a box, the state directory is not a choice. `$HOME` there is the
    // host's path over a sealed overlay: `~/.local/state` is not writable, and a
    // session that cannot write its registry cannot start at all. What *is*
    // writable is the box's own `/tmp`, which is private to the box and lives
    // exactly as long as it does, which is also how long its sessions can.
    //
    // `temp_dir` rather than a literal `/tmp`, because it follows the redirect
    // the box was given rather than assuming what it was.
    if crate::env::in_env_box() {
        return Ok(std::env::temp_dir().join("h5i").join("browser"));
    }
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let state = PathBuf::from(state);
        if state.is_absolute() {
            return Ok(state.join("h5i").join("browser"));
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        H5iError::Metadata(
            "cannot resolve where to keep browser sessions — set $HOME, \
             $XDG_STATE_HOME, or $H5I_BROWSER_HOME"
                .into(),
        )
    })?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("h5i")
        .join("browser"))
}

/// `<root>/sessions`, created if it is not there.
pub fn sessions_dir(root: &Path) -> Result<PathBuf, H5iError> {
    let dir = root.join(SESSIONS);
    create_private_dir_all(&dir)?;
    Ok(dir)
}

/// `create_dir_all`, but the directories this crate makes are the owner's.
fn create_private_dir_all(dir: &Path) -> Result<(), H5iError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(H5iError::Io)
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir).map_err(H5iError::Io)
}

/// The directory holding one session's record, control file, log and artifacts.
pub fn dir(root: &Path, id: &str) -> PathBuf {
    root.join(SESSIONS).join(id)
}

/// Whether an id is one path component and nothing else.
///
/// An id reaches [`dir`] from a `--session` selector and from a `session.json`
/// that, for a boxed session, boxed code can write. Everything under a session
/// directory — jar, control channel, message store — is addressed by joining
/// onto it, so a `..` here names a directory outside the registry.
pub fn id_is_one_component(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Mint an id no session has ever had, and create its directory.
///
/// The directory is the claim: creating it with `create_new` is what makes two
/// concurrent `start`s unable to agree on the same id, and its continued
/// existence after the session ends is what stops the id coming back.
pub fn new_id(root: &Path) -> Result<String, H5iError> {
    let sessions = sessions_dir(root)?;
    for _ in 0..64 {
        let id = format!("br_{}", suffix());
        let path = sessions.join(&id);
        match private_create_dir(&path) {
            Ok(()) => {
                create_private_dir_all(&path.join(ARTIFACTS_DIR))?;
                return Ok(id);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(H5iError::Io(e)),
        }
    }
    Err(H5iError::Metadata(
        "could not mint a free browser session id after 64 tries".into(),
    ))
}

/// `create_dir` with the mode set, keeping the `AlreadyExists` error that
/// [`new_id`] uses as its claim on an id.
fn private_create_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fs::create_dir(path)
}

/// Six characters of `[0-9a-z]`, avoiding the letters that read as digits.
///
/// Short because an agent types it on every verb, and an id is not a secret:
/// reaching a session needs the control file, not the name.
fn suffix() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstuvwxyz";
    (0..6)
        .map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char)
        .collect()
}

/// Write a record, replacing whatever was there.
pub fn write(root: &Path, session: &Session) -> Result<(), H5iError> {
    let dir = dir(root, &session.id);
    fs::create_dir_all(&dir).map_err(H5iError::Io)?;
    let body = serde_json::to_string_pretty(session)
        .map_err(|e| H5iError::Metadata(format!("could not serialize the session record: {e}")))?;
    // Rename over, so a reader never sees half a record.
    let tmp = dir.join(".session.json.tmp");
    fs::write(&tmp, format!("{body}\n")).map_err(H5iError::Io)?;
    fs::rename(&tmp, dir.join(RECORD)).map_err(H5iError::Io)
}

/// Read one record by id.
pub fn read(root: &Path, id: &str) -> Result<Session, H5iError> {
    if !id_is_one_component(id) {
        return Err(unknown(root, id));
    }
    let path = dir(root, id).join(RECORD);
    let body = fs::read_to_string(&path).map_err(|_| unknown(root, id))?;
    serde_json::from_str(&body)
        .map_err(|e| H5iError::Metadata(format!("`{id}`'s record is unreadable: {e}")))
}

/// Every session this host knows about, newest first.
pub fn list(root: &Path) -> Result<Vec<Session>, H5iError> {
    let sessions = root.join(SESSIONS);
    if !sessions.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&sessions).map_err(H5iError::Io)? {
        let entry = entry.map_err(H5iError::Io)?;
        let record = entry.path().join(RECORD);
        if !record.exists() {
            continue;
        }
        if let Ok(body) = fs::read_to_string(&record)
            && let Ok(session) = serde_json::from_str::<Session>(&body)
        {
            out.push(session);
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(out)
}

/// The error for an id that names nothing, with the ids that do.
fn unknown(root: &Path, id: &str) -> H5iError {
    let known: Vec<String> = list(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.state.is_live())
        .map(|s| s.id)
        .collect();
    if known.is_empty() {
        H5iError::Metadata(format!(
            "`{id}` is not a browser session on this machine, and none are running. \
             Start one with `h5i browser open <url>`."
        ))
    } else {
        H5iError::Metadata(format!(
            "`{id}` is not a browser session on this machine. Live sessions: {}.",
            known.join(", ")
        ))
    }
}

/// The id of the default session, if one has been set.
pub fn read_default(root: &Path) -> Option<String> {
    let raw = fs::read_to_string(root.join(DEFAULT_POINTER)).ok()?;
    let id = raw.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// Point the default at this session.
pub fn set_default(root: &Path, id: &str) -> Result<(), H5iError> {
    fs::create_dir_all(root).map_err(H5iError::Io)?;
    fs::write(root.join(DEFAULT_POINTER), format!("{id}\n")).map_err(H5iError::Io)
}

/// Clear the default if it points at this session.
///
/// Used for one case only: the pointer names a record that is *gone*. A pointer
/// to a session that merely *ended* is deliberately kept, because following it
/// is what lets the next bare verb say "the session you were on was closed"
/// instead of "no session is open".
///
/// Conditional on the id, because closing a named session must not disturb a
/// default someone else is using.
pub fn clear_default_if(root: &Path, id: &str) {
    if read_default(root).as_deref() == Some(id) {
        let _ = fs::remove_file(root.join(DEFAULT_POINTER));
    }
}

/// What one session's directory holds, in bytes, split at the store.
///
/// The store is the heavy half and the only half that holds credentials, which
/// is why it is counted apart: reclaiming it is a different act from erasing a
/// session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    /// The record, the logs, the ledger, the jar: everything but the store.
    pub account: u64,
    /// `messages/`: the bytes a capture kept.
    pub store: u64,
}

impl Held {
    pub fn total(&self) -> u64 {
        self.account + self.store
    }
}

/// How much disk one session is using.
pub fn held(root: &Path, id: &str) -> Held {
    let dir = dir(root, id);
    let store = dir.join(MESSAGES_DIR);
    Held {
        account: dir_bytes(&dir).saturating_sub(dir_bytes(&store)),
        store: dir_bytes(&store),
    }
}

/// Bytes under a directory, following nothing.
///
/// Bounded by the tree it is given: a session directory is flat apart from
/// `messages/bodies` and `recon/jobs`, and a symlink inside one is not followed
/// because the walk reads metadata rather than opening anything.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_bytes(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// Erase one session: its record, its logs, its jar and its store.
///
/// The whole directory, because a session's parts are only meaningful
/// together: a request log with no record names fetches nobody can attribute.
/// Refuses an id that is not one path component, since a boxed session writes
/// its own record and that id becomes this path.
pub fn remove(root: &Path, id: &str) -> Result<Held, H5iError> {
    if !id_is_one_component(id) {
        return Err(H5iError::Metadata(format!(
            "`{id}` is not an id this registry could have minted, so nothing was removed"
        )));
    }
    let dir = dir(root, id);
    if !dir.is_dir() {
        return Err(H5iError::Metadata(format!("no session `{id}` to remove")));
    }
    let held = held(root, id);
    fs::remove_dir_all(&dir).map_err(|e| H5iError::with_path(e, &dir))?;
    // The pointer would otherwise name a record that is gone, which is the one
    // case `clear_default_if` exists for.
    clear_default_if(root, id);
    Ok(held)
}

/// What a session says about a store it no longer has.
pub const RECLAIMED_FILE: &str = "capture-reclaimed.json";

/// The note left where a store was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reclaimed {
    pub at: String,
    /// Messages the store held, counted before it went.
    pub messages: usize,
    pub bytes: u64,
}

/// Drop one session's stored messages, keeping everything else.
///
/// The account survives: the record, the request log and the ledger still say
/// what this session did and what it found. What goes is the half that holds
/// bodies, cookies and `Authorization` in full.
///
/// A note is left behind, because "the bytes were kept and then reclaimed" and
/// "the bytes were never kept" are different facts and a reader would otherwise
/// see the same empty space for both.
pub fn drop_store(root: &Path, id: &str) -> Result<u64, H5iError> {
    if !id_is_one_component(id) {
        return Err(H5iError::Metadata(format!(
            "`{id}` is not an id this registry could have minted"
        )));
    }
    let dir = dir(root, id);
    let store = dir.join(MESSAGES_DIR);
    if !store.is_dir() {
        return Ok(0);
    }
    let bytes = dir_bytes(&store);
    let messages = fs::read_dir(&store)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.ends_with(".request.json"))
                })
                .count()
        })
        .unwrap_or(0);
    fs::remove_dir_all(&store).map_err(|e| H5iError::with_path(e, &store))?;
    let note = Reclaimed {
        at: now(),
        messages,
        bytes,
    };
    if let Ok(text) = serde_json::to_vec(&note) {
        let _ = fs::write(dir.join(RECLAIMED_FILE), text);
    }
    Ok(bytes)
}

/// The note a reclaimed store left, if there is one.
pub fn reclaimed(root: &Path, id: &str) -> Option<Reclaimed> {
    let text = fs::read_to_string(dir(root, id).join(RECLAIMED_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The live session carrying this name, if any.
pub fn find_by_name(root: &Path, name: &str) -> Option<Session> {
    list(root)
        .unwrap_or_default()
        .into_iter()
        .find(|s| s.state.is_live() && s.name.as_deref() == Some(name))
}

/// A session that has ended, by the name it was opened with. Newest first.
///
/// Separate from [`find_by_name`] on purpose: a verb that *acts* must only find
/// a live session. This is for the readers, whose subject is usually the run
/// that just finished — and `close` says the record stays.
pub fn find_ended_by_name(root: &Path, name: &str) -> Option<Session> {
    list(root)
        .unwrap_or_default()
        .into_iter()
        .find(|s| !s.state.is_live() && s.name.as_deref() == Some(name))
}

/// Turn what the caller said (or did not say) into a session a verb may act on.
pub fn resolve(root: &Path, selector: Option<&str>) -> Result<Session, SessionGone> {
    let selector = selector
        .map(str::to_string)
        .or_else(|| std::env::var("H5I_BROWSER_SESSION").ok())
        .filter(|s| !s.trim().is_empty());

    match selector {
        Some(wanted) => {
            // A name first: that is what a person typed. An id is accepted too,
            // because `--json` and receipts hand one back and it should work
            // where it is pasted.
            if let Some(session) = find_by_name(root, &wanted) {
                return Ok(session);
            }
            match read(root, &wanted) {
                Ok(session) => open_it(root, session),
                Err(_) => Err(SessionGone::Unknown(unknown_selector(root, &wanted))),
            }
        }
        None => match read_default(root) {
            Some(id) => match read(root, &id) {
                Ok(session) => open_it(root, session),
                // The pointer outlived what it pointed at. Say so plainly
                // rather than reporting an id nobody typed.
                Err(_) => {
                    clear_default_if(root, &id);
                    Err(SessionGone::Unknown(no_default(root)))
                }
            },
            None => Err(SessionGone::Unknown(no_default(root))),
        },
    }
}

/// The error for a `--session` that names nothing.
fn unknown_selector(root: &Path, wanted: &str) -> H5iError {
    let live: Vec<String> = list(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.state.is_live())
        .map(|s| match &s.name {
            Some(name) => format!("{name} ({})", s.id),
            None => format!("{} (unnamed)", s.id),
        })
        .collect();
    if live.is_empty() {
        H5iError::Metadata(format!(
            "no browser session called `{wanted}`, and none is running. \
             Open one with `h5i browser open <url>`."
        ))
    } else {
        H5iError::Metadata(format!(
            "no browser session called `{wanted}`. Running: {}.",
            live.join(", ")
        ))
    }
}

/// The error for a verb sent with nothing to send it to.
fn no_default(root: &Path) -> H5iError {
    let named: Vec<String> = list(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.state.is_live())
        .filter_map(|s| s.name)
        .collect();
    if named.is_empty() {
        H5iError::Metadata(
            "no browser session is open. Open one with `h5i browser open <url>`.".into(),
        )
    } else {
        H5iError::Metadata(format!(
            "no default browser session, and the ones running are named. \
             Say which: {}.",
            named
                .iter()
                .map(|n| format!("`--session {n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// Resolve an id to a session that a verb may act on.
///
/// This is the one place the "died" rule is enforced, and it is enforced by
/// refusing rather than by restarting. The error names the ending and points at
/// `--restore`, because the agent's next move is a decision (continue from that
/// storage, or start clean) and not something this function may make for it.
pub fn open_live(root: &Path, id: &str) -> Result<Session, SessionGone> {
    let session = match read(root, id) {
        Ok(s) => s,
        Err(e) => return Err(SessionGone::Unknown(e)),
    };
    open_it(root, session)
}

/// The liveness half of [`open_live`], over a record already in hand.
fn open_it(root: &Path, session: Session) -> Result<Session, SessionGone> {
    if !session.state.is_live() {
        return Err(SessionGone::Ended {
            state: session.state,
            reason: session.end_reason.clone(),
            id: session.id.clone(),
        });
    }
    if !session.probe() {
        // Seen dead now, so record it now: the next reader should not have to
        // re-derive it, and a receipt that says "died" needs a time.
        let mut dead = session.clone();
        end(root, &mut dead, State::Died, "the engine stopped answering");
        return Err(SessionGone::Ended {
            state: State::Died,
            reason: dead.end_reason.clone(),
            id: dead.id,
        });
    }
    Ok(session)
}

/// Why a verb could not be delivered.
#[derive(Debug)]
pub enum SessionGone {
    /// No such id here.
    Unknown(H5iError),
    /// The selector names a session that has ended.
    Ended {
        state: State,
        reason: Option<String>,
        /// The opaque id, which is what `--restore` takes: a name can be reused
        /// and so cannot carry storage forward unambiguously.
        id: String,
    },
}

impl std::fmt::Display for SessionGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionGone::Unknown(e) => write!(f, "{e}"),
            SessionGone::Ended { state, reason, id } => {
                write!(f, "browser session `{id}` {}", state.describe())?;
                if let Some(reason) = reason {
                    write!(f, ": {reason}")?;
                }
                write!(
                    f,
                    ". It will not be restarted automatically. \
                     Open a new one with `h5i browser open <url>`, or carry this one's \
                     storage forward with `h5i browser open <url> --restore {id}`."
                )
            }
        }
    }
}

impl std::error::Error for SessionGone {}

/// Write an ending into a record. Idempotent: a session that already ended
/// keeps the first ending, because the first one is the true one.
pub fn end(root: &Path, session: &mut Session, state: State, reason: &str) {
    if !session.state.is_live() {
        return;
    }
    session.state = state;
    session.ended_at = Some(now());
    session.end_reason = Some(reason.to_string());
    session.control = Control::default();
    let _ = write(root, session);
}

/// Mark every live session placed in `box_name` as evicted.
///
/// Called when a box is removed. Without it the sessions would be found dead
/// later by probe and recorded as `Died`, which is true but less informative
/// than the cause this side of the boundary actually knows.
pub fn evict_box(root: &Path, box_name: &str) -> Result<usize, H5iError> {
    let mut n = 0;
    for mut session in list(root)? {
        if session.state.is_live() && session.placement.box_name() == Some(box_name) {
            end(
                root,
                &mut session,
                State::Evicted,
                &format!("box `{box_name}` was removed while the session was live"),
            );
            n += 1;
        }
    }
    Ok(n)
}

/// Close every live session that has outlived its `expires_at`.
///
/// Expiry is a sweep rather than a timer because there is no daemon to hold
/// one, and it is recorded rather than enacted by deletion for the reason the
/// whole module exists: an ending nobody wrote down is indistinguishable from a
/// session that never ran.
pub fn expire_due(root: &Path) -> Result<usize, H5iError> {
    let now_ts = now();
    let mut n = 0;
    for mut session in list(root)? {
        if !session.state.is_live() {
            continue;
        }
        if let Some(expires) = session.expires_at.clone()
            && expires <= now_ts
        {
            end(
                root,
                &mut session,
                State::Expired,
                &format!("the session's time limit ({expires}) passed"),
            );
            n += 1;
        }
    }
    Ok(n)
}

/// Where a named artifact goes.
///
/// The host names the file. The engine, and anything the page persuaded it
/// to do, chooses only the bytes. `name` is reduced to a single path component
/// of a known-safe alphabet before it is joined, so a session cannot write
/// through `..`, through a symlink it planted, or onto a dotfile. The same
/// rule the runner import applies to a tree that came home from a machine we
/// assume is broken ([`crate::quarantine`]).
pub fn artifact_path(root: &Path, id: &str, name: &str) -> PathBuf {
    dir(root, id).join(ARTIFACTS_DIR).join(safe_name(name))
}

/// One path component, `[A-Za-z0-9._-]`, never empty, never leading-dot,
/// bounded in length.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    out.truncate(96);
    if out.is_empty() {
        out.push_str("artifact");
    }
    out
}

/// Largest string h5i will relay from a session, in bytes.
///
/// A snapshot of a real page is tens of kilobytes and a markdown rendering can
/// be more, so this is generous. It is not unbounded, because the thing on the
/// other end composed the bytes and an agent's context window is a resource a
/// page should not be able to spend.
const MAX_STRING: usize = 256 * 1024;

/// Longest array relayed. A request log or a snapshot ref list is long; a page
/// that can make it arbitrarily long can make one verb cost everything.
const MAX_ARRAY: usize = 10_000;

/// Deepest nesting relayed, past which the value is replaced.
const MAX_DEPTH: usize = 64;

/// Make a session's answer safe to print and safe to hand to a model.
pub fn scrub(value: &mut serde_json::Value) {
    scrub_at(value, 0);
}

fn scrub_at(value: &mut serde_json::Value, depth: usize) {
    use serde_json::Value;
    if depth > MAX_DEPTH {
        *value = Value::String("[nesting too deep to relay]".into());
        return;
    }
    match value {
        Value::String(s) => {
            let cleaned = scrub_text(s);
            *s = cleaned;
        }
        Value::Array(items) => {
            let dropped = items.len().saturating_sub(MAX_ARRAY);
            items.truncate(MAX_ARRAY);
            for item in items.iter_mut() {
                scrub_at(item, depth + 1);
            }
            if dropped > 0 {
                items.push(Value::String(format!("[{dropped} more items not relayed]")));
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                scrub_at(v, depth + 1);
            }
        }
        _ => {}
    }
}

/// One string, with escapes and control characters gone and a stated cap.
pub fn scrub_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_STRING));
    let mut budget = MAX_STRING;
    let mut dropped = 0usize;
    for ch in text.chars() {
        let keep = match ch {
            '\t' | '\n' => Some(ch),
            // Carriage return is line-overwrite. Newline carries the meaning.
            '\r' => None,
            // C0, DEL, and the C1 block, which some terminals decode as escapes
            // in their own right.
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => None,
            c if ((c as u32) >= 0x80 && (c as u32) <= 0x9f) => None,
            c => Some(c),
        };
        let Some(ch) = keep else {
            dropped += 1;
            continue;
        };
        let len = ch.len_utf8();
        if len > budget {
            let rest = text.len().saturating_sub(out.len());
            out.push_str(&format!("…[{rest} more bytes not relayed]"));
            return out;
        }
        budget -= len;
        out.push(ch);
    }
    if dropped > 0 {
        out.push_str(&format!(" [{dropped} control characters removed]"));
    }
    out
}

/// One handover, as the host recorded it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ControlEvent {
    pub at: String,
    /// `agent` or `human`.
    pub holder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Append a handover. Best effort by design: a `take` must not fail because the
/// journal could not be written, since the point of `take` is that a person
/// wants the pointer *now*.
///
/// That makes this the one log here that is *not* fail-closed, and the audit
/// says so rather than presenting it beside two logs that are.
pub fn journal_control(dir: &Path, holder: &str, note: Option<&str>) {
    let event = ControlEvent {
        at: now(),
        holder: holder.to_string(),
        note: note.map(str::to_string),
    };
    let Ok(line) = serde_json::to_string(&event) else {
        return;
    };
    let _ = fs::create_dir_all(dir);
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(CONTROL_JOURNAL))
    {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }
}

/// Every handover this session recorded, oldest first.
pub fn control_journal(dir: &Path) -> Vec<ControlEvent> {
    let Ok(text) = fs::read_to_string(dir.join(CONTROL_JOURNAL)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Which of an audit's sources this machine could actually read.
///
/// Reported beside the events, because "no rows" and "no log" are different
/// findings and an audit that renders them the same way reports coverage it
/// does not have.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Sources {
    pub actions: Availability,
    pub requests: Availability,
    pub control: Availability,
    /// The helper log, which exists only for sessions that ran one.
    #[serde(default)]
    pub helpers: Availability,
    /// The message store, for a session opened with `--capture`.
    ///
    /// Counted from the directory rather than asked of the engine, which is the
    /// whole point of it being here: the engine's own account of its store is a
    /// claim, and this is what h5i can see of it from outside. A store that
    /// holds fewer messages than the request log has requests is an evidence gap
    /// whoever is reading the messages needs to know about, and neither log
    /// alone shows it.
    #[serde(default)]
    pub messages: Availability,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Availability {
    /// Read, and it had content.
    Read,
    /// Read, and it was empty. The session really did nothing of this kind.
    #[default]
    Empty,
    /// Not readable from here. Nothing can be concluded from its silence.
    Unavailable,
    /// Read, and only the start of it: the byte cap cut the rest off.
    ///
    /// Its own state because the rows that *are* here look no different either
    /// way, so a timeline that stops reads as a session that went quiet.
    Partial,
}

impl Availability {
    pub fn as_str(self) -> &'static str {
        match self {
            Availability::Read => "read",
            Availability::Empty => "empty",
            Availability::Unavailable => "unavailable",
            Availability::Partial => "partial",
        }
    }

    fn of(text: &Option<String>) -> Availability {
        match text {
            None => Availability::Unavailable,
            Some(t) if t.trim().is_empty() => Availability::Empty,
            Some(_) => Availability::Read,
        }
    }

    /// The same, for a read that says whether the cap cut it.
    fn of_capped(read: &Option<(String, bool)>) -> Availability {
        match read {
            Some((_, true)) => Availability::Partial,
            other => Availability::of(&other.as_ref().map(|(text, _)| text.clone())),
        }
    }
}

/// Everything recorded about one session, in one ordered timeline.
#[derive(Serialize, Debug, Clone)]
pub struct Audit {
    pub session: Session,
    pub sources: Sources,
    /// Oldest first. Each row carries the lane it came from and, where the
    /// source said so, the event that caused it.
    pub events: Vec<crate::browser_events::ViewerEvent>,
    /// How many rows the cap discarded. Rendered, never hidden.
    pub dropped: u64,
}

/// What h5i can see of a session's message store, from outside it.
///
/// `Empty` is the ordinary session: no `--capture`, no directory, nothing
/// stored, and nothing to say. `Read` means the store is there and holds
/// messages. `Unavailable` means the directory exists and this machine cannot
/// read it, which is the answer that matters: a boxed session's store can be
/// inside a box whose filesystem this machine never sees, and reporting that as
/// "no messages" would describe a session that captured nothing.
fn messages_availability(dir: &Path) -> Availability {
    let messages = dir.join(MESSAGES_DIR);
    match fs::read_dir(&messages) {
        Ok(entries) => {
            let any = entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".json"))
            });
            if any {
                Availability::Read
            } else {
                Availability::Empty
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Availability::Empty,
        Err(_) => Availability::Unavailable,
    }
}

/// Most rows an audit holds before it starts dropping the oldest.
///
/// A page that loads a thousand subresources must not make one session's audit
/// unreadable, and a cap that dropped silently would report a quiet session
/// where there was a loud one, so the count comes back in [`Audit::dropped`].
const AUDIT_CAPACITY: usize = 5000;

/// Most of a box-written log this reads.
///
/// The live path has been reading these bounded since it was written ("a
/// four-gigabyte `browser-requests.jsonl` is a four-gigabyte read on the next
/// poll"); the export path read them whole.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// Read one of the box's own logs: bounded, and without following a link.
///
/// For a boxed session these paths are the host's view of files inside the
/// box's `/tmp`, which the box writes by design, so `read_to_string` was an
/// allocation whose size the box chose — and `ln -sf /dev/zero <log>` made it
/// one that never returns, from inside the box, during the one command whose
/// output a reviewer trusts.
///
/// Opened `O_NOFOLLOW` first and `fstat`ed after, rather than stat-then-open:
/// in a directory the box writes, those are two resolutions of a path and only
/// the second one is read.
pub fn read_log_capped(path: &Path) -> Option<String> {
    read_log_capped_saying(path).map(|(text, _)| text)
}

/// The same read, and whether the cap cut it short. A bound that says nothing
/// makes a run past [`MAX_LOG_BYTES`] read as one that stopped making requests.
pub fn read_log_capped_saying(path: &Path) -> Option<(String, bool)> {
    use std::io::Read as _;
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path).ok()?;
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut buf = Vec::new();
    file.take(MAX_LOG_BYTES).read_to_end(&mut buf).ok()?;
    let cut = buf.len() as u64 >= MAX_LOG_BYTES;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // These logs are JSONL and the cap can land mid-line. Ending on a whole
    // line means the parse below drops nothing it could have read.
    Some(match text.rfind('\n') {
        Some(at) => (text[..=at].to_string(), cut),
        None => (text, cut),
    })
}

/// Assemble the whole record of a session: what the agent asked for, what the engine decided,
/// who was driving, and how it ended.
pub fn audit(root: &Path, session: &Session) -> Audit {
    use crate::browser_events as ev;

    let dir = dir(root, &session.id);
    // The flag comes back with the text: a log the cap cut short is a timeline
    // that stops early, and the rows that are here look no different for it.
    let read = |path: &Option<PathBuf>| -> Option<(String, bool)> {
        path.as_ref().and_then(|p| read_log_capped_saying(p))
    };
    let actions = read(&session.logs.actions);
    let requests = read(&session.logs.requests);
    // Straight off the session directory rather than out of `session.logs`: the
    // helper log is h5i's own, written beside the session by whoever ran the
    // helper, and it exists for sessions opened before this file did.
    //
    // Not-found and unreadable are different facts and `Availability::of`
    // collapses them, because `read_to_string(...).ok()` gives `None` for both. A
    // session that never ran a helper must read as `Empty`, and a log this
    // machine could not open must read as `Unavailable`, which is the one an
    // auditor has to see.
    let helpers_path = dir.join(HELPERS_FILE);
    // The *reason* the read failed, not `exists()`. `exists()` answers false
    // for any metadata error (an unreadable session directory, a stale mount)
    // so a log h5i could not open would have been reported as one that never
    // existed, which is the misreport this distinction is here to prevent, in
    // the failure mode most likely to cause it.
    let helpers = fs::read_to_string(&helpers_path);
    let helpers_absent = helpers
        .as_ref()
        .err()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound);
    let helpers = helpers.ok();
    let handovers = control_journal(&dir);
    let read_at = now();

    // Gathered with their times first and ordered afterwards, because the
    // sources are three files and a timeline grouped by file is not a timeline.
    // A stable sort keeps each source's own order inside a tie, which is what
    // keeps a request ahead of its response and a verb ahead of the fetches it
    // caused.
    let mut rows: Vec<Row> = Vec::new();

    rows.push(Row::host(
        &session.started_at,
        ev::Draft::host(ev::EventKind::Lifecycle {
            state: "opened".into(),
            reason: Some(format!("{} — {}", session.url, session.where_it_ran())),
        }),
    ));

    // The action log before the request log: the causal map is filled by the
    // first and read by the second. The one ordering dependency here, and the
    // same one `BoxStream::poll` states in its own comment.
    let mut caused = std::collections::BTreeMap::new();
    if let Some((text, _)) = &actions {
        for draft in ev::ingest_light_actions_with(text, &mut caused) {
            rows.push(Row::engine(&session.started_at, &read_at, draft));
        }
    }
    if let Some((text, _)) = &requests {
        for draft in ev::ingest_request_log_with(text, &caused) {
            rows.push(Row::engine(&session.started_at, &read_at, draft));
        }
    }

    // Host rows, like the handovers and the lifecycle: h5i ran the program and
    // wrote this down from outside it. That is a stronger grade than anything
    // in the engine's two logs, and it is the grade the one lane the engine
    // cannot see deserves.
    if let Some(text) = &helpers {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(row) = serde_json::from_str::<HelperRow>(line) else {
                continue;
            };
            rows.push(Row::host(
                &row.at,
                ev::Draft::host(ev::EventKind::Helper {
                    name: row.name,
                    argv: row.argv,
                    status: row.status,
                    note: row.note,
                }),
            ));
        }
    }

    for handover in &handovers {
        rows.push(Row::host(
            &handover.at,
            ev::Draft::host(ev::EventKind::Control {
                holder: handover.holder.clone(),
                note: handover.note.clone(),
            }),
        ));
    }

    if let (Some(ended_at), state) = (&session.ended_at, session.state)
        && !state.is_live()
    {
        rows.push(Row::host(
            ended_at,
            ev::Draft::host(ev::EventKind::Lifecycle {
                state: state.as_str().into(),
                reason: session.end_reason.clone(),
            }),
        ));
    }

    rows.sort_by_key(|row| row.order);

    let mut log = ev::EventLog::new(AUDIT_CAPACITY);
    for row in rows {
        log.extend([row.draft], &row.observed_at);
    }

    Audit {
        session: session.clone(),
        sources: Sources {
            actions: Availability::of_capped(&actions),
            requests: Availability::of_capped(&requests),
            control: if handovers.is_empty() {
                Availability::Empty
            } else {
                Availability::Read
            },
            messages: messages_availability(&dir),
            helpers: match (&helpers, helpers_absent) {
                (Some(text), _) if text.trim().is_empty() => Availability::Empty,
                (Some(_), _) => Availability::Read,
                // Not there: no helper ever ran for this session.
                (None, true) => Availability::Empty,
                // There, or unknowable. Either way h5i could not read it, and
                // nothing can be concluded from its silence, which is exactly
                // what `Unavailable` says.
                (None, false) => Availability::Unavailable,
            },
        },
        events: log.since(0).into_iter().cloned().collect(),
        dropped: log.dropped(),
    }
}

/// One line of [`HELPERS_FILE`], as h5i wrote it.
///
/// Its own type rather than the event enum: the file is h5i's record of what it
/// ran, and the audit's event shape is a rendering concern that should be free
/// to change without rewriting a log already on disk.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HelperRow {
    /// RFC3339, from h5i's clock. Unlike the engine's stamps, this one is the
    /// clock the reader is on, and [`record_helper`] sets it. What a caller
    /// puts here is ignored.
    pub at: String,
    /// The program, as h5i names the lane: `yt-dlp`.
    pub name: String,
    /// What was actually executed. h5i built it, so it is a fact and not a
    /// claim, and it carries no credential, because this lane is not given
    /// one.
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Append one row to a session's helper log.
///
/// Best effort by design, and the *caller* decides what a failure means. It is
/// deliberately not fail-closed the way the engine's receipts are: those gate
/// bytes reaching a page, and this records that a program ran, after it has
/// already run. Refusing to report a result h5i already has because a log line
/// would not write would lose the result and the record both.
pub fn record_helper(root: &Path, id: &str, row: &HelperRow) -> Result<(), H5iError> {
    let dir = dir(root, id);
    fs::create_dir_all(&dir)
        .map_err(|e| H5iError::Metadata(format!("could not open the session directory: {e}")))?;
    append_helper(&dir.join(HELPERS_FILE), row)
}

/// The helper log for the runs that belong to no session.
pub const SESSIONLESS_HELPERS_FILE: &str = "helpers.jsonl";

/// Append one row for a run that had no session. See
/// [`SESSIONLESS_HELPERS_FILE`]; best effort in the same way [`record_helper`]
/// is, and for the same reason.
pub fn record_sessionless_helper(root: &Path, row: &HelperRow) -> Result<(), H5iError> {
    fs::create_dir_all(root).map_err(|e| {
        H5iError::Metadata(format!("could not open the browser state directory: {e}"))
    })?;
    append_helper(&root.join(SESSIONLESS_HELPERS_FILE), row)
}

/// Read back the rows written by [`record_sessionless_helper`], oldest first.
///
/// A log that is not there is an empty one: no run of this kind has happened,
/// which is the ordinary case. A log that is there and cannot be read is an
/// error rather than an empty answer, because the two mean opposite things to
/// whoever is asking.
pub fn sessionless_helpers(root: &Path) -> Result<Vec<HelperRow>, H5iError> {
    let text = match fs::read_to_string(root.join(SESSIONLESS_HELPERS_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(H5iError::Metadata(format!(
                "could not read the helper log at {}: {e}",
                root.join(SESSIONLESS_HELPERS_FILE).display()
            )));
        }
    };
    // A line that will not parse is skipped rather than failing the read: this
    // file is appended to by concurrent runs, and one torn line must not hide
    // every row written before it.
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<HelperRow>(line).ok())
        .collect())
}

/// Stamp a row and append it, whichever log it belongs in.
///
/// Stamped here rather than by the caller. The audit interleaves these with the
/// engine's rows on time alone, so a second clock reading taken in another
/// module is a second clock the timeline can be wrong about, and this is the
/// one lane whose whole value is being h5i's own observation.
fn append_helper(path: &Path, row: &HelperRow) -> Result<(), H5iError> {
    use std::io::Write;
    let row = HelperRow {
        at: now(),
        ..row.clone()
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| H5iError::Metadata(format!("could not open the helper log: {e}")))?;
    let line = serde_json::to_string(&row)
        .map_err(|e| H5iError::Metadata(format!("could not write the helper row: {e}")))?;
    writeln!(file, "{line}")
        .map_err(|e| H5iError::Metadata(format!("could not append to the helper log: {e}")))
}

/// One audit row, with the instant it sorts on.
///
/// `order` is a parsed instant rather than the string, because the two clocks
/// print at different precisions: `2026-01-01T00:00:00Z` sorts *after*
/// `2026-01-01T00:00:00.500000Z` as text, which would put a handover after
/// engine rows it actually preceded.
struct Row {
    order: i64,
    observed_at: String,
    draft: crate::browser_events::Draft,
}

impl Row {
    /// A row h5i wrote itself: its time is an observation, and it is the same
    /// value the timeline sorts on.
    fn host(at: &str, draft: crate::browser_events::Draft) -> Row {
        Row {
            order: micros(at),
            observed_at: at.to_string(),
            draft,
        }
    }

    /// A row from one of the engine's logs.
    ///
    /// It sorts on the engine's own claim, because that is the only clock that can
    /// order the two engine logs against each other. What it is *stamped* with is
    /// `read_at`, the moment h5i read the file, because `observed_at` means "when
    /// h5i saw this" everywhere else in this module. A row with no claim at all
    /// falls back to the session's start, which puts it at the top rather than
    /// pretending to a position it cannot support.
    fn engine(started_at: &str, read_at: &str, draft: crate::browser_events::Draft) -> Row {
        let order = draft
            .claimed_at
            .as_deref()
            .map(micros)
            .unwrap_or_else(|| micros(started_at));
        Row {
            order,
            observed_at: read_at.to_string(),
            draft,
        }
    }
}

/// An RFC3339 stamp as microseconds since the epoch, or `0` when it will not
/// parse. Zero rather than a failure: a row with an unreadable time still
/// belongs in the audit, at the top, where its lack of a position is visible.
fn micros(at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|t| t.timestamp_micros())
        .unwrap_or(0)
}

/// The host's clock, RFC3339 with microseconds.
///
/// Microseconds because these stamps have to interleave with the engine's, and
/// the engine writes a whole agent loop inside one second. At second precision
/// every host row lands on the `.000000` boundary and sorts ahead of engine
/// rows it actually followed. A timeline that is arithmetically correct and
/// tells the wrong story, which is worse than one that is obviously broken.
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// `kill(pid, 0)`: does a process with this id exist and are we allowed to
/// signal it? A pid we cannot signal is not ours and so is not our session's.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    // No portable equivalent, and a wrong `false` would report a live session
    // dead. The control file is the fallback everywhere else.
    true
}

#[cfg(test)]
mod tests {
    /// A name only resolves to a live session, so a closed one asked for by
    /// name came back as "no such session" — though its record stays.
    #[test]
    fn a_closed_session_is_still_findable_by_the_name_it_was_opened_with() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut session = named(root, "auth");

        // While it is live, only the live lookup finds it.
        assert!(find_by_name(root, "auth").is_some());
        assert!(find_ended_by_name(root, "auth").is_none());

        end(root, &mut session, State::Closed, "closed by the user");

        // And once it has ended, the other way round.
        assert!(find_by_name(root, "auth").is_none());
        let found = find_ended_by_name(root, "auth").expect("the record stays");
        assert_eq!(found.id, session.id);
        assert!(find_ended_by_name(root, "never").is_none());
    }

    /// And the audit built on it says so, rather than rendering the head of a
    /// log as the whole run. The rows that are there look no different either
    /// way, so a timeline that stops reads as a session that went quiet.
    #[test]
    fn an_audit_over_a_capped_log_reports_the_source_as_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut session = named(root, "long");
        let dir = dir(root, &session.id);

        // A request log past the cap, and an action log comfortably under it.
        let mut requests = String::new();
        while requests.len() as u64 <= super::MAX_LOG_BYTES {
            requests.push_str(
                "{\"seq\":0,\"at\":\"2026-09-04T00:00:00.000000Z\",\"phase\":\"request\",\
                 \"initiator\":\"navigation\",\"method\":\"GET\",\
                 \"url\":\"https://app.test/\",\"allowed\":true}\n",
            );
        }
        std::fs::write(dir.join(RECEIPTS_FILE), requests.as_bytes()).unwrap();
        std::fs::write(dir.join("browser-actions.jsonl"), b"").unwrap();
        session.logs.requests = Some(dir.join(RECEIPTS_FILE));
        session.logs.actions = Some(dir.join("browser-actions.jsonl"));

        let audit = audit(root, &session);
        assert_eq!(
            audit.sources.requests,
            Availability::Partial,
            "a log the cap cut short is not one that was read whole"
        );
        assert_eq!(audit.sources.actions, Availability::Empty);
    }

    /// A bound that says nothing makes a run past the cap read as a session
    /// that simply stopped making requests.
    #[test]
    fn a_log_read_says_whether_the_cap_cut_it() {
        let tmp = tempfile::tempdir().unwrap();
        let small = tmp.path().join("small.jsonl");
        std::fs::write(&small, b"{\"seq\":0}\n{\"seq\":1}\n").unwrap();
        let (text, cut) = read_log_capped_saying(&small).expect("read");
        assert!(!cut, "a log under the cap is whole");
        assert_eq!(text.lines().count(), 2);

        let big = tmp.path().join("big.jsonl");
        let line = format!("{}\n", "{\"seq\":0,\"pad\":\"aaaaaaaaaaaaaaaa\"}");
        let mut body = String::new();
        while body.len() as u64 <= super::MAX_LOG_BYTES {
            body.push_str(&line);
        }
        std::fs::write(&big, body.as_bytes()).unwrap();
        let (text, cut) = read_log_capped_saying(&big).expect("read");
        assert!(cut, "a log over the cap says so");
        // And what comes back is still whole lines, so nothing half-parsed.
        assert!(text.ends_with('\n'));
    }

    /// An id addresses the jar, the control channel and the message store, and
    /// arrives from a selector and from a file boxed code can write.
    #[test]
    fn an_id_that_is_not_one_component_names_no_directory() {
        for bad in ["..", "../../etc", "a/b", "a\\b", ".hidden", ""] {
            assert!(!super::id_is_one_component(bad), "`{bad}` should not be an id");
        }
        for good in ["br_g9pftf", "br_1", "a.b-c_d"] {
            assert!(super::id_is_one_component(good), "`{good}` should be an id");
        }
    }

    /// And `read` answers "unknown session" rather than following it out.
    #[test]
    fn reading_a_traversing_id_is_an_unknown_session() {
        let root = tempfile::tempdir().unwrap();
        assert!(super::read(root.path(), "../../../etc/passwd").is_err());
    }

    /// The two logs an audit reads live inside the box's own `/tmp`, which the
    /// box writes. Reading them whole made `h5i box export` allocate whatever
    /// the box wrote, and following a link made it read whatever the box
    /// pointed at.
    #[test]
    fn a_box_written_log_is_read_bounded_and_without_following_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("actions.jsonl");

        // Bounded, and cut on a line boundary so the parse loses nothing it
        // could have read.
        let line = format!("{}\n", "x".repeat(4095));
        let big: String = std::iter::repeat_n(line, 4096).collect();
        assert!(big.len() as u64 > super::MAX_LOG_BYTES);
        std::fs::write(&log, &big).unwrap();
        let read = super::read_log_capped(&log).expect("reads");
        assert!(read.len() as u64 <= super::MAX_LOG_BYTES);
        assert!(read.ends_with('\n'), "must end on a whole line");

        // And a link is not a log.
        #[cfg(unix)]
        {
            let linked = dir.path().join("linked.jsonl");
            std::os::unix::fs::symlink(&log, &linked).unwrap();
            assert!(super::read_log_capped(&linked).is_none());
        }
    }

    use super::*;

    fn session(id: &str, placement: Placement) -> Session {
        Session {
            id: id.to_string(),
            name: None,
            engine: Engine::H5iLight,
            lane: Session::lane_for(&placement, true),
            placement,
            url: "https://example.com/".into(),
            started_at: now(),
            expires_at: None,
            storage: Storage::Ephemeral,
            policy_digest: "sha256:test".into(),
            identity: "native".into(),
            identity_digest: "test".into(),
            restored_from: None,
            state: State::Live,
            ended_at: None,
            end_reason: None,
            confinement: crate::browser_sandbox::Confinement::Process,
            enclosing_box: None,
            control: Control::default(),
            logs: Logs::default(),
            permissive_cors: false,
        }
    }

    #[test]
    fn removing_a_session_takes_its_whole_directory_and_frees_the_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let s = session(&id, Placement::Host);
        write(root, &s).unwrap();
        set_default(root, &id).unwrap();
        std::fs::create_dir_all(dir(root, &id).join(MESSAGES_DIR)).unwrap();
        std::fs::write(dir(root, &id).join(MESSAGES_DIR).join("0.request.json"), "x".repeat(100))
            .unwrap();

        let held = remove(root, &id).expect("removed");
        assert_eq!(held.store, 100, "the store is counted apart");
        assert!(held.account > 0, "and the record is counted too");
        assert!(!dir(root, &id).exists());
        assert_eq!(
            read_default(root),
            None,
            "a pointer to a record that is gone would send the next verb nowhere"
        );
    }

    #[test]
    fn dropping_the_store_keeps_the_account() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let s = session(&id, Placement::Host);
        write(root, &s).unwrap();
        let store = dir(root, &id).join(MESSAGES_DIR);
        std::fs::create_dir_all(store.join("bodies")).unwrap();
        std::fs::write(store.join("bodies").join("aa"), "y".repeat(64)).unwrap();
        std::fs::write(dir(root, &id).join(RECEIPTS_FILE), "{}\n").unwrap();

        let freed = drop_store(root, &id).expect("dropped");
        assert_eq!(freed, 64);
        assert!(!store.exists(), "the bytes go");
        let note = reclaimed(root, &id).expect("a note where the store was");
        assert_eq!(note.bytes, 64);
        assert!(
            !note.at.is_empty(),
            "`kept and reclaimed` and `never kept` are different facts"
        );
        assert!(dir(root, &id).join(RECORD).exists(), "the record stays");
        assert!(
            dir(root, &id).join(RECEIPTS_FILE).exists(),
            "and so does what it says the session did"
        );
        assert_eq!(drop_store(root, &id).expect("again"), 0, "and it is idempotent");
    }

    #[test]
    fn an_id_that_is_not_one_component_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        // A boxed session writes its own record, and that id becomes this path.
        assert!(remove(tmp.path(), "../elsewhere").is_err());
        assert!(drop_store(tmp.path(), "a/b").is_err());
    }

    #[test]
    fn ids_are_never_reused_even_after_the_session_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        write(root, &s).unwrap();
        end(root, &mut s, State::Closed, "closed by the user");

        // The directory survives the ending, which is what forbids the reuse.
        assert!(dir(root, &id).exists());
        for _ in 0..32 {
            assert_ne!(new_id(root).unwrap(), id);
        }
    }

    #[test]
    fn a_verb_on_an_ended_session_is_refused_rather_than_restarted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        write(root, &s).unwrap();
        end(root, &mut s, State::Closed, "closed by the user");

        match open_live(root, &id) {
            Err(SessionGone::Ended { state, .. }) => assert_eq!(state, State::Closed),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_control_file_is_recorded_as_died_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        // Boxed, and the `box run` that carried it is gone: liveness falls to
        // the control file as the host sees it, and there is none.
        let mut s = session(
            &id,
            Placement::Box {
                name: "web".into(),
            },
        );
        s.control.witness = Some(dir(root, &id).join(CONTROL_FILE));
        write(root, &s).unwrap();

        match open_live(root, &id) {
            Err(SessionGone::Ended { state, .. }) => assert_eq!(state, State::Died),
            other => panic!("expected died, got {other:?}"),
        }
        // And it is now written down, not re-derived by the next reader.
        assert_eq!(read(root, &id).unwrap().state, State::Died);
    }

    #[test]
    fn a_session_we_cannot_see_into_is_not_declared_dead() {
        // Image-backed tier: no host pid left, no host-visible control file.
        // Guessing "died" here would close a session that is still serving.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let s = session(
            &id,
            Placement::Box {
                name: "web".into(),
            },
        );
        assert!(s.control.witness.is_none() && s.control.pid.is_none());
        write(root, &s).unwrap();
        assert!(open_live(root, &id).is_ok());
        assert_eq!(read(root, &id).unwrap().state, State::Live);
    }

    #[test]
    fn the_first_ending_is_the_one_that_sticks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        write(root, &s).unwrap();
        end(root, &mut s, State::Closed, "closed by the user");
        end(root, &mut s, State::Died, "engine stopped");
        assert_eq!(read(root, &id).unwrap().state, State::Closed);
    }

    #[test]
    fn removing_a_box_evicts_its_sessions_and_leaves_host_ones_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let boxed = new_id(root).unwrap();
        let hosted = new_id(root).unwrap();
        write(
            root,
            &session(
                &boxed,
                Placement::Box {
                    name: "web".into(),
                },
            ),
        )
        .unwrap();
        write(root, &session(&hosted, Placement::Host)).unwrap();

        assert_eq!(evict_box(root, "web").unwrap(), 1);
        assert_eq!(read(root, &boxed).unwrap().state, State::Evicted);
        assert_eq!(read(root, &hosted).unwrap().state, State::Live);
    }

    #[test]
    fn expiry_is_an_event_on_the_record_not_a_disappearance() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        s.expires_at = Some("2000-01-01T00:00:00Z".into());
        write(root, &s).unwrap();

        assert_eq!(expire_due(root).unwrap(), 1);
        let after = read(root, &id).unwrap();
        assert_eq!(after.state, State::Expired);
        assert!(after.ended_at.is_some());
        assert!(dir(root, &id).exists());
    }

    fn named(root: &Path, name: &str) -> Session {
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        s.name = Some(name.to_string());
        write(root, &s).unwrap();
        s
    }

    #[test]
    fn the_ordinary_case_names_nothing_and_lands_on_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        write(root, &session(&id, Placement::Host)).unwrap();
        set_default(root, &id).unwrap();

        assert_eq!(resolve(root, None).unwrap().id, id);
    }

    #[test]
    fn a_name_addresses_a_session_and_so_does_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let auth = named(root, "auth");
        let public = named(root, "public");

        assert_eq!(resolve(root, Some("auth")).unwrap().id, auth.id);
        assert_eq!(resolve(root, Some("public")).unwrap().id, public.id);
        // The id from `--json` works where it is pasted.
        assert_eq!(resolve(root, Some(&auth.id)).unwrap().id, auth.id);
    }

    #[test]
    fn there_is_no_lone_session_shortcut() {
        // One live session and no default: still an error, not a guess. A rule
        // that silently picks "the only one" moves under an agent the moment a
        // second session exists.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        named(root, "auth");
        let why = resolve(root, None).unwrap_err().to_string();
        assert!(why.contains("--session auth"), "{why}");
    }

    #[test]
    fn the_default_outlives_the_session_so_the_next_verb_can_say_how_it_ended() {
        // Clearing the pointer here would turn "the session you were on was
        // closed" into "no session is open", which reads as though there never
        // was one. The first tells an agent what happened to its page.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        write(root, &s).unwrap();
        set_default(root, &id).unwrap();

        end(root, &mut s, State::Closed, "closed by the user");
        match resolve(root, None) {
            Err(SessionGone::Ended { state, id: gone, .. }) => {
                assert_eq!(state, State::Closed);
                assert_eq!(gone, id);
            }
            other => panic!("expected the ending, got {other:?}"),
        }
    }

    #[test]
    fn a_default_naming_a_record_that_is_gone_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        set_default(root, "br_never").unwrap();

        let why = resolve(root, None).unwrap_err().to_string();
        assert!(why.contains("h5i browser open"), "{why}");
        assert_eq!(read_default(root), None, "a pointer to nothing is not kept");
    }

    #[test]
    fn a_name_can_be_reused_once_its_session_has_ended() {
        // This is what makes a name comfortable to type, and exactly why the
        // id is what gets written into the record.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut first = named(root, "auth");
        end(root, &mut first, State::Closed, "closed by the user");

        let second = named(root, "auth");
        assert_ne!(second.id, first.id);
        assert_eq!(resolve(root, Some("auth")).unwrap().id, second.id);
    }

    /// A session with both engine logs, a handover between two verbs, and an
    /// ending. The point of the test is the *order*: grouped by source is not
    /// a timeline, and "a human was driving between these two verbs" is the
    /// question an audit exists to answer.
    #[test]
    fn the_audit_interleaves_the_engine_and_the_host_by_time() {
        use crate::browser_events::EventKind as K;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let dir = dir(root, &id);

        std::fs::write(
            dir.join(ACTIONS_FILE),
            "{\"seq\":0,\"at\":\"2026-01-01T00:00:01.000000Z\",\"phase\":\"result\",\"verb\":\"snapshot\",\"ok\":true}\n\
             {\"seq\":1,\"at\":\"2026-01-01T00:00:03.000000Z\",\"phase\":\"result\",\"verb\":\"click\",\"target\":\"@e1\",\"ok\":true,\"requests\":[7]}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(RECEIPTS_FILE),
            "{\"seq\":7,\"at\":\"2026-01-01T00:00:03.500000Z\",\"phase\":\"request\",\"initiator\":\"navigation\",\"method\":\"GET\",\"url\":\"https://example.com/\",\"allowed\":true}\n",
        )
        .unwrap();
        journal_control(&dir, "human", Some("taken"));

        let mut session = session(&id, Placement::Host);
        session.started_at = "2026-01-01T00:00:00.000000Z".into();
        session.logs = Logs {
            actions: Some(dir.join(ACTIONS_FILE)),
            requests: Some(dir.join(RECEIPTS_FILE)),
        };
        write(root, &session).unwrap();
        end(root, &mut session, State::Closed, "closed by the user");

        let audit = audit(root, &read(root, &id).unwrap());
        let shape: Vec<&str> = audit
            .events
            .iter()
            .map(|e| match &e.kind {
                K::Lifecycle { state, .. } if state == "opened" => "open",
                K::Lifecycle { .. } => "end",
                K::Control { .. } => "control",
                K::AgentAction { action, .. } if action.starts_with("snapshot") => "snapshot",
                K::AgentAction { .. } => "click",
                K::Request { .. } => "request",
                _ => "other",
            })
            .collect();

        // The handover was journalled with today's clock, so it lands after the
        // 2026-01-01 rows; what matters here is that the engine rows themselves
        // came out in time order rather than file order.
        let engine: Vec<&&str> = shape
            .iter()
            .filter(|k| matches!(**k, "snapshot" | "click" | "request"))
            .collect();
        assert_eq!(
            engine,
            vec![&"snapshot", &"click", &"request"],
            "grouped by source rather than ordered by time: {shape:?}"
        );
        assert_eq!(shape.first(), Some(&"open"));
        assert_eq!(shape.last(), Some(&"end"));

        // The causal link the action log carried is resolved, not inferred.
        let request = audit
            .events
            .iter()
            .find(|e| matches!(e.kind, K::Request { .. }))
            .expect("the request row");
        assert!(
            request.caused_by.is_some(),
            "the click that caused this fetch is not linked to it"
        );
    }

    /// A log this machine cannot read is `unavailable`, never an empty list.
    /// An empty list looks like a session that did nothing.
    #[test]
    fn an_unreadable_log_is_reported_as_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut session = session(&id, Placement::Host);
        session.logs = Logs::default();
        write(root, &session).unwrap();

        let audit = audit(root, &session);
        assert_eq!(audit.sources.actions, Availability::Unavailable);
        assert_eq!(audit.sources.requests, Availability::Unavailable);
        assert_eq!(audit.sources.control, Availability::Empty);
    }

    /// The two lanes stay apart. A row h5i wrote from outside must never be
    /// presented as something the engine reported about itself.
    #[test]
    fn host_rows_and_engine_rows_keep_their_lanes() {
        use crate::browser_events::{EventKind as K, Lane};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let dir = dir(root, &id);
        std::fs::write(
            dir.join(ACTIONS_FILE),
            "{\"seq\":0,\"at\":\"2026-01-01T00:00:01.000000Z\",\"phase\":\"result\",\"verb\":\"snapshot\",\"ok\":true}\n",
        )
        .unwrap();
        let mut session = session(&id, Placement::Host);
        session.logs.actions = Some(dir.join(ACTIONS_FILE));
        write(root, &session).unwrap();
        journal_control(&dir, "human", None);

        let audit = audit(root, &session);
        for event in &audit.events {
            let expected = match event.kind {
                K::Lifecycle { .. } | K::Control { .. } => Lane::HostObserved,
                _ => Lane::BoxClaimed,
            };
            assert_eq!(event.lane, expected, "{:?} is in the wrong lane", event.kind);
        }
    }

    /// A session opened from inside a box is mechanically a host session and is
    /// *not* uncontained. Saying "no containment beyond the engine" there
    /// would understate what is true, which is the same class of error as
    /// overstating it. Just in the direction that happens to be safe.
    #[test]
    fn a_session_opened_from_inside_a_box_names_the_box_it_is_in() {
        let mut inside = session("br_inside", Placement::Host);
        inside.enclosing_box = Some("env/human/web".into());
        let said = inside.where_it_ran();
        assert!(said.contains("env/human/web"), "{said}");
        assert!(
            !said.contains("no containment"),
            "understating a box is still describing it wrong: {said}"
        );
        // And nothing is claimed about what that box enforces, because from in
        // there the policy is sealed.
        assert!(said.contains("not readable here"), "{said}");
        assert_eq!(
            Session::lane_for(&inside.placement, false),
            Lane::EngineClaimed
        );
    }

    /// The channel is recorded rather than re-derived. A verb that guessed the
    /// address would be a second place that has to agree with the first, and
    /// the two failures it produces (a port nothing is listening on, a socket
    /// path that is not there) both look like a session that is not running.
    #[test]
    fn the_channel_travels_with_the_record() {
        assert_eq!(Channel::Port.flag(), "--control-file");
        assert_eq!(Channel::Socket.flag(), "--control-socket");

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = new_id(root).unwrap();
        let mut s = session(&id, Placement::Host);
        s.control.channel = Channel::Socket;
        s.control.file = Some(dir(root, &id).join("control.sock"));
        write(root, &s).unwrap();

        let back = read(root, &id).unwrap();
        assert_eq!(back.control.channel, Channel::Socket);
        assert_eq!(back.control.file, s.control.file);
    }

    /// A record written before the channel existed still loads, and loads as
    /// the channel those sessions actually used.
    #[test]
    fn a_record_without_a_channel_reads_as_a_port() {
        let raw = r#"{"id":"br_old","engine":"h5i-light","placement":{"kind":"host"},
            "lane":"engine-claimed","url":"https://example.com/","started_at":"2026-01-01T00:00:00Z",
            "expires_at":null,"storage":"ephemeral","policy_digest":"sha256:x","restored_from":null,
            "state":"closed","ended_at":null,"end_reason":null,
            "control":{"file":null,"witness":null,"pid":null}}"#;
        let session: Session = serde_json::from_str(raw).expect("an older record still loads");
        assert_eq!(session.control.channel, Channel::Port);
        assert_eq!(session.enclosing_box, None);
    }

    /// A host that cannot confine runs the session anyway and says so, with the
    /// reason. A sandbox nobody can see is indistinguishable from one that was
    /// never applied.
    #[test]
    fn an_unconfined_session_says_so_and_why() {
        let mut outside = session("br_outside", Placement::Host);
        outside.confinement = crate::browser_sandbox::Confinement::None {
            why: "this host has no Landlock".into(),
        };
        let said = outside.where_it_ran();
        assert!(said.contains("unconfined"), "{said}");
        assert!(said.contains("no Landlock"), "{said}");
    }

    /// And a confined one names the two things it does *not* contain, because
    /// those are the two a reader would otherwise assume it does.
    #[test]
    fn a_confined_session_names_what_it_does_not_contain() {
        let inside = session("br_inside", Placement::Host);
        assert_eq!(
            inside.confinement,
            crate::browser_sandbox::Confinement::Process
        );
        let said = inside.where_it_ran();
        assert!(said.contains("sandbox"), "{said}");
        assert!(said.contains("not its network"), "{said}");
        // The sandbox is not evidence: it corroborates no part of the log.
        assert_eq!(
            Session::lane_for(&inside.placement, false),
            Lane::EngineClaimed
        );
    }

    /// `--in` and "I am already in a box" are different facts, and the record
    /// keeps them apart: one is h5i placing a session somewhere it can see the
    /// policy of, the other is h5i already being there.
    #[test]
    fn placed_in_a_box_and_opened_inside_one_are_not_the_same_row() {
        let placed = session(
            "br_placed",
            Placement::Box {
                name: "web".into(),
            },
        );
        assert!(placed.where_it_ran().contains("in box `web`"));
        assert!(placed.enclosing_box.is_none());
    }

    #[test]
    fn a_box_earns_the_host_observed_lane_only_by_enforcing_at_its_boundary() {
        let boxed = Placement::Box { name: "w".into() };
        assert_eq!(Session::lane_for(&Placement::Host, true), Lane::EngineClaimed);
        assert_eq!(Session::lane_for(&boxed, true), Lane::HostObserved);
        // A box that lets the engine reach the whole network corroborates
        // nothing, so it does not upgrade the lane just by being a box.
        assert_eq!(Session::lane_for(&boxed, false), Lane::EngineClaimed);
    }

    #[test]
    fn an_escape_sequence_never_survives_the_relay() {
        // A page that can print ESC into an agent's terminal can repaint the
        // line above it, which is the whole attack.
        let hostile = "ok\u{1b}[2K\u{1b}[1A malicious\r overwrite\u{0}";
        let clean = scrub_text(hostile);
        assert!(!clean.contains('\u{1b}'), "{clean}");
        assert!(!clean.contains('\r'), "{clean}");
        assert!(!clean.contains('\u{0}'), "{clean}");
        assert!(clean.starts_with("ok"), "{clean}");
        assert!(clean.contains("control characters removed"), "{clean}");
    }

    #[test]
    fn newlines_and_tabs_are_page_text_and_stay() {
        assert_eq!(scrub_text("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn truncation_is_stated_in_the_value_not_performed_quietly() {
        let huge = "x".repeat(MAX_STRING * 2);
        let clean = scrub_text(&huge);
        assert!(clean.len() < huge.len());
        assert!(clean.contains("not relayed"), "silent truncation");

        let mut value = serde_json::json!({
            "refs": (0..MAX_ARRAY + 5).map(|i| i.to_string()).collect::<Vec<_>>(),
        });
        scrub(&mut value);
        let refs = value["refs"].as_array().unwrap();
        assert_eq!(refs.len(), MAX_ARRAY + 1);
        assert!(refs.last().unwrap().as_str().unwrap().contains("not relayed"));
    }

    #[test]
    fn scrub_reaches_nested_strings() {
        let mut value = serde_json::json!({"page": {"title": "a\u{1b}[31mb"}});
        scrub(&mut value);
        assert!(!value["page"]["title"].as_str().unwrap().contains('\u{1b}'));
    }

    #[test]
    fn the_host_names_every_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for hostile in [
            "../../etc/passwd",
            "/etc/passwd",
            "..",
            ".bashrc",
            "a/b/c.png",
            "",
        ] {
            let path = artifact_path(root, "br_test", hostile);
            let parent = dir(root, "br_test").join(ARTIFACTS_DIR);
            assert_eq!(path.parent().unwrap(), parent, "escaped with {hostile:?}");
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(!name.starts_with('.'), "dotfile from {hostile:?}");
            assert!(!name.is_empty());
        }
    }
}
