//! The broker, as a named set of operations rather than a struct.

use std::sync::Arc;

use url::Url;

use crate::budget::{Limits, Spent};
use crate::cors::{Credentials, Mode};
use crate::receipt::{Initiator, RequestRecord};
use crate::secrets::Resolved;
use crate::wsclient::Event;

pub use crate::net::FetchOutcome;

/// One request, with everything the broker needs to decide about it.
///
/// A struct rather than eight arguments because it crosses a process boundary:
/// what the broker is told is exactly this, written down in one place, and a
/// caller cannot add a field the far side does not know how to police.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Fetch {
    pub url: Url,
    pub initiator: Initiator,
    pub method: String,
    /// Empty for a GET. Carried out of band on the wire, never in the JSON.
    #[serde(skip)]
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    /// Headers the *caller* is setting, beyond the ones the engine adds.
    ///
    /// Empty for every fetch a page makes: a page's headers arrive in
    /// [`CorsAsk`] and are subject to the same-origin rules, which is a
    /// different question from an agent naming a header on a request of its
    /// own. This is the agent's, and the agent is the principal.
    ///
    /// Not a free hand. Three are the client's to compute and are refused here
    /// however they are spelled (`content-length`, `transfer-encoding`,
    /// `connection`), because a message whose framing disagrees with its body is
    /// not a request this engine can honestly claim to have sent. The refusal is
    /// named in the receipt rather than performed silently. Everything else,
    /// `host` and `authorization` and `cookie` included, is carried: overriding
    /// those is the test, not an accident.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// The document that asked, when a document did.
    ///
    /// Load-bearing rather than bookkeeping: without it the policy reads every
    /// subresource as the agent naming a URL, and a page from the open web
    /// reaches the box's dev server. See [`crate::policy::Policy::check_from`].
    pub document: Option<Url>,
    /// How many redirect hops this request may follow, when the caller wants a
    /// different answer from the session's.
    ///
    /// `None` is the policy's limit, which is what every page load uses. `Some(0)`
    /// stops at the first `Location` and hands it back unfollowed, which is the
    /// only way to see a redirect *as a message*: where an authentication flow
    /// sends you, what an open redirect accepts, which `Set-Cookie` rides on the
    /// 302. Following it silently is right for a browser and wrong for a test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_redirects: Option<usize>,
    /// Set when a *page* asked, which is what subjects the answer to the
    /// same-origin policy. `None` is the agent exercising its own authority
    /// over a URL it named, which is a different question. See [`crate::cors`].
    pub cors: Option<CorsAsk>,
}

impl Fetch {
    /// A URL the agent named. No document, so the loopback rule treats this as
    /// an instruction rather than as something a page reached for.
    pub fn get(url: &Url, initiator: Initiator) -> Self {
        Self {
            url: url.clone(),
            initiator,
            method: "GET".to_string(),
            body: Vec::new(),
            content_type: None,
            headers: Vec::new(),
            max_redirects: None,
            document: None,
            cors: None,
        }
    }

    /// The headers the caller is setting.
    ///
    /// Order is kept, because a server that treats two headers of one name as a
    /// list is a server whose answer depends on which came first, and a
    /// workbench that reordered them would be reproducing something other than
    /// what it was told to send.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// The document that asked for this, for the origin the policy reasons
    /// about.
    pub fn from_document(mut self, document: Option<&Url>) -> Self {
        self.document = document.cloned();
        self
    }

    pub fn with_body(mut self, method: &str, body: &[u8], content_type: Option<&str>) -> Self {
        self.method = method.to_string();
        self.body = body.to_vec();
        self.content_type = content_type.map(str::to_string);
        self
    }
}

/// What a page's own `fetch` brings with it, and nothing an agent's navigation
/// has.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorsAsk {
    /// The document whose origin is asking.
    pub document: Url,
    pub headers: Vec<(String, String)>,
    pub mode: Mode,
    pub credentials: Credentials,
}

/// The whole request log, in the three numbers a windowed read still needs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogSummary {
    /// Every record, both phases, over the life of the session.
    pub total: usize,
    /// Requests the policy refused.
    pub denied: usize,
    /// The highest sequence recorded, over both phases.
    pub highest: Option<u64>,
    /// The cursor to pass back as `since`.
    ///
    /// Not [`Self::highest`]: a request and its response share a sequence
    /// number and the request half is written *before* the wire, so a poll
    /// during a fetch took that number and never saw the response half. This
    /// stops below anything in flight, so the worst it does is repeat a row.
    pub cursor: Option<u64>,
}

