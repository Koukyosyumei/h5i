//! `h5i policy` — governance policy enforcement at commit time.
//!
//! Policy is stored in `.h5i/policy.toml` (workdir, not `.git/.h5i/`).
//! It is evaluated automatically on every `h5i commit` and can also be
//! run manually with `h5i policy check`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{H5iError, Result};
use crate::metadata::AiMetadata;

// ── TOML schema ───────────────────────────────────────────────────────────────

/// Root `[commit]` section in `.h5i/policy.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CommitPolicy {
    /// Reject commits that have no AI provenance (model/agent/prompt).
    /// Default: false.
    pub require_ai_provenance: bool,

    /// Reject commits whose commit message is shorter than this many chars.
    /// Default: 0 (disabled).
    pub min_message_len: usize,

    /// When true, commits without `--audit` are blocked for all paths marked
    /// `require_audit = true` below.  Default: false.
    pub require_audit_on_flagged_paths: bool,

    /// Free-form label for the policy (shown in output).
    pub label: Option<String>,
}

/// Per-path section, e.g. `[paths."src/auth/**"]`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PathPolicy {
    /// Commits touching paths that match this glob must carry AI provenance.
    pub require_ai_provenance: bool,

    /// Commits touching paths that match this glob must include `--audit`.
    pub require_audit: bool,

    /// Maximum fraction of AI-generated commits allowed (0.0–1.0).
    /// Only enforced in `h5i compliance`; not at commit time.
    pub max_ai_ratio: Option<f64>,

    /// Maximum blind-edit ratio allowed (0.0–1.0).
    /// Only enforced in `h5i compliance`; not at commit time.
    pub max_blind_edit_ratio: Option<f64>,
}

/// Top-level config deserialized from `.h5i/policy.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PolicyConfig {
    pub commit: CommitPolicy,
    /// Keys are glob patterns like `"src/auth/**"`.
    pub paths: HashMap<String, PathPolicy>,
}

// ── Violation types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ViolationSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyViolation {
    pub severity: ViolationSeverity,
    pub rule: String,
    pub detail: String,
}

// ── File path helpers ─────────────────────────────────────────────────────────

/// Path to the policy file in the workdir.
pub fn policy_path(workdir: &Path) -> PathBuf {
    workdir.join(".h5i").join("policy.toml")
}

/// Load policy from `workdir/.h5i/policy.toml`.
/// Returns `Ok(None)` when the file does not exist (policy is optional).
pub fn load_policy(workdir: &Path) -> Result<Option<PolicyConfig>> {
    let path = policy_path(workdir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(H5iError::Io)?;
    let cfg: PolicyConfig = toml::from_str(&raw)?;
    Ok(Some(cfg))
}

/// Write a starter policy file to `workdir/.h5i/policy.toml`.
pub fn init_policy(workdir: &Path) -> Result<PathBuf> {
    let h5i_dir = workdir.join(".h5i");
    std::fs::create_dir_all(&h5i_dir).map_err(H5iError::Io)?;
    let path = h5i_dir.join("policy.toml");
    if path.exists() {
        return Ok(path);
    }
    let content = r#"# h5i policy configuration
# All rules are opt-in — remove or comment out any block to disable it.

[commit]
# Reject commits that carry no AI provenance (--model / --agent / --prompt).
# require_ai_provenance = true

# Minimum number of characters required in the commit message.
# min_message_len = 10

# Human-readable label shown in policy output.
# label = "company-standard-v1"

# Per-path rules: keys are glob patterns relative to the repository root.
# [paths."src/auth/**"]
# require_ai_provenance = true   # all auth changes must record AI involvement
# require_audit = true           # all auth changes must pass --audit
# max_ai_ratio = 0.8             # compliance: flag if >80 % AI commits
# max_blind_edit_ratio = 0.3     # compliance: flag if >30 % blind edits
"#;
    std::fs::write(&path, content).map_err(H5iError::Io)?;
    Ok(path)
}

// ── Glob matching ─────────────────────────────────────────────────────────────

/// Minimal glob matcher supporting `*`, `**`, and `?`.
/// `pattern` is relative to the repo root; `file` is also relative.
pub fn glob_matches(pattern: &str, file: &str) -> bool {
    let pat_parts: Vec<&str> = pattern.split('/').collect();
    let file_parts: Vec<&str> = file.split('/').collect();
    glob_match_parts(&pat_parts, &file_parts)
}

fn glob_match_parts(pat: &[&str], file: &[&str]) -> bool {
    match (pat.first(), file.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(&"**"), _) => {
            // `**` can consume zero or more path components
            if glob_match_parts(&pat[1..], file) {
                return true;
            }
            for i in 0..=file.len() {
                if glob_match_parts(&pat[1..], &file[i..]) {
                    return true;
                }
            }
            false
        }
        (_, None) => false,
        (Some(p), Some(f)) => {
            if segment_matches(p, f) {
                glob_match_parts(&pat[1..], &file[1..])
            } else {
                false
            }
        }
    }
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let seg: Vec<char> = segment.chars().collect();
    segment_match_chars(&pat, &seg)
}

