use clap::{Parser, Subcommand};
use console::style;
use git2::Oid;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use h5i_core::blame::BlameMode;
use h5i_core::metadata::{AiMetadata, IntegrityLevel};
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

        /// Enable automatic test provenance detection
        #[arg(long)]
        tests: bool,

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
            ast,
            audit,
            force,
        } => {
            let repo = H5iRepository::open(".")?;
            let sig = repo.git().signature()?; // Fetch system-default Git signature

            if audit {
                let report = repo.verify_integrity(prompt.as_deref(), &message)?;
                match report.level {
                    IntegrityLevel::Violation => {
                        println!(
                            "{} {} {}",
                            ERROR,
                            style("INTEGRITY VIOLATION").red().bold(),
                            style(format!("(Score: {:.2})", report.score)).dim()
                        );
                        for f in report.findings {
                            println!("  {} {}", style("-").red(), f);
                        }
                        if !force {
                            println!(
                                "\n{} Commit aborted. Use {} to override.",
                                style("!").red(),
                                style("--force").bold()
                            );
                            return Ok(());
                        }
                    }
                    IntegrityLevel::Warning => {
                        println!(
                            "{} {} {}",
                            WARN,
                            style("INTEGRITY WARNING").yellow().bold(),
                            style(format!("(Score: {:.2})", report.score)).dim()
                        );
                        for f in report.findings {
                            println!("  {} {}", style("-").yellow(), f);
                        }
                    }
                    IntegrityLevel::Valid => {
                        println!("{} {}", SUCCESS, style("Integrity check passed.").green());
                    }
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

            // Simple demo AST parser hook
            let ast_parser = if ast {
                Some(
                    &(|_p: &std::path::Path| Some("(ast-node-root)".to_string()))
                        as &dyn Fn(&std::path::Path) -> Option<String>,
                )
            } else {
                None
            };

            let oid = repo.commit(&message, &sig, &sig, ai_meta, tests, ast_parser)?;
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
        }
    }

    Ok(())
}