/// A stored request, sent again with changes.
///
/// The whole operation is one call, and that is the design rather than an
/// accident of convenience. The stored request holds the credential it was sent
/// with, so reading it, editing it and sending it all happen in the broker, and
/// the renderer never holds an `Authorization` header it could not otherwise
/// read. Splitting this into "fetch the stored request" and "send this request"
/// would hand the untrusted half exactly what the jar is kept from it to protect.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Edited {
    /// The receipt sequence the replay was recorded under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Each edit, as it was applied.
    pub applied: Vec<crate::edits::Applied>,
    /// One entry per send. A single replay has one; `--repeat` has that many,
    /// oldest first, and the caller reads a median off them.
    ///
    /// Repetition belongs here rather than in a shell loop because the thing
    /// being measured is milliseconds and starting a process costs tens of
    /// them: a loop outside the engine measures the loop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Timing>,
    /// The request as sent, for the record. Header *names* only: the values are
    /// in the store, which is the artifact that is allowed to hold them.
    pub sent: Sent,
    pub outcome: FetchOutcome,
}

/// A request handed to a session that never made it.
///
/// The shape cross-session replay needs: the message another session recorded,
/// carried here to be sent under *this* session's identity. The body travels
/// base64 in the JSON rather than beside it, because this one crosses a control
/// channel rather than the broker's own socket; a request body large enough for
/// that to matter is one the caller should be sending with `body.raw` from a
/// file anyway.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Given {
    pub method: String,
    pub url: Url,
    /// Headers to carry. What a caller composes, and never a credential unless
    /// the caller put one here on purpose: see `h5i browser resend --as`.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Vec<u8>,
}

/// A request written directly to the socket, without URL parsing.
pub struct RawRequest {
    /// The authority used for policy checks and dialing. Its path is ignored.
    pub url: Url,
    /// The method and target recorded in receipts. `wire` contains the sent values.
    pub method: String,
    pub target: String,
    /// The complete request bytes, written unchanged.
    pub wire: Vec<u8>,
    /// The body offset within `wire`.
    pub body_at: usize,
    /// Parsed headers for capture and receipts.
    pub headers: Vec<(String, String)>,
    /// Deliberately broken framing invariants recorded in the receipt.
    pub broke: Vec<String>,
}

/// Why a replay did not happen, in a form a script can branch on.
///
/// A code as well as a sentence, because these are four different situations
/// with four different fixes and a caller that can only match on prose will
/// eventually match on the wrong thing. The sentence is for a person; the code
/// is the contract.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SendError {
    /// `no-capture`, `no-such-request`, `unreplayable-body` or `bad-edit`.
    pub code: String,
    pub message: String,
}

impl SendError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// How many times to send it, and whether at once.
///
/// Two different questions wearing one shape. `count` sends in a row answers
/// "is this reliably slower", which is a timing test. `count` sends at the same
/// instant answers "does this application check and then act", which is a race.
/// The same request, the same receipts, and nothing else in common.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Sends {
    /// How many. Always at least one.
    pub count: u32,
    /// Stop at the first redirect instead of following it.
    ///
    /// A browser follows; a test often wants the 302 itself. See
    /// [`Fetch::max_redirects`].
    #[serde(default)]
    pub no_follow: bool,
    /// Released together rather than one after another.
    ///
    /// A burst, and named as one. Every request is written and flushed at
    /// roughly the same moment from `count` threads that meet at a barrier
    /// first; it is not a single-packet attack, which needs the request split
    /// across two writes and the last byte of each held back. What it does
    /// reach is the ordinary check-then-act window, which is where nearly every
    /// real one lives.
    pub together: bool,
    /// A request-target to write unchanged. `None` uses the normal sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_target: Option<String>,
    /// A complete request to write unchanged. `None` uses the normal sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_request: Option<Vec<u8>>,
    /// Send once per target value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub each: Option<Each>,
    /// Preserve header-name casing via the raw sender.
    #[serde(default)]
    pub raw_headers: bool,
    /// Sends per second, at most. `None` sends as fast as the wire allows.
    ///
    /// Not a courtesy. A walk of a thousand values is the one verb here that
    /// can look like an attack to whoever is watching the target, and an
    /// authorised engagement usually comes with a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
}

