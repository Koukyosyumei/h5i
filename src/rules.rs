/// Rule-based integrity engine for AI-generated commits.
///
/// Every rule is a pure function `fn check_*(ctx: &DiffContext) -> Vec<RuleFinding>`.
/// Rules are deterministic, have no AI involvement, and are designed to be easy
/// for humans to audit and reason about.
///
/// To add a new rule:
///   1. Add a `pub const` to `rule_id`.
///   2. Write a `fn check_<name>(ctx: &DiffContext) -> Vec<RuleFinding>` function.
///   3. Call it inside `run_all_rules`.
use std::path::Path;

use crate::error::H5iError;
use crate::metadata::{RuleFinding, Severity};

// ── Rule identifiers ──────────────────────────────────────────────────────────

pub mod rule_id {
    pub const CREDENTIAL_LEAK: &str = "CREDENTIAL_LEAK";
    pub const CODE_EXECUTION: &str = "CODE_EXECUTION";
    pub const CI_CD_MODIFIED: &str = "CI_CD_MODIFIED";
    pub const SENSITIVE_FILE_MODIFIED: &str = "SENSITIVE_FILE_MODIFIED";
    pub const LOCKFILE_MODIFIED: &str = "LOCKFILE_MODIFIED";
    pub const UNDECLARED_DELETION: &str = "UNDECLARED_DELETION";
    pub const SCOPE_EXPANSION: &str = "SCOPE_EXPANSION";
    pub const LARGE_DIFF: &str = "LARGE_DIFF";
    pub const REFACTOR_ANOMALY: &str = "REFACTOR_ANOMALY";
    pub const PERMISSION_CHANGE: &str = "PERMISSION_CHANGE";
    pub const BINARY_FILE_CHANGED: &str = "BINARY_FILE_CHANGED";
    pub const CONFIG_FILE_MODIFIED: &str = "CONFIG_FILE_MODIFIED";
}

// ── Diff context ──────────────────────────────────────────────────────────────

/// A file entry extracted from the staged diff.
pub struct ChangedFile {
    pub path: String,
    pub is_binary: bool,
}

/// All data a rule needs about the staged diff, computed once and shared.
pub struct DiffContext {
    /// Lines added in the diff (the `+` lines, stripped of the leading `+`).
    pub added_lines: Vec<String>,
    /// Lines removed in the diff (the `-` lines).
    pub removed_lines: Vec<String>,
    pub changed_files: Vec<ChangedFile>,
    pub insertions: usize,
    pub deletions: usize,
    /// The user's stated intent: prompt if available, otherwise commit message.
    pub primary_intent: String,
}

impl DiffContext {
    pub fn from_diff(
        diff: &git2::Diff,
        primary_intent: String,
        insertions: usize,
        deletions: usize,
    ) -> Result<Self, H5iError> {
        let mut added_lines: Vec<String> = Vec::new();
        let mut removed_lines: Vec<String> = Vec::new();
        let mut changed_files: Vec<ChangedFile> = Vec::new();

        for delta in diff.deltas() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            changed_files.push(ChangedFile {
                path,
                is_binary: delta.new_file().is_binary() || delta.old_file().is_binary(),
            });
        }

        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let content = std::str::from_utf8(line.content())
                .unwrap_or("")
                .trim_end_matches('\n')
                .to_string();
            match line.origin() {
                '+' => added_lines.push(content),
                '-' => removed_lines.push(content),
                _ => {}
            }
            true
        })?;

        Ok(DiffContext {
            added_lines,
            removed_lines,
            changed_files,
            insertions,
            deletions,
            primary_intent,
        })
    }
}

// ── Rule runner ───────────────────────────────────────────────────────────────

/// Runs all rules against `ctx` and returns every finding.
/// The caller (repository) decides the overall severity level and score.
pub fn run_all_rules(ctx: &DiffContext) -> Vec<RuleFinding> {
    let mut findings = Vec::new();
    findings.extend(check_credential_leak(ctx));
    findings.extend(check_code_execution(ctx));
    findings.extend(check_ci_cd_modified(ctx));
    findings.extend(check_sensitive_file_modified(ctx));
    findings.extend(check_lockfile_modified(ctx));
    findings.extend(check_undeclared_deletion(ctx));
    findings.extend(check_scope_expansion(ctx));
    findings.extend(check_large_diff(ctx));
    findings.extend(check_refactor_anomaly(ctx));
    findings.extend(check_permission_change(ctx));
    findings.extend(check_binary_file_changed(ctx));
    findings.extend(check_config_file_modified(ctx));
    findings
}

