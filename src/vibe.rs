//! `h5i vibe` — instant AI footprint audit for a repository.
//!
//! Scans recent commits to answer:
//! - What fraction of this codebase was AI-generated?
//! - Which directories are fully AI-written?
//! - Where are the riskiest files (high AI %, no tests, blind edits)?

use std::collections::{HashMap, HashSet};

use crate::error::H5iError;
use crate::repository::H5iRepository;
use crate::session_log;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct VibeReport {
    pub repo_name: String,
    /// Number of commits scanned.
    pub total_commits: usize,
    /// Commits that carry `AiMetadata`.
    pub ai_commits: usize,
    /// Distinct human author names seen.
    pub human_authors: Vec<String>,
    /// (model_name, commit_count) sorted by count descending.
    pub ai_models: Vec<(String, usize)>,
    /// Per-directory AI concentration, sorted by ratio descending.
    pub dir_stats: Vec<DirAiStat>,
    /// Sum of blind edits across all analysed sessions.
    pub total_blind_edits: usize,
    /// Number of distinct files with at least one blind edit.
    pub blind_edit_file_count: usize,
    /// Top risky files (high AI %, no tests, uncertainty/blind-edit signals).
    pub risky_files: Vec<RiskyFile>,
}

impl VibeReport {
    pub fn ai_pct(&self) -> f32 {
        if self.total_commits == 0 {
            0.0
        } else {
            self.ai_commits as f32 / self.total_commits as f32 * 100.0
        }
    }
}

#[derive(Debug)]
pub struct DirAiStat {
    pub path: String,
    pub total_commits: usize,
    pub ai_commits: usize,
}

impl DirAiStat {
    pub fn ai_ratio(&self) -> f32 {
        if self.total_commits == 0 {
            0.0
        } else {
            self.ai_commits as f32 / self.total_commits as f32
        }
    }
}

#[derive(Debug)]
pub struct RiskyFile {
    pub path: String,
    pub ai_ratio: f32,
    pub has_tests: bool,
    pub uncertainty_count: usize,
    pub blind_edit_count: usize,
}

// ── Core computation ──────────────────────────────────────────────────────────

