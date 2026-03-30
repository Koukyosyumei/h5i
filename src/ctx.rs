/// Agent context workspace — structured reasoning memory for LLM agents.
///
/// Implements the data structures and operations from:
///   "Git Context Controller: Manage the Context of Agents by Agentic Git"
///   arXiv:2508.00031
///
/// The context workspace is stored entirely in the `refs/h5i/context` Git ref
/// — a lightweight commit-chain whose tree mirrors the former `.h5i-ctx/` layout:
///
/// ```text
/// refs/h5i/context tree:
/// ├── main.md               # global roadmap: goals, milestones, active branches
/// ├── .current_branch       # active branch name
/// └── branches/
///     └── <branch-name>/
///         ├── commit.md     # milestone summaries (append-only log)
///         ├── trace.md      # OTA (Observation–Thought–Action) execution trace
///         └── metadata.yaml # file structure, deps, env config
/// ```
///
/// Exposed via `h5i context` subcommands.
use std::fmt::Write as FmtWrite;
use std::path::Path;

use chrono::Utc;
use git2::{ObjectType, Oid, Repository, Signature};
use serde::{Deserialize, Serialize};

use crate::error::H5iError;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Git ref that stores the context workspace as a commit chain.
pub const CTX_REF: &str = "refs/h5i/context";

/// Legacy directory name kept for display / migration messages only.
pub const CTX_DIR: &str = ".h5i-ctx";
#[doc(hidden)]
pub const GCC_DIR: &str = CTX_DIR;

pub const MAIN_BRANCH: &str = "main";

// ── Data types ────────────────────────────────────────────────────────────────

/// A single commit entry appended to `commit.md`.
#[derive(Debug, Clone)]
pub struct CommitEntry {
    pub branch_purpose: String,
    pub previous_summary: String,
    pub contribution: String,
    pub timestamp: String,
    pub short_id: String,
}

/// Options for the CONTEXT command.
#[derive(Debug, Default)]
pub struct ContextOpts {
    pub branch: Option<String>,
    /// If set, return only the commit entry whose short ID starts with this hash prefix.
    pub commit_hash: Option<String>,
    pub show_log: bool,
    /// Offset `k` into the log lines (sliding-window start position).
    pub log_offset: usize,
    pub metadata_segment: Option<String>,
    pub window: usize, // number of recent commits to show (default K)
}

/// Structured metadata stored in `metadata.yaml`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GccMetadata {
    pub file_structure: std::collections::HashMap<String, String>,
    pub env_config: std::collections::HashMap<String, String>,
    pub dependencies: Vec<DepEntry>,
    #[serde(default)]
    pub extra: std::collections::HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DepEntry {
    pub name: String,
    pub purpose: String,
}

/// High-level view returned by `gcc_context`.
#[derive(Serialize, Debug, Clone, Default)]
pub struct GccContext {
    pub project_goal: String,
    pub milestones: Vec<String>,
    pub active_branches: Vec<String>,
    pub current_branch: String,
    pub recent_commits: Vec<String>,
    pub recent_log_lines: Vec<String>,
    pub metadata_snippet: Option<String>,
}

// ── Git helpers ───────────────────────────────────────────────────────────────

fn ctx_git_repo(workdir: &Path) -> Result<Repository, H5iError> {
    Repository::discover(workdir).map_err(H5iError::Git)
}

/// Read a single virtual file from the tip of `refs/h5i/context`.
fn ctx_read_file(repo: &Repository, vpath: &str) -> Option<String> {
    let reference = repo.find_reference(CTX_REF).ok()?;
    let commit = reference.peel_to_commit().ok()?;
    let tree = commit.tree().ok()?;
    let entry = tree.get_path(Path::new(vpath)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    std::str::from_utf8(blob.content()).ok().map(str::to_owned)
}

/// Create a new commit on `refs/h5i/context` applying the given (path, content) changes
/// to the current tree. Handles arbitrarily nested paths (e.g. `branches/main/trace.md`).
fn ctx_write_files(
    repo: &Repository,
    changes: &[(&str, &str)],
    message: &str,
) -> Result<(), H5iError> {
    let sig = repo
        .signature()
        .or_else(|_| Signature::now("h5i", "h5i@local"))
        .map_err(H5iError::Git)?;

    let parent = repo
        .find_reference(CTX_REF)
        .ok()
        .and_then(|r| r.peel_to_commit().ok());
    let current_tree = parent.as_ref().and_then(|c| c.tree().ok());

    let new_tree_oid = apply_changes_to_tree(repo, current_tree.as_ref(), changes)?;
    let new_tree = repo.find_tree(new_tree_oid).map_err(H5iError::Git)?;

    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some(CTX_REF), &sig, &sig, message, &new_tree, &parents)
        .map_err(H5iError::Git)?;

    Ok(())
}

