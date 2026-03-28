/// `h5i resume [branch]` — structured AI session handoff.
///
/// Assembles a briefing from locally-stored h5i data (no API calls required):
///   - context workspace goal and milestone progress
///   - last session statistics (message count, tool calls, files edited)
///   - high-risk files ranked by uncertainty history + churn
///   - causal exposure of the HEAD commit
///   - memory changes since the last snapshot
///   - a template-generated opening prompt for the next agent session
use chrono::{DateTime, TimeZone, Utc};
use console::style;
use std::collections::HashMap;
use std::path::Path;

use crate::ctx::{self, ContextOpts};
use crate::error::H5iError;
use crate::memory;
use crate::repository::H5iRepository;
use crate::session_log;

// ── Data types ────────────────────────────────────────────────────────────────

/// One file's composite risk derived from its uncertainty and churn history.
#[derive(Debug)]
pub struct RiskyFile {
    pub path: String,
    pub uncertainty_count: usize,
    /// Average confidence across all uncertainty signals for this file (0 = most uncertain).
    pub avg_confidence: f32,
    /// edit / (edit + read) ratio from the last session.
    pub churn_score: f32,
    /// Weighted risk score 0.0–1.0.
    pub risk_score: f32,
    /// The most-frequently-occurring uncertainty phrase for this file.
    pub top_phrase: Option<String>,
}

/// Everything needed to print a complete session handoff briefing.
#[derive(Debug)]
pub struct ResumeBriefing {
    // Git
    pub git_branch: String,
    pub head_oid: String,
    pub head_message: String,
    pub last_active: DateTime<Utc>,
    pub agent: Option<String>,
    pub model: Option<String>,

    // Context workspace (.h5i-ctx/)
    pub ctx_initialized: bool,
    pub ctx_branch: String,
    pub goal: String,
    pub completed_milestones: Vec<String>,
    pub pending_milestones: Vec<String>,
    pub recent_ctx_commits: Vec<String>,

    // Session analysis (.git/.h5i/session_log/)
    pub session_id: Option<String>,
    pub message_count: usize,
    pub tool_call_count: usize,
    pub edited_file_count: usize,

    // Risk
    pub risky_files: Vec<RiskyFile>,

    // Causal exposure
    pub causal_descendants: usize,

    // Memory changes: (added, removed, modified) file counts
    pub memory_changes: Option<(usize, usize, usize)>,