fn segment_match_chars(pat: &[char], seg: &[char]) -> bool {
    match (pat.first(), seg.first()) {
        (None, None) => true,
        (None, _) | (_, None) if !pat.is_empty() && seg.is_empty() => {
            pat.iter().all(|c| *c == '*')
        }
        (None, _) => false,
        (Some('*'), _) => {
            segment_match_chars(&pat[1..], seg)
                || (!seg.is_empty() && segment_match_chars(pat, &seg[1..]))
        }
        (Some('?'), _) if !seg.is_empty() => segment_match_chars(&pat[1..], &seg[1..]),
        (Some(p), Some(s)) if p == s => segment_match_chars(&pat[1..], &seg[1..]),
        _ => false,
    }
}

// ── Commit-time policy check ──────────────────────────────────────────────────

pub struct CommitCheckInput<'a> {
    pub message: &'a str,
    pub ai_meta: Option<&'a AiMetadata>,
    /// Files staged for this commit (relative paths from repo root).
    pub staged_files: &'a [String],
    /// Whether `--audit` was passed.
    pub audit_passed: bool,
}

/// Evaluate the policy for a commit-time check.
/// Returns a list of violations (empty = pass).
pub fn check_commit(cfg: &PolicyConfig, input: &CommitCheckInput<'_>) -> Vec<PolicyViolation> {
    let mut violations = Vec::new();

    // ── Global [commit] rules ──
    if cfg.commit.require_ai_provenance && input.ai_meta.is_none() {
        violations.push(PolicyViolation {
            severity: ViolationSeverity::Error,
            rule: "commit.require_ai_provenance".into(),
            detail: "This commit has no AI provenance (--model / --agent / --prompt required by policy).".into(),
        });
    }

    if cfg.commit.min_message_len > 0
        && input.message.trim().len() < cfg.commit.min_message_len
    {
        violations.push(PolicyViolation {
            severity: ViolationSeverity::Error,
            rule: "commit.min_message_len".into(),
            detail: format!(
                "Commit message is {} chars; policy requires at least {}.",
                input.message.trim().len(),
                cfg.commit.min_message_len
            ),
        });
    }

    // ── Per-path rules ──
    for (glob, path_policy) in &cfg.paths {
        // Check whether any staged file matches this glob.
        let matching: Vec<&String> = input
            .staged_files
            .iter()
            .filter(|f| glob_matches(glob, f))
            .collect();

        if matching.is_empty() {
            continue;
        }

        if path_policy.require_ai_provenance && input.ai_meta.is_none() {
            violations.push(PolicyViolation {
                severity: ViolationSeverity::Error,
                rule: format!("paths.{glob}.require_ai_provenance"),
                detail: format!(
                    "Files matching `{glob}` require AI provenance; none recorded."
                ),
            });
        }

        if path_policy.require_audit
            && cfg.commit.require_audit_on_flagged_paths
            && !input.audit_passed
        {
            violations.push(PolicyViolation {
                severity: ViolationSeverity::Error,
                rule: format!("paths.{glob}.require_audit"),
                detail: format!(
                    "Files matching `{glob}` require `--audit`; flag not passed."
                ),
            });
        }
    }

    violations
}

/// Returns `true` if any staged file matches a path that has `require_audit = true`
/// AND the global `require_audit_on_flagged_paths` flag is set.
pub fn should_force_audit(cfg: &PolicyConfig, staged_files: &[String]) -> bool {
    if !cfg.commit.require_audit_on_flagged_paths {
        return false;
    }
    for (glob, path_policy) in &cfg.paths {
        if path_policy.require_audit {
            if staged_files.iter().any(|f| glob_matches(glob, f)) {
                return true;
            }
        }
    }
    false
}

// ── Terminal output ───────────────────────────────────────────────────────────

pub fn print_violations(violations: &[PolicyViolation]) {
    use console::style;
    for v in violations {
        let (icon, rule_style) = match v.severity {
            ViolationSeverity::Error => (
                style("✖").red().bold(),
                style(format!("[{}]", v.rule)).red().bold(),
            ),
            ViolationSeverity::Warning => (
                style("⚠").yellow().bold(),
                style(format!("[{}]", v.rule)).yellow().bold(),
            ),
        };
        println!("  {} {} {}", icon, rule_style, v.detail);
    }
}

