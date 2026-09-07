//! The request log, and the reason it is not merely a log.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use h5i_error::H5iError;
use serde::{Deserialize, Serialize};

// The row itself is data, and data with two readers that must not link an
// engine to name it: the websec plugin and recon's ledger. It lives in
// `h5i-wire` and is re-exported here so every caller keeps the one import it
// has always used (design-websec.md W21, design-recon.md N4).
pub use h5i_wire::record::{Initiator, Phase, RequestRecord, now_rfc3339};

/// Somewhere records are durably written.
///
/// The trait returns a `Result` for exactly one reason: so a failure to record
/// can stop a fetch. An implementation that swallows its errors turns the
/// fail-closed guarantee back into a hope.
pub trait Sink: Send + Sync + 'static {
    fn append(&self, record: &RequestRecord) -> Result<(), H5iError>;
}

/// A JSON-lines file, one record per line.
pub struct JsonlSink {
    file: Mutex<File>,
}

impl JsonlSink {
    pub fn create(path: &Path) -> Result<Self, H5iError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| H5iError::with_path(e, parent))?;
        }
        let file = open_owner_only(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

/// Open a session artifact for appending, readable only by its owner.
///
/// The request log names every URL this session fetched and the action log names
/// every verb the agent ran, which together are a complete account of what the
/// agent was doing. h5i writes them into the session directory, and a boxed
/// session's directory is under a `/tmp` the `agent` profile shares with the
/// host, so the umask's 0644 published the account to anything on the machine.
/// The mode is set at creation, so there is no window in which the file exists
/// and is readable; an existing file is narrowed too.
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
        // `mode` applies only when the file is created. A log left behind by an
        // earlier session (a `--restore`, a crash) keeps whatever it had.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

impl Sink for JsonlSink {
    fn append(&self, record: &RequestRecord) -> Result<(), H5iError> {
        let line = serde_json::to_string(record)?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| H5iError::Internal("receipt sink lock was poisoned".to_string()))?;
        // Flush rather than buffer: a record that is only in our address space
        // has not recorded anything if the process dies mid-request.
        writeln!(file, "{line}").map_err(H5iError::Io)?;
        file.flush().map_err(H5iError::Io)?;
        Ok(())
    }
}

/// A sink that accepts everything and keeps nothing.
///
/// Not the same as having no sink. The broker always has one, and the
/// fail-closed rule is about what happens when a sink *refuses*. This one
/// never does. It is what a session without `--receipts` writes to: the
/// broker's own in-memory log is still kept, and still printed at the end.
pub struct NullSink;

impl Sink for NullSink {
    fn append(&self, _record: &RequestRecord) -> Result<(), H5iError> {
        Ok(())
    }
}

/// An in-memory sink, for tests and for `--dry-run` style inspection.
#[derive(Default)]
pub struct MemorySink {
    records: Mutex<Vec<RequestRecord>>,
}

impl MemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn records(&self) -> Vec<RequestRecord> {
        self.records.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// The highest sequence number written so far, or `None` on an empty log.
    ///
    /// The mark a verb takes before it runs, so the receipts written while it
    /// ran can be identified afterwards. The highest rather than the count:
    /// numbers are taken before the append, so append order and sequence order
    /// can differ and a count would drift.
    pub fn high_water(&self) -> Option<u64> {
        self.records()
            .iter()
            .map(|r| r.seq)
            .max()
    }

    /// Sequence numbers written after `mark`, deduplicated and in order.
    ///
    /// A request and its response share a sequence number, so the pair collapses
    /// to one entry: what a reader wants is "which fetches", not "how many rows".
    pub fn since(&self, mark: Option<u64>) -> Vec<u64> {
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

    /// Just the URLs that actually reached the wire, in order.
    pub fn fetched_urls(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter(|r| r.phase == Phase::Request && r.allowed)
            .map(|r| r.url)
            .collect()
    }

    pub fn denied_urls(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter(|r| r.phase == Phase::Request && !r.allowed)
            .map(|r| r.url)
            .collect()
    }
}

impl Sink for MemorySink {
    fn append(&self, record: &RequestRecord) -> Result<(), H5iError> {
        self.records
            .lock()
            .map_err(|_| H5iError::Internal("receipt sink lock was poisoned".to_string()))?
            .push(record.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn jsonl_sink_writes_one_parseable_line_per_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("requests.jsonl");
        let sink = JsonlSink::create(&path).expect("sink creates its parent directory");

        let req = RequestRecord::request(1, Initiator::Navigation, "GET", "https://example.com/");
        sink.append(&req).expect("append request");
        let mut resp = req.response();
        resp.status = Some(200);
        sink.append(&resp).expect("append response");

        let mut contents = String::new();
        File::open(&path)
            .expect("open log")
            .read_to_string(&mut contents)
            .expect("read log");

        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let parsed: RequestRecord = serde_json::from_str(line).expect("each line is a record");
            assert_eq!(parsed.seq, 1);
        }
    }

    #[test]
    fn memory_sink_separates_what_was_fetched_from_what_was_refused() {
        let sink = MemorySink::new();
        sink.append(&RequestRecord::request(
            1,
            Initiator::Navigation,
            "GET",
            "https://example.com/",
        ))
        .unwrap();
        sink.append(
            &RequestRecord::request(2, Initiator::Subresource, "GET", "https://tracker.test/p")
                .denied("nope"),
        )
        .unwrap();

        assert_eq!(sink.fetched_urls(), vec!["https://example.com/"]);
        assert_eq!(sink.denied_urls(), vec!["https://tracker.test/p"]);
    }
}

// ── the agent's own verbs ───────────────────────────────────────────────────

/// One verb an agent asked the resident session for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    pub seq: u64,
    /// When the engine wrote this row, RFC3339. The engine's own claim, like
    /// [`RequestRecord::at`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    /// `request` before the verb runs, `result` after it.
    pub phase: String,
    pub verb: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Receipt sequence numbers written while this verb ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<u64>,
}

