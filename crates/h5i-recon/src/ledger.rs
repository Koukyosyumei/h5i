//! The endpoint ledger: one append-only log of what recon learned, and the
//! fold that turns it into an inventory.
//!
//! Append-only because provenance, resume and "what is new since my last turn"
//! are three reads of the same log, and because a discovery run that dies has
//! still earned what it found. The file holds observations; an [`Endpoint`] is
//! what folding them produces, never what is stored.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use h5i_error::H5iError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The ledger, inside a session directory.
pub const RECON_DIR: &str = "recon";

/// The log itself, one JSON observation per line.
pub const LEDGER_FILE: &str = "ledger.jsonl";

/// How far the request log has been folded in, so a second sync is cheap and
/// does not write the same observation twice.
pub const STATE_FILE: &str = "state.json";

/// The longest URL or path an observation may carry.
///
/// A target can emit an arbitrarily long string and a ledger is a file on the
/// operator's disk, so the bound is here rather than in the caller.
pub const MAX_FIELD_BYTES: usize = 8 * 1024;

/// How much of a ledger one read will fold.
pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

/// How much recon knows about one endpoint, and how it knows it.
///
/// The order of these matters: [`State::advances_over`] uses it to keep a
/// re-disclosure from overwriting an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Something disclosed it. No request was ever sent.
    Candidate,
    /// A request was sent and a response came back.
    Observed,
    /// Observed, and distinguished from the target's not-found baseline.
    /// The only state that means "this exists".
    Confirmed,
    /// Policy declined it. Kept rather than dropped: "recon wanted to reach
    /// this and was not allowed" is a fact about the scope, not a non-event.
    Refused,
    /// Confirmed once, and now answering like a path that is not there.
    Gone,
}

impl State {
    /// Whether `self` may overwrite `prior`.
    ///
    /// Only one rule, and it is the discipline the ledger exists for: nothing
    /// that never sent a request may overwrite something that did. A bundle
    /// that mentions `/admin` again does not un-confirm `/admin`.
    pub fn advances_over(self, prior: State) -> bool {
        !(self == State::Candidate && prior != State::Candidate)
    }
}

/// Where in a request a parameter lived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Where {
    Query,
    Form,
    Json,
    Header,
    Cookie,
    /// Named by a definition or a disclosure rather than seen in a request.
    Declared,
}

/// One parameter name, and where it was seen.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub at: Where,
}

/// How an endpoint was learned.
///
/// Every variant that came off the wire names the message it came from, so
/// "why does the ledger think this exists" always answers with stored bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "kebab-case")]
pub enum Source {
    /// The HTML of a page this session fetched.
    Page { req: String },
    /// A script this session fetched.
    Script { req: String },
    /// A response body that was not markup.
    Json { req: String },
    /// A `Location`, `Link` or CSP header.
    Header { req: String },
    /// `robots.txt`, `sitemap.xml` and the rest of the well-known set.
    KnownFile { req: String },
    /// The session's own request log: h5i went there.
    Receipt { req: String },
    /// A wordlist entry, named by the list it came from.
    Wordlist { list: String },
    /// An API definition.
    Openapi { at: String },
    /// A file the operator produced with another tool. Testimony, and it stays
    /// a candidate until an h5i request observes it (N17, N19).
    Import { tool: String },
    /// A person or an agent said so.
    Manual,
}

/// One line of the ledger: what was learned, once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// `ep_<16 hex>` over the key: origin, path, method, identity.
    pub id: String,
    pub origin: String,
    pub path: String,
    pub method: String,
    /// The identity that observed it, or `anonymous`. In the key, not beside
    /// it: "does this answer differently when logged in" is the question an
    /// inventory is for, and one row per URL cannot hold the answer.
    pub identity: String,
    pub state: State,
    pub source: Source,
    /// RFC3339, from the same clock the receipts use.
    pub at: String,
    /// The message this observation rests on, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The triage cluster this response fell into, once triage has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// Why policy refused it, for a [`State::Refused`] row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Agent-written. Nothing in h5i ever writes a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The name an endpoint keeps for the life of the ledger.
