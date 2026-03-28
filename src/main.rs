use clap::{Parser, Subcommand};
use console::style;
use git2::Oid;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use h5i_core::blame::BlameMode;
use h5i_core::claude::{keyword_search, AnthropicClient};
use h5i_core::ctx;
use h5i_core::memory;
use h5i_core::metadata::{AiMetadata, IntegrityLevel, Severity, TestSource};
use h5i_core::session_log;
use h5i_core::repository::H5iRepository;
use h5i_core::review::REVIEW_THRESHOLD;
use h5i_core::session::LocalSession;
use h5i_core::ui::{ERROR, LOOKING, STEP, SUCCESS, WARN};
use h5i_core::watcher::start_h5i_watcher;

#[derive(Parser)]
#[command(name = "h5i", about = "Advanced Git for the AI Era", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the h5i sidecar in the current repository
    Init,

    /// Start a real-time recording session for a specific file
    Session {
        /// The source file to watch and sync via CRDT
        #[arg(short, long)]
        file: PathBuf,
    },

    /// Commit staged changes with AI provenance and quality tracking
    Commit {
        /// Standard Git commit message
        #[arg(short, long)]
        message: String,

        // Prompt
        #[arg(long)]
        prompt: Option<String>,

        /// The name of the AI model that assisted in these changes
        #[arg(long)]
        model: Option<String>,

        /// The unique ID of the AI agent
        #[arg(long)]
        agent: Option<String>,

        /// Scan staged source files for `// h5_i_test_start` / `// h5_i_test_end` markers
        #[arg(long)]
        tests: bool,

        /// Path to a JSON file produced by a test adapter (any tool, any language).
        /// Takes precedence over --tests and H5I_TEST_RESULTS.
        /// Schema: { "tool", "passed", "failed", "skipped", "total",
        ///           "duration_secs", "coverage", "exit_code", "summary" }
        #[arg(long, value_name = "FILE")]
        test_results: Option<std::path::PathBuf>,

        /// Shell command to run as the test suite.
        /// h5i captures its exit code and tries to parse stdout as h5i JSON.
        /// Used when no --test-results file is provided.
        #[arg(long, value_name = "CMD")]
        test_cmd: Option<String>,

        /// Enable AST-based structural tracking for the commit
        #[arg(long)]
        ast: bool,

        #[arg(long)]
        audit: bool,

        #[arg(long)]
        force: bool,

        /// OID(s) of commits that causally triggered this one.
        /// Can be specified multiple times: --caused-by abc123 --caused-by def456
        #[arg(long, value_name = "OID", action = clap::ArgAction::Append)]
        caused_by: Option<Vec<String>>,
    },

    /// Display the enriched 5D commit history
    Log {
        /// Number of recent commits to display
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },

    /// Analyze file ownership with optional structural (AST) logic
    Blame {
        /// Path to the file to inspect
        file: PathBuf,

        /// Mode of blame: 'line' (standard) or 'ast' (semantic)
        #[arg(short, long, default_value = "line")]
        mode: String,
    },

    /// Resolve branch conflicts using CRDT-based semantic merging
    Resolve {
        /// OID of the local branch (OURS)
        ours: String,
        /// OID of the incoming branch (THEIRS)
        theirs: String,
        /// Relative path to the file to resolve
        file: String,
    },

    /// Show the AST-level structural diff for a file
    Diff {
        /// Path to the file to analyse (must be a supported language, e.g. .py)
        file: PathBuf,

        /// Compare from this commit OID (default: HEAD)
        #[arg(long)]
        from: Option<String>,

        /// Compare to this commit OID (default: working-tree file)
        #[arg(long)]
        to: Option<String>,
    },

    /// Revert the AI-generated commit whose intent best matches a description
    Rollback {
        /// Natural-language description of the change to undo (e.g. "OAuth login")
        intent: String,

        /// Number of recent commits to search
        #[arg(short, long, default_value_t = 50)]
        limit: usize,

        /// Show the matched commit without actually reverting
        #[arg(long)]
        dry_run: bool,

        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Launch the h5i web dashboard in your browser
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value_t = 7150)]
        port: u16,
    },

    /// Push all h5i refs (notes + memory) to a remote in one shot
    Push {
        /// Remote to push to
        #[arg(short, long, default_value = "origin")]
        remote: String,
    },

    /// Print the Claude Code hook configuration to enable automatic prompt capture
    Hooks,

    /// Version-control Claude's memory state alongside your code
    Memory {
        #[command(subcommand)]
        action: MemoryCommands,
    },

    /// Inspect AI session activity: footprint, uncertainty, churn, and intent graph
    /// (analogous to `git notes` — structured annotations attached to commits)
    Notes {
        #[command(subcommand)]
        action: NotesCommands,
    },

    /// Manage the agent reasoning workspace across sessions
    /// (git-style branching/committing applied to `.h5i-ctx/`, arXiv:2508.00031)
    Context {
        #[command(subcommand)]
        action: ContextCommands,
    },

    /// Generate a structured handoff briefing to resume an AI session
    Resume {
        /// Branch to resume (defaults to current branch)
        branch: Option<String>,
    },
}

#[derive(Subcommand)]
enum NotesCommands {
    /// Parse a Claude Code session log and store enriched metadata linked to a commit
    /// (footprint, causal chain, uncertainty, file churn)
    Analyze {
        /// Path to the Claude Code .jsonl session file (default: auto-detect latest session)
        #[arg(long, value_name = "JSONL")]
        session: Option<PathBuf>,
        /// Commit OID to link this analysis to (default: HEAD)
        #[arg(long)]
        commit: Option<String>,
    },

    /// Show which files the AI consulted vs edited for a given commit
    Show {
        /// Commit OID whose session analysis to display (default: HEAD)
        commit: Option<String>,
    },

    /// Show moments where the AI expressed uncertainty, optionally filtered by file
    Uncertainty {
        /// Commit OID whose session analysis to display (default: HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Filter to annotations recorded while editing this file
        #[arg(long)]
        file: Option<String>,
    },

