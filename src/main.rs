use clap::{Parser, Subcommand};
use h5i_core::repository::H5iRepository;
use h5i_core::session::LocalAgentSession;
use h5i_core::watcher::start_h5i_watcher;
use std::path::PathBuf;

/*

#[derive(Parser)]
#[command(name = "h5i", about = "Advanced Git for the AI Era")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 指定したファイルのリアルタイム記録とAI同期を開始する
    Start {
        #[arg(short, long)]
        file: PathBuf,
    },
    // ... 他のコマンド (commit, log, blame等)
}

fn main() -> h5i_core::error::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start { file } => {
            // 1. リポジトリの発見
            let repo = H5iRepository::open(".")?;

            // 2. セッションの開始
            println!("🚀 Initializing h5i session for: {:?}", file);
            let session = LocalAgentSession::new(repo.h5i_root.clone(), file)?;

            // 3. ウォッチャーの起動 (無限ループ)
            println!("👀 Watching for changes... (Press Ctrl+C to stop)");
            start_h5i_watcher(session)?;
        }
    }
    Ok(())
}*/