// ── Individual rules ──────────────────────────────────────────────────────────

/// CREDENTIAL_LEAK — Violation
///
/// Fires when an added line contains a credential keyword (e.g. `api_key`,
/// `password`) together with an assignment operator and a quoted string value.
/// Also catches private-key PEM headers directly.
///
/// Logic: keyword ∧ assignment character ∧ quoted string ∧ line length > 20.
/// Length threshold cuts false positives on short placeholder lines.
fn check_credential_leak(ctx: &DiffContext) -> Vec<RuleFinding> {
    const CRED_KEYWORDS: &[&str] = &[
        "api_key",
        "apikey",
        "api-key",
        "secret_key",
        "secretkey",
        "secret-key",
        "access_token",
        "accesstoken",
        "auth_token",
        "authtoken",
        "password",
        "passwd",
        "private_key",
        "client_secret",
    ];
    const PEM_HEADERS: &[&str] = &[
        "BEGIN RSA PRIVATE KEY",
        "BEGIN PRIVATE KEY",
        "BEGIN EC PRIVATE KEY",
        "BEGIN OPENSSH PRIVATE KEY",
        "BEGIN PGP PRIVATE KEY",
    ];

    let mut findings = Vec::new();
    for (i, line) in ctx.added_lines.iter().enumerate() {
        let lower = line.to_lowercase();

        // Check for private-key PEM headers regardless of other conditions.
        if PEM_HEADERS.iter().any(|h| line.contains(h)) {
            findings.push(RuleFinding {
                rule_id: rule_id::CREDENTIAL_LEAK.to_string(),
                severity: Severity::Violation,
                detail: format!(
                    "Private key PEM header detected in added content (line {}).",
                    i + 1
                ),
            });
            continue;
        }

        let has_keyword = CRED_KEYWORDS.iter().any(|k| lower.contains(k));
        let has_assign = lower.contains('=') || lower.contains(':');
        let has_quoted_value = lower.contains('"') || lower.contains('\'');
        // Ignore very short lines to reduce noise on struct field declarations
        if has_keyword && has_assign && has_quoted_value && line.len() > 20 {
            let kw = CRED_KEYWORDS.iter().find(|k| lower.contains(*k)).unwrap();
            findings.push(RuleFinding {
                rule_id: rule_id::CREDENTIAL_LEAK.to_string(),
                severity: Severity::Violation,
                detail: format!(
                    "Possible credential '{}' assigned to a string value (added line {}).",
                    kw,
                    i + 1
                ),
            });
        }
    }
    findings
}

/// CODE_EXECUTION — Violation
///
/// Fires when an added line contains a dangerous code-execution call.
/// These patterns can be used for command injection or arbitrary code execution
/// and should always be explicitly declared in the commit intent.
///
/// Logic: substring match on known dangerous function names.
fn check_code_execution(ctx: &DiffContext) -> Vec<RuleFinding> {
    const DANGEROUS: &[(&str, &str)] = &[
        ("eval(", "eval()"),
        ("exec(", "exec()"),
        ("os.system(", "os.system()"),
        ("subprocess.call(", "subprocess.call()"),
        ("subprocess.run(", "subprocess.run()"),
        ("subprocess.Popen(", "subprocess.Popen()"),
        ("Runtime.getRuntime().exec(", "Runtime.exec()"),
        ("__import__(", "__import__()"),
        ("child_process.exec(", "child_process.exec()"),
        ("child_process.spawn(", "child_process.spawn()"),
        ("child_process.execSync(", "child_process.execSync()"),
        ("shell_exec(", "shell_exec() [PHP]"),
        ("system(", "system() [C/PHP]"),
        ("passthru(", "passthru() [PHP]"),
    ];

    let mut findings = Vec::new();
    for (i, line) in ctx.added_lines.iter().enumerate() {
        // Skip comment lines (// # -- /*).
        let trimmed = line.trim_start();
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("--")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        for (pattern, label) in DANGEROUS {
            if line.contains(pattern) {
                findings.push(RuleFinding {
                    rule_id: rule_id::CODE_EXECUTION.to_string(),
                    severity: Severity::Violation,
                    detail: format!(
                        "Dangerous execution pattern '{}' added (line {}). \
                         Verify this is intentional and use --force to override.",
                        label,
                        i + 1
                    ),
                });
                break; // one finding per line is enough
            }
        }
    }
    findings
}