impl Default for Sends {
    fn default() -> Self {
        Self {
            count: 1,
            together: false,
            no_follow: false,
            raw_target: None,
            raw_headers: false,
            each: None,
            raw_request: None,
            rate: None,
        }
    }
}

impl Sends {
    pub fn once() -> Self {
        Self::default()
    }
}

/// One send's timing and result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Timing {
    pub seq: Option<u64>,
    pub status: Option<u16>,
    /// Enumeration value, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// To the response's headers: the server's decision.
    pub ttfb_ms: u64,
    /// To the body in hand: the decision plus the transfer.
    pub total_ms: u64,
    pub bytes: u64,
    /// Bounded body preview; capture retains the full body.
    #[serde(default)]
    pub body_preview: String,
    /// Whether [`Self::body_preview`] omits trailing bytes.
    #[serde(default)]
    pub body_truncated: bool,
}

pub const BODY_PREVIEW_BYTES: usize = 4096;

pub fn body_preview(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(BODY_PREVIEW_BYTES)]).into_owned()
}

/// One send of a walk: the edits that make it different, and what to call it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Step {
    /// What names this send in the samples. The value, when one target is
    /// walked; the whole combination, when several are.
    pub label: String,
    /// `target=value` edits, applied after the shared ones.
    pub set: Vec<String>,
}

/// A walk: one send per step, in order.
///
/// One shape for one target and for several, because the sender's job is the
/// same either way and two paths through it would drift.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Each {
    pub steps: Vec<Step>,
}

impl Each {
    /// The one-target form: `query.id` over a file of values.
    pub fn over(target: &str, values: Vec<String>) -> Self {
        Self {
            steps: values
                .into_iter()
                .map(|value| Step {
                    set: vec![format!("{target}={value}")],
                    label: value,
                })
                .collect(),
        }
    }
}

/// What went out, in the parts that are safe to hand back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Sent {
    pub method: String,
    pub url: String,
    pub header_names: Vec<String>,
    pub body_bytes: u64,
}

/// What a page has spent, and what it was allowed.
///
/// A reading rather than a live handle. `budget()` used to hand back a
/// `&Budget` the caller could poll; across a process boundary there is nothing
/// to borrow, so the answer is a value taken at one moment and named as one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Allowance {
    pub spent: Spent,
    pub limits: Limits,
}

/// A long-lived connection, from the side that does not own it.
pub trait Channel: Send + Sync {
    /// Send a text frame. `Err` for a stream that cannot be written to, which
    /// is what a server-sent event stream is.
    fn send(&self, text: &str) -> Result<(), String>;
    /// Everything that has arrived since the last drain.
    fn drain(&self) -> Vec<Event>;
    /// Stop. Idempotent, because the page may close a socket the peer already
    /// did.
    fn close(&self);
}

/// The one way bytes enter this engine.
pub trait Broker: Send + Sync {
    /// Check policy, record the decision, use the wire. In that order, and
    /// with no second path. Every other fetch entry point on this trait is a
    /// convenience over this one.
    fn send(&self, fetch: &Fetch) -> FetchOutcome;

    /// The same send, with `while_waiting` run while the answer is in flight.
    fn send_while(&self, fetch: &Fetch, while_waiting: &mut dyn FnMut()) -> FetchOutcome {
        let _ = while_waiting;
        self.send(fetch)
    }

    /// Send a stored request again, with the caller's changes.
    ///
    /// `Err` for a request that is not in the store, or an edit that cannot
    /// apply. A refusal by policy is not an error here: it is an outcome, with
    /// a receipt, like every other refused fetch.
    fn send_edited(
        &self,
        _from: u64,
        _edits: &[crate::edits::Edit],
        _create: bool,
        _plan: Sends,
    ) -> Result<Edited, SendError> {
        Err(SendError::new(
            "no-capture",
            "this session was not opened with `--capture`, so it has no stored request to \
             send again. Open it with `--capture` and the messages it makes will be replayable",
        ))
    }

    /// Send a request this session did not make.
    ///
    /// The cross-session half of replay: one session's stored request, sent
    /// under another's cookies, identity and policy. Everything that makes a
    /// request *this session's* comes from here rather than from the message,
    /// which is the whole point of the verb.
    fn send_given(
        &self,
        _request: Given,
        _edits: &[crate::edits::Edit],
        _create: bool,
        _plan: Sends,
    ) -> Result<Edited, SendError> {
        Err(SendError::new(
            "not-supported",
            "this broker cannot send a composed request",
        ))
    }