    // Suggested opening prompt for the next session
    pub suggested_prompt: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Assemble a `ResumeBriefing` from all locally-stored h5i data.
///
/// `branch` is an optional git branch name; defaults to the current HEAD branch.
/// No AI API calls are made.
pub fn generate_briefing(
    repo: &H5iRepository,
    workdir: &Path,
    branch: Option<&str>,
) -> Result<ResumeBriefing, H5iError> {
    let git = repo.git();

    // ── 1. Resolve branch + HEAD commit ───────────────────────────────────────
    let (git_branch, head_oid) = match branch {
        Some(b) => {
            let oid = git
                .find_branch(b, git2::BranchType::Local)?
                .get()
                .peel_to_commit()?
                .id();
            (b.to_string(), oid)
        }
        None => {
            let head = git.head()?;
            let name = head.shorthand().unwrap_or("HEAD").to_string();
            let oid = head.peel_to_commit()?.id();
            (name, oid)
        }
    };

    // ── 2. Commit metadata ────────────────────────────────────────────────────
    let git_commit = git.find_commit(head_oid)?;
    let head_message = git_commit.summary().unwrap_or("").to_string();
    let head_oid_str = head_oid.to_string();

    let h5i_record = repo.load_h5i_record(head_oid).ok();
    let (agent, model, last_active) = if let Some(ref r) = h5i_record {
        let ai = r.ai_metadata.as_ref();
        (
            ai.map(|a| a.agent_id.clone()).filter(|s| !s.is_empty()),
            ai.map(|a| a.model_name.clone()).filter(|s| !s.is_empty()),
            r.timestamp,
        )
    } else {
        let secs = git_commit.time().seconds();
        let dt = Utc
            .timestamp_opt(secs, 0)
            .single()
            .unwrap_or_else(Utc::now);
        (None, None, dt)
    };

    // ── 3. Context workspace ──────────────────────────────────────────────────
    let ctx_initialized = ctx::is_initialized(workdir);
    let (ctx_branch, goal, completed_milestones, pending_milestones, recent_ctx_commits) =
        if ctx_initialized {
            let opts = ContextOpts {
                branch: branch.map(|s| s.to_string()),
                window: 5,
                ..Default::default()
            };
            match ctx::gcc_context(workdir, &opts) {
                Ok(gcc) => {
                    let (done, todo): (Vec<_>, Vec<_>) = gcc
                        .milestones
                        .iter()
                        .partition(|m| m.contains("[x]") || m.contains("[X]"));
                    (
                        gcc.current_branch,
                        gcc.project_goal,
                        done.iter().map(|m| strip_milestone_marker(m)).collect(),
                        todo.iter().map(|m| strip_milestone_marker(m)).collect(),
                        gcc.recent_commits,
                    )
                }
                Err(_) => default_ctx_fields(&git_branch),
            }
        } else {
            default_ctx_fields(&git_branch)
        };

    // ── 4. Session analysis ───────────────────────────────────────────────────
    let analysis = find_recent_analysis(repo, &head_oid_str, 10);

    let (session_id, message_count, tool_call_count, edited_file_count, risky_files) =
        match &analysis {
            Some(a) => {
                let risky = compute_risky_files(a);
                (
                    Some(a.session_id.clone()),
                    a.message_count,
                    a.tool_call_count,
                    a.footprint.edited.len(),
                    risky,
                )
            }
            None => (None, 0, 0, 0, vec![]),
        };

    // ── 5. Causal exposure ────────────────────────────────────────────────────
    let causal_descendants = repo.causal_dependents(head_oid, 200).len();

    // ── 6. Memory changes ─────────────────────────────────────────────────────
    let memory_changes = compute_memory_summary(&repo.h5i_root, workdir);

    // ── 7. Suggested prompt ───────────────────────────────────────────────────
    let suggested_prompt = build_suggested_prompt(
        &goal,
        &completed_milestones,
        &pending_milestones,
        &risky_files,
    );

    Ok(ResumeBriefing {
        git_branch,
        head_oid: head_oid_str,
        head_message,
        last_active,
        agent,
        model,
        ctx_initialized,
        ctx_branch,
        goal,
        completed_milestones,
        pending_milestones,
        recent_ctx_commits,
        session_id,
        message_count,
        tool_call_count,
        edited_file_count,
        risky_files,
        causal_descendants,
        memory_changes,
        suggested_prompt,
    })
}

/// Print a formatted handoff briefing to stdout.
pub fn print_briefing(b: &ResumeBriefing) {
    // ── Header ────────────────────────────────────────────────────────────────
    println!(
        "{}",
        style("── Session Handoff ─────────────────────────────────────────────────").dim()
    );
    println!(
        "  {}  ·  Last active: {}",
        style(format!("Branch: {}", b.git_branch)).bold(),
        style(b.last_active.format("%Y-%m-%d %H:%M UTC")).dim(),
    );
    if b.agent.is_some() || b.model.is_some() {
        println!(
            "  Agent: {}  ·  Model: {}",
            style(b.agent.as_deref().unwrap_or("unknown")).cyan(),
            style(b.model.as_deref().unwrap_or("unknown")).cyan(),
        );
    }
    println!(
        "  HEAD: {}  {}",
        style(&b.head_oid[..8.min(b.head_oid.len())]).magenta(),
        style(&b.head_message).dim().italic(),
    );
    println!();

    // ── Goal & milestones ─────────────────────────────────────────────────────
    if b.ctx_initialized && !b.goal.is_empty() {
        println!("  {}", style("Goal").bold());
        println!("    {}", style(&b.goal).italic());
        println!();
        if !b.completed_milestones.is_empty() || !b.pending_milestones.is_empty() {
            println!("  {}", style("Progress").bold());
            for m in &b.completed_milestones {
                println!("    {} {}", style("✔").green(), style(m).dim());
            }
            for (i, m) in b.pending_milestones.iter().enumerate() {
                if i == 0 {
                    println!(
                        "    {} {}  {}",
                        style("○").yellow(),
                        style(m).bold(),
                        style("← resume here").yellow().dim()
                    );
                } else {
                    println!("    {} {}", style("○").dim(), style(m).dim());
                }
            }
            println!();
        }
    } else if !b.ctx_initialized {
        println!(
            "  {} Context workspace not initialized. Run {} to track goal and milestones.",
            style("ℹ").blue(),
            style("h5i context init --goal \"...\"").bold()
        );
        println!();
    }

    // ── Last session ──────────────────────────────────────────────────────────
    if b.message_count > 0 {
        println!("  {}", style("Last Session").bold());
        let sid = b.session_id.as_deref().unwrap_or("unknown");
        println!(
            "    {}  ·  {} messages  ·  {} tool calls  ·  {} file{} edited",
            style(&sid[..8.min(sid.len())]).magenta(),
            style(b.message_count).cyan(),
            style(b.tool_call_count).cyan(),
            style(b.edited_file_count).cyan(),
            if b.edited_file_count == 1 { "" } else { "s" },
        );
        println!();
    } else {
        println!(
            "  {} No session analysis found. Run {} after a session to enable risk tracking.",
            style("ℹ").blue(),
            style("h5i notes analyze").bold()
        );
        println!();
    }

    // ── High-risk files ───────────────────────────────────────────────────────
    if !b.risky_files.is_empty() {
        println!(
            "  {}",
            style("⚠  High-Risk Files  (review before continuing)").bold().yellow()
        );
        let max_risk = b
            .risky_files
            .iter()
            .map(|f| f.risk_score)
            .fold(0.0_f32, f32::max);
        for f in b.risky_files.iter().take(5) {
            let bar_filled = if max_risk > 0.0 {
                ((f.risk_score / max_risk) * 10.0).round() as usize
            } else {
                0
            }
            .min(10);
            let bar = format!(
                "{}{}",
                style("█".repeat(bar_filled)).red(),
                style("░".repeat(10 - bar_filled)).dim(),
            );
            let phrase_note = f
                .top_phrase
                .as_deref()
                .map(|p| format!("  \"{}\"", p))
                .unwrap_or_default();
            println!(
                "    {}  {:<38}  {} signal{}  churn {:.0}%{}",
                bar,
                style(shorten_path(&f.path, 38)).yellow(),
                style(f.uncertainty_count).red(),
                if f.uncertainty_count == 1 { " " } else { "s" },
                f.churn_score * 100.0,
                style(phrase_note).dim().italic(),
            );
        }
        println!();
    }

    // ── Causal exposure ───────────────────────────────────────────────────────
    if b.causal_descendants > 0 {
        println!(
            "  {} {} later commit{} causally depend{} on HEAD — review before pushing.",
            style("⚠").yellow(),
            style(b.causal_descendants).yellow().bold(),
            if b.causal_descendants == 1 { "" } else { "s" },
            if b.causal_descendants == 1 { "s" } else { "" },
        );
        println!();
    }

    // ── Memory changes ────────────────────────────────────────────────────────
    if let Some((added, removed, modified)) = b.memory_changes {
        if added + removed + modified > 0 {
            println!("  {}", style("Memory Changes Since Last Snapshot").bold());
            if added > 0 {
                println!(
                    "    {} {} file{} added",
                    style("+").green(),
                    added,
                    if added == 1 { "" } else { "s" }
                );
            }
            if removed > 0 {
                println!(
                    "    {} {} file{} removed",
                    style("-").red(),
                    removed,
                    if removed == 1 { "" } else { "s" }
                );
            }
            if modified > 0 {
                println!(
                    "    {} {} file{} modified",
                    style("~").yellow(),
                    modified,
                    if modified == 1 { "" } else { "s" }
                );
            }
            println!(
                "    {} Run {} to see the full diff.",
                style("ℹ").blue(),
                style("h5i memory diff").bold()
            );
            println!();
        }
    }

    // ── Recent context commits ────────────────────────────────────────────────
    if !b.recent_ctx_commits.is_empty() {
        println!("  {}", style("Recent Context Commits").bold());
        for c in b.recent_ctx_commits.iter().take(3) {
            let preview: String = c.chars().take(80).collect();
            println!("    {} {}", style("◈").dim(), style(preview).dim());
        }
        println!();
    }

    // ── Suggested opening prompt ──────────────────────────────────────────────
    println!("  {}", style("Suggested Opening Prompt").bold());
    println!("  {}", style("─".repeat(68)).dim());
    for line in word_wrap(&b.suggested_prompt, 70) {
        println!("  {}", style(line).italic().cyan());
    }
    println!("  {}", style("─".repeat(68)).dim());
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn default_ctx_fields(branch: &str) -> (String, String, Vec<String>, Vec<String>, Vec<String>) {
    (branch.to_string(), String::new(), vec![], vec![], vec![])
}

/// Strip markdown checkbox markers: `- [x] Foo` or `- [ ] Foo` → `Foo`.
fn strip_milestone_marker(s: &str) -> String {
    s.trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim_start_matches("[x] ")
        .trim_start_matches("[X] ")
        .trim_start_matches("[ ] ")
        .trim()
        .to_string()
}

/// Walk back from `head_oid_str` (inclusive) looking for any stored session analysis.
/// Returns the first one found within `search_depth` commits, or `None`.
fn find_recent_analysis(
    repo: &H5iRepository,
    head_oid_str: &str,
    search_depth: usize,
) -> Option<session_log::SessionAnalysis> {
    // Try the exact HEAD first
    if let Ok(Some(a)) = session_log::load_analysis(&repo.h5i_root, head_oid_str) {
        return Some(a);
    }
    // Walk ancestors
    let head_oid = git2::Oid::from_str(head_oid_str).ok()?;
    let mut revwalk = repo.git().revwalk().ok()?;
    revwalk.push(head_oid).ok()?;
    for oid in revwalk.take(search_depth).flatten() {
        let s = oid.to_string();
        if s == head_oid_str {
            continue;
        }
        if let Ok(Some(a)) = session_log::load_analysis(&repo.h5i_root, &s) {
            return Some(a);
        }
    }
    None
}

/// Compute per-file risk scores from a session analysis.
/// Risk = 40 % uncertainty level + 30 % churn + 30 % signal density (normalised).
fn compute_risky_files(analysis: &session_log::SessionAnalysis) -> Vec<RiskyFile> {
    // Collect confidence values and phrases per file
    let mut file_data: HashMap<String, (Vec<f32>, Vec<String>)> = HashMap::new();
    for ann in &analysis.uncertainty {
        if ann.context_file.is_empty() {
            continue;
        }
        let entry = file_data.entry(ann.context_file.clone()).or_default();
        entry.0.push(ann.confidence);
        entry.1.push(ann.phrase.clone());
    }

    // Churn lookup
    let churn_map: HashMap<&str, f32> = analysis
        .churn
        .iter()
        .map(|c| (c.file.as_str(), c.churn_score))
        .collect();

    let max_count = file_data.values().map(|(v, _)| v.len()).max().unwrap_or(1) as f32;

    let mut risky: Vec<RiskyFile> = file_data
        .into_iter()
        .map(|(file, (confs, phrases))| {
            let count = confs.len();
            let avg_conf = confs.iter().sum::<f32>() / count as f32;
            let churn_score = churn_map.get(file.as_str()).copied().unwrap_or(0.0);
            let risk_score = 0.4 * (1.0 - avg_conf)
                + 0.3 * churn_score
                + 0.3 * (count as f32 / max_count);

            // Most-frequent phrase
            let top_phrase = {
                let mut freq: HashMap<String, usize> = HashMap::new();
                for p in &phrases {
                    *freq.entry(p.clone()).or_insert(0) += 1;
                }
                freq.into_iter().max_by_key(|(_, c)| *c).map(|(p, _)| p)
            };

            RiskyFile {
                path: file,
                uncertainty_count: count,
                avg_confidence: avg_conf,
                churn_score,
                risk_score,
                top_phrase,
            }
        })
        .collect();

    risky.sort_by(|a, b| b.risk_score.partial_cmp(&a.risk_score).unwrap());
    risky
}

/// Diff the most recent memory snapshot against live memory and return
/// `(added, removed, modified)` file counts, or `None` if no snapshots exist.
fn compute_memory_summary(
    h5i_root: &Path,
    workdir: &Path,
) -> Option<(usize, usize, usize)> {
    let snapshots = memory::list_snapshots(h5i_root).ok()?;
    if snapshots.is_empty() {
        return None;
    }
    // Pick the most recent snapshot
    let latest = snapshots.iter().max_by_key(|s| s.timestamp)?;
    let diff = memory::diff_snapshots(h5i_root, workdir, &latest.commit_oid, None).ok()?;
    Some((diff.added_files.len(), diff.removed_files.len(), diff.modified_files.len()))
}

/// Build the template-based suggested opening prompt from briefing fields.
fn build_suggested_prompt(
    goal: &str,
    done: &[String],
    pending: &[String],
    risky: &[RiskyFile],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !goal.is_empty() {
        parts.push(format!("Continue: {}.", goal));
    }

    match done.len() {
        0 => {}
        1 => parts.push(format!("Completed: {}.", done[0])),
        n => parts.push(format!(
            "Completed: {} and {} more milestone{}.",
            done[0],
            n - 1,
            if n - 1 == 1 { "" } else { "s" }
        )),
    }

    if let Some(next) = pending.first() {
        parts.push(format!("Next milestone: {}.", next));
    }

    if let Some(top) = risky.first() {
        let short = shorten_path(&top.path, 40);
        let phrase_note = top
            .top_phrase
            .as_deref()
            .map(|p| format!(" Last session flagged \"{}\" here.", p))
            .unwrap_or_default();
        parts.push(format!(
            "Review {} carefully before editing.{}",
            short, phrase_note
        ));
    }

    if parts.is_empty() {
        "Resume where we left off.".to_string()
    } else {
        parts.join(" ")
    }
}

fn shorten_path(p: &str, max: usize) -> String {
    if p.len() <= max {
        p.to_string()
    } else {
        format!("…{}", &p[p.len().saturating_sub(max - 1)..])
    }
}

/// Naive word-wrap at `width` characters, splitting only on spaces.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current.clone());
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::{
        CausalChain, ExplorationFootprint, FileChurn, SessionAnalysis, UncertaintyAnnotation,
    };
    use chrono::Utc;

    // ── strip_milestone_marker ────────────────────────────────────────────────

    #[test]
    fn strip_milestone_marker_removes_checkbox_done() {
        assert_eq!(strip_milestone_marker("- [x] Set up CI"), "Set up CI");
        assert_eq!(strip_milestone_marker("- [X] Set up CI"), "Set up CI");
    }

    #[test]
    fn strip_milestone_marker_removes_checkbox_todo() {
        assert_eq!(strip_milestone_marker("- [ ] Add rate limiting"), "Add rate limiting");
    }

    #[test]
    fn strip_milestone_marker_removes_bullet_only() {
        assert_eq!(strip_milestone_marker("- Plain item"), "Plain item");
        assert_eq!(strip_milestone_marker("* Plain item"), "Plain item");
    }

    #[test]
    fn strip_milestone_marker_plain_text_unchanged() {
        assert_eq!(strip_milestone_marker("No marker here"), "No marker here");
    }

    // ── word_wrap ─────────────────────────────────────────────────────────────

    #[test]
    fn word_wrap_short_text_is_single_line() {
        let lines = word_wrap("Hello world", 40);
        assert_eq!(lines, vec!["Hello world"]);
    }

    #[test]
    fn word_wrap_wraps_at_boundary() {
        let text = "one two three four five";
        let lines = word_wrap(text, 12);
        // "one two" = 7, "three four" = 10, "five" = 4
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(l.len() <= 12, "line too long: {:?}", l);
        }
    }