/// Recursively build a Git tree by applying `(relative_path, content)` changes onto
/// an optional base tree. Supports nested paths like `branches/main/commit.md`.
fn apply_changes_to_tree(
    repo: &Repository,
    base: Option<&git2::Tree>,
    changes: &[(&str, &str)],
) -> Result<Oid, H5iError> {
    // Partition into leaves (single component) and nested (two+ components).
    let mut leaves: Vec<(&str, &str)> = Vec::new();
    let mut nested: std::collections::HashMap<&str, Vec<(&str, &str)>> =
        std::collections::HashMap::new();

    for &(path, content) in changes {
        match path.split_once('/') {
            Some((dir, rest)) => nested.entry(dir).or_default().push((rest, content)),
            None => leaves.push((path, content)),
        }
    }

    let mut builder = repo.treebuilder(base).map_err(H5iError::Git)?;

    // Write leaf blobs.
    for (name, content) in leaves {
        let oid = repo.blob(content.as_bytes()).map_err(H5iError::Git)?;
        builder.insert(name, oid, 0o100644).map_err(H5iError::Git)?;
    }

    // Recurse into subdirectories.
    for (dir, sub_changes) in nested {
        let sub_base = base.and_then(|t| {
            t.get_name(dir)
                .filter(|e| e.kind() == Some(ObjectType::Tree))
                .and_then(|e| repo.find_tree(e.id()).ok())
        });
        let sub_oid = apply_changes_to_tree(repo, sub_base.as_ref(), &sub_changes)?;
        builder.insert(dir, sub_oid, 0o040000).map_err(H5iError::Git)?;
    }

    builder.write().map_err(H5iError::Git)
}

/// List branch names stored under `branches/` in the context tree.
fn ctx_list_branches_git(repo: &Repository) -> Vec<String> {
    let tree = repo
        .find_reference(CTX_REF)
        .ok()
        .and_then(|r| r.peel_to_commit().ok())
        .and_then(|c| c.tree().ok());
    let tree = match tree {
        Some(t) => t,
        None => return vec![],
    };
    let branches_oid = match tree
        .get_name("branches")
        .filter(|e| e.kind() == Some(ObjectType::Tree))
        .map(|e| e.id())
    {
        Some(oid) => oid,
        None => return vec![],
    };
    let branches_tree = match repo.find_tree(branches_oid) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let mut names: Vec<String> = Vec::new();
    collect_branch_names(repo, &branches_tree, "", &mut names);
    names.sort();
    names
}

/// Recursively walk a subtree under `branches/`. A tree entry is considered a
/// branch if it contains a blob named `commit.md`; otherwise we recurse into
/// nested trees (supporting slash-separated names like `experiment/alt`).
fn collect_branch_names(repo: &Repository, tree: &git2::Tree, prefix: &str, out: &mut Vec<String>) {
    for entry in tree.iter() {
        let Some(entry_name) = entry.name() else { continue };
        if entry.kind() != Some(ObjectType::Tree) {
            continue;
        }
        let full_name = if prefix.is_empty() {
            entry_name.to_owned()
        } else {
            format!("{prefix}/{entry_name}")
        };
        let Ok(subtree) = repo.find_tree(entry.id()) else { continue };
        // A branch directory contains `commit.md`.
        if subtree.get_name("commit.md").is_some() {
            out.push(full_name);
        } else {
            collect_branch_names(repo, &subtree, &full_name, out);
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialize the context workspace in `refs/h5i/context`.
pub fn init(workdir: &Path, goal: &str) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;

    // If the ref already exists, only ensure the main branch files are present.
    if repo.find_reference(CTX_REF).is_ok() {
        return ensure_branch_git(&repo, MAIN_BRANCH, "Primary development branch");
    }

    let main_content = format!(
        "# Project Roadmap\n\n\
         ## Goal\n{goal}\n\n\
         ## Milestones\n- [ ] Initial setup\n\n\
         ## Active Branches\n- main (primary)\n\n\
         ## Notes\n_Add project-wide notes here._\n"
    );
    let commit_content = format!(
        "# Branch: {MAIN_BRANCH}\n\n\
         **Purpose:** Primary development branch\n\n\
         _Commits will be appended below._\n\n"
    );
    let trace_content = format!("# OTA Log — Branch: {MAIN_BRANCH}\n\n");
    let meta_content = "file_structure: {}\nenv_config: {}\ndependencies: []\n";

    ctx_write_files(
        &repo,
        &[
            ("main.md", &main_content),
            (".current_branch", MAIN_BRANCH),
            (
                &format!("branches/{MAIN_BRANCH}/commit.md"),
                &commit_content,
            ),
            (
                &format!("branches/{MAIN_BRANCH}/trace.md"),
                &trace_content,
            ),
            (
                &format!("branches/{MAIN_BRANCH}/metadata.yaml"),
                meta_content,
            ),
        ],
        "h5i context init",
    )
}

/// Return `true` if `refs/h5i/context` exists in this repository.
pub fn is_initialized(workdir: &Path) -> bool {
    ctx_git_repo(workdir)
        .map(|repo| repo.find_reference(CTX_REF).is_ok())
        .unwrap_or(false)
}

/// Return the current active branch name.
pub fn current_branch(workdir: &Path) -> String {
    ctx_git_repo(workdir)
        .ok()
        .and_then(|repo| ctx_read_file(&repo, ".current_branch"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| MAIN_BRANCH.to_string())
}

fn set_current_branch(repo: &Repository, branch: &str) -> Result<(), H5iError> {
    ctx_write_files(repo, &[(".current_branch", branch)], "h5i context checkout")
}

/// COMMIT — checkpoint the agent's current progress as a structured milestone.
pub fn gcc_commit(workdir: &Path, summary: &str, contribution: &str) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;
    let branch = current_branch(workdir);

    let commit_path = format!("branches/{branch}/commit.md");
    let trace_path = format!("branches/{branch}/trace.md");

    let existing_commit = ctx_read_file(&repo, &commit_path).unwrap_or_default();
    let previous_summary = extract_latest_summary(&existing_commit);
    let branch_purpose = extract_branch_purpose(&existing_commit)
        .unwrap_or_else(|| format!("Branch: {branch}"));

    let short_id = short_timestamp_id();
    let ts = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();

    let entry = format!(
        "## Commit {short_id} — {ts}\n\n\
         ### Branch Purpose\n{branch_purpose}\n\n\
         ### Previous Progress Summary\n{previous_summary}\n\n\
         ### This Commit's Contribution\n{contribution}\n\n\
         ---\n\n"
    );
    let new_commit_md = format!("{existing_commit}{entry}");

    let existing_trace = ctx_read_file(&repo, &trace_path).unwrap_or_default();
    let log_marker = format!("\n\n---\n_[Checkpoint: {short_id} — {summary}]_\n---\n\n");
    let new_trace = format!("{existing_trace}{log_marker}");

    let existing_main = ctx_read_file(&repo, "main.md").unwrap_or_default();
    let new_main = append_main_note(&existing_main, &branch, summary);

    ctx_write_files(
        &repo,
        &[
            (&commit_path, &new_commit_md),
            (&trace_path, &new_trace),
            ("main.md", &new_main),
        ],
        &format!("h5i context commit: {summary}"),
    )
}

/// BRANCH — create a new isolated reasoning workspace and switch to it.
pub fn gcc_branch(workdir: &Path, name: &str, purpose: &str) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;
    ensure_branch_git(&repo, name, purpose)?;
    set_current_branch(&repo, name)
}

/// Switch the active branch without creating it.
pub fn gcc_checkout(workdir: &Path, name: &str) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;
    if !ctx_list_branches_git(&repo).contains(&name.to_string()) {
        return Err(H5iError::InvalidPath(format!(
            "Context branch '{name}' does not exist. Run `h5i context branch {name}` first."
        )));
    }
    set_current_branch(&repo, name)
}

