//! The box console: `h5i ui`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Serialize;
use serde_json::Value;

use crate::browser_session as bs;
use crate::env::{self, EnvEvent, EnvManifest, LiveSession, ServiceStatus};
use crate::error::H5iError;
use crate::receipt::ExecRecord;
use crate::sandbox::Profile;

/// The cookie the page uses after it has spent the token in the URL.
const COOKIE: &str = "h5i_console";

/// 128 bits, hex. The same budget [`crate::view`] mints for a box viewer.
const TOKEN_BYTES: usize = 16;

/// Newest-first cap on the receipts carried in one box's detail payload. A
/// long-lived box accumulates thousands; the console shows the working set and
/// says how many it folded rather than paging silently.
const RECEIPT_CAP: usize = 400;

/// Distinct refused destinations carried in a fleet row.
const DENIED_HOSTS_CAP: usize = 8;

#[derive(Clone)]
pub struct AppState {
    pub repo_path: PathBuf,
    /// This session's token. Never written to disk.
    pub token: String,
    /// A second token, minted only when the console is asked to hand itself to
    /// a browser, and spent by the first request that presents it.
    ///
    /// `--open` puts a URL on another program's command line, and on Linux
    /// `/proc/<pid>/cmdline` is world-readable, so the session token would sit
    /// there for as long as the browser process lives — readable by every other
    /// uid on the machine, which is the whole population the token exists to
    /// keep out. A one-shot is worth nothing the moment the page has loaded.
    handoff: Arc<std::sync::Mutex<Option<String>>>,
    /// One [`crate::browser_events::BoxStream`] per box the console has looked at, kept for the
    /// life of the process.
    browser: Arc<std::sync::Mutex<std::collections::HashMap<String, crate::browser_events::BoxStream>>>,
    /// One live-view reader per box currently being watched. Separate from
    /// `browser` because their lifetimes differ: the event stream is cheap and
    /// kept, while a relay holds a socket into a box and is dropped as soon as
    /// the box stops serving a view.
    frames:
        Arc<std::sync::Mutex<std::collections::HashMap<String, crate::browser_frames::FrameRelay>>>,
    /// One folded session row per session, keyed by what its files looked like
    /// when the row was made.
    ///
    /// A registry grows without bound and most of it is finished: an ended
    /// session's files never change again, so folding them once and checking a
    /// stat afterwards is the difference between a console that costs nothing
    /// and one that reads the disk every eight seconds.
    sessions: Arc<std::sync::Mutex<std::collections::HashMap<String, (SessionStamp, SessionRow)>>>,
}

/// What a session's files looked like: enough to notice a change, and nothing
/// that has to be read to find out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionStamp {
    record: (u64, Option<std::time::SystemTime>),
    receipts: (u64, Option<std::time::SystemTime>),
    ledger: (u64, Option<std::time::SystemTime>),
    /// The control lock: a handover changes what a row says and touches
    /// nothing else.
    control: (u64, Option<std::time::SystemTime>),
    jobs: Option<std::time::SystemTime>,
    messages: Option<std::time::SystemTime>,
}

fn stamp_of(path: &std::path::Path) -> (u64, Option<std::time::SystemTime>) {
    match std::fs::metadata(path) {
        Ok(meta) => (meta.len(), meta.modified().ok()),
        Err(_) => (0, None),
    }
}

fn touched(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

impl SessionStamp {
    fn of(h5i_root: &std::path::Path, id: &str) -> Self {
        let dir = bs::dir(h5i_root, id);
        Self {
            record: stamp_of(&dir.join(bs::RECORD)),
            receipts: stamp_of(&dir.join(bs::RECEIPTS_FILE)),
            ledger: stamp_of(&dir.join("recon").join("ledger.jsonl")),
            control: stamp_of(&dir.join(crate::control::CONTROL_JSON)),
            jobs: touched(&dir.join("recon").join("jobs")),
            messages: touched(&dir.join(bs::MESSAGES_DIR)),
        }
    }
}

impl AppState {
    pub fn new(repo_path: PathBuf, token: String) -> Self {
        Self {
            repo_path,
            token,
            handoff: Arc::new(std::sync::Mutex::new(None)),
            browser: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            frames: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            sessions: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// The live one-shot token, if one was minted and not yet spent.
    fn handoff(&self) -> Option<String> {
        self.handoff.lock().ok().and_then(|h| h.clone())
    }

    /// Spend it. Idempotent, and safe to call when there is none.
    fn spend_handoff(&self) {
        if let Ok(mut h) = self.handoff.lock() {
            *h = None;
        }
    }
}

// ── the gate ─────────────────────────────────────────────────────────────────

/// Why a request was turned away. Distinct variants because they mean very
/// different things to whoever is reading the logs: one is a missing token, the
/// other is a page that should not have been asking at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No token, or the wrong one.
    Unauthorized,
    /// An `Origin` header naming somewhere other than this console.
    ForeignOrigin,
}

impl Refusal {
    pub fn status(self) -> (StatusCode, &'static str) {
        match self {
            Refusal::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "h5i console: missing or wrong token — open the URL `h5i ui` printed",
            ),
            Refusal::ForeignOrigin => (
                StatusCode::FORBIDDEN,
                "h5i console: refused a cross-origin request",
            ),
        }
    }
}

/// The whole authorization decision, as a pure function of the four things it depends on, so it
/// can be tested without a socket.
pub fn authorize(
    query: Option<&str>,
    cookie_header: Option<&str>,
    origin: Option<&str>,
    sec_fetch_site: &[&str],
    expected: &str,
    self_origin: &str,
) -> Result<(), Refusal> {
    if let Some(o) = origin
        && o != self_origin
    {
        return Err(Refusal::ForeignOrigin);
    }
    let from_query = query.and_then(|q| param(q, "token"));
    // Foreign if *any* reading of the head says so: one value answers for
    // itself, two that disagree are a head no browser produced.
    if sec_fetch_site.iter().any(|site| {
        !matches!(site.trim(), "same-origin" | "none") && from_query.is_none()
    }) {
        return Err(Refusal::ForeignOrigin);
    }
    // A token in the query answers for itself, right or wrong: it is what the
    // operator just pasted, and falling back to the cookie behind a wrong one
    // would make a bad link look like a good one.
    if let Some(t) = from_query {
        return if token_matches(expected, &t) { Ok(()) } else { Err(Refusal::Unauthorized) };
    }
    let matched = cookie_header
        .is_some_and(|c| cookies(c, COOKIE).any(|value| token_matches(expected, value)));
    if matched { Ok(()) } else { Err(Refusal::Unauthorized) }
}

/// Compare in constant time w.r.t. content. The lengths are public (both are
/// fixed-width hex), so only the bytes are hidden.
fn token_matches(expected: &str, given: &str) -> bool {
    if expected.len() != given.len() {
        return false;
    }
    expected
        .bytes()
        .zip(given.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// First value of `key` in a `a=b&c=d` query string.
fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Every value of `name` in a `Cookie:` header.
///
/// Every, not the first. Cookies are not scoped by port, so a page served from
/// any other loopback port — a box's dev server, which is the case `h5i join`
/// exists for — can set a second `h5i_console` on a longer path. RFC 6265 §5.4
/// orders longer paths first, so reading one value let that page lock the
/// operator out of their own console until they cleared a cookie they have no
/// way to see. Reopening the printed URL did not repair it: that sets the
/// `Path=/` cookie, which is the one being shadowed.
fn cookies<'a>(header: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> {
    header.split(';').filter_map(move |kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

async fn gate(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1");
    // The Origin check below compares against our own Host header, which a
    // DNS-rebinding page satisfies trivially: it arrives as
    // `Host: evil.test:<port>` / `Origin: http://evil.test:<port>`, which match
    // each other. Requiring the Host to actually name loopback is what makes
    // "foreign origins refused" true rather than self-referential. (The token
    // cookie already held this shut; the documented property did not.)
    if !is_loopback_host(host) {
        return Refusal::ForeignOrigin.status().into_response();
    }
    let self_origin = format!("http://{host}");
    // Every value, not the first: see `authorize`. A repeated field arrives as
    // two lines through any HTTP/2-to-1.1 hop, and reading only one of them
    // lets the other say something else.
    let sites: Vec<&str> = req
        .headers()
        .get_all("sec-fetch-site")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let verdict = authorize(
        req.uri().query(),
        req.headers()
            .get(header::COOKIE)
            .and_then(|h| h.to_str().ok()),
        req.headers()
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok()),
        &sites,
        &state.token,
        &self_origin,
    );
    // The one-shot handoff gets the same head and the same rules, with a
    // different expected token, and is spent on the way through. Only an
    // *Unauthorized* falls through to it: a foreign origin is a refusal
    // whatever token it carries.
    let verdict = match verdict {
        Err(Refusal::Unauthorized) => match state.handoff() {
            Some(handoff) => {
                let second = authorize(
                    req.uri().query(),
                    req.headers()
                        .get(header::COOKIE)
                        .and_then(|h| h.to_str().ok()),
                    req.headers()
                        .get(header::ORIGIN)
                        .and_then(|h| h.to_str().ok()),
                    &sites,
                    &handoff,
                    &self_origin,
                );
                if second.is_ok() {
                    state.spend_handoff();
                }
                second
            }
            None => Err(Refusal::Unauthorized),
        },
        other => other,
    };
    match verdict {
        Ok(()) => next.run(req).await,
        Err(r) => r.status().into_response(),
    }
}

/// Does this `Host` header name the loopback interface? Port-insensitive: the
/// console binds an ephemeral port, so the name is the part worth pinning.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        // Only strip a trailing all-digit port; `[::1]` has colons of its own.
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    let name = name.trim_matches(|c| c == '[' || c == ']');
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1"
}

// ── the bundle ───────────────────────────────────────────────────────────────

#[derive(rust_embed::Embed)]
// The frontend project and its build output live at the workspace root; this
// crate is one level down. `build.rs` guarantees the directory exists.
#[folder = "../../web/dist/"]
struct Asset;

fn embedded(path: &str) -> Response {
    match Asset::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("no such asset: {path}")).into_response(),
    }
}

