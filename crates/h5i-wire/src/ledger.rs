//! The endpoint ledger's shapes, and the fold that turns a log of observations
//! into an inventory.
//!
//! Here rather than in the plugin because three programs read a ledger: recon,
//! which writes it, the console, which shows it, and whatever comes next. The
//! file IO stays with recon (design-recon.md N5).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// Whether `self` may overwrite `prior`. Two rules, and both are the
    /// ledger's discipline: a disclosure never overwrites evidence, and a plain
    /// sighting never undoes a verdict triage reached against a baseline.
    pub fn advances_over(self, prior: State) -> bool {
        match (self, prior) {
            (State::Candidate, State::Candidate) => true,
            (State::Candidate, _) => false,
            (State::Observed, State::Confirmed | State::Gone) => false,
            _ => true,
        }
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
    /// A path recon asked for on purpose because it should not exist, to learn
    /// what "not there" looks like in this directory. In the ledger because it
    /// happened, and named so a reader is not left wondering why the inventory
    /// holds a path nobody would have.
    Calibration { req: String },
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
            at: crate::record::now_rfc3339(),
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

    /// Whether this row can be kept: bounded, and saying only what it can back
    /// up. Public because the writer checks it before an append, and the fold
    /// checks it again on the way back in.
    ///
    /// A target can emit a megabyte of URL, and the ledger is a file on the
    /// operator's disk. The second half is N1's claim, enforced rather than
    /// promised: a state that means a request happened has to name the message
    /// it happened in.
    pub fn is_keepable(&self) -> bool {
        let bounded = self.origin.len() <= MAX_FIELD_BYTES
            && self.path.len() <= MAX_FIELD_BYTES
            && self.method.len() <= 64
            && self.identity.len() <= 256
            && self.params.len() <= 512;
        let backed = match self.state {
            State::Observed | State::Confirmed | State::Gone => self.req.is_some(),
            State::Candidate | State::Refused => true,
        };
        bounded && backed
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
pub const MAX_EVIDENCE: usize = 64;

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
        // The newest answer, whatever the state did: a fresh status is a fact
        // even when it changes nothing about what this endpoint is.
        if obs.status.is_some() {
            self.status = obs.status;
        }
        if obs.cluster.is_some() {
            self.cluster = obs.cluster;
        }
        if obs.state.advances_over(self.state) {
            self.state = obs.state;
            self.reason = obs.reason;
            if obs.state == State::Refused {
                // Nothing reached the wire, so the status belongs to a request
                // that is no longer the last word on this endpoint.
                self.status = None;
            }
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

/// Fold a log of observations into an inventory.
///
/// Pure: the caller reads the file, this reads the lines. A line that will not
/// parse, or whose id does not name its own key, is counted rather than
/// guessed at, because a killed run leaves a torn last line and a boxed
/// session's directory is writable by boxed code.
pub fn fold<'a>(lines: impl Iterator<Item = &'a str>) -> Inventory {
    let mut inventory = Inventory::default();
    let mut folded: BTreeMap<String, Endpoint> = BTreeMap::new();
    for (index, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_number = index as u64 + 1;
        inventory.cursor = line_number;
        let Ok(obs) = serde_json::from_str::<Observation>(line) else {
            inventory.unreadable += 1;
            continue;
        };
        if !obs.is_keepable()
            || obs.id != endpoint_id(&obs.origin, &obs.path, &obs.method, &obs.identity)
        {
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
    inventory
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(state: State, path: &str, req: Option<&str>) -> String {
        let mut obs = Observation::new(
            "https://t.test",
            path,
            "GET",
            "anonymous",
            state,
            Source::Manual,
        );
        obs.req = req.map(str::to_string);
        serde_json::to_string(&obs).expect("json")
    }

    #[test]
    fn the_fold_reads_a_log_into_endpoints() {
        let log = [
            line(State::Candidate, "/a", None),
            line(State::Observed, "/a", Some("req_1")),
            line(State::Candidate, "/b", None),
        ]
        .join("\n");
        let inventory = fold(log.lines());
        assert_eq!(inventory.endpoints.len(), 2);
        assert_eq!(inventory.cursor, 3);
        assert_eq!(inventory.endpoints[0].state, State::Observed);
        assert_eq!(inventory.endpoints[0].evidence, vec!["req_1".to_string()]);
    }

    #[test]
    fn a_line_the_fold_cannot_read_is_counted_rather_than_guessed_at() {
        // A torn last line is what a killed run leaves, and a boxed session's
        // directory is writable by boxed code.
        let log = format!("{}\n{{\"id\":\"ep_\",\"trunc", line(State::Candidate, "/a", None));
        let inventory = fold(log.lines());
        assert_eq!(inventory.endpoints.len(), 1);
        assert_eq!(inventory.unreadable, 1);
    }

    #[test]
    fn a_state_that_claims_a_request_without_naming_one_is_refused() {
        let inventory = fold(line(State::Observed, "/a", None).lines());
        assert!(inventory.endpoints.is_empty());
        assert_eq!(inventory.unreadable, 1);
    }
}
