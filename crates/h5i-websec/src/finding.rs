//! `h5i websec finding`: what the agent concluded, and the evidence it stands
//! on (design-websec.md W23).
//!
//! The division of labour is the same one the rest of this file set keeps.
//! The agent writes the claim: the title, the state, the notes. h5i writes
//! nothing about whether the claim is true, and asserts one thing only, that
//! every message id named here is a message this session actually holds. A
//! finding pointing at evidence that does not exist is the failure worth
//! refusing; a finding whose author was wrong is an ordinary finding.
//!
//! `state` is deliberately free text. In the ledger a state drives the fold,
//! so it is an enum; here it drives nothing, and an agent that has to pick the
//! nearest of five words spends a turn on vocabulary instead of on the target.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use h5i_core::browser_session as bs;
use h5i_wire::read::sequences;
use h5i_wire::record::now_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The findings, inside a session directory.
pub const FINDINGS_DIR: &str = "findings";

/// The log itself, one JSON entry per line.
pub const FINDINGS_FILE: &str = "findings.jsonl";

/// The longest a title, state or note may be.
///
/// The agent writes these, but the agent is quoting a target often enough, and
/// this file is on the operator's disk.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// How much of a findings log one read will fold.
pub const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

/// One write. The first entry for an id creates the finding; the rest change
/// it. Append-only, so what an agent believed at turn 30 is still readable at
/// turn 300, which is the half of this that a report generator would lose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Message ids, in the order they were offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// A file that reproduces it: a sequence, or an experiment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repro: Option<String>,
}

/// One note, and when it was written.
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub at: String,
    pub text: String,
}

/// A finding, as folding its entries produces it.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    /// The agent's word for where this stands. Anything it likes.
    pub state: String,
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repro: Option<String>,
    pub notes: Vec<Note>,
    pub created: String,
    pub updated: String,
}

impl Finding {
    /// The list view: enough to decide which one to open.
    pub fn brief(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "state": self.state,
            "evidence": self.evidence.len(),
            "notes": self.notes.len(),
            "updated": self.updated,
        })
    }
}

/// The findings log, inside a session directory.
pub struct Findings {
    path: PathBuf,
}

impl Findings {
    pub fn open(root: &Path, session: &str) -> anyhow::Result<Self> {
        let dir = bs::dir(root, session).join(FINDINGS_DIR);
        std::fs::create_dir_all(&dir)?;
        owner_only(&dir);
        Ok(Self {
            path: dir.join(FINDINGS_FILE),
        })
    }