/// `/`: the console page, plus the cookie that lets its `fetch` calls back in.
///
/// Reaching this handler means [`gate`] already accepted the request, so the
/// cookie is only ever handed to someone who proved they had the token.
async fn index(State(state): State<Arc<AppState>>) -> Response {
    let mut resp = embedded("index.html");
    let cookie = format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
        state.token
    );
    if let Ok(v) = header::HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

async fn asset(Path(path): Path<String>) -> Response {
    // The `/assets/*path` route strips the prefix; rust-embed indexes by the
    // full path under web/dist/, so it goes back on.
    embedded(&format!("assets/{path}"))
}

// ── API types ────────────────────────────────────────────────────────────────

/// Boundary activity for one box, as arithmetic over its receipts.
const HOST_OBSERVED_LANES: [&str; 5] = [
    "host-env-run",
    "shell-egress",
    "viewer",
    "browser-proxy",
    "share",
];

#[derive(Serialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct Signals {
    /// Receipts on this box's log.
    pub runs: usize,
    /// Runs that exited non-zero.
    pub failed: usize,
    /// Runs the wall-clock limit killed.
    pub timed_out: usize,
    /// Egress the allowlist proxy permitted. Host-observed (container tier).
    pub egress_allowed: u64,
    /// Egress the proxy refused. The single highest-fidelity signal here,
    /// and the only thing that turns a row red.
    pub egress_denied: u64,
    /// Distinct refused `host:port` destinations, capped.
    pub denied_hosts: Vec<String>,
    /// Console errors, page errors and failed requests the in-box browser
    /// reported, summed over runs that drove it.
    pub browser_issues: usize,
    /// Runs h5i observed from the host. See [`HOST_OBSERVED_LANES`].
    pub host_observed: usize,
    /// Runs recorded by the in-box tee shim. The box's own account.
    pub box_claimed: usize,
    /// Runs observed from outside the box, on a machine we do not own: a runner
    /// (design-runner.md R10).
    /// Its own count rather than folded into either neighbour, because it is
    /// genuinely neither. The box could not forge these, the worker having seen
    /// them from outside it over a channel authenticated with a pinned host
    /// key. But this machine did not watch them either, so a compromised runner
    /// could. The honest reading is that they collapse to box-claimed exactly
    /// when the runner host is compromised, and the runner host is the machine
    /// chosen to be losable.
    #[serde(default)]
    pub runner_observed: usize,
    /// Timestamp of the newest receipt.
    pub last_run_ts: Option<String>,
    /// `denial` | `attention` | `clean`.
    pub verdict: &'static str,
    /// The box runs at `workspace` tier: nothing was confined. Reported beside
    /// the verdict rather than folded into it. It is a standing property of
    /// the box, not something a run did.
    pub weak_isolation: bool,
    /// Runs exist and none of them was host-observed.
    pub box_claimed_only: bool,
    /// The create-time filesystem-authority validator (§P2) found the
    /// effective config not confined to the declared policy. A grant outside
    /// the declared subset, a write not declared writable, a read-only overlay
    /// left writable, or a grant escaping the worktree by symlink. A boundary
    /// trip, so it colors the verdict.
    pub authority_unconfined: bool,
    /// Share sessions on this log: times somebody outside was let in to a port
    /// inside the box.
    ///
    /// Deliberately outside the verdict. Letting a peer in is something an
    /// operator did on purpose, and a screen that turned amber for it would be
    /// grading an intent rather than reporting a boundary trip. It is counted
    /// so the pane can *say* it happened, which is a different job.
    pub shares: usize,
    /// Of those, the ones whose traffic a third party could read. Anything but
    /// `p2p`, decided by
    /// [`crate::receipt::ShareEvidence::third_party_can_read`] rather than
    /// re-derived here, so one rule answers this question everywhere.
    pub shares_third_party_readable: usize,
    /// Distinct peers admitted across every recorded share, including any past
    /// an individual receipt's record cap.
    pub share_peers: u64,
    /// Other boxes of this repository whose effective grants overlap
    /// this one's, as the NEWEST host-observed run/shell receipt recorded
    /// them (`env/<id> via <path>`). Latest-record semantics on purpose:
    /// overlap is a property of what is materialized on the host right now,
    /// and a union over history would show boxes long gone. Empty when the
    /// last check found none, the machine-checked noninterference guarantee
    /// applies to such pairs, and also for envs whose receipts never carried
    /// the check; the pane only speaks when there is something recorded.
    pub fs_overlap: Vec<String>,
    /// Receipts whose kernel-observed lane actually watched the run
    /// (design-detect.md D10).
    /// Its own count, beside `host_observed` rather than inside it, for the
    /// same reason `runner_observed` is its own: they are different claims. A
    /// host-observed run is one h5i was the parent of; a kernel-watched run is
    /// one an eBPF collector saw from underneath. The second is strictly harder
    /// for a box to defeat and strictly narrower in what it covers.
    #[serde(default)]
    pub kernel_watched: usize,
    /// Receipts that carry a runtime block which observed nothing. The probe
    /// could not attach, or the tier is one a host probe cannot see into.
    ///
    /// Counted separately and shown, because the whole point of writing the
    /// block anyway is that "not watched" must never render as "watched and
    /// quiet".
    #[serde(default)]
    pub kernel_unwatched: usize,
    /// Matches at `alert` severity, summed across watched runs.
    #[serde(default)]
    pub kernel_alerts: u64,
    /// Matches at `notice` severity.
    #[serde(default)]
    pub kernel_notices: u64,
    /// Events the kernel or the reader dropped. Nonzero means every count
    /// above is a lower bound, and the pane says so rather than implying
    /// completeness.
    #[serde(default)]
    pub kernel_events_lost: u64,
    /// Distinct rule ids that fired, capped like `denied_hosts`.
    #[serde(default)]
    pub kernel_rules: Vec<String>,
}

/// [`signals`], for a test in another module of this crate.
///
/// The lane split is a property the whole product rests on, and the code that
/// creates a runner-observed receipt lives in `env`. Testing it there against
/// the real counter beats asserting a string and hoping this agreed.
#[cfg(test)]
pub(crate) fn signals_for_test(m: &EnvManifest, receipts: &[ExecRecord]) -> Signals {
    signals(m, receipts)
}