/// Compute a [`VibeReport`] by scanning up to `limit` recent commits.
pub fn compute_vibe_report(repo: &H5iRepository, limit: usize) -> Result<VibeReport, H5iError> {
    let git = repo.git();

    // ── Repo name ─────────────────────────────────────────────────────────────
    let repo_name = git
        .workdir()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // ── Walk commits ──────────────────────────────────────────────────────────
    let mut revwalk = git.revwalk()?;
    if revwalk.push_head().is_err() {
        // Empty repo or no HEAD — return an empty report.
        return Ok(VibeReport {
            repo_name,
            total_commits: 0,
            ai_commits: 0,
            human_authors: vec![],
            ai_models: vec![],
            dir_stats: vec![],
            total_blind_edits: 0,
            blind_edit_file_count: 0,
            risky_files: vec![],
        });
    }

    let mut total_commits = 0usize;
    let mut ai_commits = 0usize;
    let mut human_author_set: HashSet<String> = HashSet::new();
    let mut model_counts: HashMap<String, usize> = HashMap::new();

    // file path → (total_commits_touching, ai_commits_touching)
    let mut file_commit_stats: HashMap<String, (usize, usize)> = HashMap::new();
    // file path → true if any touching commit had passing test_metrics
    let mut file_has_tests: HashMap<String, bool> = HashMap::new();

    for oid_result in revwalk.take(limit) {
        let oid = oid_result?;
        let commit = git.find_commit(oid)?;

        let author = commit.author().name().unwrap_or("Unknown").to_string();
        let record = repo.load_h5i_record(oid).ok();
        let is_ai = record
            .as_ref()
            .and_then(|r| r.ai_metadata.as_ref())
            .is_some();

        total_commits += 1;

        if is_ai {
            ai_commits += 1;
            if let Some(model) = record
                .as_ref()
                .and_then(|r| r.ai_metadata.as_ref())
                .map(|m| &m.model_name)
            {
                *model_counts.entry(model.clone()).or_insert(0) += 1;
            }
        } else {
            human_author_set.insert(author);
        }

        let has_passing_tests = record
            .as_ref()
            .and_then(|r| r.test_metrics.as_ref())
            .map(|m| m.is_passing())
            .unwrap_or(false);

        // Diff this commit against its first parent to find touched files.
        let commit_tree = commit.tree()?;
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let diff =
            git.diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), None)?;

        for delta in diff.deltas() {
            let path_opt = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .map(|s| s.to_string());

            if let Some(p) = path_opt {
                if is_artifact_path(&p) {
                    continue;
                }
                let entry = file_commit_stats.entry(p.clone()).or_insert((0, 0));
                entry.0 += 1;
                if is_ai {
                    entry.1 += 1;
                }
                if has_passing_tests {
                    file_has_tests.insert(p, true);
                }
            }
        }
    }

    // ── Directory stats ───────────────────────────────────────────────────────
    // Group file-level stats into directories (up to 2 path components).
    let mut dir_map: HashMap<String, (usize, usize)> = HashMap::new();
    for (file, (total, ai)) in &file_commit_stats {
        let dir = dir_key(file);
        let entry = dir_map.entry(dir).or_insert((0, 0));
        entry.0 += total;
        entry.1 += ai;
    }

    let mut dir_stats: Vec<DirAiStat> = dir_map
        .into_iter()
        .map(|(path, (total, ai))| DirAiStat {
            path,
            total_commits: total,
            ai_commits: ai,
        })
        .collect();

    dir_stats.sort_by(|a, b| {
        b.ai_ratio()
            .partial_cmp(&a.ai_ratio())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.total_commits.cmp(&a.total_commits))
    });

    // ── Session analyses: blind edits + uncertainty ───────────────────────────
    let oids = session_log::list_analyses(&repo.h5i_root);
    let mut file_blind_edits: HashMap<String, usize> = HashMap::new();
    let mut file_uncertainty: HashMap<String, usize> = HashMap::new();
    let mut total_blind_edits = 0usize;

    for oid_str in &oids {
        if let Ok(Some(analysis)) = session_log::load_analysis(&repo.h5i_root, oid_str) {
            for cov in &analysis.coverage {
                let entry = file_blind_edits.entry(cov.file.clone()).or_insert(0);
                *entry += cov.blind_edit_count;
                total_blind_edits += cov.blind_edit_count;
            }
            for unc in &analysis.uncertainty {
                if !unc.context_file.is_empty() {
                    *file_uncertainty.entry(unc.context_file.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let blind_edit_file_count = file_blind_edits.values().filter(|&&v| v > 0).count();

    // ── Risky files ───────────────────────────────────────────────────────────
    let mut risky_files: Vec<RiskyFile> = file_commit_stats
        .iter()
        .filter_map(|(path, (total, ai))| {
            if *total == 0 {
                return None;
            }
            let ai_ratio = *ai as f32 / *total as f32;
            let uncertainty_count = file_uncertainty.get(path).copied().unwrap_or(0);
            let blind_edit_count = file_blind_edits.get(path).copied().unwrap_or(0);
            let has_tests = *file_has_tests.get(path).unwrap_or(&false);

            // Only surface files with high AI concentration AND at least one risk signal.
            if ai_ratio >= 0.7 && (!has_tests || uncertainty_count > 0 || blind_edit_count > 0) {
                Some(RiskyFile {
                    path: path.clone(),
                    ai_ratio,
                    has_tests,
                    uncertainty_count,
                    blind_edit_count,
                })
            } else {
                None
            }
        })
        .collect();

    risky_files.sort_by(|a, b| {
        risk_score(b)
            .partial_cmp(&risk_score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    risky_files.truncate(5);

    // ── Final assembly ────────────────────────────────────────────────────────
    let mut ai_models: Vec<(String, usize)> = model_counts.into_iter().collect();
    ai_models.sort_by(|a, b| b.1.cmp(&a.1));

    let mut human_authors: Vec<String> = human_author_set.into_iter().collect();
    human_authors.sort();

    Ok(VibeReport {
        repo_name,
        total_commits,
        ai_commits,
        human_authors,
        ai_models,
        dir_stats,
        total_blind_edits,
        blind_edit_file_count,
        risky_files,
    })
}

// ── Terminal display ──────────────────────────────────────────────────────────

pub fn print_vibe_report(report: &VibeReport) {
    use console::style;

    let width = 54usize;
    let bar_char = "─";

    println!();
    println!(
        "  {}  {}",
        style("Vibe Report").bold().underlined(),
        style(&report.repo_name).cyan().bold()
    );
    println!("  {}", style(bar_char.repeat(width)).dim());

    // ── Overall AI %  ─────────────────────────────────────────────────────────
    let ai_pct = report.ai_pct();
    let pct_color = if ai_pct >= 70.0 {
        style(format!("{:.0}%", ai_pct)).red().bold()
    } else if ai_pct >= 40.0 {
        style(format!("{:.0}%", ai_pct)).yellow().bold()
    } else {
        style(format!("{:.0}%", ai_pct)).green().bold()
    };
    println!(
        "  {}  {} of {} commits touched by AI",
        style("🤖").bold(),
        pct_color,
        style(report.total_commits).bold()
    );

    // ── Contributors ──────────────────────────────────────────────────────────
    let models_str = if report.ai_models.is_empty() {
        "—".to_string()
    } else {
        report
            .ai_models
            .iter()
            .map(|(m, n)| {
                if report.ai_models.len() == 1 {
                    m.clone()
                } else {
                    format!("{} ({})", m, n)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "  {}  {} human{}  ·  {} model{}",
        style("👥").bold(),
        style(report.human_authors.len()).bold(),
        if report.human_authors.len() == 1 { "" } else { "s" },
        style(report.ai_models.len()).bold(),
        if report.ai_models.len() == 1 { "" } else { "s" },
    );
    if !models_str.is_empty() && models_str != "—" {
        println!("      {}", style(&models_str).dim());
    }
    if !report.human_authors.is_empty() {
        println!(
            "      {}",
            style(report.human_authors.join(", ")).dim()
        );
    }

    // ── Fully-AI directories ──────────────────────────────────────────────────
    let fully_ai: Vec<&DirAiStat> = report
        .dir_stats
        .iter()
        .filter(|d| d.ai_ratio() >= 1.0 && d.total_commits >= 2)
        .collect();

    if !fully_ai.is_empty() {
        println!("  {}", style(bar_char.repeat(width)).dim());
        for d in fully_ai.iter().take(3) {
            println!(
                "  {}  {} {} fully AI-written ({} commits, 0 human)",
                style("📁").bold(),
                style(&d.path).yellow().bold(),
                style("←").dim(),
                d.total_commits,
            );
        }
    }

    // ── Hot directories (>= 80% AI, not already fully AI) ─────────────────────
    let hot_dirs: Vec<&DirAiStat> = report
        .dir_stats
        .iter()
        .filter(|d| d.ai_ratio() >= 0.8 && d.ai_ratio() < 1.0 && d.total_commits >= 3)
        .collect();

    if !hot_dirs.is_empty() {
        if fully_ai.is_empty() {
            println!("  {}", style(bar_char.repeat(width)).dim());
        }
        for d in hot_dirs.iter().take(3) {
            println!(
                "  {}  {}  {:.0}% AI  ({}/{} commits)",
                style("🔥").bold(),
                style(&d.path).yellow(),
                d.ai_ratio() * 100.0,
                d.ai_commits,
                d.total_commits,
            );
        }
    }

    // ── Blind edits ───────────────────────────────────────────────────────────
    if report.total_blind_edits > 0 {
        println!("  {}", style(bar_char.repeat(width)).dim());
        let be_color = if report.total_blind_edits >= 20 {
            style(format!("{}", report.total_blind_edits)).red().bold()
        } else {
            style(format!("{}", report.total_blind_edits)).yellow().bold()
        };
        println!(
            "  {}  {} blind edit{} across {} file{}",
            style("⚠ ").yellow().bold(),
            be_color,
            if report.total_blind_edits == 1 { "" } else { "s" },
            style(report.blind_edit_file_count).bold(),
            if report.blind_edit_file_count == 1 { "" } else { "s" },
        );
    }

    // ── Risky files ───────────────────────────────────────────────────────────
    if !report.risky_files.is_empty() {
        println!("  {}", style(bar_char.repeat(width)).dim());
        for rf in &report.risky_files {
            let mut signals: Vec<String> = Vec::new();
            if !rf.has_tests {
                signals.push("no tests".to_string());
            }
            if rf.blind_edit_count > 0 {
                signals.push(format!("{} blind edit{}", rf.blind_edit_count,
                    if rf.blind_edit_count == 1 { "" } else { "s" }));
            }
            if rf.uncertainty_count > 0 {
                signals.push(format!("{} uncertainty flag{}", rf.uncertainty_count,
                    if rf.uncertainty_count == 1 { "" } else { "s" }));
            }
            println!(
                "  {}  {}  {:.0}% AI  {}",
                style("💀").bold(),
                style(&rf.path).red().bold(),
                rf.ai_ratio * 100.0,
                style(signals.join(", ")).dim(),
            );
        }
    }

    println!("  {}", style(bar_char.repeat(width)).dim());
    println!(
        "  {} scanned {} commits",
        style("ℹ").dim(),
        style(report.total_commits).dim()
    );
    println!();
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Derive a directory display key from a file path.
/// Returns the first 2 path components with a trailing slash, or just the
/// filename if there is no directory component.
fn dir_key(path: &str) -> String {
    let parts: Vec<&str> = path.splitn(3, '/').collect();
    match parts.len() {
        1 => parts[0].to_string(),
        2 => format!("{}/", parts[0]),
        _ => format!("{}/{}/", parts[0], parts[1]),
    }
}

/// Composite risk score for sorting risky files.
fn risk_score(f: &RiskyFile) -> f32 {
    let test_penalty = if f.has_tests { 0.0 } else { 0.35 };
    f.ai_ratio * 0.35
        + (f.blind_edit_count as f32 * 0.08).min(0.25)
        + (f.uncertainty_count as f32 * 0.06).min(0.20)
        + test_penalty
}

fn is_artifact_path(path: &str) -> bool {
    const ARTIFACT_DIRS: &[&str] = &[
        "__pycache__",
        ".pytest_cache",
        "node_modules",
        ".next",
        ".nuxt",
        "dist",
        ".eggs",
        ".tox",
        ".mypy_cache",
        ".ruff_cache",
    ];
    const ARTIFACT_EXTS: &[&str] = &[
        ".pyc", ".pyo", ".class", ".jar", ".war", ".ear",
        ".min.js", ".min.css", ".map",
    ];

    for seg in path.split('/') {
        if ARTIFACT_DIRS.contains(&seg) {
            return true;
        }
    }
    for ext in ARTIFACT_EXTS {
        if path.ends_with(ext) {
            return true;
        }
    }
    false
}