pub fn print_policy(cfg: &PolicyConfig, path: &Path) {
    use console::style;
    println!(
        "\n{} {}\n",
        style("──").dim(),
        style(format!("h5i policy  ({})", path.display())).cyan().bold()
    );

    let label = cfg
        .commit
        .label
        .as_deref()
        .unwrap_or("(unlabelled)");
    println!("  {} {}", style("label:").dim(), style(label).yellow());
    println!(
        "  {} {}",
        style("require_ai_provenance:").dim(),
        if cfg.commit.require_ai_provenance {
            style("true").green()
        } else {
            style("false").dim()
        }
    );
    println!(
        "  {} {}",
        style("min_message_len:").dim(),
        style(cfg.commit.min_message_len.to_string()).cyan()
    );
    println!(
        "  {} {}",
        style("require_audit_on_flagged_paths:").dim(),
        if cfg.commit.require_audit_on_flagged_paths {
            style("true").green()
        } else {
            style("false").dim()
        }
    );

    if cfg.paths.is_empty() {
        println!("\n  {} (no per-path rules)", style("paths:").dim());
    } else {
        println!("\n  {}:", style("paths").dim());
        let mut sorted: Vec<(&String, &PathPolicy)> = cfg.paths.iter().collect();
        sorted.sort_by_key(|(k, _)| k.as_str());
        for (glob, pp) in sorted {
            println!("    {}", style(glob).yellow().bold());
            if pp.require_ai_provenance {
                println!("      require_ai_provenance = true");
            }
            if pp.require_audit {
                println!("      require_audit = true");
            }
            if let Some(r) = pp.max_ai_ratio {
                println!("      max_ai_ratio = {:.2}", r);
            }
            if let Some(r) = pp.max_blind_edit_ratio {
                println!("      max_blind_edit_ratio = {:.2}", r);
            }
        }
    }
    println!();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta() -> AiMetadata {
        AiMetadata {
            model_name: "claude-sonnet-4-6".into(),
            agent_id: "claude-code".into(),
            prompt: "test prompt".into(),
            usage: None,
        }
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_matches("src/main.rs", "src/main.rs"));
        assert!(!glob_matches("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn glob_star_in_segment() {
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn glob_double_star() {
        assert!(glob_matches("src/**", "src/a/b/c.rs"));
        assert!(glob_matches("src/**/*.rs", "src/a/b/c.rs"));
        assert!(!glob_matches("tests/**", "src/a.rs"));
    }

    #[test]
    fn commit_policy_no_provenance() {
        let cfg = PolicyConfig {
            commit: CommitPolicy {
                require_ai_provenance: true,
                ..Default::default()
            },
            paths: HashMap::new(),
        };
        let input = CommitCheckInput {
            message: "some commit",
            ai_meta: None,
            staged_files: &[],
            audit_passed: false,
        };
        let violations = check_commit(&cfg, &input);
        assert!(!violations.is_empty());
        assert_eq!(violations[0].rule, "commit.require_ai_provenance");
    }

    #[test]
    fn commit_policy_passes_with_meta() {
        let cfg = PolicyConfig {
            commit: CommitPolicy {
                require_ai_provenance: true,
                ..Default::default()
            },
            paths: HashMap::new(),
        };
        let meta = make_meta();
        let input = CommitCheckInput {
            message: "some commit",
            ai_meta: Some(&meta),
            staged_files: &[],
            audit_passed: false,
        };
        let violations = check_commit(&cfg, &input);
        assert!(violations.is_empty());
    }

    #[test]
    fn min_message_len_violation() {
        let cfg = PolicyConfig {
            commit: CommitPolicy {
                min_message_len: 20,
                ..Default::default()
            },
            paths: HashMap::new(),
        };
        let input = CommitCheckInput {
            message: "fix",
            ai_meta: None,
            staged_files: &[],
            audit_passed: false,
        };
        let violations = check_commit(&cfg, &input);
        assert!(!violations.is_empty());
        assert_eq!(violations[0].rule, "commit.min_message_len");
    }

    #[test]
    fn path_policy_require_provenance() {
        let mut paths = HashMap::new();
        paths.insert(
            "src/auth/**".to_string(),
            PathPolicy {
                require_ai_provenance: true,
                ..Default::default()
            },
        );
        let cfg = PolicyConfig {
            commit: CommitPolicy::default(),
            paths,
        };
        let staged = vec!["src/auth/login.rs".to_string()];
        let input = CommitCheckInput {
            message: "update auth",
            ai_meta: None,
            staged_files: &staged,
            audit_passed: false,
        };
        let violations = check_commit(&cfg, &input);
        assert!(!violations.is_empty());
        assert!(violations[0].rule.contains("require_ai_provenance"));
    }
}