fn signals(m: &EnvManifest, receipts: &[ExecRecord]) -> Signals {
    let mut s = Signals {
        runs: receipts.len(),
        weak_isolation: m.isolation_claim == "workspace",
        ..Default::default()
    };
    let mut denied_hosts: Vec<String> = Vec::new();
    let mut kernel_rules: Vec<String> = Vec::new();
    for r in receipts {
        if r.exit_code.is_some_and(|c| c != 0) {
            s.failed += 1;
        }
        if r.timed_out {
            s.timed_out += 1;
        }
        // Allowlist, not a catch-all. `_ => host_observed` counted any unknown
        // source as evidence h5i collected, including `inbox-capture`, which
        // the boxed process writes into its own spool. A box could therefore
        // clear the grey "box-claimed" badge by writing its own records, which
        // is precisely the distinction this screen exists to keep.
        if HOST_OBSERVED_LANES.contains(&r.source.as_str()) {
            s.host_observed += 1;
        } else if r.source == crate::placement::RUNNER_OBSERVED_LANE {
            // Deliberately a third arm rather than a sixth entry in the
            // allowlist above. See `Signals::runner_observed`.
            s.runner_observed += 1;
        } else {
            s.box_claimed += 1;
        }
        if let Some(e) = &r.egress {
            s.egress_allowed += e.allowed;
            s.egress_denied += e.denied;
            for h in e.hosts.iter().filter(|h| h.denied > 0) {
                let label = format!("{}:{}", h.host, h.port);
                if !denied_hosts.contains(&label) {
                    denied_hosts.push(label);
                }
            }
        }
        if let Some(b) = &r.browser {
            s.browser_issues += b.console.len() + b.errors.len() + b.failed_requests.len();
        }
        // Newest run/shell record wins, including winning with an empty
        // list, which clears an overlap a departed box used to cause.
        if matches!(r.source.as_str(), "host-env-run" | "host-env-shell") {
            s.fs_overlap = r.fs_overlap.clone();
        }
        if let Some(rt) = &r.runtime {
            if rt.observed() {
                s.kernel_watched += 1;
            } else {
                s.kernel_unwatched += 1;
            }
            s.kernel_events_lost += rt.events_lost;
            for d in &rt.detections {
                match d.severity {
                    h5i_bpf::Severity::Alert => s.kernel_alerts += d.count,
                    h5i_bpf::Severity::Notice => s.kernel_notices += d.count,
                    h5i_bpf::Severity::Info => {}
                }
                if !kernel_rules.contains(&d.rule) {
                    kernel_rules.push(d.rule.clone());
                }
            }
        }
        if let Some(sh) = &r.share {
            s.shares += 1;
            if sh.third_party_can_read() {
                s.shares_third_party_readable += 1;
            }
            s.share_peers += sh.peers;
        }
    }
    denied_hosts.truncate(DENIED_HOSTS_CAP);
    s.denied_hosts = denied_hosts;
    kernel_rules.sort();
    kernel_rules.truncate(DENIED_HOSTS_CAP);
    s.kernel_rules = kernel_rules;
    s.fs_overlap.truncate(DENIED_HOSTS_CAP);
    // Receipts are appended in order and their timestamps sort lexically.
    s.last_run_ts = receipts.last().map(|r| r.timestamp.clone());
    // "Nothing was watched from outside the box", which a runner-observed run
    // *was*, by an h5i we authenticated. Counting only `host_observed` here
    // would put the grey box-claimed badge on a runner box whose every run was
    // seen from outside it, which is the opposite of what the badge means. The
    // weaker evidence is still visible: `runner_observed` is its own count.
    s.box_claimed_only = s.runs > 0 && s.host_observed == 0 && s.runner_observed == 0;
    s.authority_unconfined =
        m.fs_authority.is_some_and(|a| !a.confined() || a.symlink_clean == Some(false));
    // A kernel-observed alert raises `attention`, never `denial`, and the
    // difference is not pedantry. `denial` means *something was refused* (the
    // egress proxy said no, the authority validator found a grant outside the
    // declared set) and this lane refuses nothing: it reports that a box read
    // a credential file, not that it was stopped. Folding it into `denial`
    // would quietly redefine the one word on this screen that currently has a
    // precise meaning (`box-console-honesty-model`). The count is shown on its
    // own, which is the honest way to make it loud.
    s.verdict = if s.egress_denied > 0 || s.authority_unconfined {
        "denial"
    } else if s.failed > 0 || s.timed_out > 0 || s.browser_issues > 0 || s.kernel_alerts > 0 {
        "attention"
    } else {
        "clean"
    };
    s
}

/// Sort key: most pressing first. `denial` outranks `attention` outranks
/// everything else, and within a rank the most recently touched box wins.
fn rank(s: &Signals) -> u8 {
    match s.verdict {
        "denial" => 2,
        "attention" => 1,
        _ => 0,
    }
}

/// One row in the fleet.
///
/// The manifest is flattened in, so this payload is a superset of
/// `h5i box list --json`: the console and the scriptable CLI describe a box
/// with the same field names, and there is no second shape to keep in step.
#[derive(Serialize)]
pub struct BoxRow {
    #[serde(flatten)]
    pub manifest: EnvManifest,
    /// Stable kind (`up-to-date`, `parent-ahead`, …) plus its one-line summary.
    pub drift: String,
    pub drift_summary: String,
    /// PID-verified sessions from the live registry.
    pub live: Vec<LiveSession>,
    /// The durable status says `running` but no live writer holds the box. A
    /// crash leftover to flag, never to trust.
    pub stale_running: bool,
    /// Is the workspace materialized here (false = pulled from another clone)?
    pub has_workspace: bool,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub last_event: Option<EnvEvent>,
    pub signals: Signals,
    /// A share serving this box *right now*, if one is.
    ///
    /// The receipt lands when the share ends, so until this the console showed
    /// nothing at all while a box was open to somebody on another machine,
    /// and the console's whole job is saying what is pressing on a boundary.
    /// The one lane that lets somebody *in* was the one it could not see while
    /// it was open.
    pub shared_now: Option<SharedNow>,
}

/// What the console needs to say about a share that is open.
///
/// Read off `<env>/share.json`, which is where the share keeps it, rather than
/// through `h5i-share`: that crate is above this one. No secret is in the file
/// and none is read here. The grant table stores digests, and this takes the
/// transport, the port and the number of grants that can still admit anybody.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SharedNow {
    pub transport: String,
    pub port: u64,
    pub grants: usize,
}

fn shared_now(env_dir: &std::path::Path) -> Option<SharedNow> {
    // Only when somebody can actually reach in. A share that is winding up, or
    // whose grants have all been revoked, has a live pid and admits nobody,
    // and the badge said "somebody outside can reach port 3000 right now" of
    // it, for as long as that process took to exit. On a screen whose job is
    // saying what is pressing on a boundary, overclaiming toward alarm is the
    // failure that costs it its credibility.
    let r = crate::share_record::read_live(env_dir)?;
    if !r.is_admitting() {
        return None;
    }
    Some(SharedNow {
        transport: r.transport,
        port: r.port as u64,
        grants: r.live_grants,
    })
}

fn build_row(
    git: &git2::Repository,
    h5i_root: &std::path::Path,
    m: &EnvManifest,
    events: &[EnvEvent],
    receipts: &[ExecRecord],
) -> BoxRow {
    let drift = env::drift(git, m);
    let live = env::live_sessions(&m.dir(h5i_root));
    let stale_running =
        m.status == "running" && !live.iter().any(|s| env::live_is_writer(&s.kind));
    let (files_changed, insertions, deletions) =
        env::diffstat_numbers(git, h5i_root, m, env::DiffSource::Worktree).unwrap_or((0, 0, 0));
    BoxRow {
        drift: drift.kind_str().to_string(),
        drift_summary: drift.summary(),
        live,
        stale_running,
        has_workspace: env::has_workspace(m, h5i_root),
        files_changed,
        insertions,
        deletions,
        last_event: events.last().cloned(),
        signals: signals(m, receipts),
        shared_now: shared_now(&m.dir(h5i_root)),
        manifest: m.clone(),
    }
}

/// The resolved policy, flattened for the console's allowance panel: what was
/// actually enforced, not what the profile asked for.
#[derive(Serialize)]
pub struct EnforcedPolicy {
    pub isolation: String,
    pub net_mode: String,
    pub net_egress: Vec<String>,
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub fs_deny: Vec<String>,
    pub tools: Vec<String>,
    pub env_pass: Vec<String>,
    pub image: Option<String>,
    pub mem_bytes: Option<u64>,
    pub max_procs: Option<u64>,
    pub wall_secs: u64,
    pub cpu_secs: Option<u64>,
    pub fsize_bytes: Option<u64>,
    /// Whether this tier actually applies `mem_bytes` on the host serving this
    /// console. The panel is headed "what was actually allowed", so a cap the
    /// host cannot impose has to be drawn as declared-only rather than silently
    /// rendered next to the ones that are real. See
    /// [`crate::sandbox::limit_support`].
    pub mem_enforced: bool,
    /// As `mem_enforced`, for `max_procs`.
    pub procs_enforced: bool,
}