    /// Show file edit-churn across all analyzed sessions
    Churn {
        /// Number of files to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },

    /// Visualise the chain of intents associated with recent commits
    Graph {
        /// Number of recent commits to include
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
        /// Intent source: 'prompt' uses the stored AI prompt; 'analyze' calls Claude
        #[arg(long, default_value = "prompt")]
        mode: String,
    },

    /// Identify commits most likely to benefit from human review
    Review {
        /// Number of recent commits to scan
        #[arg(short, long, default_value_t = 100)]
        limit: usize,
        /// Minimum score threshold (0.0–1.0) for a commit to be flagged
        #[arg(long, default_value_t = REVIEW_THRESHOLD)]
        min_score: f32,
        /// Output raw JSON instead of the styled table
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ContextCommands {
    /// Initialize the `.h5i-ctx/` reasoning workspace for this project
    Init {
        /// High-level project goal written to main.md
        #[arg(long, default_value = "")]
        goal: String,
    },

    /// Checkpoint the agent's current progress as a structured milestone
    /// (like `git commit` but for the reasoning workspace)
    Commit {
        /// One-line summary of what was accomplished
        summary: String,
        /// Detailed description of this commit's contribution
        #[arg(long, default_value = "")]
        detail: String,
    },

    /// Create a new isolated reasoning branch for exploring an alternative
    /// (like `git branch` but for the `.h5i-ctx/` workspace)
    Branch {
        /// Branch name (e.g. "experiment/cache-strategy")
        name: String,
        /// Why this branch exists / what hypothesis it explores
        #[arg(long, default_value = "")]
        purpose: String,
    },

    /// Switch to an existing reasoning branch
    /// (like `git checkout` but for the `.h5i-ctx/` workspace)
    Checkout {
        /// Branch name to switch to
        name: String,
    },

    /// Merge a completed reasoning branch into the current branch
    /// (like `git merge` but for the `.h5i-ctx/` workspace)
    Merge {
        /// Name of the branch to merge in
        branch: String,
    },

    /// Retrieve the current project state at multiple levels of detail
    /// (like `git show` — global roadmap, recent commits, optional trace)
    Show {
        /// Show context for this branch (default: current branch)
        #[arg(long)]
        branch: Option<String>,
        /// Return the complete record for a specific commit hash
        #[arg(long)]
        commit: Option<String>,
        /// Include recent OTA execution trace from trace.md
        #[arg(long)]
        trace: bool,
        /// Retrieve a specific metadata segment from metadata.yaml (e.g. "file_structure")
        #[arg(long)]
        metadata: Option<String>,
        /// Number of recent commits to show (context window K)
        #[arg(long, default_value_t = 3)]
        window: usize,
        /// Scroll back N lines in the trace (sliding-window offset k)
        #[arg(long, default_value_t = 0)]
        trace_offset: usize,
    },

    /// Append an OTA (Observation–Thought–Action) step to the current branch trace
    Trace {
        /// Step type: OBSERVE, THINK, ACT, or NOTE
        #[arg(long, default_value = "NOTE")]
        kind: String,
        /// Trace entry content
        content: String,
    },

    /// Show the current reasoning workspace state (branch, commit count, trace size)
    Status,

    /// Print a system prompt for injecting h5i context commands into a Claude agent session
    Prompt,
}

#[derive(Subcommand)]
enum MemoryCommands {
    /// Snapshot Claude's current memory into .git/.h5i/memory/<commit-oid>/
    Snapshot {
        /// Git commit OID to associate this snapshot with (default: HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Override the source directory to snapshot (default: ~/.claude/projects/<repo>/memory/)
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
    },

    /// Show how Claude's memory changed between two snapshots
    Diff {
        /// Snapshot to diff from (default: second-to-last snapshot)
        from: Option<String>,
        /// Snapshot to diff to; omit to compare against live memory (default: latest snapshot)
        to: Option<String>,
    },

    /// List all memory snapshots
    Log,

    /// Restore Claude's memory to the state captured in a snapshot
    Restore {
        /// Commit OID whose snapshot to restore
        commit: String,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Push the latest memory snapshot to a git remote via refs/h5i/memory
    Push {
        /// Remote to push to
        #[arg(short, long, default_value = "origin")]
        remote: String,
    },

    /// Fetch a teammate's memory snapshot from a git remote
    Pull {
        /// Remote to pull from
        #[arg(short, long, default_value = "origin")]
        remote: String,
    },
}

const H5I_CLAUDE_INSTRUCTIONS: &str = r#"## h5i Integration

This repository uses **h5i** (a Git sidecar for AI-era version control). Prefer h5i commands over raw git equivalents. h5i stores metadata in `refs/h5i/notes` and `refs/h5i/memory`; these refs are NOT included in a plain `git push` — use `h5i push` to share them.

---

### Committing

Always use `h5i commit` instead of `git commit`.

When **you** (Claude) made or assisted with the change, always record AI provenance:

```
h5i commit -m "add retry logic to HTTP client" \
  --model claude-sonnet-4-6 \
  --agent claude-code \
  --prompt "add exponential backoff to the HTTP client"
```

Additional flags to add when relevant:
- `--tests`  — when tests were added or modified (captures test metrics)
- `--audit`  — on security-sensitive, authentication, or high-risk changes

**Example output:**
```
✔  Committed a3f8c12  add retry logic to HTTP client
   model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
```

---

### Understanding History

```
h5i log --limit 10    # recent commits with AI metadata (model, agent, token count)
h5i blame src/main.rs # line-level blame annotated with AI provenance per commit
```

**Example `h5i log` output:**
```
● a3f8c12  add retry logic to HTTP client
  2026-03-27 14:02  Alice <alice@example.com>
  model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
  prompt: "add exponential backoff to the HTTP client"

● 9e21b04  fix off-by-one in parser
  2026-03-26 11:45  Bob <bob@example.com>
  (no AI metadata)
```

---

### Notes — Session Analysis

`h5i notes` parses Claude Code session logs and stores enriched metadata (exploration footprint, causal chain, uncertainty moments, file churn) linked to a commit.

**Typical workflow after finishing a task:**

```bash
# 1. Analyze the just-completed Claude Code session and link to HEAD
h5i notes analyze

# 2. Inspect what files Claude consulted vs edited
h5i notes show

# 3. See where Claude expressed uncertainty
h5i notes uncertainty

# 4. See where Claude expressed uncertainty while editing a specific file
h5i notes uncertainty --file src/repository.rs

# 5. View cumulative edit-churn across all analyzed sessions
h5i notes churn

# 6. Visualize the chain of intents across recent commits
h5i notes graph --limit 20

# 7. Identify commits that most need human review
h5i notes review --limit 50
```

**Example `h5i notes show` output:**
```
── Exploration Footprint ──────────────────────────────────
  Session a3f8c12d  ·  42 messages  ·  138 tool calls

  Files Consulted:
    📖 src/repository.rs  ×4  (Read,Grep)
    📖 src/metadata.rs    ×2  (Read)

  Files Edited:
    ✏ src/repository.rs  ×3 edit(s)
    ✏ src/main.rs         ×1 edit(s)

── Causal Chain ─────────────────────────────────────────────
  Trigger:
    "add exponential backoff to the HTTP client"

  Key Decisions:
    1. Used tokio::time::sleep for async-compatible delay
    2. Capped retries at 5 to avoid infinite loops

  Considered / Rejected:
    - Synchronous std::thread::sleep (incompatible with async runtime)
```

**Example `h5i notes review` output:**
```
Suggested Review Points — 2 commits flagged (scanned 50, min_score=0.40)
──────────────────────────────────────────────────────────────
  #1  a3f8c12  score 0.74  ████████░░
     Alice · 2026-03-27 14:02 UTC
     add retry logic to HTTP client
     ⚠ high uncertainty · 5 edits · 4 files touched

  #2  9e21b04  score 0.45  ████░░░░░░
     Bob · 2026-03-26 11:45 UTC
     refactor parser
     moderate complexity
```

---

### Context — Reasoning Workspace

`h5i context` manages a `.h5i-ctx/` workspace that lets you checkpoint, branch, and review your own reasoning across sessions — analogous to git but for *agent thinking* rather than code.

**Initialize once per project (or per major task):**

```bash
h5i context init --goal "refactor the HTTP client to support retries and timeouts"
```

**During a task, use these commands to structure your reasoning:**

```bash
# Checkpoint progress after completing a logical step
h5i context commit "analyzed existing HTTP client" \
  --detail "read repository.rs and metadata.rs; identified retry entry points"

# Log individual OTA (Observe–Think–Act) steps as you work
h5i context trace --kind OBSERVE "HttpClient::send has no retry logic"
h5i context trace --kind THINK   "exponential backoff with jitter is safest"
h5i context trace --kind ACT     "added retry loop in send() with 5-attempt cap"

# Explore an alternative approach without losing your current thread
h5i context branch experiment/sync-retry --purpose "try sync retry as a simpler fallback"
# ... explore ...
h5i context checkout main   # return to main reasoning branch
h5i context merge experiment/sync-retry  # merge findings back if useful

# Review current state before continuing a task
h5i context show --trace --window 5
h5i context status
```

**Example `h5i context show` output:**
```
── h5i-ctx · branch: main ──────────────────────────────────
  Goal: refactor the HTTP client to support retries and timeouts

  Recent commits (3):
    [c1a2b3] analyzed existing HTTP client
    [d4e5f6] implemented retry loop
    [g7h8i9] added timeout parameter

── Trace (last 10 lines) ────────────────────────────────────
  [OBSERVE] HttpClient::send has no retry logic
  [THINK]   exponential backoff with jitter is safest
  [ACT]     added retry loop in send() with 5-attempt cap
  [NOTE]    TODO: add integration test for timeout path
```

Use `h5i context prompt` to get a ready-made system prompt you can prepend to an agent session to inject full context awareness.

---

### Memory Snapshots

After a significant Claude Code session, snapshot Claude's memory so it can be shared or restored:

```bash
h5i memory snapshot        # snapshot current ~/.claude/projects/<repo>/memory/ → HEAD
h5i memory log             # list all snapshots
h5i memory diff            # show what changed since the previous snapshot
h5i memory restore <oid>   # restore memory to the state at a given commit
```

---

### Sharing h5i Data

```bash
h5i push   # push all h5i refs (notes, memory) to origin
h5i pull   # pull h5i refs from origin
```
"#;

fn write_claude_instructions(workdir: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;

    let claude_dir = workdir.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;

    let h5i_md = claude_dir.join("h5i.md");
    if !h5i_md.exists() {
        std::fs::write(&h5i_md, H5I_CLAUDE_INSTRUCTIONS)?;
    }

    let claude_md = workdir.join("CLAUDE.md");
    let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();
    if !existing.contains("@.claude/h5i.md") {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&claude_md)?;
        writeln!(f, "\n@.claude/h5i.md")?;
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => {
            let repo = H5iRepository::open(".")?;
            println!(
                "{} {} at {}",
                SUCCESS,
                style("h5i sidecar initialized").green().bold(),
                style(repo.h5i_path().display()).dim()
            );

            let workdir = std::env::current_dir()?;
            match write_claude_instructions(&workdir) {
                Ok(()) => println!(
                    "{} {} (imported via {})",
                    SUCCESS,
                    style("Claude instructions written to .claude/h5i.md").green(),
                    style("CLAUDE.md").yellow()
                ),
                Err(e) => println!(
                    "{} Could not write Claude instructions: {}",
                    style("warn:").yellow(),
                    e
                ),
            }

            println!();
            println!("  {}", style("Quick-start:").bold());
            println!(
                "    {}  capture AI provenance on every commit",
                style("h5i commit -m \"…\" --prompt \"…\" --agent claude-code").cyan()
            );
            println!(
                "    {}  snapshot Claude's memory after a session",
                style("h5i memory snapshot").cyan()
            );
            println!(
                "    {}  push all h5i data to your remote",
                style("h5i push").cyan()
            );
            println!();
            println!(
                "  {} h5i stores metadata in {} and {}.",
                style("Note:").dim(),
                style("refs/h5i/notes").yellow(),
                style("refs/h5i/memory").yellow()
            );
            println!(
                "  {} These refs are NOT included in a plain {}.",
                style("     ").dim(),
                style("git push").yellow()
            );
            println!(
                "  {} Run {} (or see README §9) to share them with your team.",
                style("     ").dim(),
                style("h5i push").bold()
            );
        }

        Commands::Session { file } => {
            let repo = H5iRepository::open(".")?;
            println!(
                "{} {} for: {}",
                STEP,
                style("Initializing session").cyan().bold(),
                style(file.display()).yellow()
            );

            let mut rng: fastrand::Rng = fastrand::Rng::new();
            let client_id: u64 = rng.u64(0..u64::MAX);
            let session = LocalSession::new(repo.h5i_root.clone(), file, client_id)?;
            let session_arc = Arc::new(Mutex::new(session));

            println!(
                "{} {} (Press Ctrl+C to stop)",
                LOOKING,
                style("Watching for changes...").magenta().italic()
            );

            start_h5i_watcher(session_arc)?;
        }

        Commands::Commit {
            message,
            prompt,
            model,
            agent,
            tests,
            test_results,
            test_cmd,
            ast,
            audit,
            force,
            caused_by,
        } => {
            let repo = H5iRepository::open(".")?;
            let sig = repo.git().signature()?; // Fetch system-default Git signature

            // Resolution order: CLI flag > environment variable > pending_context.json
            let pending = repo.read_pending_context()?;
            let prompt = prompt
                .or_else(|| std::env::var("H5I_PROMPT").ok())
                .or_else(|| pending.as_ref().and_then(|c| c.prompt.clone()));
            let model = model
                .or_else(|| std::env::var("H5I_MODEL").ok())
                .or_else(|| pending.as_ref().and_then(|c| c.model.clone()));
            let agent = agent
                .or_else(|| std::env::var("H5I_AGENT_ID").ok())
                .or_else(|| pending.as_ref().and_then(|c| c.agent_id.clone()));

            if audit {
                let report = repo.verify_integrity(prompt.as_deref(), &message)?;

                // Print a header line based on the overall level.
                match report.level {
                    IntegrityLevel::Violation => println!(
                        "{} {} {}",
                        ERROR,
                        style("INTEGRITY VIOLATION").red().bold(),
                        style(format!("(score: {:.2})", report.score)).dim()
                    ),
                    IntegrityLevel::Warning => println!(
                        "{} {} {}",
                        WARN,
                        style("INTEGRITY WARNING").yellow().bold(),
                        style(format!("(score: {:.2})", report.score)).dim()
                    ),
                    IntegrityLevel::Valid => {
                        println!("{} {}", SUCCESS, style("Integrity check passed.").green());
                    }
                }

                // Print each finding with its rule ID and severity colour.
                for f in &report.findings {
                    let (bullet, label) = match f.severity {
                        Severity::Violation => (
                            style("✖").red().bold(),
                            style(format!("[{}]", f.rule_id)).red().bold(),
                        ),
                        Severity::Warning => (
                            style("⚠").yellow().bold(),
                            style(format!("[{}]", f.rule_id)).yellow().bold(),
                        ),
                        Severity::Info => (
                            style("ℹ").cyan(),
                            style(format!("[{}]", f.rule_id)).cyan(),
                        ),
                    };
                    println!("  {} {} {}", bullet, label, f.detail);
                }

                if matches!(report.level, IntegrityLevel::Violation) && !force {
                    println!(
                        "\n{} Commit aborted. Use {} to override.",
                        style("!").red(),
                        style("--force").bold()
                    );
                    return Ok(());
                }
            }

            let ai_meta = if prompt.is_some() || model.is_some() || agent.is_some() {
                Some(AiMetadata {
                    model_name: model.unwrap_or_else(|| "unknown".into()),
                    agent_id: agent.unwrap_or_else(|| "unknown".into()),
                    prompt: prompt.unwrap_or_else(|| "".into()),
                    usage: None,
                })
            } else {
                None
            };

            // Resolve TestSource — priority:
            //   1. --test-results <file>
            //   2. H5I_TEST_RESULTS env var (path to a JSON file)
            //   3. --test-cmd <cmd>
            //   4. --tests flag (scan staged files for markers)
            //   5. Nothing
            let env_results = std::env::var("H5I_TEST_RESULTS").ok();
            let test_source = if let Some(ref path) = test_results {
                let metrics = repo.load_test_results_from_file(path)?;
                TestSource::Provided(metrics)
            } else if let Some(ref env_path) = env_results {
                let metrics = repo.load_test_results_from_file(std::path::Path::new(env_path))?;
                TestSource::Provided(metrics)
            } else if let Some(ref cmd) = test_cmd {
                println!(
                    "{} Running test command: {}",
                    style("▶").cyan(),
                    style(cmd).yellow()
                );
                let metrics = repo.run_test_command(cmd)?;
                let passing = metrics.is_passing();
                let icon = if passing {
                    style("✔").green()
                } else {
                    style("✖").red()
                };
                if let Some(ref s) = metrics.summary {
                    println!("  {} {}", icon, style(s).dim());
                }
                TestSource::Provided(metrics)
            } else if tests {
                TestSource::ScanMarkers
            } else {
                TestSource::None
            };

            // Build a real language-aware AST parser closure.
            let parser_box = repo.make_ast_parser();
            let ast_parser: Option<&dyn Fn(&std::path::Path) -> Option<String>> = if ast {
                Some(parser_box.as_ref())
            } else {
                None
            };

            let caused_by = caused_by.unwrap_or_default();
            let oid = repo.commit(&message, &sig, &sig, ai_meta, test_source, ast_parser, caused_by)?;
            repo.clear_pending_context()?;
            println!(
                "{} {} {}",
                SUCCESS,
                style("h5i Commit Created:").green(),
                style(oid).magenta().bold()
            );
        }

        Commands::Log { limit } => {
            let repo = H5iRepository::open(".")?;
            repo.print_log(limit)?;
        }

        Commands::Blame { file, mode } => {
            let repo = H5iRepository::open(".")?;
            let blame_mode = if mode.to_lowercase() == "ast" {
                BlameMode::Ast
            } else {
                BlameMode::Line
            };

            let results = repo.blame(&file, blame_mode)?;
            println!(
                "{}",
                style(format!(
                    "{:<4} {:<8} {:<15} | {}",
                    "STAT", "COMMIT", "AUTHOR/AGENT", "CONTENT"
                ))
                .bold()
                .underlined()
            );

            for r in results {
                let test_indicator = match r.test_passed {
                    Some(true) => "✅",
                    Some(false) => "❌",
                    None => "  ",
                };
                let semantic_indicator = if r.is_semantic_change { "✨" } else { "  " };

                println!(
                    "{} {} {} {:<15} | {}",
                    test_indicator,
                    semantic_indicator,
                    style(&r.commit_id[..8]).dim(),
                    style(r.agent_info).blue(),
                    r.line_content
                );
            }
        }

        Commands::Diff { file, from, to } => {
            let repo = H5iRepository::open(".")?;

            let from_oid = from.map(|s| Oid::from_str(&s)).transpose()?;
            let to_oid = to.map(|s| Oid::from_str(&s)).transpose()?;

            let label = match (&from_oid, &to_oid) {
                (None, None) => "HEAD → working tree".to_string(),
                (Some(f), None) => format!("{}… → working tree", &f.to_string()[..8]),
                (None, Some(t)) => format!("HEAD → {}…", &t.to_string()[..8]),
                (Some(f), Some(t)) => format!("{}… → {}…", &f.to_string()[..8], &t.to_string()[..8]),
            };

            println!(
                "{} {} {} {}",
                LOOKING,
                style("Computing structural diff for").cyan().bold(),
                style(file.display()).yellow(),
                style(format!("({label})")).dim(),
            );

            let ast_diff = repo.diff_ast(&file, from_oid, to_oid)?;
            ast_diff.print_stylish(&file.to_string_lossy());
        }

        Commands::Rollback {
            intent,
            limit,
            dry_run,
            yes,
        } => {
            let repo = H5iRepository::open(".")?;

            println!(
                "{} {} \"{}\" {} {} commits",
                LOOKING,
                style("Searching for intent:").cyan().bold(),
                style(&intent).yellow(),
                style("across last").dim(),
                style(limit).dim(),
            );

            let commits = repo.list_ai_commits(limit)?;
            if commits.is_empty() {
                println!("{} No commits found in this repository.", WARN);
                return Ok(());
            }

            // Semantic search via Claude, or fall back to keyword matching.
            let matched_oid: Option<String> = if let Some(claude) = AnthropicClient::from_env() {
                println!(
                    "{} {} {}",
                    STEP,
                    style("Using Claude for semantic search").dim(),
                    style(format!("({})", claude.model())).dim(),
                );
                claude.find_matching_commit(&commits, &intent)?
            } else {
                println!(
                    "{} {} {}",
                    WARN,
                    style("ANTHROPIC_API_KEY not set — using keyword fallback.").yellow(),
                    style("Set it for semantic search.").dim(),
                );
                keyword_search(&commits, &intent).map(|c| c.oid.clone())
            };

            let oid_str = match matched_oid {
                Some(o) => o,
                None => {
                    println!(
                        "{} No commit found matching: \"{}\"",
                        WARN,
                        style(&intent).yellow()
                    );
                    return Ok(());
                }
            };

            let oid = Oid::from_str(&oid_str)?;
            let commit = repo.git().find_commit(oid)?;
            let record = repo.load_h5i_record(oid).ok();

            println!("\n{}", style("Matched commit:").bold().underlined());
            println!(
                "  {} {}",
                style("commit").yellow(),
                style(&oid_str).magenta().bold()
            );
            println!(
                "  {:<10} {}",
                style("Message:").dim(),
                commit.message().unwrap_or("").trim()
            );
            if let Some(ref r) = record {
                if let Some(ref ai) = r.ai_metadata {
                    if !ai.agent_id.is_empty() {
                        println!(
                            "  {:<10} {} {}",
                            style("Agent:").dim(),
                            style(&ai.agent_id).cyan(),
                            style(format!("({})", ai.model_name)).dim(),
                        );
                    }
                    if !ai.prompt.is_empty() {
                        println!(
                            "  {:<10} \"{}\"",
                            style("Prompt:").dim(),
                            style(&ai.prompt).italic()
                        );
                    }
                }
                println!(
                    "  {:<10} {}",
                    style("Date:").dim(),
                    r.timestamp.format("%Y-%m-%d %H:%M UTC")
                );
            }

            if dry_run {
                println!(
                    "\n{} {}",
                    style("--dry-run").bold(),
                    style("No changes made.").dim()
                );
                return Ok(());
            }

            // Warn if later commits causally depend on this one
            let dependents = repo.causal_dependents(oid, 200);
            if !dependents.is_empty() {
                println!(
                    "\n{} {} later commit{} causally depend{} on this one:",
                    style("⚠ Warning:").yellow().bold(),
                    dependents.len(),
                    if dependents.len() == 1 { "" } else { "s" },
                    if dependents.len() == 1 { "s" } else { "" },
                );
                for (dep_oid, dep_msg) in &dependents {
                    println!(
                        "  {} {} {}",
                        style("→").yellow(),
                        style(&dep_oid.to_string()[..8]).magenta(),
                        style(format!("\"{}\"", dep_msg)).dim().italic()
                    );
                }
                if !yes {
                    print!("\nContinue anyway? [y/N] ");
                    use std::io::Write as _;
                    std::io::stdout().flush()?;
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if !input.trim().eq_ignore_ascii_case("y") {
                        println!("{} Aborted.", style("!").dim());
                        return Ok(());
                    }
                }
            }

            if !yes {
                print!("\n{} [y/N] ", style("Revert this commit?").bold());
                use std::io::Write as _;
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("{} Aborted.", style("!").dim());
                    return Ok(());
                }
            }

            let new_oid = repo.revert_commit(oid)?;
            println!(
                "{} {} {}",
                SUCCESS,
                style("Revert commit created:").green(),
                style(new_oid).magenta().bold()
            );
        }

        Commands::Notes { action } => match action {
            NotesCommands::Analyze { session, commit } => {
                let repo = H5iRepository::open(".")?;
                let workdir = repo
                    .git()
                    .workdir()
                    .ok_or_else(|| anyhow::anyhow!("Bare repository not supported"))?
                    .to_path_buf();
                let oid_str = match commit {
                    Some(ref s) => s.clone(),
                    None => repo.git().head()?.peel_to_commit()?.id().to_string(),
                };
                let jsonl_path = match session {
                    Some(p) => p,
                    None => match session_log::find_latest_session(&workdir) {
                        Some(p) => {
                            println!("{} {}", STEP,
                                style(format!("Auto-detected session: {}", p.display())).dim());
                            p
                        }
                        None => {
                            println!("{} No Claude Code session found in ~/.claude/projects/.", WARN);
                            println!("  {} Use {} to specify a session file.",
                                style("ℹ").blue(),
                                style("h5i notes analyze --session <path>").bold());
                            return Ok(());
                        }
                    },
                };
                println!("{} {} → commit {}", STEP,
                    style("Analyzing session log").cyan().bold(),
                    style(&oid_str[..8.min(oid_str.len())]).magenta());
                let analysis = session_log::analyze_session(&jsonl_path)?;
                session_log::save_analysis(&repo.h5i_root, &oid_str, &analysis)?;
                println!("{} {} messages · {} tool calls · {} edited · {} consulted",
                    SUCCESS,
                    style(analysis.message_count).cyan(),
                    style(analysis.tool_call_count).cyan(),
                    style(analysis.footprint.edited.len()).green(),
                    style(analysis.footprint.consulted.len()).yellow());
                println!("  {} Run {} to inspect results.",
                    style("ℹ").blue(),
                    style(format!("h5i notes show {}", &oid_str[..8])).bold());
            }

            NotesCommands::Show { commit } => {
                let repo = H5iRepository::open(".")?;
                let oid_str = match commit {
                    Some(ref s) => s.clone(),
                    None => repo.git().head()?.peel_to_commit()?.id().to_string(),
                };
                match session_log::load_analysis(&repo.h5i_root, &oid_str)? {
                    None => println!(
                        "{} No session analysis for {}. Run {} first.",
                        WARN,
                        style(&oid_str[..8.min(oid_str.len())]).magenta(),
                        style("h5i notes analyze").bold()
                    ),
                    Some(analysis) => {
                        session_log::print_footprint(&analysis);
                        session_log::print_causal_chain(&analysis);
                    }
                }
            }

            NotesCommands::Uncertainty { commit, file } => {
                let repo = H5iRepository::open(".")?;
                let oid_str = match commit {
                    Some(ref s) => s.clone(),
                    None => repo.git().head()?.peel_to_commit()?.id().to_string(),
                };
                match session_log::load_analysis(&repo.h5i_root, &oid_str)? {
                    None => println!(
                        "{} No session analysis for commit {}. Run {} first.",
                        WARN,
                        style(&oid_str[..8.min(oid_str.len())]).magenta(),
                        style("h5i notes analyze").bold()
                    ),
                    Some(analysis) => {
                        session_log::print_uncertainty(&analysis, file.as_deref());
                    }
                }
            }

            NotesCommands::Churn { limit } => {
                let repo = H5iRepository::open(".")?;
                let mut churn = session_log::aggregate_churn(&repo.h5i_root);
                churn.truncate(limit);
                if churn.is_empty() {
                    println!(
                        "{} No churn data yet. Run {} after sessions to build history.",
                        WARN,
                        style("h5i notes analyze").bold()
                    );
                } else {
                    session_log::print_churn(&churn);
                }
            }

            NotesCommands::Graph { limit, mode } => {
                let repo = H5iRepository::open(".")?;
                let analyze = mode.to_lowercase() == "analyze";
                if analyze {
                    if std::env::var("ANTHROPIC_API_KEY").is_err() {
                        println!(
                            "{} {} — set {} to enable Claude analysis.",
                            WARN,
                            style("ANTHROPIC_API_KEY not set, falling back to stored prompts").yellow(),
                            style("ANTHROPIC_API_KEY").bold(),
                        );
                    } else {
                        println!(
                            "{} {} for {} commits…",
                            STEP,
                            style("Calling Claude to generate intent labels").cyan().bold(),
                            style(limit).cyan(),
                        );
                    }
                }
                repo.print_intent_graph(limit, analyze)?;
            }

            NotesCommands::Review { limit, min_score, json } => {
                let repo = H5iRepository::open(".")?;
                let points = repo.suggest_review_points(limit, min_score)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&points)?);
                } else if points.is_empty() {
                    println!(
                        "{} No commits exceeded the review threshold (min_score={:.2}) in the last {} commits.",
                        SUCCESS, min_score, limit
                    );
                } else {
                    println!(
                        "{} — {} commit{} flagged (scanned {}, min_score={:.2})",
                        style("Suggested Review Points").bold().underlined(),
                        style(points.len()).yellow().bold(),
                        if points.len() == 1 { "" } else { "s" },
                        limit, min_score
                    );
                    println!("{}", style("─".repeat(62)).dim());
                    for (i, rp) in points.iter().enumerate() {
                        let filled = (rp.score * 10.0).round() as usize;
                        let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
                        let score_color = if rp.score >= 0.7 {
                            style(format!("{:.2}", rp.score)).red().bold()
                        } else if rp.score >= 0.45 {
                            style(format!("{:.2}", rp.score)).yellow().bold()
                        } else {
                            style(format!("{:.2}", rp.score)).cyan().bold()
                        };
                        println!(
                            "\n  {} {}  score {}  {}",
                            style(format!("#{}", i + 1)).dim(),
                            style(&rp.short_oid).magenta().bold(),
                            score_color,
                            style(&bar).dim()
                        );
                        println!("     {} · {}", style(&rp.author).blue(),
                            style(rp.timestamp.format("%Y-%m-%d %H:%M UTC")).dim());
                        println!("     {}", style(&rp.message).bold());
                        for trigger in &rp.triggers {
                            let bullet = match trigger.rule_id.as_str() {
                                "TEST_REGRESSION" | "INTEGRITY_VIOLATION" => style("⬦").red(),
                                "LARGE_DIFF" | "WIDE_IMPACT" => style("⬦").yellow(),
                                _ => style("⬦").cyan(),
                            };
                            println!("       {} {:<18}  {}", bullet,
                                style(&trigger.rule_id).bold(), style(&trigger.detail).dim());
                        }
                    }
                    println!("\n{}", style("─".repeat(62)).dim());
                }
            }
        },