    #[test]
    fn word_wrap_empty_string_returns_empty() {
        assert!(word_wrap("", 20).is_empty());
    }

    #[test]
    fn word_wrap_single_long_word_is_not_split() {
        let lines = word_wrap("superlongwordthatexceedswidth", 10);
        assert_eq!(lines, vec!["superlongwordthatexceedswidth"]);
    }

    // ── shorten_path ──────────────────────────────────────────────────────────

    #[test]
    fn shorten_path_short_path_unchanged() {
        assert_eq!(shorten_path("src/auth.rs", 20), "src/auth.rs");
    }

    #[test]
    fn shorten_path_long_path_truncated() {
        let p = "src/very/deep/nested/module/auth.rs";
        let result = shorten_path(p, 20);
        // …  is 3 bytes but 1 char; check visual width via char count
        assert!(result.chars().count() <= 20);
        assert!(result.starts_with('…'));
        assert!(result.ends_with("auth.rs"));
    }

    #[test]
    fn shorten_path_exact_max_unchanged() {
        let p = "x".repeat(20);
        assert_eq!(shorten_path(&p, 20), p);
    }

    // ── build_suggested_prompt ────────────────────────────────────────────────

    #[test]
    fn build_suggested_prompt_fallback_when_empty() {
        let p = build_suggested_prompt("", &[], &[], &[]);
        assert_eq!(p, "Resume where we left off.");
    }