/// CI_CD_MODIFIED — Violation
///
/// Fires when a CI/CD pipeline file is modified by AI without the commit intent
/// explicitly mentioning CI/CD work. Attackers who compromise an AI coding
/// agent could use pipeline file edits to exfiltrate secrets or backdoor builds.
///
/// Logic: file path matches known CI/CD directories or filenames AND the
/// primary intent contains none of the CI/CD-related keywords.
fn check_ci_cd_modified(ctx: &DiffContext) -> Vec<RuleFinding> {
    const CI_PATHS: &[&str] = &[
        ".github/workflows/",
        ".github/actions/",
        ".gitlab-ci",
        "Jenkinsfile",
        ".travis.yml",
        ".circleci/",
        "circle.yml",
        ".buildkite/",
        "azure-pipelines.yml",
        ".drone.yml",
    ];
    const CI_INTENT_WORDS: &[&str] = &[
        "ci",
        "cd",
        "deploy",
        "pipeline",
        "workflow",
        "action",
        "github action",
        "gitlab",
        "jenkins",
        "travis",
        "build",
        "release",
    ];

    let intent = ctx.primary_intent.to_lowercase();
    let has_ci_intent = CI_INTENT_WORDS.iter().any(|k| intent.contains(k));
    if has_ci_intent {
        return vec![];
    }

    ctx.changed_files
        .iter()
        .filter(|f| CI_PATHS.iter().any(|p| f.path.contains(p)))
        .map(|f| RuleFinding {
            rule_id: rule_id::CI_CD_MODIFIED.to_string(),
            severity: Severity::Violation,
            detail: format!(
                "CI/CD file '{}' modified without explicit CI/CD intent. \
                 Pipeline changes can expose secrets or backdoor builds.",
                f.path
            ),
        })
        .collect()
}