    /// Send exact request bytes after the usual policy, budget, and receipt checks.
    fn send_raw(&self, _request: &RawRequest) -> FetchOutcome {
        FetchOutcome::failed(
            _request.url.clone(),
            "this broker cannot send a raw request".to_string(),
        )
    }

    /// What this session's message store has done, when it has one.
    ///
    /// `None` for the ordinary session, which stores no message and so has no
    /// health to report. A store that exists always answers, including when
    /// what it has to say is that it dropped something: an evidence gap nobody
    /// surfaces is an evidence gap nobody knows to distrust.
    fn capture(&self) -> Option<crate::capture::Health> {
        None
    }

    /// The requests this broker has decided about, in the order it recorded
    /// them.
    ///
    /// The renderer displays these; it does not own them. The authoritative copy
    /// is the broker's own sink and the file it writes, which is the distinction
    /// worth keeping in mind when the renderer prints a table: a compromised
    /// renderer holds the terminal and can print whatever it likes, but it
    /// cannot change what was written.
    fn records(&self) -> Vec<RequestRecord>;

    /// The highest sequence recorded so far, or `None` on an empty log. The
    /// mark a verb takes before it runs.
    fn high_water(&self) -> Option<u64> {
        self.records().iter().map(|r| r.seq).max()
    }

    /// The records after `mark`, which is what a reader polling for what
    /// changed actually wants.
    ///
    /// Separate from [`Self::records`] because this crosses a process boundary:
    /// answering "what happened since sequence 4000" by shipping four thousand
    /// records and throwing them away is the shape the `since` cursor exists to
    /// avoid, and it would grow with the session rather than with the answer.
    fn records_since(&self, mark: Option<u64>) -> Vec<RequestRecord> {
        self.records()
            .into_iter()
            .filter(|r| mark.is_none_or(|floor| r.seq > floor))
            .collect()
    }

    /// The whole log in three numbers.
    ///
    /// The counts a windowed read still has to report: "nothing was refused" is
    /// a claim about the session rather than about the window, and an agent
    /// that only ever asks for windows should still be able to make it. Three
    /// numbers rather than the log they are derived from, for the same reason
    /// [`Self::records_since`] exists.
    fn log_summary(&self) -> LogSummary {
        let records = self.records();
        // The lowest sequence whose pair is not finished. Everything below it
        // is complete and safe to move a cursor past.
        let mut answered: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for record in &records {
            if record.phase == crate::receipt::Phase::Response {
                answered.insert(record.seq);
            }
        }
        let in_flight = records
            .iter()
            .filter(|r| r.phase == crate::receipt::Phase::Request)
            .map(|r| r.seq)
            .filter(|seq| !answered.contains(seq))
            .min();
        let highest = records.iter().map(|r| r.seq).max();
        let cursor = match in_flight {
            None => highest,
            // Stop below the first request still waiting for its answer.
            Some(0) => None,
            Some(seq) => Some(seq - 1),
        };
        LogSummary {
            cursor,
            total: records.len(),
            denied: records
                .iter()
                .filter(|r| r.phase == crate::receipt::Phase::Request && !r.allowed)
                .count(),
            // The highest sequence, not the last appended: numbers are taken
            // before the append and a socket's reader thread appends
            // concurrently with the page's own fetches, so append order and
            // sequence order can differ.
            highest,
        }
    }

