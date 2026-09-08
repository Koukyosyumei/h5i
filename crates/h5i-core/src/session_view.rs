//! What the console shows about browser sessions: the account, and which of
//! them wants a human ([`attention`], from evidence rather than a score).
//!
//! Nothing here reads a stored message. That store holds bodies, cookies and
//! `Authorization` in full, so reading it stays a command someone types.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::browser_session as bs;

/// How much of a ledger the console folds on one poll. Smaller than recon's own
/// cap: this runs every few seconds, for every session.
pub const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;

/// The most receipts one detail view returns.
pub const MAX_REQUESTS_SHOWN: usize = 200;

/// The most endpoints one detail view returns.
pub const MAX_ENDPOINTS_SHOWN: usize = 500;

/// Anything newer than this counts as "now" for the working/idle line.
pub const RECENT_SECONDS: i64 = 60;

/// How loudly a session is asking to be looked at.
///
/// Five states, herdr's words because the problem is herdr's. `done` is the one
/// a client clears by looking, so the server reports what is true and each
/// client remembers its own seen set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attention {
    /// `blocked`, `working`, `done`, `idle` or `unknown`.
    pub state: &'static str,
    /// The evidence that produced the state, in one clause. A badge without
    /// this is a score, and this console does not score.
    pub why: String,
}

impl Attention {
    fn new(state: &'static str, why: impl Into<String>) -> Self {
        Self {
            state,
            why: why.into(),
        }
    }
}

/// What a session's own files say about it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionSignals {
    /// Requests the engine recorded, both phases counted once.
    pub requests: usize,
    /// Requests policy refused. Kept apart from the total: a refusal is the
    /// boundary working, not a failure.
    pub denied: usize,
    /// Origins this session reached, most recent first, capped.
    pub origins: Vec<String>,
    /// The last request's timestamp, RFC3339, when there is one.
    pub last_request_at: Option<String>,
    /// Whether the session was opened with `--capture`, and how many messages
    /// are stored. The bytes stay on disk; this is a count.
    pub captured: Option<usize>,
    /// What a reclaimed store left behind. Kept apart from `captured`, because
    /// "the bytes were kept and then reclaimed" and "the bytes were never
    /// kept" are different facts.
    pub reclaimed: Option<bs::Reclaimed>,
    /// The recon ledger, folded into counts by state.
    pub ledger: Option<LedgerCounts>,
    /// Recon runs that spent requests, newest last.
    pub jobs: Vec<JobRow>,
}

/// A ledger, as a fleet row needs it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LedgerCounts {
    pub candidate: usize,
    pub observed: usize,
    pub confirmed: usize,
    pub refused: usize,
    pub gone: usize,
    /// Lines folded, which is what `--since` counts in.
    pub cursor: u64,
    /// Lines that would not read as an observation.
    pub unreadable: u64,
    /// Whether the fold stopped at [`MAX_LEDGER_BYTES`].
    pub truncated: bool,
}

/// One recorded run of a verb that spends requests.
#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub id: String,
    pub verb: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub requests: u64,
    pub written: u64,
    pub stopped: Option<String>,
}

/// Which of the five states this session is in, and why. Strict about
/// `blocked`: only two things block, and both are somebody waiting on a person.
pub fn attention(
    session: &bs::Session,
    signals: &SessionSignals,
    holder_is_human: bool,
    engine_reachable: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Attention {
    if holder_is_human {
        return Attention::new(
            "blocked",
            "a human holds the control lock, so the agent cannot drive until `h5i browser release`",
        );
    }
    if let Some(job) = signals.jobs.last()
        && let Some(why) = &job.stopped
        && job.ended_at.is_some()
        && needs_a_person(why)
    {
        return Attention::new("blocked", format!("the last {} run stopped: {why}", job.verb));
    }
    if !session.state.is_live() {
        let ended = session.end_reason.clone().unwrap_or_else(|| {
            format!("the session is {}", state_word(&session.state))
        });
        return Attention::new("done", ended);
    }
    if !engine_reachable {
        return Attention::new(
            "unknown",
            "the record says live and the engine's control file is gone, so this cannot be classified",
        );
    }
    if let Some(job) = signals.jobs.last()
        && job.ended_at.is_none()
    {
        return Attention::new("working", format!("a {} run is in flight", job.verb));
    }
    match signals.last_request_at.as_deref().and_then(seconds_since(now)) {
        Some(age) if age <= RECENT_SECONDS => Attention::new(
            "working",
            format!("{} requests, the last {age}s ago", signals.requests),
        ),
        Some(age) => Attention::new(
            "idle",
            format!("live, and nothing since {age}s ago"),
        ),
        None => Attention::new("idle", "live, and nothing fetched yet"),
    }
}

/// Whether a run's stopping reason is one only a person can answer.
///
/// Spending the allowance the operator set is a bounded run ending as asked; a
/// login that went away, or a policy that refused, is a question.
pub fn needs_a_person(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    if reason.contains("allowance") || reason.contains("ran out of frontier") {
        return false;
    }
    reason.contains("logged in")
        || reason.contains("login")
        || reason.contains("refused")
        || reason.contains("not in the allowlist")
        || reason.contains("could not be sent")
}

fn state_word(state: &bs::State) -> &'static str {
    match state {
        bs::State::Live => "live",
        bs::State::Closed => "closed",
        bs::State::Died => "dead",
        bs::State::Expired => "expired",
        _ => "ended",
    }
}