    /// Every finding, oldest first.
    ///
    /// A line that does not parse is skipped rather than guessed at: two
    /// processes appending at once can tear one, and half an agent's claim is
    /// not a smaller claim.
    pub fn read(&self) -> anyhow::Result<Vec<Finding>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        if text.len() as u64 > MAX_READ_BYTES {
            anyhow::bail!(
                "{} is larger than {MAX_READ_BYTES} bytes, which is more than one read folds",
                self.path.display()
            );
        }
        let entries: Vec<Entry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(fold(&entries))
    }

    pub fn find(&self, id: &str) -> anyhow::Result<Finding> {
        let want = normalise_id(id)?;
        self.read()?
            .into_iter()
            .find(|finding| finding.id == want)
            .ok_or_else(|| anyhow::anyhow!("this session has no {want}"))
    }

    /// Append one entry. One `write_all` on an append-only file, so two
    /// processes interleave whole entries rather than lines.
    pub fn append(&self, entry: &Entry) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut file = open_owner_only(&self.path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// The next free id.
    ///
    /// Read then write, so two processes creating at the same moment can pick
    /// the same one. The fold merges them rather than losing either, which is
    /// a worse finding and not a lost one.
    pub fn next_id(&self) -> anyhow::Result<String> {
        let highest = self
            .read()?
            .iter()
            .filter_map(|finding| finding.id.strip_prefix("finding_")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        Ok(format!("finding_{}", highest + 1))
    }
}

/// `finding_7` and `7` both name finding 7. Anything else is refused rather
/// than parsed as far as it goes.
pub fn normalise_id(id: &str) -> anyhow::Result<String> {
    let bare = id.strip_prefix("finding_").unwrap_or(id);
    bare.parse::<u64>()
        .map(|n| format!("finding_{n}"))
        .map_err(|_| anyhow::anyhow!("`{id}` is not a finding id: try `finding_7` or `7`"))
}

/// Entries into findings, oldest first.
pub fn fold(entries: &[Entry]) -> Vec<Finding> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::BTreeMap<String, Finding> = std::collections::BTreeMap::new();
    for entry in entries {
        let finding = by_id.entry(entry.id.clone()).or_insert_with(|| {
            order.push(entry.id.clone());
            Finding {
                id: entry.id.clone(),
                title: String::new(),
                state: String::new(),
                evidence: Vec::new(),
                repro: None,
                notes: Vec::new(),
                created: entry.at.clone(),
                updated: entry.at.clone(),
            }
        });
        // Named fields replace, because a title is a statement of what this is
        // and two of them are one wrong. Notes and evidence accumulate,
        // because they are what was learned and it does not stop being true.
        if let Some(title) = &entry.title {
            finding.title = title.clone();
        }
        if let Some(state) = &entry.state {
            finding.state = state.clone();
        }
        if let Some(repro) = &entry.repro {
            finding.repro = Some(repro.clone());
        }
        if let Some(note) = &entry.note {
            finding.notes.push(Note {
                at: entry.at.clone(),
                text: note.clone(),
            });
        }
        for id in &entry.evidence {
            if !finding.evidence.contains(id) {
                finding.evidence.push(id.clone());
            }
        }
        finding.updated = entry.at.clone();
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// What `--evidence req_42,res_43` names, checked against the store.
///
/// The one assertion this verb makes. A finding whose evidence names nothing
/// is worthless later and reads exactly like one that does, so it is refused
/// now rather than discovered at the end of the engagement.
pub fn check_evidence(store: &Path, specs: &[String]) -> anyhow::Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    for spec in specs {
        for one in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let bare = one
                .strip_prefix("req_")
                .or_else(|| one.strip_prefix("res_"))
                .unwrap_or(one);
            let seq: u64 = bare.parse().map_err(|_| {
                anyhow::anyhow!("`{one}` is not a message id: try `req_42`, `res_42` or `42`")
            })?;
            let have = sequences(store);
            if !have.contains(&seq) {
                anyhow::bail!(
                    "this session holds no message {seq}, so a finding citing it would name \
                     nothing. It holds: {}",
                    listed(&have)
                );
            }
            // Kept as written, prefix and all: `req_42` and `res_42` are the
            // request and the response, and which half was the evidence is
            // part of the claim.
            let kept = if one.starts_with("req_") || one.starts_with("res_") {
                one.to_string()
            } else {
                format!("req_{seq}")
            };
            if !ids.contains(&kept) {
                ids.push(kept);
            }
        }
    }
    Ok(ids)
}

/// What a session holds, in a line rather than a wall of numbers.
fn listed(have: &[u64]) -> String {
    const SHOWN: usize = 20;
    if have.is_empty() {
        return "nothing yet".to_string();
    }
    let head: Vec<String> = have.iter().take(SHOWN).map(u64::to_string).collect();
    if have.len() <= SHOWN {
        return head.join(", ");
    }
    format!(
        "{}, … and {} more, up to {}",
        head.join(", "),
        have.len() - SHOWN,
        have.last().copied().unwrap_or_default()
    )
}