/// How a verb went, for [`ActionLog::finish`].
///
/// A struct rather than five more parameters: the two `Option<String>`s next to
/// each other were a call site nobody could read without counting.
#[derive(Debug, Default, Clone)]
pub struct ActionOutcome {
    pub ok: bool,
    pub url: Option<String>,
    pub error: Option<String>,
    /// Receipt sequence numbers this verb caused.
    pub requests: Vec<u64>,
}

/// Where the resident session records what it was asked to do.
pub struct ActionLog {
    file: Mutex<File>,
    seq: AtomicU64,
}

impl ActionLog {
    pub fn create(path: &Path) -> Result<Self, H5iError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| H5iError::with_path(e, parent))?;
        }
        let file = open_owner_only(path)?;
        Ok(Self {
            file: Mutex::new(file),
            seq: AtomicU64::new(0),
        })
    }

    /// Record that a verb is about to run, and return its sequence number.
    ///
    /// Before, not after, and the failure is propagated: no record, no action,
    /// the same rule the request log enforces for fetches. Recording afterwards
    /// would make a full disk into an agent that acts invisibly.
    ///
    /// Worth being exact about what that buys, since the lane is box-claimed: it
    /// is a guarantee against *accident*, a bad path or a full disk. It is not a
    /// guarantee against a box that has decided to lie.
    pub fn begin(&self, verb: &str, target: Option<&str>) -> Result<u64, H5iError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.write(&ActionRecord {
            seq,
            at: now_rfc3339(),
            phase: "request".to_string(),
            verb: verb.to_string(),
            target: target.map(str::to_string),
            ok: None,
            url: None,
            error: None,
            requests: Vec::new(),
        })?;
        Ok(seq)
    }

    /// Record how it went. Best-effort: the verb has already happened, so
    /// refusing anything now would only hide the outcome of something real.
    pub fn finish(&self, seq: u64, verb: &str, target: Option<&str>, outcome: ActionOutcome) {
        let _ = self.write(&ActionRecord {
            seq,
            at: now_rfc3339(),
            phase: "result".to_string(),
            verb: verb.to_string(),
            target: target.map(str::to_string),
            ok: Some(outcome.ok),
            url: outcome.url,
            error: outcome.error,
            requests: outcome.requests,
        });
    }

    /// An [`ActionLog`] whose every write fails, for the one test that has to
    /// prove "no record, no action" holds at the *verb*, not merely at startup.
    ///
    /// A read-only handle rather than a clever filesystem: unlinking the file
    /// does not break writes through an fd that is already open, which is how
    /// the first attempt at that test passed while proving nothing.
    #[cfg(test)]
    pub(crate) fn unwritable_for_test(path: &Path) -> Result<Self, H5iError> {
        std::fs::write(path, "").map_err(|e| H5iError::with_path(e, path))?;
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| H5iError::with_path(e, path))?;
        Ok(Self {
            file: Mutex::new(file),
            seq: AtomicU64::new(0),
        })
    }

    fn write(&self, record: &ActionRecord) -> Result<(), H5iError> {
        let line = serde_json::to_string(record)?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| H5iError::Internal("action log lock was poisoned".to_string()))?;
        writeln!(file, "{line}").map_err(H5iError::Io)?;
        file.flush().map_err(H5iError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod action_log_tests {
    use super::*;

    #[test]
    fn a_verb_is_recorded_before_it_runs_and_again_after() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("browser-actions.jsonl");
        let log = ActionLog::create(&path).expect("creates, making the directory");

        let seq = log.begin("click", Some("@e1")).expect("records");
        log.finish(
            seq,
            "click",
            Some("@e1"),
            ActionOutcome {
                ok: true,
                url: Some("https://example.com/".to_string()),
                requests: vec![4, 5],
                ..Default::default()
            },
        );

        let lines: Vec<ActionRecord> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is a record"))
            .collect();

        assert_eq!(lines.len(), 2, "a pair, like the request log");
        assert_eq!(lines[0].phase, "request");
        assert_eq!(lines[0].ok, None, "the first line cannot know the outcome");
        assert_eq!(lines[1].phase, "result");
        assert_eq!(lines[1].ok, Some(true));
        assert_eq!(lines[1].seq, seq, "the pair shares a sequence number");
    }

    #[test]
    fn a_failed_verb_is_recorded_as_fully_as_a_successful_one() {
        // The rows that matter most for a reviewer are the ones where the
        // agent did not get what it asked for.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.jsonl");
        let log = ActionLog::create(&path).expect("creates");

        let seq = log.begin("navigate", Some("https://denied.test/")).unwrap();
        log.finish(
            seq,
            "navigate",
            Some("https://denied.test/"),
            ActionOutcome {
                ok: false,
                error: Some("denied by policy".to_string()),
                ..Default::default()
            },
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("denied by policy"), "{text}");
        assert!(text.contains("\"ok\":false"), "{text}");
    }

    #[test]
    fn an_unwritable_log_refuses_rather_than_recording_nothing() {
        // No record, no action. The same rule the request log enforces for
        // fetches. A directory where the file should be is the cheapest way to
        // make the open fail without depending on permissions.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("occupied");
        std::fs::create_dir(&path).unwrap();
        assert!(
            ActionLog::create(&path).is_err(),
            "a session that cannot record must fail at startup"
        );
    }

    #[test]
    fn sequence_numbers_do_not_repeat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = ActionLog::create(&dir.path().join("a.jsonl")).unwrap();
        let seqs: Vec<u64> = (0..5).map(|_| log.begin("scroll", None).unwrap()).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }
}