    /// Sequence numbers recorded after `mark`, deduplicated and in order.
    fn since(&self, mark: Option<u64>) -> Vec<u64> {
        let mut seen: Vec<u64> = self
            .records()
            .iter()
            .map(|r| r.seq)
            .filter(|seq| mark.is_none_or(|floor| *seq > floor))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// What this page has spent, and what it may.
    fn budget(&self) -> Allowance;

    /// A navigation is starting, so the page's allowance starts again. See
    /// [`crate::budget`] for why the ceiling bounds a page rather than a
    /// session.
    fn reset_budget(&self);

    /// How many cookies the session holds. A count, never a value.
    fn cookie_count(&self) -> usize;

    /// What `document.cookie` may see: the non-`HttpOnly` subset for this URL.
    fn document_cookie(&self, url: &Url) -> String;

    /// Store a cookie script set, subject to the same rules the wire path
    /// applies, and to one more, since script may not set `HttpOnly`.
    /// Returns how many were stored.
    fn store_cookie(&self, url: &Url, header: &str) -> usize;

    /// Leaving an origin drops every other origin's cookies. `true` when
    /// something was dropped, which the page reports as a note.
    fn keep_only_origin(&self, origin: &Url) -> bool;

    /// Authorise, record, and dial a WebSocket.
    fn open_socket(&self, url: &Url, document: Option<&Url>) -> Result<Arc<dyn Channel>, String>;

    /// The same, for an event stream.
    fn open_event_stream(
        &self,
        url: &Url,
        document: Option<&Url>,
    ) -> Result<Arc<dyn Channel>, String>;

    /// The credentials this session may substitute, by name.
    #[cfg(feature = "identity")]
    fn identity(&self) -> std::sync::Arc<crate::identity::Identity>;

    fn secret_names(&self) -> Vec<String>;

    /// Resolve `$H5I_SECRET_*` placeholders on the way into a field.
    ///
    /// The limit is worth stating where the operation is declared, because it is
    /// easy to oversell: substitution happens on the way *into* the page, so the
    /// renderer receives the value for the credential that was actually used.
    /// What the split protects is every credential that was not.
    fn substitute(&self, text: &str) -> Resolved;

    /// Put the placeholder back wherever a value appears in outgoing text.
    fn redact(&self, text: &str) -> String;

    /// The same, for every string in one reply.
    ///
    /// A batch rather than a loop over [`Self::redact`], because a reply is a
    /// tree with hundreds of strings in it and this crosses a process boundary:
    /// one round trip per string would make redaction the most expensive thing
    /// the control channel does. The default is the loop, for the
    /// implementation where a round trip is a function call.
    fn redact_all(&self, texts: &[String]) -> Vec<String> {
        texts.iter().map(|text| self.redact(text)).collect()
    }

    /// Whether this broker holds anything redaction would replace.
    ///
    /// So a caller can skip the traversal rather than walk a reply, copy every
    /// string out of it, and rebuild it unchanged. A snapshot reply is tens of
    /// kilobytes across thousands of fields, and a session with no credentials
    /// in it is the common case.
    ///
    /// Defaults to `true`, which is the safe answer: an implementation that has
    /// not thought about this gets the full pass rather than a silent skip. A
    /// wrong `false` here is a leaked credential, so it is only ever returned
    /// by a broker that can enumerate what it holds.
    fn has_redactions(&self) -> bool {
        true
    }

    // ── convenience over `send` ─────────────────────────────────────────────
    //
    // Kept as default methods rather than as free functions so the call sites
    // read the way they did before the trait existed, and so there is exactly
    // one place each of them turns into a `Fetch`.

    /// Fetch a URL the agent named.
    fn fetch(&self, url: &Url, initiator: Initiator) -> FetchOutcome {
        self.send(&Fetch::get(url, initiator))
    }

    /// The same fetch, with speculative work run while it is in flight. See
    /// [`Self::send_while`] for what may go in the closure.
    fn fetch_while(
        &self,
        url: &Url,
        initiator: Initiator,
        while_waiting: &mut dyn FnMut(),
    ) -> FetchOutcome {
        self.send_while(&Fetch::get(url, initiator), while_waiting)
    }

    /// Fetch something a *document* asked for.
    fn fetch_from(&self, url: &Url, initiator: Initiator, document: Option<&Url>) -> FetchOutcome {
        self.send(&Fetch::get(url, initiator).from_document(document))
    }

    /// Send a request that may carry a body. What a form submission needs.
    fn send_from(
        &self,
        url: &Url,
        initiator: Initiator,
        method: &str,
        body: &[u8],
        content_type: Option<&str>,
        document: Option<&Url>,
    ) -> FetchOutcome {
        self.send(
            &Fetch::get(url, initiator)
                .with_body(method, body, content_type)
                .from_document(document),
        )
    }

    /// The same send, subject to the same-origin policy: a *page* exercising an
    /// authority it has to be granted.
    #[allow(clippy::too_many_arguments)]
    fn send_script(
        &self,
        url: &Url,
        method: &str,
        body: &[u8],
        content_type: Option<&str>,
        document: &Url,
        headers: &[(String, String)],
        mode: Mode,
        credentials: Credentials,
    ) -> FetchOutcome {
        let mut fetch = Fetch::get(url, Initiator::Subresource)
            .with_body(method, body, content_type)
            .from_document(Some(document));
        fetch.cors = Some(CorsAsk {
            document: document.clone(),
            headers: headers.to_vec(),
            mode,
            credentials,
        });
        self.send(&fetch)
    }
}