/// How long ago a timestamp was, in seconds, or `None` when it will not parse.
fn seconds_since(now: chrono::DateTime<chrono::Utc>) -> impl Fn(&str) -> Option<i64> {
    move |at: &str| {
        chrono::DateTime::parse_from_rfc3339(at)
            .ok()
            .map(|then| (now - then.with_timezone(&chrono::Utc)).num_seconds().max(0))
    }
}

/// Fold a session's request log into the counts a row shows.
///
/// Off the log rather than out of the engine: the console is read-only and
/// asking a live session would be driving it.
pub fn signals_from_receipts(records: &[serde_json::Value]) -> SessionSignals {
    let mut signals = SessionSignals::default();
    for record in records {
        let phase = record.get("phase").and_then(|v| v.as_str()).unwrap_or("");
        if phase != "request" {
            continue;
        }
        signals.requests += 1;
        if record.get("allowed").and_then(|v| v.as_bool()) == Some(false) {
            signals.denied += 1;
        }
        if let Some(at) = record.get("at").and_then(|v| v.as_str()) {
            signals.last_request_at = Some(at.to_string());
        }
        if let Some(url) = record.get("url").and_then(|v| v.as_str())
            && let Ok(parsed) = url::Url::parse(url)
            && let Some(host) = parsed.host_str()
        {
            let origin = match parsed.port() {
                Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
                None => format!("{}://{host}", parsed.scheme()),
            };
            if !signals.origins.contains(&origin) && signals.origins.len() < 8 {
                signals.origins.push(origin);
            }
        }
    }
    signals
}

/// The recon ledger beside a session, folded into counts.
pub fn ledger_counts(session_dir: &Path) -> Option<LedgerCounts> {
    let path = ledger_path(session_dir);
    let bytes = std::fs::read(&path).ok()?;
    let truncated = bytes.len() as u64 > MAX_LEDGER_BYTES;
    let head = if truncated {
        &bytes[..MAX_LEDGER_BYTES as usize]
    } else {
        &bytes[..]
    };
    let text = String::from_utf8_lossy(head);
    let inventory = h5i_wire::ledger::fold(text.lines());
    let mut counts = LedgerCounts {
        cursor: inventory.cursor,
        unreadable: inventory.unreadable,
        truncated,
        ..LedgerCounts::default()
    };
    for endpoint in &inventory.endpoints {
        let slot = match endpoint.state {
            h5i_wire::ledger::State::Candidate => &mut counts.candidate,
            h5i_wire::ledger::State::Observed => &mut counts.observed,
            h5i_wire::ledger::State::Confirmed => &mut counts.confirmed,
            h5i_wire::ledger::State::Refused => &mut counts.refused,
            h5i_wire::ledger::State::Gone => &mut counts.gone,
        };
        *slot += 1;
    }
    Some(counts)
}

/// The endpoints themselves, for a detail view. Capped.
pub fn ledger_endpoints(session_dir: &Path) -> Vec<h5i_wire::ledger::Endpoint> {
    let path = ledger_path(session_dir);
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let head = &bytes[..bytes.len().min(MAX_LEDGER_BYTES as usize)];
    let text = String::from_utf8_lossy(head);
    let mut endpoints = h5i_wire::ledger::fold(text.lines()).endpoints;
    endpoints.truncate(MAX_ENDPOINTS_SHOWN);
    endpoints
}

fn ledger_path(session_dir: &Path) -> PathBuf {
    session_dir.join("recon").join("ledger.jsonl")
}

/// Recon's job records for a session, oldest first.
pub fn jobs(session_dir: &Path) -> Vec<JobRow> {
    let dir = session_dir.join("recon").join("jobs");
    let mut jobs: Vec<JobRow> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return jobs;
    };
    for entry in entries.flatten().take(500) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let field = |name: &str| {
            value
                .get(name)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let number = |name: &str| value.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        jobs.push(JobRow {
            id: field("id").unwrap_or_default(),
            verb: field("verb").unwrap_or_default(),
            started_at: field("started_at").unwrap_or_default(),
            ended_at: field("ended_at"),
            requests: number("requests"),
            written: number("written"),
            stopped: field("stopped"),
        });
    }
    jobs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    jobs
}