/// MERGE — synthesize a completed branch into the current branch.
pub fn gcc_merge(workdir: &Path, source_branch: &str) -> Result<String, H5iError> {
    let repo = ctx_git_repo(workdir)?;
    let target = current_branch(workdir);

    if !ctx_list_branches_git(&repo).contains(&source_branch.to_string()) {
        return Err(H5iError::InvalidPath(format!(
            "Branch '{source_branch}' not found"
        )));
    }

    let source_commit_path = format!("branches/{source_branch}/commit.md");
    let source_trace_path = format!("branches/{source_branch}/trace.md");
    let target_commit_path = format!("branches/{target}/commit.md");
    let target_trace_path = format!("branches/{target}/trace.md");

    let source_commit_text = ctx_read_file(&repo, &source_commit_path).unwrap_or_default();
    let source_summary = extract_latest_summary(&source_commit_text);
    let source_purpose = extract_branch_purpose(&source_commit_text)
        .unwrap_or_else(|| source_branch.to_string());

    let target_commit_text = ctx_read_file(&repo, &target_commit_path).unwrap_or_default();
    let target_summary = extract_latest_summary(&target_commit_text);

    let ts = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let short_id = short_timestamp_id();

    let merged_summary = format!(
        "Merged branch '{source_branch}' into '{target}'.\n\n\
         From {source_branch}: {source_summary}\n\n\
         From {target}: {target_summary}"
    );
    let contribution = format!(
        "MERGE of '{source_branch}' (purpose: {source_purpose}) into '{target}'.\n\
         Combined reasoning and outcomes from both branches."
    );

    let source_log = ctx_read_file(&repo, &source_trace_path).unwrap_or_default();
    let target_log = ctx_read_file(&repo, &target_trace_path).unwrap_or_default();
    let new_trace = format!(
        "{target_log}\n\n---\n_[MERGE from '{source_branch}' at {ts}]_\n\n{source_log}\n---\n"
    );

    let merge_entry = format!(
        "## Commit {short_id} — {ts} [MERGE: {source_branch} → {target}]\n\n\
         ### Branch Purpose\nMerge of branch '{source_branch}'\n\n\
         ### Previous Progress Summary\n{merged_summary}\n\n\
         ### This Commit's Contribution\n{contribution}\n\n\
         ---\n\n"
    );
    let new_commit = format!("{target_commit_text}{merge_entry}");

    let existing_main = ctx_read_file(&repo, "main.md").unwrap_or_default();
    let new_main = append_main_note(
        &existing_main,
        &target,
        &format!("Merged branch '{source_branch}'"),
    );

    ctx_write_files(
        &repo,
        &[
            (&target_trace_path, &new_trace),
            (&target_commit_path, &new_commit),
            ("main.md", &new_main),
        ],
        &format!("h5i context merge: {source_branch} → {target}"),
    )?;

    Ok(merged_summary)
}

