use chrono::{DateTime, Utc};
use console::style;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::H5iError;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SnapshotMeta {
    pub commit_oid: String,
    pub timestamp: DateTime<Utc>,
    pub file_count: usize,
}

#[derive(Debug)]
pub struct MemoryDiff {
    pub from_label: String,
    pub to_label: String,
    pub added_files: Vec<(String, String)>,   // (name, content)
    pub removed_files: Vec<(String, String)>, // (name, content)
    pub modified_files: Vec<ModifiedFile>,
}

#[derive(Debug)]
pub struct ModifiedFile {
    pub name: String,
    pub hunks: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Resolves `~/.claude/projects/<encoded-workdir>/memory/`.
///
/// Claude Code encodes the project path by replacing every `/` with `-`,
/// so `/home/user/dev/repo` becomes `-home-user-dev-repo`.
pub fn claude_memory_dir(workdir: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let abs = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    let encoded = abs.to_string_lossy().replace('/', "-");
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encoded)
        .join("memory")
}

fn snapshot_dir(h5i_root: &Path, commit_oid: &str) -> PathBuf {
    h5i_root.join("memory").join(commit_oid)
}

// ── Core operations ───────────────────────────────────────────────────────────

/// Copy Claude's live memory files into `.git/.h5i/memory/<commit_oid>/`.
/// Returns the number of files snapshotted.
pub fn take_snapshot(
    h5i_root: &Path,
    workdir: &Path,
    commit_oid: &str,
) -> Result<usize, H5iError> {
    let mem_dir = claude_memory_dir(workdir);
    if !mem_dir.exists() {
        return Err(H5iError::InvalidPath(format!(
            "Claude memory directory not found: {}\n\
             Make sure Claude Code has been used in this project at least once.",
            mem_dir.display()
        )));
    }

    let snap_dir = snapshot_dir(h5i_root, commit_oid);
    fs::create_dir_all(&snap_dir)?;

    let mut count = 0;
    for entry in fs::read_dir(&mem_dir)? {
        let entry = entry?;
        if entry.path().is_file() {
            fs::copy(entry.path(), snap_dir.join(entry.file_name()))?;
            count += 1;
        }
    }

    let meta = SnapshotMeta {
        commit_oid: commit_oid.to_string(),
        timestamp: Utc::now(),
        file_count: count,
    };
    fs::write(
        snap_dir.join("_meta.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(count)
}

/// List all snapshots stored in `.git/.h5i/memory/`, sorted oldest-first.
pub fn list_snapshots(h5i_root: &Path) -> Result<Vec<SnapshotMeta>, H5iError> {
    let mem_root = h5i_root.join("memory");
    if !mem_root.exists() {
        return Ok(vec![]);
    }

    let mut snapshots = vec![];
    for entry in fs::read_dir(&mem_root)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let meta_path = entry.path().join("_meta.json");
        if meta_path.exists() {
            let raw = fs::read_to_string(&meta_path)?;
            if let Ok(meta) = serde_json::from_str::<SnapshotMeta>(&raw) {
                snapshots.push(meta);
            }
        }
    }

    snapshots.sort_by_key(|s| s.timestamp);
    Ok(snapshots)
}

/// Diff two snapshots.  Pass `to_oid = None` to diff `from_oid` against the
/// current live memory directory.
pub fn diff_snapshots(
    h5i_root: &Path,
    workdir: &Path,
    from_oid: &str,
    to_oid: Option<&str>,
) -> Result<MemoryDiff, H5iError> {
    let from_dir = snapshot_dir(h5i_root, from_oid);
    if !from_dir.exists() {
        return Err(H5iError::InvalidPath(format!(
            "No snapshot for commit {}",
            from_oid
        )));
    }

    let (to_label, to_files): (String, HashMap<String, String>) = match to_oid {
        Some(oid) => {
            let dir = snapshot_dir(h5i_root, oid);
            if !dir.exists() {
                return Err(H5iError::InvalidPath(format!(
                    "No snapshot for commit {}",
                    oid
                )));
            }
            (short_oid(oid), read_dir_files(&dir)?)
        }
        None => {
            let live = claude_memory_dir(workdir);
            if !live.exists() {
                return Err(H5iError::InvalidPath(format!(
                    "Claude memory directory not found: {}",
                    live.display()
                )));
            }
            ("live".to_string(), read_dir_files(&live)?)
        }
    };

    let from_files = read_dir_files(&from_dir)?;

    let mut added = vec![];
    let mut removed = vec![];
    let mut modified = vec![];

    for (name, content) in &to_files {
        if !from_files.contains_key(name) {
            added.push((name.clone(), content.clone()));
        }
    }
    for (name, content) in &from_files {
        if !to_files.contains_key(name) {
            removed.push((name.clone(), content.clone()));
        }
    }
    for (name, from_content) in &from_files {
        if let Some(to_content) = to_files.get(name) {
            if from_content != to_content {
                let hunks = compute_diff_with_context(from_content, to_content, 3);
                modified.push(ModifiedFile {
                    name: name.clone(),
                    hunks,
                });
            }
        }
    }

    added.sort_by(|a, b| a.0.cmp(&b.0));
    removed.sort_by(|a, b| a.0.cmp(&b.0));
    modified.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(MemoryDiff {
        from_label: short_oid(from_oid),
        to_label,
        added_files: added,
        removed_files: removed,
        modified_files: modified,
    })
}

/// Copy a snapshot back to Claude's live memory directory.
/// Returns the number of files restored.
pub fn restore_snapshot(
    h5i_root: &Path,
    workdir: &Path,
    commit_oid: &str,
) -> Result<usize, H5iError> {
    let snap_dir = snapshot_dir(h5i_root, commit_oid);
    if !snap_dir.exists() {
        return Err(H5iError::InvalidPath(format!(
            "No snapshot found for commit {}",
            commit_oid
        )));
    }

    let mem_dir = claude_memory_dir(workdir);
    fs::create_dir_all(&mem_dir)?;

    let mut count = 0;
    for entry in fs::read_dir(&snap_dir)? {
        let entry = entry?;
        let fname = entry.file_name();
        if fname == "_meta.json" || !entry.path().is_file() {
            continue;
        }
        fs::copy(entry.path(), mem_dir.join(&fname))?;
        count += 1;
    }

    Ok(count)
}

// ── Display helpers ───────────────────────────────────────────────────────────

pub fn print_memory_log(h5i_root: &Path) -> Result<(), H5iError> {
    let snapshots = list_snapshots(h5i_root)?;

    if snapshots.is_empty() {
        println!(
            "  {} No memory snapshots yet. Run {} to create one.",
            style("ℹ").blue(),
            style("h5i memory snapshot").bold()
        );
        return Ok(());
    }

    println!(
        "{}",
        style(format!(
            "{:<10}  {:<22}  {}",
            "COMMIT", "TIMESTAMP", "FILES"
        ))
        .bold()
        .underlined()
    );

    for snap in snapshots.iter().rev() {
        println!(
            "{}  {}  {} file{}",
            style(short_oid(&snap.commit_oid)).magenta().bold(),
            style(snap.timestamp.format("%Y-%m-%d %H:%M UTC")).dim(),
            style(snap.file_count).cyan(),
            if snap.file_count == 1 { "" } else { "s" },
        );
    }

    Ok(())
}

pub fn print_memory_diff(diff: &MemoryDiff) {
    let has_changes =
        !diff.added_files.is_empty() || !diff.removed_files.is_empty() || !diff.modified_files.is_empty();

    println!(
        "{} {}",
        style(format!(
            "memory diff {}..{}",
            diff.from_label, diff.to_label
        ))
        .bold(),
        if !has_changes {
            style("(no changes)").dim().to_string()
        } else {
            String::new()
        }
    );

    if !has_changes {
        return;
    }

    println!("{}", style("─".repeat(60)).dim());

    for (name, content) in &diff.added_files {
        println!("  {}  {}", style("added   ").green().bold(), style(name).green());
        for line in content.lines().take(5) {
            println!("    {}  {}", style("+").green(), style(line).dim());
        }
        let total = content.lines().count();
        if total > 5 {
            println!("    {} {} more line{}", style("+").green(), total - 5, if total - 5 == 1 { "" } else { "s" });
        }
    }

    for (name, _) in &diff.removed_files {
        println!("  {}  {}", style("removed ").red().bold(), style(name).red());
    }

    for file in &diff.modified_files {
        println!("  {}  {}", style("modified").yellow().bold(), style(&file.name).yellow());
        for line in &file.hunks {
            match line {
                DiffLine::Added(s) => println!("    {}  {}", style("+").green(), style(s).green()),
                DiffLine::Removed(s) => println!("    {}  {}", style("-").red(), style(s).red()),
                DiffLine::Context(s) => println!("     {}  {}", style(" ").dim(), style(s).dim()),
            }
        }
    }

    println!("{}", style("─".repeat(60)).dim());

    let summary = format!(
        "{} added, {} removed, {} modified",
        diff.added_files.len(),
        diff.removed_files.len(),
        diff.modified_files.len()
    );
    println!("  {}", style(summary).bold());
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn read_dir_files(dir: &Path) -> Result<HashMap<String, String>, H5iError> {
    let mut files = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname == "_meta.json" || !entry.path().is_file() {
            continue;
        }
        let content = fs::read_to_string(entry.path())?;
        files.insert(fname, content);
    }
    Ok(files)
}

fn short_oid(oid: &str) -> String {
    oid[..8.min(oid.len())].to_string()
}

/// LCS-based line diff, trimmed to `context` lines of surrounding context.
fn compute_diff_with_context(from: &str, to: &str, context: usize) -> Vec<DiffLine> {
    let a: Vec<&str> = from.lines().collect();
    let b: Vec<&str> = to.lines().collect();
    let all = lcs_diff(&a, &b);

    // Mark which indices are changed
    let changed: Vec<bool> = all
        .iter()
        .map(|l| !matches!(l, DiffLine::Context(_)))
        .collect();

    if !changed.iter().any(|&c| c) {
        return vec![];
    }

    // Build a show-mask: keep context lines around each change
    let len = all.len();
    let mut show = vec![false; len];
    for (i, &is_changed) in changed.iter().enumerate() {
        if is_changed {
            let start = i.saturating_sub(context);
            let end = (i + context + 1).min(len);
            for j in start..end {
                show[j] = true;
            }
        }
    }

    let mut result = vec![];
    let mut gap = false;
    for (i, line) in all.into_iter().enumerate() {
        if show[i] {
            if gap {
                result.push(DiffLine::Context("···".to_string()));
                gap = false;
            }
            result.push(line);
        } else if !gap && i > 0 {
            gap = true;
        }
    }

    result
}

/// Pure LCS diff — returns every line tagged as Context/Added/Removed.
fn lcs_diff<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffLine> {
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut result = vec![];
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n && a[i] == b[j] {
            result.push(DiffLine::Context(a[i].to_string()));
            i += 1;
            j += 1;
        } else if j < n && (i >= m || dp[i + 1][j] < dp[i][j + 1]) {
            result.push(DiffLine::Added(b[j].to_string()));
            j += 1;
        } else {
            result.push(DiffLine::Removed(a[i].to_string()));
            i += 1;
        }
    }

    result
}