impl From<&Profile> for EnforcedPolicy {
    fn from(p: &Profile) -> Self {
        let limits = crate::sandbox::limit_support(p.isolation);
        EnforcedPolicy {
            isolation: p.isolation.as_str().to_string(),
            net_mode: match p.net_mode {
                crate::sandbox::NetMode::Deny => "deny".into(),
                crate::sandbox::NetMode::Host => "host".into(),
            },
            net_egress: p.net_egress.clone(),
            fs_read: p.fs_read.clone(),
            fs_write: p.fs_write.clone(),
            fs_deny: p.fs_deny.clone(),
            tools: p.tools.clone(),
            env_pass: p.env_pass.clone(),
            image: p.image.clone(),
            mem_bytes: p.mem_bytes,
            max_procs: p.max_procs,
            wall_secs: p.wall_secs,
            cpu_secs: p.cpu_secs,
            fsize_bytes: p.fsize_bytes,
            mem_enforced: limits.mem,
            procs_enforced: limits.procs,
        }
    }
}

/// Everything the detail pane draws for one box.
#[derive(Serialize)]
pub struct BoxDetail {
    pub item: BoxRow,
    /// `None` when `policy.resolved.toml` is unreadable. A pulled or gc'd box.
    pub policy: Option<EnforcedPolicy>,
    pub events: Vec<EnvEvent>,
    /// Newest last, capped at [`RECEIPT_CAP`].
    pub receipts: Vec<ExecRecord>,
    /// How many older receipts the cap folded, so a clamped list never reads
    /// as the whole history.
    pub receipts_folded: usize,
    /// Declared long-lived services and their liveness.
    pub services: Vec<ServiceStatus>,
    /// `base..worktree` diffstat, as `h5i box diff --stat` prints it.
    pub diffstat: Option<String>,
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// Run a blocking domain call off the async runtime, folding "the task died"
/// and "the work found nothing" into one `None`.
///
/// Everything in `env` is synchronous git and filesystem work, so every handler
/// goes through here. `Option` rather than a `Result`: these are read-only
/// lookups whose only two outcomes on the wire are a payload and a 404, so
/// carrying an error type would be carrying something no caller can use.
async fn blocking<T, F>(f: F) -> Option<T>
where
    F: FnOnce() -> Option<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.ok().flatten()
}

/// Open the repository the console was pointed at, and its h5i root.
fn open(repo_path: &std::path::Path) -> Option<(git2::Repository, PathBuf)> {
    let git = git2::Repository::discover(repo_path).ok()?;
    let root = crate::storage::h5i_root_for_repo(&git).ok()?;
    Some((git, root))
}

fn receipts_of(h5i_root: &std::path::Path, m: &EnvManifest) -> Vec<ExecRecord> {
    crate::receipt::list(&env::env_dir(h5i_root, &m.agent, &m.slug)).unwrap_or_default()
}

/// One browser session, as the fleet view shows it.
///
/// Sessions are the surface the workbench and recon act on, and until now the
/// console could not see one at all: it showed boxes, and a session needs no
/// box. The row carries the account (counts, states, jobs) and never the
/// evidence, which stays owner-only on disk (`session_view`).
#[derive(Serialize, Clone)]
pub struct SessionRow {
    pub id: String,
    pub name: Option<String>,
    /// `live`, `closed`, `died`, `expired`, `gone`.
    pub state: String,
    /// Where it ran, in the words the CLI uses.
    pub placement: String,
    /// `engine-claimed` or `host-observed`: what the record is worth.
    pub lane: String,
    pub identity: String,
    pub url: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// Whether a human holds the control lock.
    pub held_by_human: bool,
    #[serde(flatten)]
    pub signals: crate::session_view::SessionSignals,
    pub attention: crate::session_view::Attention,
}

/// Everything one session's own files say, for the detail pane.
#[derive(Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub row: SessionRow,
    /// The newest receipts, oldest first, both phases as the log holds them.
    /// Named apart from the row's `requests` count, which is flattened in.
    pub requests_log: Vec<Value>,
    /// The recon ledger, folded.
    pub endpoints: Vec<h5i_wire::ledger::Endpoint>,
}

/// A serialisable enum as the one word it serialises to.
fn as_word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Read one session's files into a row.
fn session_row(h5i_root: &std::path::Path, session: &bs::Session) -> (SessionRow, Vec<Value>) {
    let dir = bs::dir(h5i_root, &session.id);
    let records: Vec<Value> = session
        .logs
        .requests
        .clone()
        .or_else(|| Some(dir.join(bs::RECEIPTS_FILE)))
        .and_then(|path| bs::read_log_capped_saying(&path))
        .map(|(text, _)| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect()
        })
        .unwrap_or_default();

    let mut signals = crate::session_view::signals_from_receipts(&records);
    signals.captured = crate::session_view::captured(&dir);
    signals.reclaimed = bs::reclaimed(h5i_root, &session.id);
    signals.ledger = crate::session_view::ledger_counts(&dir);
    signals.jobs = crate::session_view::jobs(&dir);

    let held_by_human = crate::control::read(&dir).holder == crate::control::Holder::Human;
    // A live record whose control file is gone is the one case this cannot
    // classify, and saying so is the point of the `unknown` state.
    let engine_reachable = session
        .control
        .file
        .as_ref()
        .map(|f| f.exists())
        .unwrap_or(false);
    let attention = crate::session_view::attention(
        session,
        &signals,
        held_by_human,
        engine_reachable,
        chrono::Utc::now(),
    );

    let row = SessionRow {
        id: session.id.clone(),
        name: session.name.clone(),
        // Through serde, not `Debug`: these are the words the CLI and the
        // receipts use, and `engine-claimed` losing its hyphen to a lowercased
        // `Debug` would be a third spelling of a term the product defines.
        state: as_word(&session.state),
        placement: session.where_it_ran(),
        lane: as_word(&session.lane),
        identity: session.identity.clone(),
        url: session.url.clone(),
        started_at: session.started_at.clone(),
        ended_at: session.ended_at.clone(),
        held_by_human,
        signals,
        attention,
    };
    (row, records)
}

/// The most sessions one poll reads in full.
///
/// A registry grows without bound: every `h5i browser open` adds a record, and
/// a benchmark run adds one per target. Reading every session's log, ledger and
/// jobs every few seconds would make the console the most expensive thing on
/// the machine, so the newest are read and the rest are counted.
pub const MAX_SESSIONS_READ: usize = 120;

/// The fleet, and what did not fit in it.
#[derive(Serialize)]
pub struct SessionFleet {
    pub sessions: Vec<SessionRow>,
    /// Records in the registry, including the ones not read.
    pub total: usize,
    /// Sessions still running, whether or not they were read.
    pub live: usize,
}

/// `GET /api/sessions`: browser sessions, the ones wanting a person first.
async fn api_sessions(State(state): State<Arc<AppState>>) -> Json<SessionFleet> {
    let cache = state.sessions.clone();
    let fleet = blocking(move || {
        // Sessions are the machine's, not the repository's: `h5i browser open`
        // needs no repo, and the registry lives in the user's state directory.
        // The console says which scope it is showing rather than letting a
        // reader assume the boxes' one.
        let h5i_root = bs::root().ok()?;
        let mut records = bs::list(&h5i_root).unwrap_or_default();
        let total = records.len();
        let live = records.iter().filter(|s| s.state.is_live()).count();
        // Live first, then newest: what a person is watching is what is running
        // now and what just finished.
        records.sort_by(|a, b| {
            b.state
                .is_live()
                .cmp(&a.state.is_live())
                .then(b.started_at.cmp(&a.started_at))
        });
        records.truncate(MAX_SESSIONS_READ);

        let mut sessions: Vec<SessionRow> = records
            .iter()
            .map(|session| {
                let stamp = SessionStamp::of(&h5i_root, &session.id);
                // Anything a row is made of leaves a mark on a file. If none of
                // them moved, the row cannot have.
                if let Ok(cached) = cache.lock()
                    && let Some((seen, row)) = cached.get(&session.id)
                    && *seen == stamp
                {
                    return row.clone();
                }
                let row = session_row(&h5i_root, session).0;
                if let Ok(mut cached) = cache.lock() {
                    // Bounded by what one poll reads, so the map cannot outgrow
                    // the fleet it describes.
                    if cached.len() > MAX_SESSIONS_READ * 4 {
                        cached.clear();
                    }
                    cached.insert(session.id.clone(), (stamp, row.clone()));
                }
                row
            })
            .collect();
        // Loudest first, then newest. The order is the whole point: a person
        // watching many sessions should not have to find the one that stopped.
        sessions.sort_by(|a, b| {
            attention_rank(b.attention.state)
                .cmp(&attention_rank(a.attention.state))
                .then(b.started_at.cmp(&a.started_at))
        });
        Some(SessionFleet {
            sessions,
            total,
            live,
        })
    })
    .await;
    Json(fleet.unwrap_or(SessionFleet {
        sessions: Vec::new(),
        total: 0,
        live: 0,
    }))
}