/// CONTEXT — retrieve structured context at multiple granularities.
pub fn gcc_context(workdir: &Path, opts: &ContextOpts) -> Result<GccContext, H5iError> {
    let repo = ctx_git_repo(workdir)?;
    let branch_name = opts
        .branch
        .clone()
        .unwrap_or_else(|| current_branch(workdir));

    let main_text = ctx_read_file(&repo, "main.md").unwrap_or_default();
    let project_goal = extract_section(&main_text, "Goal");
    let milestones = extract_list_items(&extract_section(&main_text, "Milestones"));
    let active_branches = ctx_list_branches_git(&repo);

    let commit_path = format!("branches/{branch_name}/commit.md");
    let commit_text = ctx_read_file(&repo, &commit_path).unwrap_or_default();

    let window = if opts.window == 0 { 3 } else { opts.window };
    let recent_commits = if let Some(ref hash) = opts.commit_hash {
        find_commit_by_hash(&commit_text, hash)
            .map(|e| vec![e])
            .unwrap_or_default()
    } else {
        extract_recent_commits(&commit_text, window)
    };

    let recent_log_lines = if opts.show_log {
        let trace_path = format!("branches/{branch_name}/trace.md");
        let log_text = ctx_read_file(&repo, &trace_path).unwrap_or_default();
        let all_lines: Vec<&str> = log_text.lines().collect();
        let total = all_lines.len();
        let budget = (window * 20).max(40);
        let end = total.saturating_sub(opts.log_offset);
        let start = end.saturating_sub(budget);
        all_lines[start..end].iter().map(|l| l.to_string()).collect()
    } else {
        vec![]
    };

    let metadata_snippet = if let Some(ref seg) = opts.metadata_segment {
        let meta_path = format!("branches/{branch_name}/metadata.yaml");
        let meta_text = ctx_read_file(&repo, &meta_path).unwrap_or_default();
        Some(extract_yaml_segment(&meta_text, seg))
    } else {
        None
    };

    Ok(GccContext {
        project_goal,
        milestones,
        active_branches,
        current_branch: branch_name,
        recent_commits,
        recent_log_lines,
        metadata_snippet,
    })
}

/// Append an OTA (Observation–Thought–Action) entry to the current branch's `trace.md`.
pub fn append_log(workdir: &Path, kind: &str, content: &str) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;
    let branch = current_branch(workdir);
    let trace_path = format!("branches/{branch}/trace.md");

    let ts = Utc::now().format("%H:%M:%S").to_string();
    let entry = format!("[{ts}] {}: {}\n", kind.to_uppercase(), content);

    let existing = ctx_read_file(&repo, &trace_path).unwrap_or_default();
    ctx_write_files(
        &repo,
        &[(&trace_path, &format!("{existing}{entry}"))],
        "h5i context trace",
    )
}

/// Update `metadata.yaml` for the current branch.
pub fn update_metadata(workdir: &Path, meta: &GccMetadata) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;
    let branch = current_branch(workdir);
    let meta_path = format!("branches/{branch}/metadata.yaml");
    let yaml = serde_yaml_serialize(meta);
    ctx_write_files(&repo, &[(&meta_path, &yaml)], "h5i context metadata")
}

/// Write a single arbitrary file into the context workspace.
/// Useful for directly updating `main.md` (e.g. to tick off a milestone).
pub fn write_ctx_file(workdir: &Path, vpath: &str, content: &str) -> Result<(), H5iError> {
    let repo = ctx_git_repo(workdir)?;
    ctx_write_files(&repo, &[(vpath, content)], "h5i context write")
}

/// List all branch names in the context workspace.
pub fn list_branches(workdir: &Path) -> Vec<String> {
    ctx_git_repo(workdir)
        .map(|repo| ctx_list_branches_git(&repo))
        .unwrap_or_default()
}

// ── Terminal display ──────────────────────────────────────────────────────────

