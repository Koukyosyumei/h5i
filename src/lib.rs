//! `h5i`: the red-teaming browser for AI agents.
//!
//! The library owns the whole CLI: the top-level `Cli`/`Commands` parse, the
//! argument bootstrap, and the dispatch into `cli/`, where every noun's clap enum
//! and handlers live. `src/main.rs` is a three-line binary over [`run`].
//!
//! It is a library so the clap command tree has a second consumer:
//! `examples/gen_man.rs` renders `docs/man/man1/h5i.1` from it, which is why the
//! man page cannot drift from the commands.

use std::ffi::OsString;

use clap::{CommandFactory, Parser, Subcommand};

pub mod cli;

#[derive(Parser)]
#[command(
    name = "h5i",
    // What the product is now. It read "Disposable, confined development
    // environments for coding agents" until 2026-09-10, which was the
    // positioning before the pivot (ROADMAP.md) and had outlived the README,
    // the site and the skill.
    about = "The red-teaming browser for AI agents: page automation with direct control over \
             HTTP traffic, in sessions that stay contained and auditable",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
// `Box`'s clap enum is much larger than the two generator commands; boxing it
// would break clap's derive, and the enum is constructed once per process.
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Disposable, confined development boxes. `h5i box` with no verb creates
    /// one from the current repository; `h5i box --help` has the verb table.
    ///
    /// `box` is the noun the whole product uses (the docs, the skill and the
    /// errors all say "a box") so the command says it too. `dev` is kept as a
    /// hidden alias for one release, like `env` before it.
    #[command(alias = "dev")]
    Box(BoxArgs),

    /// Deprecated alias for `h5i box`. Kept for one release.
    #[command(hide = true)]
    Env {
        #[command(subcommand)]
        action: cli::boxes::BoxCommands,
    },

    /// Open the box console in a browser: one read-only screen over the whole
    /// fleet. What each box is, what its policy actually allows, what ran inside
    /// it, and what pressed on a boundary.
    ///
    /// The server is loopback-only and every route is a GET, so the console can
    /// watch boxes but never drive them. The URL carries a token minted for this
    /// session and held in memory only.
    #[cfg(feature = "web")]
    Ui {
        /// Port to bind on 127.0.0.1. `0` asks the OS for a free one.
        #[arg(long, default_value = "8765")]
        port: u16,
        /// Also hand the URL to this desktop's browser.
        #[arg(long)]
        open: bool,
    },

    /// The rendering engine's own command line.
    ///
    /// Hidden because it is not the interface: `h5i browser` is, and it is what
    /// knows about session names, placement, the control lock and the audit. This
    /// is what `h5i browser` execs itself as to render a page, and it is
    /// documented so that anyone who genuinely wants the engine on its own can
    /// reach it without a second binary to install.
    #[command(name = "__engine", hide = true, disable_help_flag = true)]
    #[cfg(feature = "browser")]
    Engine {
        /// Everything after `__engine`, handed to the engine unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },

    /// Browser sessions: open one, drive it, close it.
    #[cfg(feature = "browser")]
    Browser {
        #[command(subcommand)]
        action: cli::browser::BrowserCommands,
    },

    /// Open a box someone else is sharing, from a ticket they sent you.
    ///
    /// Connects peer to peer, end-to-end encrypted, and serves their dev server
    /// on this machine's loopback. The local URL carries its own token, minted
    /// here; the ticket's secret is never handed to a browser.
    ///
    /// What you are opening is somebody else's agent's code.
    #[cfg(feature = "share")]
    Join {
        /// The `h5i1_…` ticket you were sent, or `-` to read it from stdin.
        ///
        /// `/proc/<pid>/cmdline` is world-readable on an ordinary Linux box and
        /// this process runs for the whole session, so a ticket passed as an
        /// argument is legible to every other user on the machine for as long as
        /// you are joined, and a ticket is the whole authorization. `pbpaste |
        /// h5i join -` keeps it out of the process table and your shell history.
        #[arg(value_name = "TICKET")]
        ticket: String,
        /// Local port to serve it on. 0 picks a free one and prints it.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Join even when the only address left is `127.0.0.1`.
        #[arg(long)]
        shared_jar: bool,
        /// Serve on this loopback address instead of a random `127.x.y.z`.
        ///
        /// The address is bound exactly, no fallback, and only loopback
        /// (`127.0.0.0/8`) is accepted. This is the WSL answer: Windows forwards
        /// only `127.0.0.1` into the VM, so the private address a join normally
        /// picks binds fine and is then unreachable from a Windows browser.
        /// `--bind 127.0.0.1` counts as shared-jar consent by itself; any other
        /// loopback address keeps a cookie jar of its own.
        #[arg(long, value_name = "ADDR")]
        bind: Option<std::net::Ipv4Addr>,
    },

    /// Run boxes on another Linux machine you own.
    #[cfg(feature = "runner")]
    Runner {
        #[command(subcommand)]
        action: cli::runner::RunnerCommands,
    },

    /// Install, list or remove a plugin.
    ///
    /// A plugin is a capability that is not in the default build: a separate
    /// executable, run in its own process, reaching a session through the same
    /// verbs a person types. `h5i plugin list` names the ones this binary knows.
    Plugin {
        #[command(subcommand)]
        action: cli::plugin::PluginCommands,
    },

    /// Run an installed plugin.
    ///
    /// `h5i websec …` and anything else `h5i plugin list` names. Hidden from the
    /// help listing because the plugins themselves are what should appear there,
    /// and they are not all present in every install.
    #[command(external_subcommand)]
    Plugged(Vec<OsString>),

    /// Write or print the agent skill this binary carries.
    Skill {
        #[command(subcommand)]
        action: cli::skill::SkillCommands,
    },

    /// Generate a shell completion script (bash, zsh, fish, …); e.g.
    /// `h5i completion bash > /etc/bash_completion.d/h5i`
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// `h5i box` with no verb is "make me a box from this source". With a verb it
/// is the lifecycle surface. clap resolves the verb first, so a source that
/// happens to be spelled like a verb needs the explicit `h5i box create` form.
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BoxArgs {
    #[command(subcommand)]
    action: Option<cli::boxes::BoxCommands>,

    /// Where the code comes from: `.` for this repository (the default), or a
    /// repository URL to copy into the box. A pull request is `--pr`.
    #[arg(value_name = "SOURCE")]
    source: Option<String>,

    /// Base the box on a GitHub pull request (number, #number, or URL).
    #[arg(long, value_name = "NUMBER|URL", conflicts_with_all = ["source", "new"])]
    pr: Option<String>,

    /// Start from an empty box instead of any source.
    #[arg(long, conflicts_with = "source")]
    new: bool,

    /// Name for the box. Derived from the source when omitted.
    #[arg(long)]
    name: Option<String>,

    /// Base revision, when the source is this repository. Pinned immutably.
    ///
    /// Same conflicts as the explicit `box create` form. Without them the short
    /// form parsed `--new --from <rev>` and `--pr N --from <rev>` happily and
    /// then discarded the base: `into_command()` builds `Create` *after* clap
    /// has validated, so the explicit form's rules never applied. A silently
    /// unpinned base is an integrity gap in a tool whose pitch is that the base
    /// is pinned immutably.
    #[arg(long, conflicts_with_all = ["pr", "new"])]
    from: Option<String>,

    /// Remote to fetch a pull request head from.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// Policy profile from .h5i/env.toml (see `h5i box create --help`).
    #[arg(long)]
    profile: Option<String>,

    /// Isolation tier: auto (default) | workspace | process | supervised | container | ...
    #[arg(long)]
    isolation: Option<String>,

    /// Container base image for isolation=container.
    #[arg(long)]
    image: Option<String>,

    /// Browser engine for the `browser` profile: chromium | lightpanda | h5i.
    #[arg(long)]
    engine: Option<String>,

    /// Emit the created box's manifest as JSON instead of the human summary.
    #[arg(long)]
    json: bool,
}

impl BoxArgs {
    /// Fold the short form into the same `Create` the explicit verb builds, so
    /// there is exactly one create path to reason about.
    fn into_command(self) -> anyhow::Result<cli::boxes::BoxCommands> {
        if let Some(action) = self.action {
            return Ok(action);
        }
        let source = self.source.unwrap_or_else(|| ".".to_string());
        let (pr, clone) = match (&self.pr, self.new) {
            (Some(spec), _) => (Some(spec.clone()), None),
            (None, true) => (None, None),
            (None, false) if source == "." => (None, None),
            // Checked *before* the repository-URL branch, and the order is the
            // point: a GitHub PR URL is also a URL, so testing for a clone first
            // would send `.../pull/42` to `git clone` and fail with a raw
            // "repository not found". A plain repository URL has no `/pull/<n>`.
            //
            // A pull request used to be spellable as a bare positional and people
            // will still type it, so say where it went rather than "unrecognized
            // source", which would read as "h5i cannot do this".
            (None, false) if cli::boxes::pr_spec(&source).is_some() => anyhow::bail!(
                "'{source}' looks like a pull request — pass it as a flag:\n  \
                 h5i box --pr {source}"
            ),
            (None, false) if cli::boxes::looks_like_repo_url(&source) => {
                (None, Some(source.clone()))
            }
            (None, false) => anyhow::bail!(
                "unrecognized source '{source}'.\n  \
                 Pass `.` for this repository, a repository URL, `--pr <number|url>` for a \
                 pull request, or `--new` for an empty box."
            ),
        };
        // clap covers `--from` with `--pr`/`--new`, but a URL source becomes
        // `clone` only here, after validation, and a detached box never reads
        // `from`. Refuse rather than accept a pin and drop it.
        if self.from.is_some() && clone.is_some() {
            anyhow::bail!(
                "`--from` pins a base revision in *this* repository, but '{source}' is an                  external source, so the box is detached and has no such base. Drop `--from`,                  or pin the revision in the URL if the host supports it."
            );
        }
        let name = match (self.name, &pr, &clone) {
            (Some(n), _, _) => n,
            // From the PR *number*, not the spec: a URL spec would otherwise
            // become a box named `pr-https://github.com/o/r/pull/42`.
            (None, Some(spec), _) => format!("pr-{}", h5i_core::source::parse_pr_spec(spec)?),
            (None, None, Some(url)) => cli::boxes::name_from_url(url)?,
            (None, None, None) if self.new => cli::boxes::free_box_name("new")?,
            (None, None, None) => cli::boxes::default_box_name()?,
        };
        Ok(cli::boxes::BoxCommands::Create {
            name,
            from: self.from,
            pr,
            clone,
            new: self.new,
            remote: self.remote,
            profile: self.profile,
            isolation: self.isolation,
            image: self.image,
            engine: self.engine,
            backend: "auto".into(),
            audit: "signal".into(),
            json: self.json,
            // The short form has no `--runner`: placing a box on another
            // machine is a deliberate choice, and `h5i box create --runner` is
            // where a deliberate choice belongs.
            #[cfg(feature = "runner")]
            runner: None,
        })
    }
}

pub fn run() -> anyhow::Result<()> {
    init_tracing();
    let argv: Vec<String> = std::env::args().collect();
    maybe_version_json(&argv);
    let cli = Cli::parse_from(argv);

    match cli.command {
        Commands::Box(args) => cli::boxes::run(args.into_command()?)?,
        Commands::Env { action } => cli::boxes::run(action)?,
        #[cfg(feature = "web")]
        Commands::Ui { port, open } => cli::ui::run(port, open)?,
        #[cfg(feature = "browser")]
        Commands::Engine { args } => {
            // Never returns: the engine's CLI owns the exit code, and a page
            // that failed to load has to be able to say so with a status the
            // caller can read.
            let argv = std::iter::once(std::ffi::OsString::from("h5i __engine")).chain(args);
            h5i_browser::cli::main(argv);
        }
        #[cfg(feature = "browser")]
        Commands::Browser { action } => cli::browser::run(action)?,
        #[cfg(feature = "share")]
        Commands::Join {
            ticket,
            port,
            shared_jar,
            bind,
        } => cli::share::join(&ticket, port, bind, shared_jar)?,
        #[cfg(feature = "runner")]
        Commands::Runner { action } => cli::runner::run(action)?,
        Commands::Plugin { action } => cli::plugin::run(action)?,
        Commands::Plugged(args) => {
            let (name, rest) = args.split_first().ok_or_else(|| {
                anyhow::anyhow!("no command given. `h5i --help` lists what there is")
            })?;
            let name = name.to_string_lossy().to_string();
            match cli::plugin::known(&name) {
                // A name h5i knows: say what it is and how to have it, rather
                // than reporting it as a typo.
                Some(_) if !cli::plugin::installed(&name) => {
                    anyhow::bail!("{}", cli::plugin::not_installed(&name))
                }
                Some(_) => cli::plugin::exec(&name, rest)?,
                None => anyhow::bail!(
                    "`{name}` is not an h5i command. `h5i --help` lists the verbs, \
                     and `h5i plugin list` the plugins."
                ),
            }
        }
        Commands::Skill { action } => cli::skill::run(action)?,
        Commands::Completion { shell } => cli::completion::run(shell)?,
    }

    Ok(())
}

fn init_tracing() {
    // Off by default. Users opt in via RUST_LOG / H5I_LOG (e.g.
    // `H5I_LOG=h5i_core=debug`). Writes to stderr so it doesn't poison stdout
    // for piped consumers.
    let filter = tracing_subscriber::EnvFilter::try_from_env("H5I_LOG")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}

/// `h5i --version --json` prints machine-readable build facts. Handled before
/// clap because clap owns `--version` and would print its own string.
fn maybe_version_json(argv: &[String]) {
    let mut wants_version = false;
    let mut wants_json = false;
    for tok in argv.iter().skip(1) {
        if tok == "--" || !tok.starts_with('-') {
            break;
        }
        match tok.as_str() {
            "--version" | "-V" => wants_version = true,
            "--json" => wants_json = true,
            _ => {}
        }
    }
    if !(wants_version && wants_json) {
        return;
    }
    let out = serde_json::json!({
        "name": "h5i",
        "version": env!("CARGO_PKG_VERSION"),
        "features": compiled_features(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&out).expect("version json is serializable")
    );
    std::process::exit(0);
}

/// Compiled feature flags for this binary, sorted so JSON output is diffable.
// One cfg-gated `push` per feature, so a new feature is a one-line addition.
// clippy sees `Vec::new()` + `push` and suggests `vec![]`, but the pushes are
// conditional. Collapsing them would reintroduce paired cfg/cfg(not) bindings.
#[allow(clippy::vec_init_then_push)]
fn compiled_features() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut features: Vec<&str> = Vec::new();
    #[cfg(feature = "web")]
    features.push("web");
    // Both switches, because they are separately selectable and a consumer
    // deciding whether to offer a share UI needs to know which half is here:
    // `share-tunnel` alone means `box share --tunnel` and no `join`.
    #[cfg(feature = "share-tunnel")]
    features.push("share-tunnel");
    #[cfg(feature = "share")]
    features.push("share");
    features.sort_unstable();
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_json_features_match_compiled_build() {
        let features = compiled_features();
        #[cfg(feature = "web")]
        assert!(features.contains(&"web"));
        #[cfg(not(feature = "web"))]
        assert!(!features.contains(&"web"));
        // `share` implies `share-tunnel`, so the p2p build reports both and a
        // tunnel-only build reports exactly one. This output is what an
        // installer or a wrapper reads to decide whether the share workflow
        // exists at all; a default build that omitted it read as one without
        // sharing compiled in.
        #[cfg(feature = "share")]
        {
            assert!(features.contains(&"share"));
            assert!(features.contains(&"share-tunnel"));
        }
        #[cfg(all(feature = "share-tunnel", not(feature = "share")))]
        {
            assert!(features.contains(&"share-tunnel"));
            assert!(!features.contains(&"share"));
        }
        #[cfg(not(feature = "share-tunnel"))]
        {
            assert!(!features.contains(&"share"));
            assert!(!features.contains(&"share-tunnel"));
        }
        // Sorted, because the JSON is diffable output.
        let mut sorted = features.clone();
        sorted.sort_unstable();
        assert_eq!(features, sorted);
    }

    /// Route `argv` through clap and the short-form fold, exactly as `main`
    /// does. The error is flattened to a string because `BoxCommands` has no
    /// `Debug`, and deriving one on a public enum purely for tests would be
    /// the tail wagging the dog.
    fn dispatch(argv: &[&str]) -> Result<cli::boxes::BoxCommands, String> {
        let parsed = Cli::try_parse_from(argv).map_err(|e| e.to_string())?;
        match parsed.command {
            Commands::Box(args) => args.into_command().map_err(|e| e.to_string()),
            // `env` is the older alias and carries only the verb form, so it
            // lands on its own variant with nothing to fold.
            Commands::Env { action } => Ok(action),
            _ => panic!("not a box command"),
        }
    }

    /// The fields the source routing decides. Panics if the fold produced
    /// anything but a `Create`, which is the only thing the short form builds.
    fn create_parts(argv: &[&str]) -> (String, Option<String>, Option<String>, bool) {
        match dispatch(argv) {
            Ok(cli::boxes::BoxCommands::Create {
                name,
                pr,
                clone,
                new,
                ..
            }) => (name, pr, clone, new),
            Ok(_) => panic!("expected Create"),
            Err(e) => panic!("dispatch failed: {e}"),
        }
    }

    #[test]
    fn a_pull_request_is_a_flag_not_a_positional() {
        let (name, pr, clone, _) = create_parts(&["h5i", "box", "--pr", "1234"]);
        assert_eq!(pr.as_deref(), Some("1234"));
        assert_eq!(clone, None);
        assert_eq!(name, "pr-1234");

        // A URL spec names the box from the *number*. Naming it from the spec
        // would produce `pr-https://github.com/o/r/pull/42`.
        let (name, pr, _, _) =
            create_parts(&["h5i", "box", "--pr", "https://github.com/o/r/pull/42"]);
        assert_eq!(pr.as_deref(), Some("https://github.com/o/r/pull/42"));
        assert_eq!(name, "pr-42");
    }

    #[test]
    fn the_old_positional_spelling_says_where_it_went() {
        // `h5i box 1234` used to mean a pull request. People will still type
        // it, so it has to point at the flag rather than read as "h5i cannot
        // do this".
        for spec in ["1234", "#7", "https://github.com/o/r/pull/42"] {
            let err = dispatch(&["h5i", "box", spec])
                .err()
                .expect("must be refused");
            assert!(err.contains("--pr"), "{spec}: {err}");
            assert!(err.contains("pull request"), "{spec}: {err}");
        }
    }

    #[test]
    fn a_repository_url_is_still_a_positional_and_still_clones() {
        // The PR check runs first, and this is what must survive it: a plain
        // repository URL has no `/pull/<n>`, so it is unaffected.
        let (_, pr, clone, _) =
            create_parts(&["h5i", "box", "https://github.com/o/r.git", "--name", "r"]);
        assert_eq!(pr, None);
        assert_eq!(clone.as_deref(), Some("https://github.com/o/r.git"));
    }

    #[test]
    fn an_unrecognized_source_names_every_way_in() {
        let err = dispatch(&["h5i", "box", "wat"])
            .err()
            .expect("must be refused");
        for hint in ["--pr", "--new", "repository URL"] {
            assert!(err.contains(hint), "{err}");
        }
    }

    #[test]
    fn the_sources_are_mutually_exclusive() {
        // Caught by clap, before any of the fold runs.
        assert!(dispatch(&["h5i", "box", ".", "--pr", "12"]).is_err());
        assert!(dispatch(&["h5i", "box", "--pr", "12", "--new"]).is_err());
        assert!(dispatch(&["h5i", "box", ".", "--new"]).is_err());
    }

    #[test]
    fn the_old_command_names_still_resolve() {
        // `env` was renamed to `dev`, then `dev` to `box`. Two renames in one
        // release cycle is exactly when a muscle-memory alias earns its keep,
        // so both still route to the same place.
        let (name, ..) = create_parts(&["h5i", "dev", "--new", "--name", "viadev"]);
        assert_eq!(name, "viadev");
        // `env` keeps only the verb form (it never had the source short form).
        assert!(dispatch(&["h5i", "env", "ls"]).is_ok());
    }

    #[test]
    fn the_short_form_carries_json() {
        match dispatch(&["h5i", "box", "--new", "--name", "scratch", "--json"]) {
            Ok(cli::boxes::BoxCommands::Create { json, .. }) => assert!(json),
            Ok(_) => panic!("expected Create"),
            Err(e) => panic!("dispatch failed: {e}"),
        }
    }

    #[test]
    fn an_empty_box_needs_no_source() {
        let (name, pr, clone, new) = create_parts(&["h5i", "box", "--new", "--name", "scratch"]);
        assert!(new);
        assert_eq!((pr, clone), (None, None));
        assert_eq!(name, "scratch");
    }

    #[test]
    fn view_defaults_to_the_browser_and_term_opts_into_the_terminal() {
        // The default must stay the web viewer: it works in every terminal,
        // and `--term` needs one that can draw images.
        match dispatch(&["h5i", "box", "view", "web-box"]) {
            Ok(cli::boxes::BoxCommands::View { term, fps, .. }) => {
                assert!(!term);
                assert_eq!(fps, 10, "a modest default: every frame crosses a PTY");
            }
            Ok(_) => panic!("expected View"),
            Err(e) => panic!("dispatch failed: {e}"),
        }
        match dispatch(&["h5i", "box", "view", "tty-box", "--term", "--fps", "24"]) {
            Ok(cli::boxes::BoxCommands::View { term, fps, .. }) => {
                assert!(term);
                assert_eq!(fps, 24);
            }
            Ok(_) => panic!("expected View"),
            Err(e) => panic!("dispatch failed: {e}"),
        }
        // A frame rate the box cannot honour is refused at the boundary rather
        // than clamped somewhere the user cannot see.
        assert!(dispatch(&["h5i", "box", "view", "b", "--fps", "0"]).is_err());
        assert!(dispatch(&["h5i", "box", "view", "b", "--fps", "600"]).is_err());
    }

    #[test]
    fn watch_defaults_to_every_row_and_deny_only_narrows_it() {
        // The default has to stay "everything": a watcher that silently showed
        // only refusals would read as "nothing happened" on a box that was
        // busy, which is the one thing this surface must never say.
        match dispatch(&["h5i", "box", "watch", "mybox"]) {
            Ok(cli::boxes::BoxCommands::Watch {
                deny_only, json, ..
            }) => {
                assert!(!deny_only);
                assert!(!json);
            }
            Ok(_) => panic!("expected Watch"),
            Err(e) => panic!("dispatch failed: {e}"),
        }
        match dispatch(&["h5i", "box", "watch", "mybox", "--deny-only", "--json"]) {
            Ok(cli::boxes::BoxCommands::Watch {
                name,
                deny_only,
                json,
            }) => {
                assert_eq!(name, "mybox");
                assert!(deny_only);
                assert!(json);
            }
            Ok(_) => panic!("expected Watch"),
            Err(e) => panic!("dispatch failed: {e}"),
        }
    }

    #[test]
    fn the_status_lines_egress_summary_says_localhost_rather_than_none() {
        // An empty allowlist still leaves loopback open, that is how the dev
        // server is reachable, so "none" would overstate the confinement in
        // the one place a human wants a one-word answer.
        use cli::boxes::egress_summary;
        assert_eq!(egress_summary(&[]), "localhost");
        assert_eq!(egress_summary(&["api.example".to_string()]), "api.example");
        assert_eq!(
            egress_summary(&["a.example".to_string(), "b.example".to_string()]),
            "a.example,b.example"
        );
        assert_eq!(
            egress_summary(&["a".to_string(), "b".to_string(), "c".to_string()]),
            "3 hosts"
        );
    }
}