/// How loudly a state asks. `done` outranks `working` because a finished run
/// nobody has read is the thing most easily lost.
fn attention_rank(state: &str) -> u8 {
    match state {
        "blocked" => 4,
        "done" => 3,
        "working" => 2,
        "idle" => 1,
        _ => 0,
    }
}

/// `GET /api/session/:id`: one session's account, in full.
async fn api_session(Path(id): Path<String>) -> Result<Json<SessionDetail>, StatusCode> {
    let detail = blocking(move || {
        let h5i_root = bs::root().ok()?;
        // The id becomes a path, and a boxed session writes its own record.
        if !bs::id_is_one_component(&id) {
            return None;
        }
        let session = bs::read(&h5i_root, &id).ok()?;
        let (row, records) = session_row(&h5i_root, &session);
        let dir = bs::dir(&h5i_root, &session.id);
        let start = records
            .len()
            .saturating_sub(crate::session_view::MAX_REQUESTS_SHOWN * 2);
        Some(SessionDetail {
            row,
            requests_log: records[start..].to_vec(),
            endpoints: crate::session_view::ledger_endpoints(&dir),
        })
    })
    .await;
    detail.map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// `GET /api/boxes`: the fleet, most pressing first.
async fn api_boxes(State(state): State<Arc<AppState>>) -> Json<Vec<BoxRow>> {
    let path = state.repo_path.clone();
    let rows = blocking(move || {
        let (git, h5i_root) = open(&path)?;
        let mut rows: Vec<BoxRow> = env::list(&h5i_root)
            .iter()
            .map(|m| {
                let events = env::read_events(&git, Some(&m.id));
                let receipts = receipts_of(&h5i_root, m);
                build_row(&git, &h5i_root, m, &events, &receipts)
            })
            .collect();
        rows.sort_by(|a, b| {
            rank(&b.signals)
                .cmp(&rank(&a.signals))
                .then(b.manifest.updated_at.cmp(&a.manifest.updated_at))
        });
        Some(rows)
    })
    .await;
    Json(rows.unwrap_or_default())
}

/// `GET /api/box/:agent/:slug`: one box in full.
async fn api_box(
    State(state): State<Arc<AppState>>,
    Path((agent, slug)): Path<(String, String)>,
) -> Response {
    let path = state.repo_path.clone();
    let detail = blocking(move || {
        let (git, h5i_root) = open(&path)?;
        let id = format!("env/{agent}/{slug}");
        let m = env::list(&h5i_root).into_iter().find(|m| m.id == id)?;
        let events = env::read_events(&git, Some(&m.id));
        let mut receipts = receipts_of(&h5i_root, &m);
        let policy = env::load_policy(&h5i_root, &m).ok().map(|rp| rp.profile);
        // Signals over the FULL history, then fold for display. Draining first
        // made the detail pane disagree with the fleet row the user just
        // clicked, and in the dangerous direction: a denial older than the cap
        // vanished on inspection while the row still showed red.
        let item = build_row(&git, &h5i_root, &m, &events, &receipts);
        let receipts_folded = receipts.len().saturating_sub(RECEIPT_CAP);
        if receipts_folded > 0 {
            receipts.drain(..receipts_folded);
        }
        Some(BoxDetail {
            item,
            policy: policy.as_ref().map(EnforcedPolicy::from),
            events,
            receipts,
            receipts_folded,
            services: env::service_status(&h5i_root, &m),
            diffstat: env::diff(&git, &h5i_root, &m, true).ok(),
        })
    })
    .await;
    match detail {
        Some(d) => Json(d).into_response(),
        None => (StatusCode::NOT_FOUND, "no such box").into_response(),
    }
}

/// `GET /api/box/:agent/:slug/receipts/:id`: one receipt, rendered exactly as
/// `h5i box inspect` renders it (which refuses ids belonging to another box).
async fn api_receipt(
    State(state): State<Arc<AppState>>,
    Path((agent, slug, id)): Path<(String, String, String)>,
) -> Response {
    let path = state.repo_path.clone();
    let render = blocking(move || {
        let (_git, h5i_root) = open(&path)?;
        let want = format!("env/{agent}/{slug}");
        let m = env::list(&h5i_root).into_iter().find(|m| m.id == want)?;
        env::inspect(&h5i_root, &m, &id).ok()
    })
    .await;
    match render {
        Some(text) => Json(serde_json::json!({ "render": text })).into_response(),
        None => (StatusCode::NOT_FOUND, "no such receipt for this box").into_response(),
    }
}

/// What the browser terminal reads: the box's event stream plus the two things
/// a pane must state rather than imply.
#[derive(Serialize)]
pub struct BrowserStream {
    /// Events after the caller's cursor, oldest first.
    pub events: Vec<crate::browser_events::ViewerEvent>,
    /// The newest id held. The caller sends this back as `since` and gets only
    /// what it has not seen.
    pub cursor: u64,
    /// Events the cap discarded. Rendered by the console, because a bound that
    /// drops silently reports a quiet session where there was a loud one.
    pub dropped: u64,
    /// The engine this box is pinned to, or `None` when it has no browser
    /// profile. The network pane's evidence grade follows from it, so the pane
    /// names the engine rather than leaving a reader to assume Chromium.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Whether a live view is being served inside the box right now. A
    /// `.stream` file next to the daemon socket, the same discovery
    /// `h5i box view` uses.
    pub live_view: bool,
    /// Sequence number of the newest frame the console holds, or `None` when it
    /// holds none.
    /// The page re-fetches the frame only when this changes, which is what
    /// keeps a still page at zero requests instead of on a timer. The same
    /// change-driven rule the engine itself follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_seq: Option<u64>,
    /// Why there is no picture, when there is a live view but no frame. Shown
    /// rather than swallowed: a viewer that cannot say why it is blank is
    /// indistinguishable from one that is broken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_error: Option<String>,
}

/// How many events one box's stream holds. A page pulling in subresources makes
/// two rows each, so this is a few hundred navigations' worth. Enough to scroll
/// back through a session, bounded enough that a long-lived console does not
/// grow without limit.
const STREAM_CAP: usize = 4000;

/// `GET /api/box/:agent/:slug/browser?since=N`: the browser terminal's stream
/// (roadmap-history.md M11a).
///
/// A `GET` like every other route here, and for the same reason: the console
/// watches and never drives. Taking the control lock and typing into a page go
/// through [`crate::view`]'s forward, which is a different surface with a
/// different token, and deliberately not this one.
async fn api_browser(
    State(state): State<Arc<AppState>>,
    Path((agent, slug)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<BrowserQuery>,
) -> Response {
    let path = state.repo_path.clone();
    let streams = state.browser.clone();
    let frames = state.frames.clone();
    let stream = blocking(move || {
        let (_git, h5i_root) = open(&path)?;
        let want = format!("env/{agent}/{slug}");
        let m = env::list(&h5i_root).into_iter().find(|m| m.id == want)?;
        let policy = env::load_policy(&h5i_root, &m).ok();
        let engine = policy
            .as_ref()
            .and_then(|p| p.profile.engine)
            .map(|e| e.as_str().to_string());

        // A poisoned lock means another request panicked mid-poll. The state is
        // a derived read cursor, not a record of anything, so the honest
        // recovery is to keep serving from it rather than to fail every
        // subsequent request for the life of the process.
        let mut held = streams.lock().unwrap_or_else(|e| e.into_inner());
        let entry = held
            .entry(want)
            .or_insert_with(|| crate::browser_events::BoxStream::new(STREAM_CAP));
        entry.poll(&h5i_root, &m);
        let log = entry.log();

        let events: Vec<_> = log.since(query.since).into_iter().cloned().collect();
        let (cursor, dropped) = (log.cursor(), log.dropped());
        drop(held);

        // Looking at the browser tab is what starts a reader; closing the
        // console is what ends it. Kept off the `browser` lock because a
        // namespace entry is slow and the event stream should not wait on it.
        let env_dir = m.dir(&h5i_root);
        let located = crate::browser_frames::locate(&env_dir);
        let live_view = located.is_some();
        let (frame_seq, frame_error) = tend_relay(&frames, &m.id, located);

        Some(BrowserStream {
            events,
            cursor,
            dropped,
            engine,
            live_view,
            frame_seq,
            frame_error,
        })
    })
    .await;
    match stream {
        Some(s) => Json(s).into_response(),
        None => (StatusCode::NOT_FOUND, "no such box").into_response(),
    }
}

/// The cursor, defaulted so a first poll needs no parameter.
#[derive(serde::Deserialize)]
pub struct BrowserQuery {
    #[serde(default)]
    pub since: u64,
}

type Relays = std::sync::Mutex<std::collections::HashMap<String, crate::browser_frames::FrameRelay>>;

/// Keep the live-view reader for one box in step with reality, and report what
/// it has.
///
/// Three transitions, all driven by the box rather than by a user action: a
/// view appears and a reader starts; a view goes away and the reader is dropped
/// (which closes the socket into the box: a console tab left open must not pin
/// a connection to a box that stopped serving); a reader dies on its own and is
/// replaced, but not faster than its retry delay.
fn tend_relay(
    relays: &Arc<Relays>,
    box_id: &str,
    located: Option<(u32, u16, std::ffi::OsString)>,
) -> (Option<u64>, Option<String>) {
    let mut held = relays.lock().unwrap_or_else(|e| e.into_inner());

    let Some((pid, port, pid_ns)) = located else {
        held.remove(box_id);
        return (None, None);
    };

    if held.get(box_id).is_some_and(|r| r.spent()) {
        held.remove(box_id);
    }
    let relay = held
        .entry(box_id.to_string())
        .or_insert_with(|| crate::browser_frames::FrameRelay::start(pid, port, pid_ns));

    let seq = relay.latest().map(|f| f.seq);
    // Only worth reporting while there is nothing to show. A relay that died
    // after delivering frames still leaves the last one on screen, and an error
    // beside a picture that is visibly there reads as a bug in the console.
    let error = (seq.is_none() && !relay.connected())
        .then(|| relay.error())
        .flatten();
    (seq, error)
}

/// `GET /api/box/:agent/:slug/browser/frame`: the newest frame the console
/// holds for this box, as a JPEG.
///
/// A `GET` that returns an image, so the console's "every route is a GET" rule
/// is untouched and the page can point an `<img>` at it. The bytes are the
/// box's, which is why two headers are not optional: `nosniff` so a crafted
/// payload cannot be re-interpreted as anything but an image, and `no-store` so
/// a frame of somebody's page does not settle into a disk cache.
async fn api_browser_frame(
    State(state): State<Arc<AppState>>,
    Path((agent, slug)): Path<(String, String)>,
) -> Response {
    let id = format!("env/{agent}/{slug}");
    let relays = state.frames.clone();
    let frame = tokio::task::spawn_blocking(move || {
        let held = relays.lock().unwrap_or_else(|e| e.into_inner());
        held.get(&id).and_then(|r| r.latest())
    })
    .await
    .ok()
    .flatten();

    match frame {
        Some(f) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            f.jpeg,
        )
            .into_response(),
        // Not an error: a box with no live view, or one whose page has not
        // changed since the reader attached, simply has no frame.
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// `GET /api/probe`: what this host can actually enforce. The same report
/// `h5i box capabilities --json` prints, so the console's top strip and the
/// CLI can never disagree.
async fn api_probe() -> Response {
    // A capability report is a diagnostic, so it is probed fresh rather than
    // served from the per-boot podman cache. Exactly as `box probe` does. The
    // freshness is a property of the call, not of the process environment: this
    // is a long-lived multithreaded server, and a `set_var` here would race
    // every other thread's `getenv` and outlive the request that wanted it.
    //
    // The only way the join fails is a panic inside the probe, so calling it
    // again inline was calling the thing that just panicked, on a runtime
    // worker, where the unwind leaves the connection dropped with no response
    // at all. Every other handler folds that case into a status through
    // [`blocking`]; this one now does too.
    match blocking(|| Some(crate::sandbox::capabilities_report_fresh())).await {
        Some(report) => Json(report).into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Every route, wired to `state`. Extracted so tests can drive the surface
/// without a socket, and so the "all of them are GETs" claim is checkable.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/*path", get(asset))
        .route("/api/probe", get(api_probe))
        .route("/api/boxes", get(api_boxes))
        .route("/api/sessions", get(api_sessions))
        .route("/api/session/:id", get(api_session))
        .route("/api/box/:agent/:slug", get(api_box))
        .route("/api/box/:agent/:slug/receipts/:id", get(api_receipt))
        .route("/api/box/:agent/:slug/browser", get(api_browser))
        .route("/api/box/:agent/:slug/browser/frame", get(api_browser_frame))
        .layer(axum::middleware::from_fn_with_state(state.clone(), gate))
        .with_state(state)
}

// ── entry point ──────────────────────────────────────────────────────────────

/// A bound console, before it starts serving.
///
/// Split from [`Console::serve`] the way [`crate::view`] splits its forward:
/// binding can fail (a port in use is the common case) and the caller wants to
/// print the URL, token and all, before blocking forever.
pub struct Console {
    listener: std::net::TcpListener,
    state: Arc<AppState>,
}

impl Console {
    /// Bind loopback and mint this session's token. `port` 0 asks the OS for a
    /// free one, which [`Console::url`] then reports.
    pub fn bind(repo_path: PathBuf, port: u16) -> Result<Console, H5iError> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                H5iError::Metadata(format!(
                    "port {port} is already in use — pick another with `h5i ui --port <N>`"
                ))
            } else {
                H5iError::Io(e)
            }
        })?;
        let token = crate::token::hex(TOKEN_BYTES)?;
        Ok(Console {
            listener,
            state: Arc::new(AppState::new(repo_path, token)),
        })
    }

    /// The one URL that works. The token is in the query string because that
    /// is the only channel a terminal has to a browser; the page trades it for
    /// a cookie on first load.
    pub fn url(&self) -> Result<String, H5iError> {
        let addr = self.listener.local_addr().map_err(H5iError::Io)?;
        Ok(format!("http://{}/?token={}", addr, self.state.token))
    }

    /// The URL to hand to a *program* rather than to a human.
    ///
    /// Carries a single-use token, because `--open` puts this string on another
    /// process's command line where any other local uid can read it. Mints one
    /// per call and forgets the previous, so only the newest handoff is live.
    pub fn handoff_url(&self) -> Result<String, H5iError> {
        let addr = self.listener.local_addr().map_err(H5iError::Io)?;
        let once = crate::token::hex(TOKEN_BYTES)?;
        if let Ok(mut slot) = self.state.handoff.lock() {
            *slot = Some(once.clone());
        }
        Ok(format!("http://{addr}/?token={once}"))
    }

    /// Serve until the process is interrupted. Builds its own runtime, so
    /// nothing above this module needs to know the console is async.
    pub fn serve(self) -> Result<(), H5iError> {
        self.listener
            .set_nonblocking(true)
            .map_err(H5iError::Io)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(H5iError::Io)?;
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(self.listener).map_err(H5iError::Io)?;
            axum::serve(listener, router(self.state))
                .await
                .map_err(H5iError::Io)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::BrowserEvidence;
    use crate::sandbox_policy::{EgressHost, EgressSummary};

    fn manifest(isolation: &str) -> EnvManifest {
        EnvManifest {
            id: "env/claude/fix-auth".into(),
            agent: "claude".into(),
            slug: "fix-auth".into(),
            base_commit: "a".repeat(40),
            base_tree: "b".repeat(40),
            parent_branch: "main".into(),
            branch: "refs/heads/h5i/env/claude/fix-auth".into(),
            source: "repo".into(),
            profile: "default".into(),
            policy_digest: "c".repeat(64),
            effective_digest: None,
            fs_authority: None,
            isolation_claim: isolation.into(),
            backend: "worktree".into(),
            created_at: "2026-08-05T00:00:00Z".into(),
            updated_at: "2026-08-05T00:00:00Z".into(),
            status: "idle".into(),
            captures: vec![],
            service_digest: None,
            persona_digest: None,
            pr: None,
            pr_head_ref: None,
            runner_id: None,
            runner: None,
        }
    }

    fn receipt(source: &str, exit: i32) -> ExecRecord {
        ExecRecord {
            id: "r0".into(),
            timestamp: "2026-08-05T01:00:00Z".into(),
            env_id: "env/claude/fix-auth".into(),
            policy_digest: None,
            effective_digest: None,
            fs_overlap: Vec::new(),
            source: source.into(),
            cmd: Some("cargo test".into()),
            cwd: None,
            exit_code: Some(exit),
            timed_out: false,
            wall_ms: None,
            cpu_ms: None,
            max_rss_kb: None,
            git_tree: None,
            files: vec![],
            egress: None,
            browser: None,
            share: None,
            runtime: None,
            redactions: vec![],
            raw_oid: "d".repeat(64),
            raw_size: 0,
            raw_lines: 0,
            raw_truncated: false,
        }
    }

    /// The box writes `inbox-capture` records into its own spool. Counting any
    /// unknown source as host-observed let it clear the grey "box-claimed"
    /// badge. The one distinction this screen is built on.
    #[test]
    fn a_box_being_shared_right_now_says_so() {
        // The receipt lands when the share *ends*, so until this the console
        // showed nothing at all while a box was open to somebody on another
        // machine. For the one lane that lets somebody in, on a screen whose
        // job is saying what is pressing on a boundary.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(shared_now(dir.path()).is_none(), "no file, nothing to say");

        let now = chrono::Utc::now().timestamp();
        let record = |extra: &str, revoked: bool| {
            serde_json::json!({
                "v": 1,
                "box_id": "env/a/demo",
                "port": 3000,
                "transport": "tunnel",
                "endpoint": "https://x",
                "started_at": "2026-01-01T00:00:00Z",
                "pid": std::process::id(),
                "winding_up": extra == "winding",
                "grants": [
                    {"id": "a", "secret_sha256": "ff", "revoked": revoked, "expires_at": now + 600},
                    {"id": "b", "secret_sha256": "ff", "revoked": true,    "expires_at": now + 600},
                    {"id": "c", "secret_sha256": "ff", "revoked": false,   "expires_at": now - 1},
                ],
            })
            .to_string()
        };

        std::fs::write(dir.path().join("share.json"), record("", false)).expect("write");
        let got = shared_now(dir.path()).expect("a live share");
        assert_eq!(got.transport, "tunnel");
        assert_eq!(got.port, 3000);
        // Only the grants that can still admit somebody: one revoked and one
        // expired are two tickets that let nobody in.
        assert_eq!(got.grants, 1);

        // A share that is winding up has a live pid and admits nobody. The
        // badge used to say "somebody outside can reach port 3000 right now"
        // of it, for as long as that process took to write its receipt.
        std::fs::write(dir.path().join("share.json"), record("winding", false)).expect("write");
        assert!(shared_now(dir.path()).is_none(), "a winding-up share is not admitting anyone");

        // And so does one whose every grant has been revoked.
        std::fs::write(dir.path().join("share.json"), record("", true)).expect("write");
        assert!(shared_now(dir.path()).is_none(), "no live grant is nobody admitted");

        // A record left by a process that is gone is not a share.
        std::fs::write(dir.path().join("share.json"), "{\"pid\":0}").expect("write");
        assert!(shared_now(dir.path()).is_none());
    }

    #[test]
    fn a_share_is_host_observed_evidence() {
        // h5i owns both ends of the share bridge and the box supplies none of
        // it, so leaving the lane off the list inverted the badge: a box whose
        // only receipt was a share read as having no host-observed evidence at
        // all, which is the grey "the box told us this" badge. For the one
        // lane the box cannot touch.
        assert!(HOST_OBSERVED_LANES.contains(&"share"));
    }

    #[test]
    fn ended_shares_are_counted_without_grading_the_box() {
        // The evidence a share leaves behind was on the receipt and nowhere the
        // console could reach it: the pane knew a box was shared *now* and
        // forgot the moment the share ended, which is the moment the record
        // finally exists.
        let m = manifest("container");
        let share = |transport: &str, peers: u64| {
            let mut r = receipt("share", 0);
            r.share = Some(crate::receipt::ShareEvidence {
                transport: transport.into(),
                port: 3000,
                peers,
                seconds: 61,
                turned_away: 1,
            });
            r
        };

        let s = signals(&m, &[share("p2p", 2), share("tunnel", 1)]);
        assert_eq!(s.shares, 2);
        assert_eq!(s.share_peers, 3);
        // The transport is read as a field. Recovering it from the rendered
        // command line is what reported a P2P share of a box called `tunnel`
        // as Cloudflare-terminated.
        assert_eq!(s.shares_third_party_readable, 1);

        // And none of it moves the verdict. A share is an operator letting
        // somebody in on purpose, not the boundary saying no.
        assert_eq!(s.verdict, "clean");
        assert_eq!(s.failed, 0);

        // A transport this h5i does not know is not a promise of end-to-end
        // encryption, and the count follows the same rule the receipt does.
        let unknown = signals(&m, &[share("something-later", 1)]);
        assert_eq!(unknown.shares_third_party_readable, 1);

        // A box that was never shared says nothing at all.
        let quiet = signals(&m, &[receipt("host-env-run", 0)]);
        assert_eq!(quiet.shares, 0);
        assert_eq!(quiet.share_peers, 0);
        assert_eq!(quiet.shares_third_party_readable, 0);
    }

    #[test]
    fn box_written_lanes_are_never_counted_as_host_observed() {
        let m = manifest("container");
        for lane in ["tee-shim", "inbox-capture", "something-added-later"] {
            let s = signals(&m, &[receipt(lane, 0)]);
            assert_eq!(s.host_observed, 0, "{lane} must not read as host-observed");
            assert_eq!(s.box_claimed, 1, "{lane}");
            assert!(s.box_claimed_only, "{lane} alone means box-claimed only");
        }
        for lane in HOST_OBSERVED_LANES {
            let s = signals(&m, &[receipt(lane, 0)]);
            assert_eq!(s.host_observed, 1, "{lane} is a host lane");
            assert!(!s.box_claimed_only, "{lane}");
        }
    }

    /// A DNS-rebinding page arrives with Host and Origin that match each other,
    /// so comparing them proves nothing. The Host has to name loopback.
    #[test]
    fn the_gate_requires_a_loopback_host() {
        for good in ["127.0.0.1:8080", "localhost:1", "[::1]:9", "LOCALHOST", "127.0.0.1"] {
            assert!(is_loopback_host(good), "{good} should be accepted");
        }
        for bad in ["evil.test:8080", "example.com", "127.0.0.1.evil.test", "10.0.0.1:80"] {
            assert!(!is_loopback_host(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn a_refused_destination_is_the_only_thing_that_turns_a_box_red() {
        let m = manifest("container");
        // A failing test run is worth attention, but it is not the boundary
        // saying no. The distinction the old dashboard's red/amber split
        // existed to preserve.
        let noisy = signals(&m, &[receipt("host-env-run", 1)]);
        assert_eq!(noisy.verdict, "attention");
        assert_eq!(noisy.failed, 1);

        let mut denied = receipt("host-env-run", 0);
        denied.egress = Some(EgressSummary {
            allowed: 3,
            denied: 2,
            hosts: vec![EgressHost {
                host: "evil.test".into(),
                port: 443,
                allowed: 0,
                denied: 2,
            }],
            hosts_truncated: false,
            log: None,
        });
        let red = signals(&m, &[denied]);
        assert_eq!(red.verdict, "denial");
        assert_eq!(red.egress_denied, 2);
        assert_eq!(red.denied_hosts, vec!["evil.test:443".to_string()]);
    }

    #[test]
    fn fs_overlap_takes_the_newest_run_record_including_an_empty_one() {
        let m = manifest("process");
        let mut first = receipt("host-env-run", 0);
        first.fs_overlap = vec!["env/tester/other via /tmp".into()];
        // A newer run recorded no overlap (the other box is gone): the pane
        // must clear, not remember. Overlap is a property of the host now.
        let second = receipt("host-env-run", 0);
        let s = signals(&m, &[first.clone(), second]);
        assert!(s.fs_overlap.is_empty());
        // Reversed order: the overlapping record is newest and wins.
        let older = receipt("host-env-run", 0);
        let s = signals(&m, &[older, first.clone()]);
        assert_eq!(s.fs_overlap, vec!["env/tester/other via /tmp".to_string()]);
        // A box-claimed lane never updates it: the box does not get to say
        // who it shares paths with.
        let mut claimed = receipt("tee-shim", 0);
        claimed.fs_overlap = vec!["env/tester/fake via /x".into()];
        let s = signals(&m, &[first, claimed]);
        assert_eq!(s.fs_overlap, vec!["env/tester/other via /tmp".to_string()]);
        // And overlap alone never moves the verdict: it is policy, not
        // enforcement firing.
        assert_eq!(s.verdict, "clean");
    }

    #[test]
    fn weak_evidence_is_reported_beside_the_verdict_not_folded_into_it() {
        // Workspace tier confines nothing, and a box that only ever saw its
        // own tee shim has no host-observed record. Both are properties of the
        // evidence, so neither is allowed to masquerade as a clean run or as a
        // boundary trip.
        let clean = signals(&manifest("workspace"), &[receipt("tee-shim", 0)]);
        assert_eq!(clean.verdict, "clean");
        assert!(clean.weak_isolation);
        assert!(clean.box_claimed_only);
        assert_eq!(clean.box_claimed, 1);
        assert_eq!(clean.host_observed, 0);

        let observed = signals(&manifest("container"), &[receipt("host-env-run", 0)]);
        assert!(!observed.weak_isolation);
        assert!(!observed.box_claimed_only);
    }

    #[test]
    fn what_the_page_reported_counts_as_something_to_look_at() {
        let mut r = receipt("host-env-run", 0);
        r.browser = Some(BrowserEvidence {
            verb: Some("click".into()),
            console: vec!["TypeError: x is not a function".into()],
            errors: vec![],
            failed_requests: vec!["GET /api/me → (blocked)".into()],
            truncated: false,
            unavailable: false,
        });
        let s = signals(&manifest("container"), &[r]);
        assert_eq!(s.browser_issues, 2);
        assert_eq!(s.verdict, "attention");
    }

    #[test]
    fn the_most_pressing_box_sorts_first() {
        let m = manifest("container");
        let mut denied = receipt("host-env-run", 0);
        denied.egress = Some(EgressSummary {
            allowed: 0,
            denied: 1,
            hosts: vec![],
            hosts_truncated: false,
            log: None,
        });
        assert!(rank(&signals(&m, &[denied])) > rank(&signals(&m, &[receipt("host-env-run", 1)])));
        assert!(
            rank(&signals(&m, &[receipt("host-env-run", 1)]))
                > rank(&signals(&m, &[receipt("host-env-run", 0)]))
        );
    }

    #[test]
    fn nothing_gets_in_without_the_token() {
        let tok = "0123456789abcdef";
        let ok = |q: Option<&str>, c: Option<&str>| {
            authorize(q, c, None, &[], tok, "http://127.0.0.1:8765").is_ok()
        };
        assert!(ok(Some("token=0123456789abcdef"), None));
        assert!(ok(None, Some("h5i_console=0123456789abcdef")));
        assert!(ok(Some("fps=10&token=0123456789abcdef"), None));
        assert!(!ok(None, None));
        assert!(!ok(Some("token=0123456789abcdee"), None));
        assert!(!ok(None, Some("h5i_console=nope")));
        // A cookie for something else on loopback is not this console's token.
        assert!(!ok(None, Some("jupyter=0123456789abcdef")));
    }

    #[test]
    fn a_page_on_another_origin_is_refused_even_holding_the_token() {
        let tok = "0123456789abcdef";
        let me = "http://127.0.0.1:8765";
        assert_eq!(
            authorize(
                Some("token=0123456789abcdef"),
                None,
                Some("http://evil.test"),
                &[],
                tok,
                me
            ),
            Err(Refusal::ForeignOrigin)
        );
        // Same-origin XHR sends Origin too, and must still work.
        assert!(
            authorize(None, Some("h5i_console=0123456789abcdef"), Some(me), &["same-origin"], tok, me)
                .is_ok()
        );
    }

    /// `--open` hands the URL to a browser opener, and on Linux
    /// `/proc/<pid>/cmdline` is world-readable, so the session token would be
    /// legible to every other uid for the life of that process. The handoff is
    /// a different token and it is spent by the page load it exists for.
    #[test]
    fn the_url_handed_to_a_browser_is_single_use_and_is_not_the_session_token() {
        let c = Console::bind(std::env::temp_dir(), 0).expect("binds");
        let once = c.handoff_url().expect("mints");
        assert!(!once.contains(&c.state.token), "the session token must not travel: {once}");
        let presented = once.rsplit_once("token=").expect("carries one").1.to_string();

        // It works, once.
        assert!(c.state.handoff().is_some());
        assert!(authorize(
            Some(&format!("token={presented}")),
            None,
            None,
            &[],
            &c.state.handoff().unwrap(),
            "http://127.0.0.1:1",
        )
        .is_ok());
        c.state.spend_handoff();
        assert!(c.state.handoff().is_none(), "and is gone after it is spent");

        // A second mint replaces the first rather than adding to it.
        let a = c.handoff_url().expect("mints");
        let b = c.handoff_url().expect("mints");
        assert_ne!(a, b);
        assert!(b.contains(&c.state.handoff().unwrap()));
    }

    /// The other half of the same fact: cookies have no port. A page on any
    /// other loopback port cannot *ride* the console cookie (above) but could
    /// set one, and reading only the first value let it lock the operator out.
    #[test]
    fn a_shadowing_cookie_from_another_loopback_port_does_not_lock_the_console() {
        let tok = "0123456789abcdef";
        let me = "http://127.0.0.1:8765";
        // RFC 6265 §5.4 puts the longer path first, so this is the order the
        // browser actually sends after `document.cookie = "h5i_console=junk;
        // path=/api"` on another port.
        let shadowed = Some("h5i_console=junk; h5i_console=0123456789abcdef");
        assert!(authorize(None, shadowed, None, &["same-origin"], tok, me).is_ok());
        // And a head carrying only wrong values is still refused.
        assert_eq!(
            authorize(None, Some("h5i_console=junk; h5i_console=nope"), None, &[], tok, me),
            Err(Refusal::Unauthorized)
        );
    }

    /// The gap `Origin` alone cannot close.
    /// A markup-driven GET (`<img>`, `<script src>`, `<link>`) sends no
    /// `Origin` at all, and the console's cookie is `SameSite=Strict`, which
    /// constrains cross-*site* requests only. Two loopback ports are different
    /// origins and the same site, so a page served by any other local service,
    /// and `h5i join` puts somebody else's agent-written app on exactly such a
    /// port, reached this console with its cookie attached.
    #[test]
    fn a_page_on_another_loopback_port_cannot_ride_the_console_cookie() {
        let tok = "0123456789abcdef";
        let me = "http://127.0.0.1:8765";
        let cookie = Some("h5i_console=0123456789abcdef");

        // What a subresource fetch from 127.0.0.1:<other> looks like: no
        // `Origin`, the cookie attached, `same-site` because the port is not
        // part of a site.
        assert_eq!(
            authorize(None, cookie, None, &["same-site"], tok, me),
            Err(Refusal::ForeignOrigin)
        );
        // And from off the machine entirely, on the cookie alone.
        assert_eq!(
            authorize(None, cookie, None, &["cross-site"], tok, me),
            Err(Refusal::ForeignOrigin)
        );
        // A head that says two things does not get to pick the generous one.
        assert_eq!(
            authorize(None, cookie, None, &["same-origin", "cross-site"], tok, me),
            Err(Refusal::ForeignOrigin)
        );

        // The console's own requests, and a URL typed or clicked from outside
        // the browser, still work.
        assert!(authorize(None, cookie, None, &["same-origin"], tok, me).is_ok());
        assert!(authorize(None, cookie, None, &["none"], tok, me).is_ok());

        // ...and so does the printed invite link, wherever it was clicked from:
        // the capability arrived *with* the request rather than out of the jar,
        // which is the whole distinction.
        assert!(
            authorize(
                Some("token=0123456789abcdef"),
                None,
                None,
                &["cross-site"],
                tok,
                me
            )
            .is_ok()
        );
    }

    #[test]
    fn the_console_binds_loopback_and_puts_the_token_in_the_url() {
        let c = Console::bind(PathBuf::from("."), 0).expect("bind an ephemeral port");
        let url = c.url().expect("a bound listener has an address");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(url.contains("?token="), "{url}");
        assert_eq!(c.state.token.len(), TOKEN_BYTES * 2);
    }
}
