//! `h5i compliance` — audit report over a date range.
//!
//! Walks historical commits, re-evaluates policy rules, aggregates session
//! data (blind edits, uncertainty), and emits a text / JSON / HTML report.

use std::collections::HashMap;

use chrono::{TimeZone, Utc};
use serde::Serialize;

use crate::error::{H5iError, Result};
use crate::metadata::H5iCommitRecord;
use crate::policy::{check_commit, CommitCheckInput, PolicyConfig, PolicyViolation};
use crate::repository::H5iRepository;
use crate::session_log;

// ── Public data types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ComplianceReport {
    pub since: Option<String>,
    pub until: Option<String>,
    pub total_commits: usize,
    pub ai_commits: usize,
    pub human_commits: usize,
    pub policy_violations: usize,
    pub commits: Vec<CommitStat>,
    pub violations: Vec<ViolationRecord>,
    /// Per-path stats for `max_ai_ratio` / `max_blind_edit_ratio` checks.
    pub path_stats: Vec<PathStat>,
}

impl ComplianceReport {
    pub fn ai_pct(&self) -> f64 {
        if self.total_commits == 0 {
            0.0
        } else {
            self.ai_commits as f64 / self.total_commits as f64 * 100.0
        }
    }

    pub fn pass_rate(&self) -> f64 {
        if self.total_commits == 0 {
            1.0
        } else {
            let passing = self.total_commits.saturating_sub(
                self.commits.iter().filter(|c| c.has_violation).count(),
            );
            passing as f64 / self.total_commits as f64 * 100.0
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CommitStat {
    pub oid: String,
    pub short_oid: String,
    pub message: String,
    pub author: String,
    pub timestamp: String,
    pub is_ai: bool,
    pub has_violation: bool,
    pub violations: Vec<ViolationRecord>,
    pub blind_edits: usize,
    pub uncertainty_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViolationRecord {
    pub commit_oid: String,
    pub rule: String,
    pub detail: String,
    pub severity: String,
}

#[derive(Debug, Serialize)]
pub struct PathStat {
    pub path: String,
    pub ai_ratio: f64,
    pub blind_edit_ratio: f64,
    pub violates_ai_ratio: bool,
    pub violates_blind_edit_ratio: bool,
}

// ── Main computation ──────────────────────────────────────────────────────────

pub fn compute_compliance_report(
    repo: &H5iRepository,
    since: Option<&str>,
    until: Option<&str>,
    policy: Option<&PolicyConfig>,
    limit: usize,
) -> Result<ComplianceReport> {
    // Parse date bounds.
    let since_ts: Option<i64> = since
        .map(|s| parse_date_to_unix(s))
        .transpose()
        .map_err(|e| H5iError::Metadata(format!("--since: {e}")))?;
    let until_ts: Option<i64> = until
        .map(|s| parse_date_to_unix(s))
        .transpose()
        .map_err(|e| H5iError::Metadata(format!("--until: {e}")))?;

    let git_repo = repo.git();
    let mut revwalk = git_repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(git2::Sort::TIME)?;

    let mut total_commits = 0usize;
    let mut ai_commits = 0usize;
    let mut human_commits = 0usize;
    let mut policy_violations = 0usize;
    let mut commit_stats: Vec<CommitStat> = Vec::new();
    let mut all_violations: Vec<ViolationRecord> = Vec::new();

    // Per-file AI commit count for path-level ratio checks.
    let mut file_ai_count: HashMap<String, usize> = HashMap::new();
    let mut file_total_count: HashMap<String, usize> = HashMap::new();
    let mut file_blind_edits: HashMap<String, usize> = HashMap::new();
    let mut file_total_edits: HashMap<String, usize> = HashMap::new();

    for oid_result in revwalk {
        if total_commits >= limit {
            break;
        }

        let oid = oid_result?;
        let commit = git_repo.find_commit(oid)?;
        let commit_ts = commit.time().seconds();

        // Date filter.
        if let Some(s) = since_ts {
            if commit_ts < s {
                continue;
            }
        }
        if let Some(u) = until_ts {
            if commit_ts > u {
                continue;
            }
        }

        total_commits += 1;
        let oid_str = oid.to_string();
        let short_oid = oid_str[..8].to_string();

        // Load h5i metadata for this commit.
        let record: Option<H5iCommitRecord> = match repo.load_h5i_record(oid) {
            Ok(r) => Some(r),
            Err(H5iError::RecordNotFound(_)) => None,
            Err(_) => None,
        };
        let ai_meta = record.as_ref().and_then(|r| r.ai_metadata.as_ref());
        let is_ai = ai_meta.is_some();

        if is_ai {
            ai_commits += 1;
        } else {
            human_commits += 1;
        }

        // Author / timestamp.
        let author = commit
            .author()
            .name()
            .unwrap_or("unknown")
            .to_string();
        let ts = Utc
            .timestamp_opt(commit_ts, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        let message = commit.message().unwrap_or("").lines().next().unwrap_or("").to_string();

        // Collect staged files for this commit (diff against first parent).
        let staged_files: Vec<String> = collect_commit_files(git_repo, &commit);

        // Update per-file counters.
        for f in &staged_files {
            *file_total_count.entry(f.clone()).or_default() += 1;
            *file_total_edits.entry(f.clone()).or_default() += 1;
            if is_ai {
                *file_ai_count.entry(f.clone()).or_default() += 1;
            }
        }

        // Session data: blind edits + uncertainty.
        let mut blind_edits = 0usize;
        let mut uncertainty_count = 0usize;
        if let Ok(Some(session_data)) = session_log::load_analysis(&repo.h5i_root, &oid_str) {
            for fc in &session_data.coverage {
                blind_edits += fc.blind_edit_count;
                for f in &staged_files {
                    if fc.file.contains(f.as_str()) {
                        *file_blind_edits.entry(f.clone()).or_default() += fc.blind_edit_count;
                    }
                }
            }
            uncertainty_count = session_data.uncertainty.len();
        }

        // Policy check.
        let mut commit_violations: Vec<ViolationRecord> = Vec::new();
        if let Some(cfg) = policy {
            let ai_meta_ref = ai_meta.cloned();
            let input = CommitCheckInput {
                message: &message,
                ai_meta: ai_meta_ref.as_ref(),
                staged_files: &staged_files,
                audit_passed: false, // historical commits: audit status unknown
            };
            let raw_violations: Vec<PolicyViolation> = check_commit(cfg, &input);
            for v in raw_violations {
                let vr = ViolationRecord {
                    commit_oid: short_oid.clone(),
                    rule: v.rule,
                    detail: v.detail,
                    severity: format!("{:?}", v.severity),
                };
                commit_violations.push(vr.clone());
                all_violations.push(vr);
                policy_violations += 1;
            }
        }

        let has_violation = !commit_violations.is_empty();
        commit_stats.push(CommitStat {
            oid: oid_str,
            short_oid,
            message,
            author,
            timestamp: ts,
            is_ai,
            has_violation,
            violations: commit_violations,
            blind_edits,
            uncertainty_count,
        });
    }

    // Build per-path stats.
    let mut path_stats: Vec<PathStat> = Vec::new();
    if let Some(cfg) = policy {
        for (glob, path_policy) in &cfg.paths {
            if path_policy.max_ai_ratio.is_none() && path_policy.max_blind_edit_ratio.is_none() {
                continue;
            }
            // Aggregate all files matching this glob.
            let mut total = 0usize;
            let mut ai = 0usize;
            let mut blind = 0usize;
            let mut edits = 0usize;
            for (file, t) in &file_total_count {
                if crate::policy::glob_matches(glob, file) {
                    total += t;
                    ai += file_ai_count.get(file).copied().unwrap_or(0);
                    blind += file_blind_edits.get(file).copied().unwrap_or(0);
                    edits += file_total_edits.get(file).copied().unwrap_or(0);
                }
            }
            if total == 0 {
                continue;
            }
            let ai_ratio = ai as f64 / total as f64;
            let blind_edit_ratio = if edits == 0 { 0.0 } else { blind as f64 / edits as f64 };
            let violates_ai = path_policy
                .max_ai_ratio
                .map(|max| ai_ratio > max)
                .unwrap_or(false);
            let violates_blind = path_policy
                .max_blind_edit_ratio
                .map(|max| blind_edit_ratio > max)
                .unwrap_or(false);
            path_stats.push(PathStat {
                path: glob.clone(),
                ai_ratio,
                blind_edit_ratio,
                violates_ai_ratio: violates_ai,
                violates_blind_edit_ratio: violates_blind,
            });
        }
    }
    path_stats.sort_by(|a, b| b.ai_ratio.partial_cmp(&a.ai_ratio).unwrap_or(std::cmp::Ordering::Equal));

    Ok(ComplianceReport {
        since: since.map(|s| s.to_string()),
        until: until.map(|s| s.to_string()),
        total_commits,
        ai_commits,
        human_commits,
        policy_violations,
        commits: commit_stats,
        violations: all_violations,
        path_stats,
    })
}

// ── Output formatters ─────────────────────────────────────────────────────────

pub fn print_compliance_text(report: &ComplianceReport) {
    use console::style;

    let date_range = match (&report.since, &report.until) {
        (Some(s), Some(u)) => format!("{s} – {u}"),
        (Some(s), None) => format!("since {s}"),
        (None, Some(u)) => format!("until {u}"),
        (None, None) => "all time".to_string(),
    };

    println!(
        "\n{} {}\n",
        style("──").dim(),
        style(format!("h5i compliance report  ({})", date_range))
            .cyan()
            .bold()
    );

    let pass_pct = report.pass_rate();
    let pass_icon = if report.policy_violations == 0 {
        style("✔").green().bold()
    } else {
        style("✖").red().bold()
    };

    println!(
        "  {} {} commits scanned  ·  {} AI ({:.0}%)  ·  {} human",
        pass_icon,
        style(report.total_commits).bold(),
        style(report.ai_commits).cyan(),
        report.ai_pct(),
        style(report.human_commits).dim()
    );
    println!(
        "  {} policy violations  ·  {:.0}% pass rate",
        style(report.policy_violations).red().bold(),
        pass_pct
    );

    // Per-path stats.
    if !report.path_stats.is_empty() {
        println!("\n  {}:", style("path rules").dim());
        for ps in &report.path_stats {
            let ai_icon = if ps.violates_ai_ratio {
                style("✖").red()
            } else {
                style("✔").green()
            };
            let be_icon = if ps.violates_blind_edit_ratio {
                style("✖").red()
            } else {
                style("✔").green()
            };
            println!(
                "    {}  {}",
                style(&ps.path).yellow(),
                style(format!(
                    "ai={:.0}% {}  blind={:.0}% {}",
                    ps.ai_ratio * 100.0,
                    ai_icon,
                    ps.blind_edit_ratio * 100.0,
                    be_icon
                ))
                .dim()
            );
        }
    }

    if report.policy_violations > 0 {
        println!("\n  {}:", style("violations").red().bold());
        for v in &report.violations {
            println!(
                "    {} {} {}",
                style(&v.commit_oid).magenta(),
                style(format!("[{}]", v.rule)).red(),
                style(&v.detail).dim()
            );
        }
    }

    // Commit list.
    println!("\n  {}:", style("commits").dim());
    for c in &report.commits {
        let ai_tag = if c.is_ai {
            style(" AI").cyan().to_string()
        } else {
            String::new()
        };
        let violation_tag = if c.has_violation {
            format!(" {}", style("⚠ policy").red())
        } else {
            String::new()
        };
        let blind_tag = if c.blind_edits > 0 {
            format!(" · {} blind", c.blind_edits)
        } else {
            String::new()
        };
        println!(
            "    {} {}{}{}{}  {}",
            style(&c.short_oid).magenta(),
            style(&c.author).dim(),
            ai_tag,
            violation_tag,
            style(blind_tag).dim(),
            style(&c.message).italic()
        );
    }
    println!();
}

pub fn to_json(report: &ComplianceReport) -> Result<String> {
    serde_json::to_string_pretty(report).map_err(H5iError::Serialization)
}

pub fn to_html(report: &ComplianceReport) -> String {
    let date_range = match (&report.since, &report.until) {
        (Some(s), Some(u)) => format!("{s} – {u}"),
        (Some(s), None) => format!("since {s}"),
        (None, Some(u)) => format!("until {u}"),
        (None, None) => "all time".to_string(),
    };

    let pass_badge = if report.policy_violations == 0 {
        r#"<span class="badge pass">PASS</span>"#
    } else {
        r#"<span class="badge fail">VIOLATIONS</span>"#
    };

    let commit_rows: String = report
        .commits
        .iter()
        .map(|c| {
            let ai_badge = if c.is_ai {
                r#"<span class="tag ai">AI</span>"#
            } else {
                ""
            };
            let viol_badge = if c.has_violation {
                r#"<span class="tag viol">policy</span>"#
            } else {
                ""
            };
            let blind_badge = if c.blind_edits > 0 {
                format!(r#"<span class="tag blind">{} blind</span>"#, c.blind_edits)
            } else {
                String::new()
            };
            format!(
                r#"<tr>
  <td class="mono">{}</td>
  <td>{}</td>
  <td>{}</td>
  <td>{}{}{}</td>
  <td class="msg">{}</td>
</tr>"#,
                c.short_oid,
                c.timestamp,
                c.author,
                ai_badge,
                viol_badge,
                blind_badge,
                html_escape(&c.message)
            )
        })
        .collect();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>h5i compliance report</title>
<style>
  body {{ font-family: 'SF Mono', 'Fira Code', monospace; background: #0d1117; color: #c9d1d9; margin: 2rem; }}
  h1 {{ color: #58a6ff; font-size: 1.4rem; }}
  .summary {{ display: flex; gap: 2rem; margin: 1rem 0; }}
  .stat {{ background: #161b22; padding: 1rem 1.5rem; border-radius: 6px; border: 1px solid #30363d; }}
  .stat .value {{ font-size: 2rem; font-weight: bold; color: #58a6ff; }}
  .stat .label {{ font-size: 0.75rem; color: #8b949e; margin-top: 0.25rem; }}
  table {{ width: 100%; border-collapse: collapse; margin-top: 1.5rem; }}
  th {{ text-align: left; color: #8b949e; font-size: 0.75rem; border-bottom: 1px solid #30363d; padding: 0.4rem 0.6rem; }}
  td {{ padding: 0.4rem 0.6rem; border-bottom: 1px solid #21262d; font-size: 0.85rem; }}
  .mono {{ font-family: monospace; color: #c9b1f5; }}
  .msg {{ color: #8b949e; }}
  .badge {{ padding: 0.2rem 0.6rem; border-radius: 4px; font-weight: bold; font-size: 0.8rem; }}
  .badge.pass {{ background: #1f6a3b; color: #3fb950; }}
  .badge.fail {{ background: #4a1a1a; color: #f85149; }}
  .tag {{ padding: 0.1rem 0.4rem; border-radius: 3px; font-size: 0.75rem; margin-right: 0.3rem; }}
  .tag.ai {{ background: #0d2e4a; color: #58a6ff; }}
  .tag.viol {{ background: #4a1a1a; color: #f85149; }}
  .tag.blind {{ background: #3a2a0a; color: #d29922; }}
  .violations {{ margin-top: 1.5rem; }}
  .violations h2 {{ color: #f85149; font-size: 1rem; }}
  .vrow {{ background: #1c1010; border-left: 3px solid #f85149; padding: 0.5rem 1rem; margin: 0.3rem 0; border-radius: 0 4px 4px 0; font-size: 0.85rem; }}
</style>
</head>
<body>
<h1>h5i compliance report &nbsp; {pass_badge}</h1>
<p style="color:#8b949e">{date_range}</p>
<div class="summary">
  <div class="stat"><div class="value">{total}</div><div class="label">commits scanned</div></div>
  <div class="stat"><div class="value">{ai_pct:.0}%</div><div class="label">AI-generated</div></div>
  <div class="stat"><div class="value">{violations}</div><div class="label">policy violations</div></div>
  <div class="stat"><div class="value">{pass_rate:.0}%</div><div class="label">pass rate</div></div>
</div>
{violation_section}
<table>
  <thead><tr><th>OID</th><th>Date</th><th>Author</th><th>Tags</th><th>Message</th></tr></thead>
  <tbody>{commit_rows}</tbody>
</table>
</body>
</html>"#,
        pass_badge = pass_badge,
        date_range = html_escape(&date_range),
        total = report.total_commits,
        ai_pct = report.ai_pct(),
        violations = report.policy_violations,
        pass_rate = report.pass_rate(),
        violation_section = if report.violations.is_empty() {
            String::new()
        } else {
            let rows: String = report
                .violations
                .iter()
                .map(|v| {
                    format!(
                        r#"<div class="vrow"><span class="mono">{}</span> <strong>[{}]</strong> — {}</div>"#,
                        v.commit_oid,
                        html_escape(&v.rule),
                        html_escape(&v.detail)
                    )
                })
                .collect();
            format!(r#"<div class="violations"><h2>Policy violations</h2>{rows}</div>"#)
        },
        commit_rows = commit_rows,
    )
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse a date string `YYYY-MM-DD` to a Unix timestamp (start of day UTC).
fn parse_date_to_unix(s: &str) -> std::result::Result<i64, String> {
    use chrono::NaiveDate;
    let nd = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| format!("cannot parse date '{}': {}", s, e))?;
    let dt = nd
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| format!("invalid date '{}'", s))?;
    Ok(Utc.from_utc_datetime(&dt).timestamp())
}

/// Collect file paths changed in a commit (relative to repo root).
fn collect_commit_files(git_repo: &git2::Repository, commit: &git2::Commit<'_>) -> Vec<String> {
    let mut files = Vec::new();
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return files,
    };
    if commit.parent_count() == 0 {
        // Initial commit: list all files.
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let path = if root.is_empty() {
                    entry.name().unwrap_or("").to_string()
                } else {
                    format!("{}{}", root, entry.name().unwrap_or(""))
                };
                files.push(path);
            }
            git2::TreeWalkResult::Ok
        })
        .ok();
    } else {
        let parent = match commit.parent(0).and_then(|p| p.tree()) {
            Ok(t) => t,
            Err(_) => return files,
        };
        let diff = match git_repo.diff_tree_to_tree(Some(&parent), Some(&tree), None) {
            Ok(d) => d,
            Err(_) => return files,
        };
        diff.foreach(
            &mut |delta, _| {
                if let Some(p) = delta.new_file().path() {
                    files.push(p.to_string_lossy().to_string());
                }
                true
            },
            None,
            None,
            None,
        )
        .ok();
    }
    files
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
