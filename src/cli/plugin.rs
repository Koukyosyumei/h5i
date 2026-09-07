//! Plugins: capabilities that are installed rather than shipped.
//!
//! A plugin is a *separate executable*, discovered by name the way `git` finds
//! its subcommands. `h5i websec …` looks for `h5i-websec`, execs it, and hands
//! over the arguments. Nothing is dynamically loaded.
//!
//! That is a security decision, not a packaging preference. The h5i process is
//! the one that resolves policy, writes the receipt before the bytes move and
//! refuses the fetch when the record cannot be written. Code loaded *into* that
//! process would sit inside the boundary it is supposed to be subject to, and
//! every guarantee in the roadmap would then rest on whatever was installed
//! last. A plugin in its own process is subject to the boundary instead: it
//! reaches a session through the same verbs a person types, so its requests are
//! the engine's fetches, checked by the engine's policy, spent from the engine's
//! budget and written into the engine's receipts.
//!
//! What that buys is narrow and worth stating exactly. It is not a sandbox: a
//! plugin runs with the user's own authority and could open a socket itself.
//! What it cannot do is borrow *h5i's* authority, or make a request that the
//! receipts do not describe.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use console::style;

/// Plugins h5i knows the name of, whether or not they are installed.
///
/// A known name is what lets `h5i websec` answer "that is a plugin, here is how
/// to install it" instead of "unknown command". A capability nobody can
/// discover is a capability nobody uses.
pub const KNOWN: &[(&str, &str)] = &[
    (
        "websec",
        "an HTTP workbench: read, edit, resend and compare what a browser session sent",
    ),
    (
        "recon",
        "the endpoint ledger: what a target exposes, where each candidate came from, \
         and which message confirmed it",
    ),
];

#[derive(Subcommand)]
pub enum PluginCommands {
    /// Install a plugin from a file.
    ///
    /// Takes a path, deliberately. There is no download here yet: a fetched
    /// executable needs a provenance story, and h5i does not have one to offer
    /// (see `docs/design/design-websec.md`, open questions). Build it or receive
    /// it, then name the file.
    Install {
        /// The plugin's name, as it will be typed: `websec` for `h5i websec`.
        #[arg(value_name = "NAME")]
        name: String,
        /// The executable to install.
        #[arg(long, value_name = "PATH")]
        from: PathBuf,
        /// Replace one that is already installed.
        #[arg(long)]
        force: bool,
    },
    /// What is installed, and what could be.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove an installed plugin.
    Remove {
        #[arg(value_name = "NAME")]
        name: String,
    },
}

/// Where plugins live: `<state>/plugins`, beside the browser sessions.
///
/// Under the state directory rather than anywhere on `$PATH`, so installing one
/// is a thing h5i did and can undo, and so `h5i plugin list` is the whole truth
/// rather than a guess about a search path.
pub fn dir() -> anyhow::Result<PathBuf> {
    let root = h5i_core::browser_session::root()?;
    let dir = root
        .parent()
        .unwrap_or(&root)
        .join("plugins");
    Ok(dir)
}

/// The file a plugin's name resolves to.
fn path_of(name: &str) -> anyhow::Result<PathBuf> {
    let file = if cfg!(windows) {
        format!("h5i-{name}.exe")
    } else {
        format!("h5i-{name}")
    };
    Ok(dir()?.join(file))
}

/// Is this name one h5i knows about?
pub fn known(name: &str) -> Option<&'static str> {
    KNOWN
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, what)| *what)
}

/// Is it installed?
pub fn installed(name: &str) -> bool {
    path_of(name).map(|p| p.is_file()).unwrap_or(false)
}

/// Run an installed plugin, replacing this process's arguments with the rest.
///
/// The plugin is told where h5i is, because composing h5i's own verbs is how it
/// reaches a session, and a plugin that guessed at `h5i` from `$PATH` could
/// find a different build from the one that launched it.
pub fn exec(name: &str, args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    let path = path_of(name)?;
    if !path.is_file() {
        anyhow::bail!("{}", not_installed(name));
    }
    let status = std::process::Command::new(&path)
        .args(args)
        .env("H5I_BIN", std::env::current_exe()?)
        .status()
        .map_err(|e| anyhow::anyhow!("{} could not be run: {e}", path.display()))?;
    // The plugin's exit code is the answer, not this process's. `websec match`
    // exits 1 for "did not match", and a wrapper that flattened that to 0 or 1
    // of its own would break every script built on it.
    std::process::exit(status.code().unwrap_or(1));
}