/// Refuse text that is too long rather than storing it truncated.
pub fn bounded(what: &str, text: &str) -> anyhow::Result<String> {
    if text.len() > MAX_TEXT_BYTES {
        anyhow::bail!(
            "the {what} is {} bytes, and {MAX_TEXT_BYTES} is the most one holds",
            text.len()
        );
    }
    Ok(text.to_string())
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

/// A finding quotes a target's bytes and an agent's reasoning about them. Same
/// rule the message store and the ledger follow: owner-only from creation, and
/// never in an export.
fn open_owner_only(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// Build the entry a `create` or an `update` writes.
#[allow(clippy::too_many_arguments)]
pub fn entry(
    id: &str,
    title: Option<&str>,
    state: Option<&str>,
    note: Option<&str>,
    evidence: Vec<String>,
    repro: Option<&str>,
) -> anyhow::Result<Entry> {
    Ok(Entry {
        id: id.to_string(),
        at: now_rfc3339(),
        title: title.map(|t| bounded("title", t)).transpose()?,
        state: state.map(|s| bounded("state", s)).transpose()?,
        note: note.map(|n| bounded("note", n)).transpose()?,
        evidence,
        repro: repro.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_at(at: &str, id: &str) -> Entry {
        Entry {
            id: id.to_string(),
            at: at.to_string(),
            title: None,
            state: None,
            note: None,
            evidence: Vec::new(),
            repro: None,
        }
    }

    #[test]
    fn a_title_replaces_and_notes_accumulate() {
        let mut first = entry_at("t1", "finding_1");
        first.title = Some("cross-tenant invoice".to_string());
        first.note = Some("bob sees alice's".to_string());
        let mut second = entry_at("t2", "finding_1");
        second.title = Some("cross-tenant invoice read".to_string());
        second.note = Some("only on the JSON endpoint".to_string());

        let folded = fold(&[first, second]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].title, "cross-tenant invoice read");
        assert_eq!(folded[0].notes.len(), 2, "what was learned does not stop being true");
        assert_eq!(folded[0].created, "t1");
        assert_eq!(folded[0].updated, "t2");
    }

    #[test]
    fn evidence_is_a_set_that_keeps_its_order() {
        let mut first = entry_at("t1", "finding_1");
        first.evidence = vec!["req_42".to_string(), "res_43".to_string()];
        let mut second = entry_at("t2", "finding_1");
        second.evidence = vec!["res_43".to_string(), "req_99".to_string()];
        let folded = fold(&[first, second]);
        assert_eq!(folded[0].evidence, ["req_42", "res_43", "req_99"]);
    }

    /// Nothing here decides what a state means, so nothing here restricts one.
    #[test]
    fn a_state_is_whatever_the_agent_wrote() {
        let mut only = entry_at("t1", "finding_1");
        only.state = Some("filter-bypass-worked?".to_string());
        assert_eq!(fold(&[only])[0].state, "filter-bypass-worked?");
    }

    #[test]
    fn a_long_store_is_named_in_a_line() {
        let many: Vec<u64> = (0..500).collect();
        let said = listed(&many);
        assert!(said.contains("and 480 more, up to 499"), "{said}");
        assert_eq!(listed(&[]), "nothing yet");
        assert_eq!(listed(&[1, 2]), "1, 2");
    }

    #[test]
    fn an_id_is_read_with_or_without_its_prefix() {
        assert_eq!(normalise_id("finding_7").unwrap(), "finding_7");
        assert_eq!(normalise_id("7").unwrap(), "finding_7");
        for bad in ["7x", "finding_", "", "one"] {
            assert!(normalise_id(bad).is_err(), "`{bad}` should be refused");
        }
    }

    #[test]
    fn the_next_id_follows_the_highest_one_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let findings = Findings {
            path: dir.path().join("findings.jsonl"),
        };
        assert_eq!(findings.next_id().unwrap(), "finding_1");
        findings.append(&entry_at("t1", "finding_1")).expect("append");
        findings.append(&entry_at("t2", "finding_4")).expect("append");
        assert_eq!(findings.next_id().unwrap(), "finding_5");
    }

    #[test]
    fn a_torn_line_is_skipped_rather_than_guessed_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("findings.jsonl");
        let findings = Findings { path: path.clone() };
        findings.append(&entry_at("t1", "finding_1")).expect("append");
        std::fs::write(
            &path,
            format!("{}{{\"id\":\"finding_2\",\"at\":\n", std::fs::read_to_string(&path).unwrap()),
        )
        .expect("write");
        assert_eq!(findings.read().unwrap().len(), 1);
    }
}