/// SENSITIVE_FILE_MODIFIED — Warning
///
/// Fires when a file that commonly contains secrets or credentials is in the diff.
///
/// Logic: file name or extension matches a fixed list of sensitive patterns.
fn check_sensitive_file_modified(ctx: &DiffContext) -> Vec<RuleFinding> {
    const SENSITIVE_EXTENSIONS: &[&str] = &[
        ".pem", ".key", ".crt", ".cert", ".p12", ".pfx", ".jks", ".keystore",
    ];
    const SENSITIVE_NAMES: &[&str] = &[
        ".env",
        "id_rsa",
        "id_ed25519",
        "id_dsa",
        "id_ecdsa",
        ".htpasswd",
        "credentials",
        "secrets",
        ".netrc",
        ".pgpass",
    ];

    ctx.changed_files
        .iter()
        .filter(|f| {
            let lower = f.path.to_lowercase();
            let file_name = Path::new(&f.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            SENSITIVE_EXTENSIONS.iter().any(|e| lower.ends_with(e))
                || SENSITIVE_NAMES
                    .iter()
                    .any(|n| file_name == *n || file_name.starts_with(n))
        })
        .map(|f| RuleFinding {
            rule_id: rule_id::SENSITIVE_FILE_MODIFIED.to_string(),
            severity: Severity::Warning,
            detail: format!(
                "Sensitive file '{}' was modified. Verify no secrets were leaked.",
                f.path
            ),
        })
        .collect()
}

/// LOCKFILE_MODIFIED — Warning
///
/// Fires when a dependency lock file is modified without the intent mentioning
/// a dependency update. Lock file changes can silently introduce malicious
/// packages or supply-chain attacks.
///
/// Logic: changed file is a known lock file AND intent has no dep-related keywords.
fn check_lockfile_modified(ctx: &DiffContext) -> Vec<RuleFinding> {
    const LOCK_FILES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "Pipfile.lock",
        "go.sum",
        "composer.lock",
        "Gemfile.lock",
    ];
    const DEP_WORDS: &[&str] = &[
        "dependency",
        "dependencies",
        "dep",
        "package",
        "crate",
        "library",
        "version",
        "upgrade",
        "update",
        "bump",
        "install",
        "lock",
    ];

    let intent = ctx.primary_intent.to_lowercase();
    if DEP_WORDS.iter().any(|k| intent.contains(k)) {
        return vec![];
    }

    ctx.changed_files
        .iter()
        .filter(|f| {
            let file_name = Path::new(&f.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            LOCK_FILES.contains(&file_name)
        })
        .map(|f| RuleFinding {
            rule_id: rule_id::LOCKFILE_MODIFIED.to_string(),
            severity: Severity::Warning,
            detail: format!(
                "Lock file '{}' modified without a dependency update intent. \
                 Verify no unexpected packages were added.",
                f.path
            ),
        })
        .collect()
}

/// UNDECLARED_DELETION — Warning
///
/// Fires when the diff removes significantly more code than it adds, with no
/// deletion/refactor intent in the prompt. Large unexplained deletions can
/// silently remove security checks, tests, or error handling.
///
/// Logic: deletions > insertions AND deletions > 20 AND deletion ratio > 60%
///        AND no deletion/refactor keyword in intent.
fn check_undeclared_deletion(ctx: &DiffContext) -> Vec<RuleFinding> {
    const DEL_WORDS: &[&str] = &[
        "delete",
        "remove",
        "cleanup",
        "clean up",
        "rm",
        "drop",
        "refactor",
        "rewrite",
        "rework",
        "simplify",
    ];

    let intent = ctx.primary_intent.to_lowercase();
    if DEL_WORDS.iter().any(|k| intent.contains(k)) {
        return vec![];
    }

    let total = ctx.insertions + ctx.deletions;
    if total == 0 || ctx.deletions <= 20 {
        return vec![];
    }

    let ratio = ctx.deletions as f32 / total as f32;
    if ratio <= 0.6 {
        return vec![];
    }

    vec![RuleFinding {
        rule_id: rule_id::UNDECLARED_DELETION.to_string(),
        severity: Severity::Warning,
        detail: format!(
            "{} lines deleted ({:.0}% of total changes) with no deletion intent stated. \
             Verify no safety checks or tests were silently removed.",
            ctx.deletions,
            ratio * 100.0
        ),
    }]
}

/// SCOPE_EXPANSION — Warning
///
/// Fires when the intent explicitly names a specific source file (e.g.
/// `fix auth.rs`) but the diff also touches additional files beyond it.
/// This catches AI "scope creep" where the agent modifies more than asked.
///
/// Logic: intent contains a word ending in a source file extension →
///        collect those "target files" → any changed file not in targets = finding.
fn check_scope_expansion(ctx: &DiffContext) -> Vec<RuleFinding> {
    const SOURCE_EXTENSIONS: &[&str] = &[
        ".rs", ".py", ".js", ".ts", ".go", ".java", ".c", ".cpp", ".h", ".rb", ".cs", ".kt",
        ".swift",
    ];
    // Automatically-changed files that are fine to ignore here.
    const AUTO_CHANGED: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "go.sum",
        "poetry.lock",
    ];

    // Determine whether the intent mentions any specific source files.
    let mentioned_files: Vec<&str> = ctx
        .primary_intent
        .split_whitespace()
        .filter(|w| SOURCE_EXTENSIONS.iter().any(|e| w.ends_with(e)))
        .collect();

    if mentioned_files.is_empty() {
        // No specific file mentioned → no scope defined → rule is dormant.
        return vec![];
    }

    let unexpected: Vec<&str> = ctx
        .changed_files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| {
            // Skip auto-generated files.
            let file_name = Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if AUTO_CHANGED.contains(&file_name) {
                return false;
            }
            // Flag if this file was not among the explicitly mentioned ones.
            !mentioned_files.iter().any(|m| p.ends_with(m) || file_name == *m)
        })
        .collect();

    if unexpected.is_empty() {
        return vec![];
    }

    vec![RuleFinding {
        rule_id: rule_id::SCOPE_EXPANSION.to_string(),
        severity: Severity::Warning,
        detail: format!(
            "Intent mentions {} but {} additional file(s) were also modified: {}",
            mentioned_files.join(", "),
            unexpected.len(),
            unexpected.join(", ")
        ),
    }]
}