/// What to say when a known plugin is not installed.
///
/// Names the capability, says how to get it, and says why it is not already
/// there. A bare "unknown command" would leave the reader to wonder whether
/// they had mistyped something.
pub fn not_installed(name: &str) -> String {
    let what = known(name).unwrap_or("a plugin");
    format!(
        "`{name}` is {what}, and it is not installed.\n\n  \
         It is not part of the default build on purpose: h5i ships a browser, and \
         an install should not quietly include everything that could be built on \
         one. Adding it is a deliberate act.\n\n  \
         Download `h5i-{name}` for this platform from the release and install \
         it:\n    \
         h5i plugin install {name} --from ./h5i-{name}\n\n  \
         Or build it first:\n    \
         cargo build --release -p h5i-{name}\n    \
         h5i plugin install {name} --from target/release/h5i-{name}\n\n  \
         `h5i plugin list` shows what is installed."
    )
}

pub fn run(action: PluginCommands) -> anyhow::Result<()> {
    match action {
        PluginCommands::Install { name, from, force } => install(&name, &from, force),
        PluginCommands::List { json } => list(json),
        PluginCommands::Remove { name } => remove(&name),
    }
}

fn install(name: &str, from: &Path, force: bool) -> anyhow::Result<()> {
    if known(name).is_none() {
        // Refused rather than allowed with a warning. A plugin directory that
        // anything can add a name to is a directory where `h5i <anything>`
        // silently becomes an execution, and the whole point of the known list
        // is that `h5i websec` means one thing.
        anyhow::bail!(
            "`{name}` is not a plugin h5i knows. It knows: {}",
            KNOWN
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !from.is_file() {
        anyhow::bail!("{} is not a file", from.display());
    }
    let target = path_of(name)?;
    if target.exists() && !force {
        anyhow::bail!(
            "`{name}` is already installed at {}. Pass --force to replace it.",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("{} could not be created: {e}", parent.display()))?;
    }
    std::fs::copy(from, &target)
        .map_err(|e| anyhow::anyhow!("{} could not be installed: {e}", from.display()))?;
    // Copied rather than linked: a symlink into a build directory turns a
    // `cargo clean` into a plugin that vanishes, and an installed capability
    // should not depend on a tree the user is free to delete.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700));
    }
    println!(
        "{}  `{name}` installed at {}",
        style("✔").green(),
        target.display()
    );
    println!("     run it as `h5i {name}`");
    Ok(())
}

fn list(json: bool) -> anyhow::Result<()> {
    let rows: Vec<serde_json::Value> = KNOWN
        .iter()
        .map(|(name, what)| {
            let path = path_of(name).ok();
            let there = path.as_ref().map(|p| p.is_file()).unwrap_or(false);
            serde_json::json!({
                "name": name,
                "description": what,
                "installed": there,
                "path": there.then(|| path.map(|p| p.display().to_string())).flatten(),
            })
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("  {:<10} {:<10} WHAT IT IS", "PLUGIN", "STATE");
    for row in &rows {
        let name = row["name"].as_str().unwrap_or_default();
        let installed = row["installed"].as_bool().unwrap_or(false);
        // Padded before it is coloured: an ANSI escape is bytes the terminal
        // does not draw, so a width applied to the coloured string pads the
        // wrong number of columns and the table stops lining up.
        let word = if installed { "installed" } else { "available" };
        let padded = format!("{word:<10}");
        let state = if installed {
            style(padded).green().to_string()
        } else {
            style(padded).dim().to_string()
        };
        println!(
            "  {:<10} {} {}",
            name,
            state,
            row["description"].as_str().unwrap_or_default()
        );
    }
    Ok(())
}

fn remove(name: &str) -> anyhow::Result<()> {
    let target = path_of(name)?;
    if !target.is_file() {
        anyhow::bail!("`{name}` is not installed, so there is nothing to remove");
    }
    std::fs::remove_file(&target)
        .map_err(|e| anyhow::anyhow!("{} could not be removed: {e}", target.display()))?;
    println!("{}  `{name}` removed", style("✔").green());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_name_explains_itself_when_it_is_not_installed() {
        let message = not_installed("websec");
        assert!(message.contains("HTTP workbench"), "{message}");
        assert!(message.contains("cargo build"), "it says how: {message}");
        assert!(
            message.contains("deliberate act"),
            "and why it is not already there: {message}"
        );
    }

    #[test]
    fn the_known_list_is_what_install_will_accept() {
        assert!(known("websec").is_some());
        assert!(
            known("../../bin/sh").is_none(),
            "a plugin directory anything can add a name to is an execution surface"
        );
    }
}