        Commands::Hooks => {
            let hook_script = r#"#!/usr/bin/env bash
# h5i Claude Code hook — writes the user prompt to .git/.h5i/pending_context.json
# so that `h5i commit` can pick it up automatically without --prompt.
set -euo pipefail
GIT_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
H5I_DIR="$GIT_ROOT/.git/.h5i"
[ -d "$H5I_DIR" ] || exit 0
jq -c '{
  prompt: .prompt,
  model: (env.H5I_MODEL // "claude-sonnet-4-6"),
  agent_id: (env.H5I_AGENT_ID // "claude-code"),
  session_id: .session_id
}' > "$H5I_DIR/pending_context.json"
"#;

            println!("{}", style("── Step 0: Installl `jq` ──").bold());
            println!(
                "If you don't have {} installed, run the following command:\n\n{}\n",
                style("jq").yellow(),
                style("apt install jq").dim()
            );

            println!("{}", style("── Step 1: Save hook script ──").bold());
            println!(
                "Save the following script to {} and make it executable:\n",
                style("~/.claude/hooks/h5i-capture-prompt.sh").yellow()
            );
            println!("{}", style(hook_script).dim());

            println!("{}", style("── Step 2: Add to ~/.claude/settings.json ──").bold());
            println!(
                "Add (or merge) the {} block into your {}:\n",
                style("hooks").yellow(),
                style("~/.claude/settings.json").yellow()
            );
            println!(
                "{}",
                style(
                    r#"{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/h5i-capture-prompt.sh"
          }
        ]
      }
    ]
  }
}"#
                )
                .dim()
            );

            println!(
                "\n{} {} {} {}",
                style("Tip:").bold(),
                "Set",
                style("H5I_MODEL").yellow(),
                "and",
            );
            println!(
                "    {} in your shell profile to override the defaults captured by the hook.",
                style("H5I_AGENT_ID").yellow()
            );
            println!(
                "\n{} {} {} {}",
                style("Env vars").bold(),
                "also work without hooks —",
                style("H5I_PROMPT").yellow() ,
                "/ H5I_MODEL / H5I_AGENT_ID are read automatically at commit time."
            );
        }

        Commands::Serve { port } => {
            let repo = H5iRepository::open(".")?;
            let repo_path = repo
                .git()
                .workdir()
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf();

            println!(
                "{} {} on port {}",
                SUCCESS,
                style("Starting h5i dashboard").green().bold(),
                style(port).cyan()
            );
            println!(
                "  Open {} in your browser",
                style(format!("http://localhost:{}", port)).underlined().blue()
            );
            println!("  Press Ctrl+C to stop\n");

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(h5i_core::server::serve(repo_path, port))?;
        }

        Commands::Push { remote } => {
            let workdir = std::env::current_dir()?;

            println!(
                "{} {} to {}",
                STEP,
                style("Pushing all h5i refs").cyan().bold(),
                style(&remote).yellow()
            );

            // Push h5i notes (AI provenance, test metrics, causal links)
            let notes_refspec = "refs/h5i/notes:refs/h5i/notes";
            print!(
                "  {} {} … ",
                style("→").dim(),
                style("refs/h5i/notes").yellow()
            );
            use std::io::Write as _;
            std::io::stdout().flush()?;
            let notes_status = std::process::Command::new("git")
                .args(["push", &remote, notes_refspec])
                .current_dir(&workdir)
                .status()
                .map_err(|e| anyhow::anyhow!("Failed to invoke git push: {e}"))?;
            if notes_status.success() {
                println!("{}", style("ok").green());
            } else {
                println!("{}", style("failed").red());
            }

            // Push memory ref (Claude memory snapshots)
            let mem_refspec = format!("+{}:{}", memory::MEMORY_REF, memory::MEMORY_REF);
            print!("  {} {} … ", style("→").dim(), style("refs/h5i/memory").yellow());
            std::io::stdout().flush()?;
            let mem_status = std::process::Command::new("git")
                .args(["push", &remote, &mem_refspec])
                .current_dir(&workdir)
                .status()
                .map_err(|e| anyhow::anyhow!("Failed to invoke git push: {e}"))?;
            if mem_status.success() {
                println!("{}", style("ok").green());
            } else {
                println!("{} (no memory snapshots yet — run {})", style("skipped").dim(), style("h5i memory snapshot").bold());
            }

            // Push context workspace (refs/h5i/context)
            print!("  {} {} … ", style("→").dim(), style("refs/h5i/context").yellow());
            std::io::stdout().flush()?;
            let ctx_status = std::process::Command::new("git")
                .args(["push", &remote, "refs/h5i/context:refs/h5i/context"])
                .current_dir(&workdir)
                .status()
                .map_err(|e| anyhow::anyhow!("Failed to invoke git push: {e}"))?;
            if ctx_status.success() {
                println!("{}", style("ok").green());
            } else {
                println!("{} (no context workspace yet — run {})", style("skipped").dim(), style("h5i context init").bold());
            }

            // Push AST blobs (refs/h5i/ast)
            print!("  {} {} … ", style("→").dim(), style("refs/h5i/ast").yellow());
            std::io::stdout().flush()?;
            let ast_status = std::process::Command::new("git")
                .args(["push", &remote, "refs/h5i/ast:refs/h5i/ast"])
                .current_dir(&workdir)
                .status()
                .map_err(|e| anyhow::anyhow!("Failed to invoke git push: {e}"))?;
            if ast_status.success() {
                println!("{}", style("ok").green());
            } else {
                println!("{} (no AST snapshots yet — commit with {})", style("skipped").dim(), style("--ast").bold());
            }

            if notes_status.success() {
                println!(
                    "\n{} To receive these refs on another machine:\n\
                    \n    git fetch {} refs/h5i/notes:refs/h5i/notes\
                    \n    git fetch {} refs/h5i/memory:refs/h5i/memory\
                    \n    git fetch {} refs/h5i/context:refs/h5i/context\
                    \n    git fetch {} refs/h5i/ast:refs/h5i/ast\
                    \n\n  Or add fetch refspecs to .git/config (see README §9) so {} picks them up automatically.",
                    style("Tip:").bold(),
                    style(&remote).yellow(),
                    style(&remote).yellow(),
                    style(&remote).yellow(),
                    style(&remote).yellow(),
                    style("git pull").bold()
                );
            }
        }

        Commands::Memory { action } => {
            let repo = H5iRepository::open(".")?;
            let workdir = repo
                .git()
                .workdir()
                .ok_or_else(|| anyhow::anyhow!("Bare repository not supported"))?
                .to_path_buf();

            match action {
                MemoryCommands::Snapshot { commit, path } => {
                    // Resolve commit OID: explicit arg or HEAD
                    let oid_str = match commit {
                        Some(ref s) => s.clone(),
                        None => {
                            let head = repo.git().head()?;
                            head.peel_to_commit()?.id().to_string()
                        }
                    };

                    let src = path.as_deref();
                    let default_dir = memory::claude_memory_dir(&workdir);
                    let display_src = src
                        .unwrap_or(&default_dir)
                        .display()
                        .to_string();

                    println!(
                        "{} {} → commit {}",
                        STEP,
                        style("Snapshotting Claude memory").cyan().bold(),
                        style(&oid_str[..8.min(oid_str.len())]).magenta()
                    );

                    let count = memory::take_snapshot(&repo.h5i_root, &workdir, &oid_str, src)?;

                    if count == 0 {
                        println!(
                            "{} {} at {}",
                            WARN,
                            style("No memory files found — empty snapshot recorded.").yellow(),
                            style(&display_src).dim()
                        );
                        println!(
                            "  {} Claude Code creates this directory the first time it saves a memory.",
                            style("ℹ").blue()
                        );
                        println!(
                            "  {} You can also snapshot any directory with {}",
                            style("ℹ").blue(),
                            style("h5i memory snapshot --path <dir>").bold()
                        );
                    } else {
                        println!(
                            "{} Saved {} file{} from {}",
                            SUCCESS,
                            style(count).cyan(),
                            if count == 1 { "" } else { "s" },
                            style(&display_src).dim()
                        );
                    }
                }

                MemoryCommands::Diff { from, to } => {
                    // Default: diff last two snapshots (or last snapshot vs. live)
                    let snapshots = memory::list_snapshots(&repo.h5i_root)?;

                    let (from_oid, to_oid_opt): (String, Option<String>) = match (from, to) {
                        (Some(f), t) => (f, t),
                        (None, Some(t)) => {
                            // from = latest snapshot, to = specified
                            let latest = snapshots.last().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "No snapshots found. Run `h5i memory snapshot` first."
                                )
                            })?;
                            (latest.commit_oid.clone(), Some(t))
                        }
                        (None, None) => {
                            // from = second-to-last, to = live
                            if snapshots.is_empty() {
                                println!(
                                    "{} No snapshots yet. Run {} first.",
                                    WARN,
                                    style("h5i memory snapshot").bold()
                                );
                                return Ok(());
                            }
                            let from = snapshots.last().unwrap().commit_oid.clone();
                            (from, None) // to=None means live
                        }
                    };

                    let to_label = to_oid_opt.as_deref().unwrap_or("live");
                    println!(
                        "{} {} {}..{}",
                        LOOKING,
                        style("Computing memory diff").cyan().bold(),
                        style(&from_oid[..8.min(from_oid.len())]).magenta(),
                        style(to_label).magenta()
                    );

                    let diff = memory::diff_snapshots(
                        &repo.h5i_root,
                        &workdir,
                        &from_oid,
                        to_oid_opt.as_deref(),
                    )?;
                    memory::print_memory_diff(&diff);
                }

                MemoryCommands::Log => {
                    println!(
                        "{}\n",
                        style("Claude Memory Snapshots").bold().underlined()
                    );
                    memory::print_memory_log(&repo.h5i_root)?;
                }

                MemoryCommands::Restore { commit, yes } => {
                    let snap_meta = {
                        let snaps = memory::list_snapshots(&repo.h5i_root)?;
                        snaps
                            .into_iter()
                            .find(|s| s.commit_oid.starts_with(&commit))
                            .ok_or_else(|| {
                                anyhow::anyhow!("No snapshot found for commit {}", commit)
                            })?
                    };

                    println!(
                        "{} Restore memory snapshot from commit {} ({} file{})?",
                        WARN,
                        style(&snap_meta.commit_oid[..8]).magenta().bold(),
                        snap_meta.file_count,
                        if snap_meta.file_count == 1 { "" } else { "s" }
                    );
                    println!(
                        "  {} This will overwrite your current Claude memory files.",
                        style("!").yellow()
                    );

                    if !yes {
                        print!("\nContinue? [y/N] ");
                        use std::io::Write as _;
                        std::io::stdout().flush()?;
                        let mut input = String::new();
                        std::io::stdin().read_line(&mut input)?;
                        if !input.trim().eq_ignore_ascii_case("y") {
                            println!("{} Aborted.", style("!").dim());
                            return Ok(());
                        }
                    }

                    let count =
                        memory::restore_snapshot(&repo.h5i_root, &workdir, &snap_meta.commit_oid)?;
                    println!(
                        "{} Restored {} file{} to {}",
                        SUCCESS,
                        style(count).cyan(),
                        if count == 1 { "" } else { "s" },
                        style(memory::claude_memory_dir(&workdir).display().to_string()).dim()
                    );
                }

                MemoryCommands::Push { remote } => {
                    println!(
                        "{} {} to {}",
                        STEP,
                        style("Pushing memory snapshot").cyan().bold(),
                        style(&remote).yellow()
                    );

                    let commit_oid = memory::push(repo.git(), &repo.h5i_root, &remote)?;
                    println!(
                        "{} Memory commit {} pushed to {} ({})",
                        SUCCESS,
                        style(&commit_oid[..8]).magenta().bold(),
                        style(&remote).yellow(),
                        style(memory::MEMORY_REF).dim()
                    );
                    println!(
                        "  {} Teammates can run {} to receive it.",
                        style("→").dim(),
                        style("h5i memory pull").bold()
                    );
                }

                MemoryCommands::Pull { remote } => {
                    println!(
                        "{} {} from {}",
                        STEP,
                        style("Pulling memory snapshot").cyan().bold(),
                        style(&remote).yellow()
                    );

                    let result = memory::pull(repo.git(), &repo.h5i_root, &remote)?;
                    println!(
                        "{} Received {} file{} linked to code commit {}",
                        SUCCESS,
                        style(result.file_count).cyan(),
                        if result.file_count == 1 { "" } else { "s" },
                        style(&result.linked_code_oid[..8.min(result.linked_code_oid.len())])
                            .magenta()
                            .bold()
                    );
                    println!(
                        "  {} Run {} to apply it to your Claude session.",
                        style("→").dim(),
                        style(format!(
                            "h5i memory restore {}",
                            &result.linked_code_oid[..8.min(result.linked_code_oid.len())]
                        ))
                        .bold()
                    );
                }
            }
        }

        Commands::Context { action } => {
            let workdir = Path::new(".");
            match action {
                ContextCommands::Init { goal } => {
                    ctx::init(workdir, &goal)?;
                    println!(
                        "{} {} at {}",
                        SUCCESS,
                        style(".h5i-ctx/ workspace initialized").green().bold(),
                        style(".h5i-ctx/").dim()
                    );
                    println!();
                    println!("  {}", style("Quick-start:").bold());
                    println!(
                        "    {}  checkpoint your progress",
                        style("h5i context commit \"summary\" --detail \"…\"").cyan()
                    );
                    println!(
                        "    {}  explore an alternative",
                        style("h5i context branch experiment/foo --purpose \"…\"").cyan()
                    );
                    println!(
                        "    {}  view current context",
                        style("h5i context show --trace").cyan()
                    );
                }

                ContextCommands::Commit { summary, detail } => {
                    if !ctx::is_initialized(workdir) {
                        anyhow::bail!(".h5i-ctx/ not initialized. Run `h5i context init` first.");
                    }
                    ctx::gcc_commit(workdir, &summary, &detail)?;
                    println!(
                        "{} {} — {}",
                        SUCCESS,
                        style("Context commit recorded").green().bold(),
                        style(&summary).cyan()
                    );
                }

                ContextCommands::Branch { name, purpose } => {
                    if !ctx::is_initialized(workdir) {
                        anyhow::bail!(".h5i-ctx/ not initialized. Run `h5i context init` first.");
                    }
                    ctx::gcc_branch(workdir, &name, &purpose)?;
                    println!(
                        "{} Created and switched to branch {}",
                        SUCCESS,
                        style(&name).magenta().bold()
                    );
                }

                ContextCommands::Checkout { name } => {
                    if !ctx::is_initialized(workdir) {
                        anyhow::bail!(".h5i-ctx/ not initialized. Run `h5i context init` first.");
                    }
                    ctx::gcc_checkout(workdir, &name)?;
                    println!(
                        "{} Switched to branch {}",
                        SUCCESS,
                        style(&name).magenta().bold()
                    );
                }

                ContextCommands::Merge { branch } => {
                    if !ctx::is_initialized(workdir) {
                        anyhow::bail!(".h5i-ctx/ not initialized. Run `h5i context init` first.");
                    }
                    let target = ctx::current_branch(workdir);
                    let summary = ctx::gcc_merge(workdir, &branch)?;
                    println!(
                        "{} Merged {} into {}",
                        SUCCESS,
                        style(&branch).magenta(),
                        style(&target).magenta().bold()
                    );
                    println!("{}", style(&summary).dim());
                }

                ContextCommands::Show {
                    branch,
                    commit,
                    trace,
                    metadata,
                    window,
                    trace_offset,
                } => {
                    if !ctx::is_initialized(workdir) {
                        anyhow::bail!(".h5i-ctx/ not initialized. Run `h5i context init` first.");
                    }
                    let opts = ctx::ContextOpts {
                        branch,
                        commit_hash: commit,
                        show_log: trace,
                        log_offset: trace_offset,
                        metadata_segment: metadata,
                        window,
                    };
                    let snapshot = ctx::gcc_context(workdir, &opts)?;
                    ctx::print_context(&snapshot);
                }

                ContextCommands::Trace { kind, content } => {
                    if !ctx::is_initialized(workdir) {
                        anyhow::bail!(".h5i-ctx/ not initialized. Run `h5i context init` first.");
                    }
                    ctx::append_log(workdir, &kind, &content)?;
                    println!(
                        "{} [{}] {}",
                        style("◈").cyan(),
                        style(kind.to_uppercase()).bold(),
                        style(&content).dim()
                    );
                }

                ContextCommands::Status => {
                    ctx::print_status(workdir)?;
                }

                ContextCommands::Prompt => {
                    print!("{}", ctx::system_prompt(workdir));
                }
            }
        }

        Commands::Resolve { ours, theirs, file } => {
            let repo = H5iRepository::open(".")?;
            let our_oid = Oid::from_str(&ours)?;
            let their_oid = Oid::from_str(&theirs)?;

            println!(
                "{} {} for {}...",
                STEP,
                style("Performing CRDT automatic merge").cyan().bold(),
                style(&file).yellow()
            );
            let merged_text = repo.merge_h5i_logic(our_oid, their_oid, &file)?;

            println!("\n{}\n{}", style("--- Merge Result ---").dim(), merged_text);
            println!(
                "\n{} Tip: Use {} to stage the resolved content.",
                style("💡").yellow(),
                style(format!("git add {}", file)).bold()
            );
            println!(
                "{} {}",
                style("ℹ").blue(),
                style("Note: Resolution was derived mathematically from Git Notes metadata.").dim()
            );
        }

        Commands::Resume { branch } => {
            let repo = H5iRepository::open(".")?;
            let workdir = repo
                .git()
                .workdir()
                .ok_or_else(|| anyhow::anyhow!("Bare repository not supported"))?
                .to_path_buf();
            if let Some(ref b) = branch {
                println!(
                    "{} {} {}",
                    STEP,
                    style("Generating handoff briefing for branch").cyan().bold(),
                    style(b).yellow()
                );
            } else {
                println!(
                    "{} {}",
                    STEP,
                    style("Generating handoff briefing...").cyan().bold()
                );
            }
            match h5i_core::resume::generate_briefing(&repo, &workdir, branch.as_deref()) {
                Ok(briefing) => h5i_core::resume::print_briefing(&briefing),
                Err(e) => println!("{} Failed to generate briefing: {}", ERROR, style(e).red()),
            }
        }
    }

    Ok(())
}