/// How many messages a session stored, or `None` when it captured nothing.
///
/// A count, never the bytes: that directory is the one artifact h5i keeps that
/// holds credentials in full.
pub fn captured(session_dir: &Path) -> Option<usize> {
    let dir = session_dir.join(bs::MESSAGES_DIR);
    let entries = std::fs::read_dir(&dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".request.json"))
            })
            .count(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(state: bs::State) -> bs::Session {
        bs::Session {
            id: "br_test".into(),
            name: None,
            engine: bs::Engine::H5iLight,
            lane: bs::Lane::EngineClaimed,
            placement: bs::Placement::Host,
            url: "https://target.test/".into(),
            started_at: "2026-09-07T10:00:00Z".into(),
            expires_at: None,
            storage: bs::Storage::Ephemeral,
            policy_digest: "sha256:test".into(),
            identity: "native".into(),
            identity_digest: "test".into(),
            restored_from: None,
            state,
            ended_at: None,
            end_reason: None,
            confinement: crate::browser_sandbox::Confinement::Process,
            enclosing_box: None,
            control: bs::Control::default(),
            logs: bs::Logs::default(),
            permissive_cors: false,
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    #[test]
    fn a_human_holding_the_lock_is_the_loudest_thing_on_the_screen() {
        let verdict = attention(&session(bs::State::Live), &SessionSignals::default(), true, true, now());
        assert_eq!(verdict.state, "blocked");
        assert!(verdict.why.contains("control lock"), "{}", verdict.why);
    }

    #[test]
    fn a_bounded_run_that_spent_its_allowance_is_not_a_question() {
        // The operator asked for 120 requests and got 120. Marking that
        // `blocked` would put every finished run in the loudest column.
        let signals = SessionSignals {
            jobs: vec![JobRow {
                id: "job_1".into(),
                verb: "paths".into(),
                started_at: "2026-09-07T10:00:00Z".into(),
                ended_at: Some("2026-09-07T10:01:00Z".into()),
                requests: 120,
                written: 120,
                stopped: Some("spent its allowance of 120 requests".into()),
            }],
            last_request_at: Some("2026-09-07T10:01:00Z".into()),
            ..SessionSignals::default()
        };
        let verdict = attention(&session(bs::State::Live), &signals, false, true, now());
        assert_ne!(verdict.state, "blocked", "{}", verdict.why);
    }

    #[test]
    fn a_run_that_stopped_for_a_reason_asks_for_a_person() {
        let signals = SessionSignals {
            jobs: vec![JobRow {
                id: "job_1".into(),
                verb: "paths".into(),
                started_at: "2026-09-07T10:00:00Z".into(),
                ended_at: Some("2026-09-07T10:01:00Z".into()),
                requests: 40,
                written: 40,
                stopped: Some(
                    "the page this walk started from answers differently now, so the session \
                     is no longer logged in as `alice`"
                        .into(),
                ),
            }],
            ..SessionSignals::default()
        };
        let verdict = attention(&session(bs::State::Live), &signals, false, true, now());
        assert_eq!(verdict.state, "blocked");
        assert!(verdict.why.contains("logged in"), "{}", verdict.why);
    }

    #[test]
    fn a_live_session_that_just_fetched_is_working_and_a_quiet_one_is_not() {
        let mut signals = SessionSignals {
            requests: 3,
            last_request_at: Some(
                (now() - chrono::Duration::seconds(5)).to_rfc3339(),
            ),
            ..SessionSignals::default()
        };
        assert_eq!(
            attention(&session(bs::State::Live), &signals, false, true, now()).state,
            "working"
        );

        signals.last_request_at = Some((now() - chrono::Duration::seconds(600)).to_rfc3339());
        let quiet = attention(&session(bs::State::Live), &signals, false, true, now());
        assert_eq!(quiet.state, "idle");
        assert!(quiet.why.contains("600s"), "{}", quiet.why);
    }

    #[test]
    fn an_ended_session_is_done_until_a_client_has_looked() {
        let verdict = attention(
            &session(bs::State::Closed),
            &SessionSignals::default(),
            false,
            false,
            now(),
        );
        assert_eq!(
            verdict.state, "done",
            "the server reports what is true; each client remembers what it has seen"
        );
    }

    #[test]
    fn a_live_record_with_no_engine_is_not_classified() {
        let verdict = attention(
            &session(bs::State::Live),
            &SessionSignals::default(),
            false,
            false,
            now(),
        );
        assert_eq!(verdict.state, "unknown");
        assert!(verdict.why.contains("control file"), "{}", verdict.why);
    }

    #[test]
    fn receipts_fold_into_counts_that_keep_refusals_apart() {
        let records = vec![
            json!({"phase": "request", "at": "2026-09-07T10:00:00Z", "url": "https://a.test/one", "allowed": true}),
            json!({"phase": "response", "at": "2026-09-07T10:00:01Z", "url": "https://a.test/one", "allowed": true, "status": 200}),
            json!({"phase": "request", "at": "2026-09-07T10:00:02Z", "url": "https://b.test/x", "allowed": false}),
        ];
        let signals = signals_from_receipts(&records);
        assert_eq!(signals.requests, 2, "a request and its response are one fetch");
        assert_eq!(signals.denied, 1);
        assert_eq!(signals.origins, vec!["https://a.test", "https://b.test"]);
        assert_eq!(signals.last_request_at.as_deref(), Some("2026-09-07T10:00:02Z"));
    }
}
