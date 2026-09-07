//! What a run was asked to do, and how far it got, so a resume runs the same
//! job rather than a similar one (design-recon.md N12).

use std::path::{Path, PathBuf};

use h5i_error::H5iError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where job records live, inside the recon directory.
pub const JOBS_DIR: &str = "jobs";

/// One run of a verb that spends requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// The verb that ran: `paths` or `crawl`.
    pub verb: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Everything the run was given, so a resume is the same job.
    pub args: Value,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub written: u64,
    /// Why it ended, when it ended for a reason worth naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

impl Job {
    pub fn new(verb: &str, args: Value) -> Self {
        let at = h5i_wire::record::now_rfc3339();
        // The clock names it, so jobs sort in the order they ran; the pid ends
        // it, so two runs starting in the same microsecond are two records.
        let id = format!(
            "job_{}_{}",
            at.replace([':', '-', '.', 'T', 'Z'], ""),
            std::process::id()
        );
        Self {
            id,
            verb: verb.to_string(),
            started_at: at,
            ended_at: None,
            args,
            requests: 0,
            written: 0,
            stopped: None,
        }
    }

    /// Whether this run stopped before it ran out of work.
    pub fn is_finished(&self) -> bool {
        self.ended_at.is_some()
    }
}

/// The jobs directory for a session's recon state.
pub fn dir(recon: &Path) -> PathBuf {
    recon.join(JOBS_DIR)
}

/// Write a job record, whole or not at all.
pub fn save(recon: &Path, job: &Job) -> Result<(), H5iError> {
    let dir = dir(recon);
    std::fs::create_dir_all(&dir).map_err(|e| H5iError::with_path(e, &dir))?;
    let path = dir.join(format!("{}.json", job.id));
    let staging = path.with_extension("json.new");
    let text = serde_json::to_string_pretty(job).map_err(H5iError::Serialization)?;
    std::fs::write(&staging, text).map_err(|e| H5iError::with_path(e, &staging))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&staging, &path).map_err(|e| H5iError::with_path(e, &path))
}

/// The most job records one listing reads.
pub const MAX_JOBS_LISTED: usize = 500;

/// Every job this session has recorded, oldest first, newest kept when there
/// are more than [`MAX_JOBS_LISTED`].
pub fn list(recon: &Path) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(recon)) else {
        return jobs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(job) = serde_json::from_str::<Job>(&text)
        {
            jobs.push(job);
        }
    }
    jobs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    if jobs.len() > MAX_JOBS_LISTED {
        jobs.drain(..jobs.len() - MAX_JOBS_LISTED);
    }
    jobs
}

/// One job by id, or the newest when no id is named.
pub fn find(recon: &Path, id: Option<&str>) -> Option<Job> {
    let jobs = list(recon);
    match id {
        Some(id) => jobs.into_iter().find(|job| job.id == id),
        None => jobs.into_iter().next_back(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_records_what_it_was_asked_to_do() {
        let dir = tempfile::tempdir().expect("tempdir");
        let job = Job::new("paths", serde_json::json!({"wordlist": "words.txt"}));
        save(dir.path(), &job).expect("save");

        let read = list(dir.path());
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].args["wordlist"], "words.txt");
        assert!(!read[0].is_finished(), "a job that never ended says so");
    }

    #[test]
    fn a_listing_keeps_the_newest_and_says_nothing_about_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..MAX_JOBS_LISTED + 10 {
            let mut job = Job::new("paths", Value::Null);
            job.id = format!("job_{n:06}");
            job.started_at = format!("2026-01-01T00:00:{n:06}Z");
            save(dir.path(), &job).expect("save");
        }
        let listed = list(dir.path());
        assert_eq!(listed.len(), MAX_JOBS_LISTED);
        assert_eq!(
            listed.last().expect("newest").id,
            format!("job_{:06}", MAX_JOBS_LISTED + 9),
            "a resume with no id means the newest run"
        );
    }

    #[test]
    fn the_newest_job_is_the_one_a_bare_resume_means() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut first = Job::new("paths", Value::Null);
        first.id = "job_20260101T000000".to_string();
        first.started_at = "2026-01-01T00:00:00.000000Z".to_string();
        let mut second = Job::new("crawl", Value::Null);
        second.id = "job_20260202T000000".to_string();
        second.started_at = "2026-02-02T00:00:00.000000Z".to_string();
        save(dir.path(), &first).expect("save");
        save(dir.path(), &second).expect("save");

        assert_eq!(find(dir.path(), None).expect("newest").verb, "crawl");
        assert_eq!(
            find(dir.path(), Some("job_20260101T000000")).expect("named").verb,
            "paths"
        );
        assert!(find(dir.path(), Some("job_nope")).is_none());
    }
}