pub fn endpoint_id(origin: &str, path: &str, method: &str, identity: &str) -> String {
    let mut hasher = Sha256::new();
    // Length-prefixed, so `("/a", "b")` and `("/ab", "")` are different keys.
    for part in [origin, path, method, identity] {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut id = String::from("ep_");
    for byte in &digest[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

impl Observation {
    /// An observation with its id computed from the key it names.
    pub fn new(
        origin: &str,
        path: &str,
        method: &str,
        identity: &str,
        state: State,
        source: Source,
    ) -> Self {
        Self {
            id: endpoint_id(origin, path, method, identity),
            origin: origin.to_string(),
            path: path.to_string(),
            method: method.to_string(),
            identity: identity.to_string(),
            state,
            source,
            at: h5i_wire::record::now_rfc3339(),
            req: None,
            params: Vec::new(),
            status: None,
            cluster: None,
            reason: None,
            note: None,
        }
    }

    pub fn with_req(mut self, req: impl Into<String>) -> Self {
        self.req = Some(req.into());
        self
    }

    pub fn with_params(mut self, params: Vec<Param>) -> Self {
        self.params = params;
        self
    }

    pub fn with_status(mut self, status: Option<u16>) -> Self {
        self.status = status;
        self
    }

    pub fn refused(mut self, reason: impl Into<String>) -> Self {
        self.state = State::Refused;
        self.reason = Some(reason.into());
        self
    }

    /// Whether this row is small enough to keep. A target can emit a megabyte
    /// of URL; the ledger is a file on the operator's disk.
    fn is_bounded(&self) -> bool {
        self.origin.len() <= MAX_FIELD_BYTES
            && self.path.len() <= MAX_FIELD_BYTES
            && self.method.len() <= 64
            && self.identity.len() <= 256
            && self.params.len() <= 512
    }
}

/// One endpoint, as the fold leaves it. Never stored, always derived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: String,
    pub origin: String,
    pub path: String,
    pub method: String,
    pub identity: String,
    pub state: State,
    pub sources: Vec<Source>,
    /// Message ids, oldest first. The evidence a websec replay picks up.
    pub evidence: Vec<String>,
    pub params: Vec<Param>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The ledger line this endpoint last changed on, so a caller can ask for
    /// what is new without re-reading the inventory.
    pub line: u64,
}

/// The most evidence any one endpoint keeps, so a loop cannot grow a row
/// without bound.
const MAX_EVIDENCE: usize = 64;

impl Endpoint {
    fn from(obs: Observation, line: u64) -> Self {
        Self {
            id: obs.id,
            origin: obs.origin,
            path: obs.path,
            method: obs.method,
            identity: obs.identity,
            state: obs.state,
            sources: vec![obs.source],
            evidence: obs.req.into_iter().collect(),
            params: obs.params,
            status: obs.status,
            cluster: obs.cluster,
            reason: obs.reason,
            first_seen: obs.at.clone(),
            last_seen: obs.at,
            notes: obs.note.into_iter().collect(),
            line,
        }
    }

    fn absorb(&mut self, obs: Observation, line: u64) {
        if obs.state.advances_over(self.state) {
            self.state = obs.state;
            if obs.status.is_some() {
                self.status = obs.status;
            }
            if obs.cluster.is_some() {
                self.cluster = obs.cluster;
            }
            self.reason = obs.reason;
        }
        if !self.sources.contains(&obs.source) {
            self.sources.push(obs.source);
        }
        if let Some(req) = obs.req
            && !self.evidence.contains(&req)
        {
            if self.evidence.len() == MAX_EVIDENCE {
                self.evidence.remove(0);
            }
            self.evidence.push(req);
        }
        for param in obs.params {
            if !self.params.contains(&param) {
                self.params.push(param);
            }
        }
        if let Some(note) = obs.note {
            self.notes.push(note);
        }
        if obs.at < self.first_seen {
            self.first_seen = obs.at.clone();
        }
        if obs.at > self.last_seen {
            self.last_seen = obs.at;
        }
        self.line = line;
    }
}

/// The folded ledger.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub endpoints: Vec<Endpoint>,
    /// Lines folded. Hand it back as `--since` to be told only what changed.
    pub cursor: u64,
    /// Lines that were not readable as an observation. Counted rather than
    /// raised: a truncated last line is what a killed job leaves behind.
    pub unreadable: u64,
    /// Whether the read stopped at [`MAX_READ_BYTES`].
    pub truncated: bool,
}

/// How much of the session's own evidence this ledger has already absorbed.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Progress {
    /// The highest receipt `seq` folded, or `None` when none has been. Not a
    /// bare number: receipts start at zero.
    #[serde(default)]
    pub receipts_through: Option<u64>,
}