/// LARGE_DIFF — Warning
///
/// Fires when the total number of changed lines exceeds a threshold.
/// Very large AI diffs are hard for humans to audit thoroughly, increasing
/// the risk that a subtle mistake or injection goes unnoticed.
///
/// Logic: insertions + deletions > THRESHOLD (default 500).
fn check_large_diff(ctx: &DiffContext) -> Vec<RuleFinding> {
    const THRESHOLD: usize = 500;
    let total = ctx.insertions + ctx.deletions;
    if total <= THRESHOLD {
        return vec![];
    }
    vec![RuleFinding {
        rule_id: rule_id::LARGE_DIFF.to_string(),
        severity: Severity::Warning,
        detail: format!(
            "{total} lines changed (threshold: {THRESHOLD}). \
             Large AI diffs are difficult to audit — consider splitting into smaller commits."
        ),
    }]
}

/// REFACTOR_ANOMALY — Warning
///
/// Fires when the intent claims a refactor/rename/comment pass but the diff
/// has a large net addition of new code. A genuine refactor should have a
/// roughly balanced insertions-to-deletions ratio.
///
/// Logic: refactor keyword in intent AND (insertions / deletions > 3.0) AND
///        insertions > 50.
fn check_refactor_anomaly(ctx: &DiffContext) -> Vec<RuleFinding> {
    const REFACTOR_WORDS: &[&str] = &[
        "refactor",
        "rename",
        "reorganize",
        "restructure",
        "clean",
        "comment",
        "annotate",
        "lint",
        "format",
    ];

    let intent = ctx.primary_intent.to_lowercase();
    if !REFACTOR_WORDS.iter().any(|k| intent.contains(k)) {
        return vec![];
    }

    if ctx.insertions <= 50 {
        return vec![]; // Small changes are fine even for refactors.
    }

    let ratio = if ctx.deletions == 0 {
        f32::MAX
    } else {
        ctx.insertions as f32 / ctx.deletions as f32
    };

    if ratio <= 3.0 {
        return vec![];
    }

    vec![RuleFinding {
        rule_id: rule_id::REFACTOR_ANOMALY.to_string(),
        severity: Severity::Warning,
        detail: format!(
            "Refactor intent but {} insertions vs {} deletions (ratio {:.1}×). \
             A genuine refactor should have a balanced diff — possible scope creep.",
            ctx.insertions,
            ctx.deletions,
            if ratio == f32::MAX {
                ctx.insertions as f32
            } else {
                ratio
            }
        ),
    }]
}

/// PERMISSION_CHANGE — Warning
///
/// Fires when an added line contains a command that modifies file permissions
/// or invokes `sudo`. These can escalate privileges or make files world-writable.
///
/// Logic: substring match on known permission-altering command patterns.
fn check_permission_change(ctx: &DiffContext) -> Vec<RuleFinding> {
    const PATTERNS: &[&str] = &[
        "chmod 777",
        "chmod 666",
        "chmod 775",
        "chmod +x",
        "chmod a+x",
        "chmod o+w",
        "chmod ugo+",
        "sudo ",
        "setuid",
        "setgid",
        "chown root",
        "chown 0:",
        ":0 ",
    ];

    let mut findings = Vec::new();
    for (i, line) in ctx.added_lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let trimmed = lower.trim_start();
        // Skip comment lines.
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("--") {
            continue;
        }
        for pat in PATTERNS {
            if lower.contains(pat) {
                findings.push(RuleFinding {
                    rule_id: rule_id::PERMISSION_CHANGE.to_string(),
                    severity: Severity::Warning,
                    detail: format!(
                        "Permission-altering pattern '{}' detected in added content (line {}).",
                        pat.trim(),
                        i + 1
                    ),
                });
                break;
            }
        }
    }
    findings
}

/// BINARY_FILE_CHANGED — Info
///
/// Reports any binary file that appears in the diff. Binary changes are opaque
/// to text-based review and warrant manual inspection.
fn check_binary_file_changed(ctx: &DiffContext) -> Vec<RuleFinding> {
    ctx.changed_files
        .iter()
        .filter(|f| f.is_binary)
        .map(|f| RuleFinding {
            rule_id: rule_id::BINARY_FILE_CHANGED.to_string(),
            severity: Severity::Info,
            detail: format!(
                "Binary file '{}' modified. Verify this is intentional.",
                f.path
            ),
        })
        .collect()
}