pub fn print_context(ctx: &GccContext) {
    use console::style;

    println!(
        "{}",
        style("── Context ─────────────────────────────────────────────").dim()
    );
    println!(
        "  {} {}  (branch: {})",
        style("Project:").bold(),
        if ctx.project_goal.is_empty() {
            style("(no goal set)".to_string()).dim()
        } else {
            style(ctx.project_goal.chars().take(80).collect::<String>()).cyan()
        },
        style(&ctx.current_branch).magenta(),
    );

    if !ctx.milestones.is_empty() {
        println!();
        println!("  {}", style("Milestones:").bold());
        for m in &ctx.milestones {
            let done = m.starts_with("[x]") || m.starts_with("[X]");
            let label: String = m.chars().take(80).collect();
            if done {
                println!("    {} {}", style("✔").green(), style(&label).dim());
            } else {
                println!("    {} {}", style("○").yellow(), label);
            }
        }
    }

    if ctx.active_branches.len() > 1 {
        println!();
        println!(
            "  {} {}",
            style("Branches:").bold(),
            ctx.active_branches
                .iter()
                .map(|b| {
                    if b == &ctx.current_branch {
                        style(format!("* {b}")).green().to_string()
                    } else {
                        style(b.clone()).dim().to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("  ·  ")
        );
    }

    if !ctx.recent_commits.is_empty() {
        println!();
        println!("  {}", style("Recent Commits:").bold());
        for c in &ctx.recent_commits {
            let preview: String = c.chars().take(100).collect();
            println!("    {} {}", style("◈").cyan(), preview);
        }
    }

    if !ctx.recent_log_lines.is_empty() {
        println!();
        println!("  {}", style("Recent OTA Log:").bold());
        for line in ctx.recent_log_lines.iter().take(10) {
            println!("    {}", style(line).dim());
        }
    }
}

pub fn print_status(workdir: &Path) -> Result<(), H5iError> {
    use console::style;

    if !is_initialized(workdir) {
        println!(
            "{} {} not initialized. Run {} to initialize.",
            style("ℹ").blue(),
            style(CTX_REF).yellow(),
            style("h5i context init").bold()
        );
        return Ok(());
    }

    let repo = ctx_git_repo(workdir)?;
    let branch = current_branch(workdir);
    let branches = ctx_list_branches_git(&repo);

    let commit_text = ctx_read_file(&repo, &format!("branches/{branch}/commit.md"))
        .unwrap_or_default();
    let trace_text = ctx_read_file(&repo, &format!("branches/{branch}/trace.md"))
        .unwrap_or_default();

    let commit_count = commit_text.matches("## Commit ").count();
    let log_lines = trace_text.lines().count();

    println!(
        "{}",
        style("── Context Status ──────────────────────────────────────────────").dim()
    );
    println!(
        "  {} {}  |  {} branch{}  |  {} commit{}  |  {} log line{}",
        style("Active branch:").dim(),
        style(&branch).magenta().bold(),
        style(branches.len()).cyan(),
        if branches.len() == 1 { "" } else { "es" },
        style(commit_count).cyan(),
        if commit_count == 1 { "" } else { "s" },
        style(log_lines).dim(),
        if log_lines == 1 { "" } else { "s" },
    );

    if branches.len() > 1 {
        let others: Vec<&String> = branches.iter().filter(|b| b.as_str() != branch).collect();
        println!(
            "  {} {}",
            style("Other branches:").dim(),
            others
                .iter()
                .map(|b| b.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(())
}

// ── System prompt ─────────────────────────────────────────────────────────────

pub fn system_prompt(workdir: &Path) -> String {
    let status_block = if is_initialized(workdir) {
        let branch = current_branch(workdir);
        let branches = list_branches(workdir);
        let goal = ctx_git_repo(workdir)
            .ok()
            .and_then(|repo| ctx_read_file(&repo, "main.md"))
            .map(|t| extract_section(&t, "Goal"))
            .unwrap_or_default();
        format!(
            "\n## Current Workspace State\n\
             - Active branch: `{branch}`\n\
             - All branches: {}\n\
             - Project goal: {}\n\
             \n\
             **Start this session** by running `h5i context show --log` to restore your full working context.\n",
            branches.join(", "),
            if goal.is_empty() { "(not set)".to_string() } else { goal }
        )
    } else {
        "\n## Getting Started\n\
         Run `h5i context init --goal \"<your project goal>\"` to initialize the workspace.\n"
            .to_string()
    };

    format!(
        r#"# Git Context Controller (GCC)

You are working within a GCC workspace that organizes your memory as a persistent,
versioned Git ref (`{CTX_REF}`). Use the commands below to manage context across
long-horizon tasks. GCC prevents context-window overflow by externalizing reasoning
into structured files that survive session boundaries.
{status_block}
## File System Layout

```
refs/h5i/context tree:
├── main.md                    # global roadmap: goal, milestones, notes
├── .current_branch            # active branch name
└── branches/
    └── <branch>/
        ├── commit.md          # milestone summaries (append-only)
        ├── trace.md           # OTA (Observation–Thought–Action) execution trace
        └── metadata.yaml      # file structure, dependencies, env config
```

## Commands

### `h5i context show [OPTIONS]`
Retrieve your current project state. Returns the global roadmap, active branches,
and recent commit summaries.

**Required calls:**
- **At the start of every session** — run `h5i context show --log` to restore context.
- **Before every MERGE** — review the target branch first.
- Proactively whenever you need to recall prior reasoning.

Options:
  `--log`              Include the recent OTA execution trace from trace.md
  `--branch <name>`    Inspect a specific branch (default: current branch)
  `--commit <hash>`    Retrieve the complete record for a specific commit
  `--window <N>`       Number of recent commits to show (default: 3)
  `--log-offset <N>`   Scroll back N lines in the log (for older traces)

### `h5i context trace --kind <KIND> "<content>"`
Append a reasoning step to the execution trace. Call **continuously** during
execution to record every significant step. KIND is one of:
  `OBSERVE`  — an external observation (tool output, test result, file content)
  `THINK`    — internal reasoning, hypothesis, or plan adjustment
  `ACT`      — an action taken (edit, command, API call)
  `NOTE`     — a free-form annotation or reminder

### `h5i context commit "<summary>" [--detail "<contribution>"]`
Checkpoint meaningful progress. Call when you complete a coherent milestone:
implementing a function, passing a test suite, resolving a subgoal.
- `summary`    — one-line description (used in main.md and as the rolling summary)
- `--detail`   — full narrative of what was achieved since the last commit

### `h5i context branch <name> [--purpose "<why>"]`
Create an isolated workspace for exploring an alternative without disrupting the
main trajectory.

### `h5i context checkout <name>`
Switch to an existing branch.

### `h5i context merge <branch>`
Integrate a completed branch into the current branch.

### `h5i context status`
Show active branch, commit count, and log size.

## Workflow Pattern

```
# Session start (mandatory)
h5i context show --trace

# During execution (continuous)
h5i context trace --kind OBSERVE "test suite output: 3 failures in auth module"
h5i context trace --kind THINK   "failures are in token validation; likely a regex issue"
h5i context trace --kind ACT     "editing src/auth/token.rs validate() function"

# Reaching a milestone
h5i context commit "Fixed token validation regex" \
  --detail "Replaced greedy quantifier with possessive; all 47 auth tests now pass."

# Session end
h5i context status
```

## Guidelines
1. Log every OTA step — fine-grained traces are the primary recovery mechanism.
2. Commit at every meaningful milestone, not just at the end.
3. Branch before any risky or divergent exploration.
4. Always run `h5i context show` at the start of a new session.
5. Update main.md milestones via `h5i context write main.md <content>` when goals complete.
"#
    )
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn ensure_branch_git(repo: &Repository, name: &str, purpose: &str) -> Result<(), H5iError> {
    // Only write files that don't already exist in the tree.
    let commit_path = format!("branches/{name}/commit.md");
    let trace_path = format!("branches/{name}/trace.md");
    let meta_path = format!("branches/{name}/metadata.yaml");

    let missing_commit = ctx_read_file(repo, &commit_path).is_none();
    let missing_trace = ctx_read_file(repo, &trace_path).is_none();
    let missing_meta = ctx_read_file(repo, &meta_path).is_none();

    if !missing_commit && !missing_trace && !missing_meta {
        return Ok(()); // already exists
    }

    let mut changes: Vec<(&str, String)> = Vec::new();
    let commit_content;
    let trace_content;
    let meta_content;

    if missing_commit {
        commit_content = format!(
            "# Branch: {name}\n\n\
             **Purpose:** {purpose}\n\n\
             _Commits will be appended below._\n\n"
        );
        changes.push((&commit_path, commit_content.clone()));
    } else {
        commit_content = String::new();
    }
    if missing_trace {
        trace_content = format!("# OTA Log — Branch: {name}\n\n");
        changes.push((&trace_path, trace_content.clone()));
    } else {
        trace_content = String::new();
    }
    if missing_meta {
        meta_content = "file_structure: {}\nenv_config: {}\ndependencies: []\n".to_string();
        changes.push((&meta_path, meta_content.clone()));
    } else {
        meta_content = String::new();
    }

    let _ = (commit_content, trace_content, meta_content); // suppress unused warnings

    let str_changes: Vec<(&str, &str)> = changes.iter().map(|(p, c)| (*p, c.as_str())).collect();
    ctx_write_files(repo, &str_changes, &format!("h5i context branch: {name}"))
}

/// Append a one-line progress note to `main.md` under `## Notes`.
fn append_main_note(content: &str, branch: &str, summary: &str) -> String {
    let ts = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let note = format!("- [{ts}] `{branch}`: {summary}\n");

    if let Some(pos) = content.find("## Notes") {
        let after = &content[pos..];
        let insert_at = pos + after.find('\n').map(|i| i + 1).unwrap_or(after.len());
        let mut new = content.to_string();
        new.insert_str(insert_at, &note);
        new
    } else {
        format!("{content}\n## Notes\n{note}")
    }
}

fn extract_latest_summary(commit_text: &str) -> String {
    let entries: Vec<&str> = commit_text.split("## Commit ").collect();
    if let Some(last) = entries.last() {
        if let Some(start) = last.find("### This Commit's Contribution") {
            let after = &last[start + "### This Commit's Contribution".len()..];
            let end = after.find("\n---").unwrap_or(after.len());
            return after[..end].trim().to_string();
        }
    }
    String::new()
}

fn extract_branch_purpose(commit_text: &str) -> Option<String> {
    let after = commit_text.split("**Purpose:**").nth(1)?;
    let end = after.find('\n').unwrap_or(after.len());
    Some(after[..end].trim().to_string())
}

fn find_commit_by_hash(commit_text: &str, hash_prefix: &str) -> Option<String> {
    for entry in commit_text.split("## Commit ").skip(1) {
        if entry.starts_with(hash_prefix) {
            if let Some(start) = entry.find("### This Commit's Contribution") {
                let after = &entry[start + "### This Commit's Contribution".len()..];
                let end = after.find("\n---").unwrap_or(after.len());
                return Some(format!("[{}] {}", hash_prefix, after[..end].trim()));
            }
            return Some(entry.lines().next().unwrap_or("").trim().to_string());
        }
    }
    None
}

fn extract_recent_commits(commit_text: &str, window: usize) -> Vec<String> {
    let entries: Vec<&str> = commit_text.split("## Commit ").skip(1).collect();
    entries
        .iter()
        .rev()
        .take(window)
        .map(|e| {
            if let Some(start) = e.find("### This Commit's Contribution") {
                let after = &e[start + "### This Commit's Contribution".len()..];
                let end = after.find("\n---").unwrap_or(after.len());
                after[..end].trim().chars().take(120).collect()
            } else {
                e.lines().next().unwrap_or("").trim().chars().take(80).collect()
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn extract_section(text: &str, section: &str) -> String {
    let header = format!("## {section}");
    if let Some(start) = text.find(&header) {
        let after = &text[start + header.len()..];
        let end = after.find("\n## ").unwrap_or(after.len());
        return after[..end].trim().to_string();
    }
    String::new()
}

fn extract_list_items(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.trim_start().starts_with("- "))
        .map(|l| l.trim_start_matches('-').trim().to_string())
        .collect()
}

fn extract_yaml_segment(yaml: &str, segment: &str) -> String {
    let key = format!("{segment}:");
    if let Some(start) = yaml.find(&key) {
        let after = &yaml[start..];
        let end = after[key.len()..]
            .find(|c: char| c.is_alphabetic() && !c.is_whitespace())
            .map(|i| i + key.len())
            .unwrap_or(after.len());
        return after[..end].trim().to_string();
    }
    String::new()
}

fn short_timestamp_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:08x}", ts)
}

fn serde_yaml_serialize(meta: &GccMetadata) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "file_structure:");
    if meta.file_structure.is_empty() {
        let _ = writeln!(out, "  {{}}");
    } else {
        let mut pairs: Vec<_> = meta.file_structure.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in pairs {
            let _ = writeln!(out, "  \"{k}\": \"{v}\"");
        }
    }

    let _ = writeln!(out, "env_config:");
    if meta.env_config.is_empty() {
        let _ = writeln!(out, "  {{}}");
    } else {
        let mut pairs: Vec<_> = meta.env_config.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in pairs {
            let _ = writeln!(out, "  \"{k}\": \"{v}\"");
        }
    }

    let _ = writeln!(out, "dependencies:");
    if meta.dependencies.is_empty() {
        let _ = writeln!(out, "  []");
    } else {
        for dep in &meta.dependencies {
            let _ = writeln!(out, "  - name: \"{}\"", dep.name);
            let _ = writeln!(out, "    purpose: \"{}\"", dep.purpose);
        }
    }

    if !meta.extra.is_empty() {
        let mut pairs: Vec<_> = meta.extra.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in pairs {
            let _ = writeln!(out, "{k}: \"{v}\"");
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::tempdir;

    /// Create a bare-minimum git repo in `dir` so ctx functions can discover it.
    fn git_init(dir: &Path) {
        Repository::init(dir).expect("failed to init git repo");
    }

    // ── init / is_initialized ─────────────────────────────────────────────────

    #[test]
    fn init_creates_workspace() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "Build something great").unwrap();
        assert!(is_initialized(dir.path()));
        assert!(list_branches(dir.path()).contains(&"main".to_string()));
    }

    #[test]
    fn is_initialized_false_before_init() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        assert!(!is_initialized(dir.path()));
    }

    #[test]
    fn is_initialized_true_after_init() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "Test goal").unwrap();
        assert!(is_initialized(dir.path()));
    }

    #[test]
    fn init_embeds_goal_in_main_md() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "Build an OAuth2 login system").unwrap();
        let ctx = gcc_context(dir.path(), &ContextOpts::default()).unwrap();
        assert!(ctx.project_goal.contains("Build an OAuth2 login system"));
    }

    #[test]
    fn init_idempotent_does_not_overwrite_goal() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "Original goal").unwrap();
        // Re-init should not overwrite because the ref already exists.
        init(dir.path(), "New goal").unwrap();
        let ctx = gcc_context(dir.path(), &ContextOpts::default()).unwrap();
        assert!(ctx.project_goal.contains("Original goal"));
    }

    // ── current_branch / set_current_branch ──────────────────────────────────

    #[test]
    fn current_branch_defaults_to_main() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        assert_eq!(current_branch(dir.path()), "main");
    }

    #[test]
    fn gcc_branch_switches_active_branch() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_branch(dir.path(), "experiment", "try new approach").unwrap();
        assert_eq!(current_branch(dir.path()), "experiment");
    }

    // ── gcc_checkout ──────────────────────────────────────────────────────────

    #[test]
    fn gcc_checkout_switches_to_existing_branch() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_branch(dir.path(), "feature", "feature work").unwrap();
        gcc_checkout(dir.path(), "main").unwrap();
        assert_eq!(current_branch(dir.path()), "main");
    }

    #[test]
    fn gcc_checkout_fails_on_nonexistent_branch() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        assert!(gcc_checkout(dir.path(), "does_not_exist").is_err());
    }

    // ── list_branches ─────────────────────────────────────────────────────────

    #[test]
    fn list_branches_after_init_has_main() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        assert!(list_branches(dir.path()).contains(&"main".to_string()));
    }

    #[test]
    fn list_branches_includes_new_branches() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_branch(dir.path(), "feat-oauth", "oauth work").unwrap();
        assert!(list_branches(dir.path()).contains(&"feat-oauth".to_string()));
    }

    // ── append_log ────────────────────────────────────────────────────────────

    #[test]
    fn append_log_adds_entry_to_trace() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        append_log(dir.path(), "OBSERVE", "Redis latency is 2ms").unwrap();
        let ctx = gcc_context(
            dir.path(),
            &ContextOpts { show_log: true, window: 3, ..Default::default() },
        )
        .unwrap();
        assert!(ctx
            .recent_log_lines
            .iter()
            .any(|l| l.contains("OBSERVE: Redis latency is 2ms")));
    }

    #[test]
    fn append_log_uppercases_kind() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        append_log(dir.path(), "think", "reasoning step").unwrap();
        let ctx = gcc_context(
            dir.path(),
            &ContextOpts { show_log: true, window: 3, ..Default::default() },
        )
        .unwrap();
        assert!(ctx.recent_log_lines.iter().any(|l| l.contains("THINK:")));
    }

    // ── gcc_commit ────────────────────────────────────────────────────────────

    #[test]
    fn gcc_commit_appends_entry() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_commit(dir.path(), "Milestone 1 done", "Implemented the login form").unwrap();
        let ctx =
            gcc_context(dir.path(), &ContextOpts { window: 3, ..Default::default() }).unwrap();
        assert!(ctx.recent_commits.iter().any(|c| c.contains("Implemented the login form")));
    }

    #[test]
    fn gcc_commit_updates_main_md_notes() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_commit(dir.path(), "Completed auth setup", "Added JWT tokens").unwrap();
        let repo = ctx_git_repo(dir.path()).unwrap();
        let main = ctx_read_file(&repo, "main.md").unwrap();
        assert!(main.contains("Completed auth setup"));
    }

    // ── gcc_context ───────────────────────────────────────────────────────────

    #[test]
    fn gcc_context_reads_goal_from_main_md() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "Build an OAuth2 login system").unwrap();
        let ctx = gcc_context(dir.path(), &ContextOpts::default()).unwrap();
        assert_eq!(ctx.project_goal, "Build an OAuth2 login system");
    }

    #[test]
    fn gcc_context_reads_milestones() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        // Update main.md to add a milestone via write_ctx_file.
        let repo = ctx_git_repo(dir.path()).unwrap();
        let mut content = ctx_read_file(&repo, "main.md").unwrap();
        content = content.replace(
            "- [ ] Initial setup",
            "- [x] Initial setup\n- [ ] Add rate limiting",
        );
        write_ctx_file(dir.path(), "main.md", &content).unwrap();

        let ctx = gcc_context(dir.path(), &ContextOpts::default()).unwrap();
        assert!(ctx.milestones.iter().any(|m| m.contains("Add rate limiting")));
    }

    #[test]
    fn gcc_context_includes_current_branch() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        let ctx = gcc_context(dir.path(), &ContextOpts::default()).unwrap();
        assert_eq!(ctx.current_branch, "main");
    }

    #[test]
    fn gcc_context_returns_recent_commits() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_commit(dir.path(), "milestone", "did the work").unwrap();
        let ctx =
            gcc_context(dir.path(), &ContextOpts { window: 3, ..Default::default() }).unwrap();
        assert!(!ctx.recent_commits.is_empty());
        assert!(ctx.recent_commits[0].contains("did the work"));
    }

    // ── gcc_merge ─────────────────────────────────────────────────────────────

    #[test]
    fn gcc_merge_combines_branches() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        gcc_branch(dir.path(), "experiment", "try algo").unwrap();
        gcc_commit(dir.path(), "Experiment done", "Found faster algorithm").unwrap();
        gcc_checkout(dir.path(), "main").unwrap();
        let summary = gcc_merge(dir.path(), "experiment").unwrap();
        assert!(summary.contains("experiment"));
    }

    #[test]
    fn gcc_merge_fails_for_nonexistent_branch() {
        let dir = tempdir().unwrap();
        git_init(dir.path());
        init(dir.path(), "goal").unwrap();
        assert!(gcc_merge(dir.path(), "ghost_branch").is_err());
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    #[test]
    fn extract_section_returns_correct_content() {
        let text = "## Goal\nBuild something\n\n## Milestones\n- item\n";
        assert_eq!(extract_section(text, "Goal"), "Build something");
    }

    #[test]
    fn extract_section_returns_empty_when_missing() {
        assert_eq!(extract_section("no sections here", "Goal"), "");
    }

    #[test]
    fn extract_list_items_parses_bullet_list() {
        let text = "- [ ] First\n- [x] Done\n- Third\n";
        let items = extract_list_items(text);
        assert_eq!(items.len(), 3);
        assert!(items[0].contains("First"));
    }

    #[test]
    fn extract_recent_commits_returns_latest_first_when_multiple() {
        let commit_text = "## Commit aaa111 — 2026-01-01\n\
            ### This Commit's Contribution\nFirst contribution\n---\n\
            ## Commit bbb222 — 2026-01-02\n\
            ### This Commit's Contribution\nSecond contribution\n---\n";
        let recent = extract_recent_commits(commit_text, 2);
        assert_eq!(recent.len(), 2);
        assert!(recent.last().unwrap().contains("Second contribution"));
    }
}