/// A session's ledger.
pub struct Ledger {
    path: PathBuf,
    state: PathBuf,
}

impl Ledger {
    /// Open, or create, the ledger under a session directory.
    pub fn open(session_dir: &Path) -> Result<Self, H5iError> {
        let dir = session_dir.join(RECON_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| H5iError::with_path(e, &dir))?;
        owner_only(&dir);
        Ok(Self {
            path: dir.join(LEDGER_FILE),
            state: dir.join(STATE_FILE),
        })
    }

    /// A ledger addressed directly, for a reader that already has the path.
    pub fn at(path: &Path) -> Self {
        Self {
            state: path.with_file_name(STATE_FILE),
            path: path.to_path_buf(),
        }
    }

    /// How far the request log has been folded. A missing or unreadable state
    /// file means "from the beginning", which costs a re-fold and loses
    /// nothing: the ledger folds duplicates away.
    pub fn progress(&self) -> Progress {
        std::fs::read_to_string(&self.state)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn set_progress(&self, progress: &Progress) -> Result<(), H5iError> {
        let text = serde_json::to_string(progress).map_err(H5iError::Serialization)?;
        let mut file = open_owner_only_truncating(&self.state)?;
        file.write_all(text.as_bytes())
            .map_err(|e| H5iError::with_path(e, &self.state))
    }

    /// Fold this session's request log in, and remember how far it got.
    ///
    /// Called by every verb that reads the ledger, so an inventory never
    /// disagrees with the receipts beside it.
    pub fn sync_receipts(
        &self,
        records: &[h5i_wire::record::RequestRecord],
        identity: &str,
    ) -> Result<usize, H5iError> {
        let progress = self.progress();
        let ingested = crate::ingest::from_receipts(records, identity, progress.receipts_through);
        if ingested.observations.is_empty() {
            return Ok(0);
        }
        let written = self.append(&ingested.observations)?;
        self.set_progress(&Progress {
            receipts_through: ingested.through_seq,
        })?;
        Ok(written)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append observations. Returns how many were written.
    ///
    /// Oversized rows are dropped and reported rather than written, and the
    /// caller is told the difference between what it offered and what landed.
    pub fn append(&self, observations: &[Observation]) -> Result<usize, H5iError> {
        let kept: Vec<&Observation> = observations.iter().filter(|o| o.is_bounded()).collect();
        if kept.is_empty() {
            return Ok(0);
        }
        let mut file = open_owner_only(&self.path)?;
        let mut buffer = String::new();
        for obs in &kept {
            let line = serde_json::to_string(obs).map_err(H5iError::Serialization)?;
            buffer.push_str(&line);
            buffer.push('\n');
        }
        file.write_all(buffer.as_bytes())
            .map_err(|e| H5iError::with_path(e, &self.path))?;
        Ok(kept.len())
    }

    /// Fold the ledger into an inventory.
    ///
    /// Every field in here came from a target. Unreadable lines are counted,
    /// never guessed at, and the read is capped.
    pub fn read(&self) -> Result<Inventory, H5iError> {
        let mut inventory = Inventory::default();
        let text = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(inventory),
            Err(e) => return Err(H5iError::with_path(e, &self.path)),
        };
        let text = if text.len() as u64 > MAX_READ_BYTES {
            inventory.truncated = true;
            &text[..MAX_READ_BYTES as usize]
        } else {
            &text[..]
        };
        let text = String::from_utf8_lossy(text);

        let mut folded: BTreeMap<String, Endpoint> = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let line_number = index as u64 + 1;
            inventory.cursor = line_number;
            let Ok(obs) = serde_json::from_str::<Observation>(line) else {
                inventory.unreadable += 1;
                continue;
            };
            if !obs.is_bounded() || obs.id != endpoint_id(&obs.origin, &obs.path, &obs.method, &obs.identity) {
                // An id that does not name its own key is a row nothing wrote:
                // fold it and two endpoints would share a name.
                inventory.unreadable += 1;
                continue;
            }
            match folded.get_mut(&obs.id) {
                Some(existing) => existing.absorb(obs, line_number),
                None => {
                    folded.insert(obs.id.clone(), Endpoint::from(obs, line_number));
                }
            }
        }
        inventory.endpoints = folded.into_values().collect();
        inventory
            .endpoints
            .sort_by(|a, b| (&a.origin, &a.path, &a.method).cmp(&(&b.origin, &b.path, &b.method)));
        Ok(inventory)
    }
}