    #[test]
    fn build_suggested_prompt_includes_goal() {
        let p = build_suggested_prompt("Build an OAuth2 login system", &[], &[], &[]);
        assert!(p.contains("Build an OAuth2 login system"));
    }

    #[test]
    fn build_suggested_prompt_includes_next_milestone() {
        let p = build_suggested_prompt(
            "Build auth",
            &["Initial setup".to_string()],
            &["Add rate limiting".to_string()],
            &[],
        );
        assert!(p.contains("Add rate limiting"));
    }

    #[test]
    fn build_suggested_prompt_multiple_done_milestones() {
        let done = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let p = build_suggested_prompt("Goal", &done, &[], &[]);
        // Should mention "A and 2 more milestones"
        assert!(p.contains('A'));
        assert!(p.contains('2'));
    }

    #[test]
    fn build_suggested_prompt_mentions_risky_file() {
        let risky = vec![RiskyFile {
            path: "src/auth.rs".to_string(),
            uncertainty_count: 3,
            avg_confidence: 0.2,
            churn_score: 0.8,
            risk_score: 0.7,
            top_phrase: Some("not sure".to_string()),
        }];
        let p = build_suggested_prompt("", &[], &[], &risky);
        assert!(p.contains("auth.rs"));
        assert!(p.contains("not sure"));
    }

