use clap::{Parser, Subcommand};
use console::style;
use git2::Oid;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use h5i_core::blame::BlameMode;
use h5i_core::claude::{keyword_search, AnthropicClient};
use h5i_core::metadata::{AiMetadata, IntegrityLevel, Severity, TestSource};
use h5i_core::repository::H5iRepository;
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

    /// Print the Claude Code hook configuration to enable automatic prompt capture
    InstallHooks,

    /// Launch the h5i web dashboard in your browser
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value_t = 7150)]
        port: u16,
    },
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

            let oid = repo.commit(&message, &sig, &sig, ai_meta, test_source, ast_parser)?;
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

        Commands::InstallHooks => {
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
    }

    Ok(())
}