/// The state file is rewritten rather than appended to, and it holds no
/// evidence, but it sits beside the ledger and keeps the same mode.
fn open_owner_only_truncating(path: &Path) -> Result<File, H5iError> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| H5iError::with_path(e, path))
}

fn owner_only(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// The ledger carries a target's URLs, and a URL carries session tokens often
/// enough. Same rule the message store follows: owner-only from creation.
fn open_owner_only(path: &Path) -> Result<File, H5iError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|e| H5iError::with_path(e, path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(dir.path()).expect("open");
        (dir, ledger)
    }

    fn candidate(path: &str) -> Observation {
        Observation::new(
            "https://target.test",
            path,
            "GET",
            "anonymous",
            State::Candidate,
            Source::Script {
                req: "req_7".to_string(),
            },
        )
    }

    fn observed(path: &str, identity: &str) -> Observation {
        Observation::new(
            "https://target.test",
            path,
            "GET",
            identity,
            State::Observed,
            Source::Receipt {
                req: "req_9".to_string(),
            },
        )
        .with_req("req_9")
        .with_status(Some(200))
    }

    #[test]
    fn a_candidate_never_overwrites_something_that_was_actually_sent() {
        let (_dir, ledger) = ledger();
        ledger
            .append(&[observed("/admin", "anonymous")])
            .expect("append");
        ledger.append(&[candidate("/admin")]).expect("append");

        let inventory = ledger.read().expect("read");
        assert_eq!(inventory.endpoints.len(), 1);
        let endpoint = &inventory.endpoints[0];
        assert_eq!(
            endpoint.state,
            State::Observed,
            "a bundle mentioning a URL again is not evidence it stopped answering"
        );
        assert_eq!(endpoint.sources.len(), 2, "both disclosures are kept");
        assert_eq!(endpoint.evidence, vec!["req_9".to_string()]);
    }

    #[test]
    fn the_same_url_under_two_identities_is_two_endpoints() {
        let (_dir, ledger) = ledger();
        ledger
            .append(&[observed("/account", "anonymous"), observed("/account", "alice")])
            .expect("append");

        let inventory = ledger.read().expect("read");
        assert_eq!(
            inventory.endpoints.len(),
            2,
            "identity is in the key, so the anonymous and logged-in answers are two observations"
        );
        assert_ne!(inventory.endpoints[0].id, inventory.endpoints[1].id);
    }

    #[test]
    fn confirmation_and_refusal_carry_their_reason_forward() {
        let (_dir, ledger) = ledger();
        let refused = Observation::new(
            "https://elsewhere.test",
            "/",
            "GET",
            "anonymous",
            State::Candidate,
            Source::Page {
                req: "req_1".to_string(),
            },
        )
        .refused("origin `https://elsewhere.test` is not in the allowlist");
        ledger.append(&[refused]).expect("append");

        let inventory = ledger.read().expect("read");
        let endpoint = &inventory.endpoints[0];
        assert_eq!(endpoint.state, State::Refused);
        assert!(
            endpoint.reason.as_deref().unwrap().contains("allowlist"),
            "a refusal without its reason is a gap in the map nobody can explain"
        );
    }

    #[test]
    fn evidence_accumulates_without_growing_without_bound() {
        let (_dir, ledger) = ledger();
        for n in 0..(MAX_EVIDENCE + 10) {
            let obs = observed("/api/users", "alice").with_req(format!("req_{n}"));
            ledger.append(&[obs]).expect("append");
        }
        let inventory = ledger.read().expect("read");
        let endpoint = &inventory.endpoints[0];
        assert_eq!(endpoint.evidence.len(), MAX_EVIDENCE);
        assert_eq!(
            endpoint.evidence.last().unwrap(),
            &format!("req_{}", MAX_EVIDENCE + 9),
            "the newest evidence is the one a replay wants"
        );
    }

    #[test]
    fn params_are_unioned_with_where_they_lived() {
        let (_dir, ledger) = ledger();
        ledger
            .append(&[observed("/search", "anonymous").with_params(vec![Param {
                name: "q".to_string(),
                at: Where::Query,
            }])])
            .expect("append");
        ledger
            .append(&[observed("/search", "anonymous").with_params(vec![
                Param {
                    name: "q".to_string(),
                    at: Where::Query,
                },
                Param {
                    name: "q".to_string(),
                    at: Where::Form,
                },
            ])])
            .expect("append");

        let params = &ledger.read().expect("read").endpoints[0].params;
        assert_eq!(params.len(), 2, "the same name in two places is two facts");
    }

    #[test]
    fn a_line_that_cannot_be_read_is_counted_rather_than_fatal() {
        let (_dir, ledger) = ledger();
        ledger.append(&[observed("/ok", "anonymous")]).expect("append");
        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger.path())
            .expect("open");
        // What a killed job leaves behind, and what a target could write into
        // a boxed session's directory.
        file.write_all(b"{\"id\":\"ep_\",\"trunca").expect("write");

        let inventory = ledger.read().expect("read");
        assert_eq!(inventory.endpoints.len(), 1);
        assert_eq!(inventory.unreadable, 1);
    }

    #[test]
    fn a_row_whose_id_does_not_name_its_own_key_is_refused() {
        let (_dir, ledger) = ledger();
        let mut forged = observed("/admin", "anonymous");
        forged.id = endpoint_id("https://target.test", "/harmless", "GET", "anonymous");
        let line = serde_json::to_string(&forged).expect("json");
        std::fs::write(ledger.path(), format!("{line}\n")).expect("write");

        let inventory = ledger.read().expect("read");
        assert!(
            inventory.endpoints.is_empty(),
            "an id that does not hash its own key would let two endpoints share a name"
        );
        assert_eq!(inventory.unreadable, 1);
    }

    #[test]
    fn an_oversized_row_is_refused_and_reported() {
        let (_dir, ledger) = ledger();
        let huge = candidate(&"/a".repeat(MAX_FIELD_BYTES));
        let written = ledger
            .append(&[huge, candidate("/small")])
            .expect("append");
        assert_eq!(written, 1, "the caller is told what landed, not just that something did");
        assert_eq!(ledger.read().expect("read").endpoints.len(), 1);
    }

    #[test]
    fn the_cursor_names_what_a_next_turn_has_not_seen() {
        let (_dir, ledger) = ledger();
        ledger.append(&[candidate("/one")]).expect("append");
        let first = ledger.read().expect("read");
        assert_eq!(first.cursor, 1);

        ledger.append(&[candidate("/two")]).expect("append");
        let second = ledger.read().expect("read");
        assert_eq!(second.cursor, 2);
        let fresh: Vec<_> = second
            .endpoints
            .iter()
            .filter(|e| e.line > first.cursor)
            .collect();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].path, "/two");
    }

    #[cfg(unix)]
    #[test]
    fn the_ledger_is_owner_only_because_a_url_carries_tokens() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, ledger) = ledger();
        ledger.append(&[candidate("/x")]).expect("append");

        let file = std::fs::metadata(ledger.path()).expect("stat").permissions();
        assert_eq!(file.mode() & 0o777, 0o600);
        let recon = std::fs::metadata(dir.path().join(RECON_DIR))
            .expect("stat")
            .permissions();
        assert_eq!(recon.mode() & 0o777, 0o700);
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use h5i_wire::record::{Initiator, RequestRecord};

    #[test]
    fn syncing_twice_folds_each_receipt_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(dir.path()).expect("open");

        let request = RequestRecord::request(1, Initiator::Navigation, "GET", "https://t.test/a");
        let mut response = request.response();
        response.status = Some(200);
        let records = vec![request, response];

        assert_eq!(ledger.sync_receipts(&records, "anonymous").expect("sync"), 1);
        assert_eq!(
            ledger.sync_receipts(&records, "anonymous").expect("sync"),
            0,
            "a second read of the same log must not write the same observation again"
        );
        let inventory = ledger.read().expect("read");
        assert_eq!(inventory.endpoints.len(), 1);
        assert_eq!(inventory.cursor, 1);
        assert_eq!(ledger.progress().receipts_through, Some(1));
    }
}