    // ── compute_risky_files ───────────────────────────────────────────────────

    fn make_analysis(annotations: Vec<UncertaintyAnnotation>, churn: Vec<FileChurn>) -> SessionAnalysis {
        SessionAnalysis {
            session_id: "test-session".to_string(),
            footprint: ExplorationFootprint::default(),
            causal_chain: CausalChain::default(),
            uncertainty: annotations,
            omissions: vec![],
            churn,
            replay_hash: String::new(),
            analyzed_at: Utc::now(),
            message_count: 10,
            tool_call_count: 5,
        }
    }

    fn ann(file: &str, confidence: f32, phrase: &str) -> UncertaintyAnnotation {
        UncertaintyAnnotation {
            context_file: file.to_string(),
            snippet: String::new(),
            phrase: phrase.to_string(),
            confidence,
            turn: 0,
        }
    }

    #[test]
    fn compute_risky_files_ranks_by_risk_score() {
        let analysis = make_analysis(
            vec![
                ann("src/auth.rs", 0.1, "not sure"),  // very uncertain
                ann("src/auth.rs", 0.2, "not sure"),
                ann("src/utils.rs", 0.9, "perhaps"),  // mostly confident
            ],
            vec![
                FileChurn { file: "src/auth.rs".to_string(), edit_count: 8, read_count: 2, churn_score: 0.8 },
            ],
        );
        let risky = compute_risky_files(&analysis);
        assert!(!risky.is_empty());
        assert_eq!(risky[0].path, "src/auth.rs");
    }

    #[test]
    fn compute_risky_files_skips_empty_file() {
        let analysis = make_analysis(
            vec![ann("", 0.1, "not sure")],
            vec![],
        );
        // Empty context_file should be ignored
        assert!(compute_risky_files(&analysis).is_empty());
    }

    #[test]
    fn compute_risky_files_top_phrase_is_most_frequent() {
        let analysis = make_analysis(
            vec![
                ann("src/a.rs", 0.3, "not sure"),
                ann("src/a.rs", 0.3, "not sure"),
                ann("src/a.rs", 0.3, "let me check"),
            ],
            vec![],
        );
        let risky = compute_risky_files(&analysis);
        assert_eq!(risky[0].top_phrase.as_deref(), Some("not sure"));
    }
}