/// CONFIG_FILE_MODIFIED — Info
///
/// Reports changes to configuration files (YAML, TOML, JSON, INI, etc.).
/// Config changes are often innocuous but worth noting for audit trails.
///
/// Logic: file extension matches a list of config formats. Lock files are
/// excluded (they are already handled by LOCKFILE_MODIFIED).
fn check_config_file_modified(ctx: &DiffContext) -> Vec<RuleFinding> {
    const CONFIG_EXTENSIONS: &[&str] = &[
        ".yaml", ".yml", ".toml", ".json", ".ini", ".cfg", ".conf", ".properties",
    ];
    const LOCK_SUFFIXES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "go.sum",
        "poetry.lock",
    ];

    ctx.changed_files
        .iter()
        .filter(|f| {
            let lower = f.path.to_lowercase();
            let file_name = Path::new(&f.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let is_config = CONFIG_EXTENSIONS.iter().any(|e| lower.ends_with(e));
            let is_lock = LOCK_SUFFIXES.contains(&file_name);
            is_config && !is_lock
        })
        .map(|f| RuleFinding {
            rule_id: rule_id::CONFIG_FILE_MODIFIED.to_string(),
            severity: Severity::Info,
            detail: format!("Configuration file '{}' modified.", f.path),
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(intent: &str, added: &[&str], files: &[&str]) -> DiffContext {
        DiffContext {
            added_lines: added.iter().map(|s| s.to_string()).collect(),
            removed_lines: vec![],
            changed_files: files
                .iter()
                .map(|p| ChangedFile {
                    path: p.to_string(),
                    is_binary: false,
                })
                .collect(),
            insertions: added.len(),
            deletions: 0,
            primary_intent: intent.to_string(),
        }
    }

    // ── CREDENTIAL_LEAK ───────────────────────────────────────────────────────

    #[test]
    fn credential_leak_fires_on_api_key_assignment() {
        let c = ctx("add feature", &[r#"api_key = "sk-abc123def456ghi789""#], &[]);
        let findings = check_credential_leak(&c);
        assert!(!findings.is_empty());
        assert_eq!(findings[0].severity, Severity::Violation);
        assert!(findings[0].rule_id == rule_id::CREDENTIAL_LEAK);
    }

    #[test]
    fn credential_leak_fires_on_pem_header() {
        let c = ctx("", &["-----BEGIN RSA PRIVATE KEY-----"], &[]);
        let findings = check_credential_leak(&c);
        assert!(!findings.is_empty());
        assert_eq!(findings[0].severity, Severity::Violation);
    }

    #[test]
    fn credential_leak_ignores_short_lines() {
        // A struct field declaration like `password: String,` should not trigger.
        let c = ctx("", &["password: String,"], &[]);
        assert!(check_credential_leak(&c).is_empty());
    }

    #[test]
    fn credential_leak_ignores_placeholder_without_value() {
        let c = ctx("", &["// TODO: set api_key here"], &[]);
        // No quoted value → should not trigger.
        assert!(check_credential_leak(&c).is_empty());
    }

    // ── CODE_EXECUTION ────────────────────────────────────────────────────────

    #[test]
    fn code_execution_fires_on_eval() {
        let c = ctx("add feature", &["result = eval(user_input)"], &[]);
        let findings = check_code_execution(&c);
        assert!(!findings.is_empty());
        assert_eq!(findings[0].severity, Severity::Violation);
    }

    #[test]
    fn code_execution_skips_comment_lines() {
        let c = ctx("", &["// do not use eval() here"], &[]);
        assert!(check_code_execution(&c).is_empty());
    }

    #[test]
    fn code_execution_fires_on_os_system() {
        let c = ctx("", &["os.system('rm -rf /tmp/cache')"], &[]);
        assert!(!check_code_execution(&c).is_empty());
    }

    // ── CI_CD_MODIFIED ────────────────────────────────────────────────────────

    #[test]
    fn ci_cd_fires_without_intent() {
        let c = ctx("fix auth bug", &[], &[".github/workflows/test.yaml"]);
        assert!(!check_ci_cd_modified(&c).is_empty());
    }

    #[test]
    fn ci_cd_silent_with_ci_intent() {
        let c = ctx("update CI pipeline", &[], &[".github/workflows/test.yaml"]);
        assert!(check_ci_cd_modified(&c).is_empty());
    }

    // ── LOCKFILE_MODIFIED ─────────────────────────────────────────────────────

    #[test]
    fn lockfile_fires_without_dep_intent() {
        let c = ctx("fix login bug", &[], &["Cargo.lock"]);
        assert!(!check_lockfile_modified(&c).is_empty());
    }

    #[test]
    fn lockfile_silent_with_dep_intent() {
        let c = ctx("update dependency serde", &[], &["Cargo.lock"]);
        assert!(check_lockfile_modified(&c).is_empty());
    }

    // ── UNDECLARED_DELETION ───────────────────────────────────────────────────

    #[test]
    fn undeclared_deletion_fires_on_heavy_deletion() {
        let mut c = ctx("add logging", &[], &[]);
        c.insertions = 5;
        c.deletions = 80;
        let findings = check_undeclared_deletion(&c);
        assert!(!findings.is_empty());
    }

    #[test]
    fn undeclared_deletion_silent_when_refactor_stated() {
        let mut c = ctx("refactor auth module", &[], &[]);
        c.insertions = 10;
        c.deletions = 80;
        assert!(check_undeclared_deletion(&c).is_empty());
    }

    // ── SCOPE_EXPANSION ───────────────────────────────────────────────────────

    #[test]
    fn scope_expansion_fires_when_extra_files_changed() {
        let c = ctx(
            "fix bug in auth.rs",
            &[],
            &["src/auth.rs", "src/database.rs", "src/config.rs"],
        );
        assert!(!check_scope_expansion(&c).is_empty());
    }

    #[test]
    fn scope_expansion_silent_when_no_file_mentioned() {
        // Intent has no specific filename → rule is dormant.
        let c = ctx("fix auth bug", &[], &["src/auth.rs", "src/database.rs"]);
        assert!(check_scope_expansion(&c).is_empty());
    }

    // ── LARGE_DIFF ────────────────────────────────────────────────────────────

    #[test]
    fn large_diff_fires_over_threshold() {
        let mut c = ctx("big feature", &[], &[]);
        c.insertions = 400;
        c.deletions = 200;
        assert!(!check_large_diff(&c).is_empty());
    }

    #[test]
    fn large_diff_silent_under_threshold() {
        let mut c = ctx("small fix", &[], &[]);
        c.insertions = 100;
        c.deletions = 50;
        assert!(check_large_diff(&c).is_empty());
    }

    // ── REFACTOR_ANOMALY ──────────────────────────────────────────────────────

    #[test]
    fn refactor_anomaly_fires_on_large_net_addition() {
        let mut c = ctx("refactor auth module", &[], &[]);
        c.insertions = 300;
        c.deletions = 20;
        assert!(!check_refactor_anomaly(&c).is_empty());
    }

    #[test]
    fn refactor_anomaly_silent_without_refactor_intent() {
        let mut c = ctx("add new feature", &[], &[]);
        c.insertions = 300;
        c.deletions = 20;
        assert!(check_refactor_anomaly(&c).is_empty());
    }

    // ── PERMISSION_CHANGE ─────────────────────────────────────────────────────

    #[test]
    fn permission_change_fires_on_chmod_777() {
        let c = ctx("setup script", &["chmod 777 /var/www/uploads"], &[]);
        assert!(!check_permission_change(&c).is_empty());
    }

    #[test]
    fn permission_change_skips_comment() {
        let c = ctx("", &["# chmod 777 is dangerous, use 755 instead"], &[]);
        assert!(check_permission_change(&c).is_empty());
    }

    // ── CONFIG_FILE_MODIFIED ──────────────────────────────────────────────────

    #[test]
    fn config_file_detected() {
        let c = ctx("update settings", &[], &["config/app.yaml"]);
        assert!(!check_config_file_modified(&c).is_empty());
    }

    #[test]
    fn config_file_excludes_lockfile() {
        // Cargo.lock has .toml-like metadata but is a lock file, covered by LOCKFILE rule.
        let c = ctx("", &[], &["Cargo.lock"]);
        assert!(check_config_file_modified(&c).is_empty());
    }
}
