//! `h5i websec`: the HTTP workbench, as an h5i plugin.
//!
//! Burp Suite is an HTTP workbench for humans. This is one for agents: read what
//! a browser session sent, change a part of it, send it again, and compare the
//! answers. The design is `docs/design/design-websec.md`.
//!
//! This binary holds no privilege of its own. Every verb here runs an `h5i
//! browser` verb in a subprocess, which means a plugin cannot reach a session by
//! any path the CLI does not already offer, and its requests are the engine's
//! fetches: checked by the engine's policy, spent from the engine's budget,
//! written into the engine's receipts. That is the whole reason a plugin is a
//! separate process rather than something loaded into h5i (see `src/cli/
//! plugin.rs`).
//!
//! What it adds over typing the underlying verbs is naming. `req_42` rather than
//! a bare number, one noun for the workbench rather than verbs scattered through
//! `h5i browser`, and defaults chosen for a loop rather than for a person
//! reading a page.

mod read;

use std::ffi::OsString;
use std::process::Command;

use clap::{Parser, Subcommand};

/// The h5i that launched this plugin.
///
/// Passed in rather than found on `$PATH`: a plugin that guessed could compose
/// verbs from a *different* build than the one the user ran, which would make
/// "the same verbs a person types" quietly false.
fn h5i() -> OsString {
    std::env::var_os("H5I_BIN").unwrap_or_else(|| OsString::from("h5i"))
}

#[derive(Parser)]
#[command(
    name = "h5i websec",
    version,
    about = "An HTTP workbench for agents: read, edit, resend and compare what a session sent."
)]
struct Cli {
    #[command(subcommand)]
    command: Verb,

    /// Which session, when more than one is open.
    #[arg(long, short = 's', global = true, value_name = "NAME")]
    session: Option<String>,

    /// Emit the human-readable view instead of JSON.
    #[arg(long, global = true)]
    human: bool,

    /// Accepted and redundant: JSON is already the default.
    ///
    /// Here because `h5i browser` takes `--json` and anybody moving between the
    /// two surfaces types it out of habit. Refusing it would be technically
    /// correct and would cost a person a round trip to find out the flag they
    /// typed was the one thing they did not need.
    #[arg(long, global = true, conflicts_with = "human")]
    json: bool,
}

#[derive(Subcommand)]
enum Verb {
    /// What this session sent, newest last.
    Requests {
        /// Only this method.
        #[arg(long, value_name = "METHOD")]
        method: Option<String>,
        /// Only rows whose URL contains this.
        #[arg(long, value_name = "TEXT")]
        url_contains: Option<String>,
        /// Only responses with this status.
        #[arg(long, value_name = "CODE")]
        status: Option<u16>,
        /// Only `navigation`, `subresource`, `frame`, `redirect` or `replay`.
        #[arg(long, value_name = "KIND")]
        initiator: Option<String>,
        /// Only what policy refused.
        #[arg(long)]
        denied_only: bool,
        /// At most this many rows.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
    },

    /// One stored message, as it went out or as it came back.
    Show {
        /// `req_42`, `res_42`, or just `42`.
        #[arg(value_name = "ID")]
        id: String,
        /// Print it as an HTTP message.
        ///
        /// The one verb here whose output is not JSON, because a wire message
        /// is bytes and bytes inside a JSON string are no longer the message.
        #[arg(long)]
        raw: bool,
        /// Write the exact body to a file, or to stdout with `-`.
        #[arg(long = "body-to", value_name = "PATH")]
        body_to: Option<String>,
    },

    /// Send one of this session's requests again, with changes.
    Replay {
        /// `req_42`, or just `42`.
        #[arg(value_name = "ID")]
        id: String,
        /// Set a request field; repeatable and ordered. JSON values are typed,
        /// nested by dots, and arrays use numeric segments. Preserve numeric
        /// strings with shell-safe quotes: `--set 'json.jsonrpc="2.0"'`.
        #[arg(long = "set", value_name = "TARGET=VALUE")]
        set: Vec<String>,
        /// `multipart.userfile=./payload.jpg`: the value is the file's bytes.
        ///
        /// For the edits a command line cannot carry — a real image, a
        /// polyglot, anything a magic-number check will read. Applied after
        /// every `--set`.
        #[arg(long = "set-file", value_name = "TARGET=PATH")]
        set_file: Vec<String>,
        /// Remove a target.
        #[arg(long = "unset", value_name = "TARGET")]
        unset: Vec<String>,
        /// Add targets that are not there.
        #[arg(long)]
        create: bool,
        /// Send it from another session, with that session's credentials.
        #[arg(long = "as", value_name = "SESSION")]
        as_session: Option<String>,
        /// Carry the source session's `Cookie` and `Authorization` into the
        /// other session. Only meaningful with `--as`.
        #[arg(long)]
        keep_credentials: bool,
        /// Send it this many times and report the clock.
        #[arg(long, value_name = "N")]
        repeat: Option<u32>,
        /// Release the repeats together, for a race.
        #[arg(long)]
        race: bool,
        /// Stop at the first redirect and report it.
        #[arg(long)]
        no_follow: bool,
        /// Start the page's network allowance again before sending, for a loop
        /// that is deliberately long.
        #[arg(long)]
        reset_budget: bool,
        /// Send this exact request-target, around the URL parser.
        ///
        /// A parsed URL resolves `.` and `..` and percent-decodes before the
        /// request exists, so `/files/%252e%252e%252f` reaches the wire as
        /// something the server was never asked for. With this the target is
        /// written byte for byte, through the same policy and receipts.
        #[arg(long = "raw-target", value_name = "TARGET")]
        raw_target: Option<String>,
        /// Preserve header-name casing while retaining edits and cookies.
        #[arg(long = "raw-headers")]
        raw_headers: bool,
        /// Send once per line, as `query.id=./ids.txt`.
        #[arg(long = "set-each", value_name = "TARGET=PATH")]
        set_each: Option<String>,
        /// Send a whole request, written byte for byte from this file.
        ///
        /// The general form of `--raw-target`, framing headers included and
        /// recomputed by nothing, for request smuggling. `-` reads standard
        /// input. Use it when `--set` cannot express the body.
        #[arg(long = "raw-request", value_name = "PATH")]
        raw_request: Option<String>,
    },

    /// How two of this session's responses differ.
    Diff {
        #[arg(value_name = "ID")]
        left: String,
        #[arg(value_name = "ID")]
        right: String,
    },

    /// Ask a stored response a question. Exits 0 when it holds, 1 when it does
    /// not, 2 when it could not be asked.
    Match {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(long, value_name = "PATTERN")]
        regex: Option<String>,
        #[arg(long, value_name = "TEXT")]
        contains: Option<String>,
        #[arg(long = "json-path", value_name = "PATH[=VALUE]")]
        json_path: Option<String>,
        #[arg(long, value_name = "NAME[=VALUE]")]
        header: Option<String>,
        #[arg(long, value_name = "CODE")]
        status: Option<u16>,
        #[arg(long, value_name = "BYTES")]
        longer_than: Option<u64>,
        #[arg(long, value_name = "BYTES")]
        shorter_than: Option<u64>,
    },

    /// Open a WebSocket, send frames, and report what came back.
    Socket {
        /// The `ws://` or `wss://` endpoint to open.
        #[arg(value_name = "URL")]
        url: String,
        /// A text frame to send. Repeatable, sent in the order given.
        #[arg(long = "send", value_name = "TEXT")]
        send: Vec<String>,
        /// How long to listen for replies, in milliseconds.
        #[arg(long = "wait-ms", value_name = "MS")]
        wait_ms: Option<u64>,
    },

    /// What this session reached, as origins and endpoints.
    Sitemap,

    /// Run a multi-step flow with bindings between the steps.
    ///
    /// Steps run in order, bind response values, and stop on failure:
    ///
    /// ```json
    /// {"steps": [
    ///   {"resend": 3, "extract": {"csrf": "regex:value=\"([^\"]+)\""}},
    ///   {"resend": 5, "set": ["header.X-CSRF-Token=${csrf}", "json.role=admin"]}
    /// ]}
    /// ```
    Sequence {
        /// The sequence file.
        #[arg(value_name = "FILE")]
        file: String,
        /// Set an initial `name=value` binding; repeatable.
        #[arg(long = "var", value_name = "NAME=VALUE")]
        vars: Vec<String>,
        /// Continue after failed steps.
        #[arg(long)]
        keep_going: bool,
    },
}

/// `req_42`, `res_42` and `42` all name sequence 42.
///
/// The prefixes exist because a finding reads better with them and because a
/// request and its response share a number; neither is a different thing to
/// look up. Anything else is refused rather than parsed as far as it goes: `42x`
/// silently becoming 42 is how a loop tests the wrong request.
fn sequence_of(id: &str) -> anyhow::Result<String> {
    let bare = id
        .strip_prefix("req_")
        .or_else(|| id.strip_prefix("res_"))
        .unwrap_or(id);
    bare.parse::<u64>()
        .map(|n| n.to_string())
        .map_err(|_| anyhow::anyhow!("`{id}` is not a message id: try `req_42`, `res_42` or `42`"))
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("{e}");
        std::process::exit(2);
    }
}

/// Ask the running engine for something only it has: a live session's log is
/// in its memory, not in a file.
pub fn ask_browser(args: &[&str], session: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let mut command = Command::new(h5i());
    command.arg("browser").args(args).arg("--json");
    if let Some(name) = session {
        command.arg("--session").arg(name);
    }
    let out = command
        .output()
        .map_err(|e| anyhow::anyhow!("could not run h5i: {e}"))?;
    serde_json::from_slice(&out.stdout).map_err(|e| anyhow::anyhow!("h5i answered oddly: {e}"))
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // The reading verbs are this binary's, and they read the store directly.
    // Everything that sends is still `h5i browser`, below.
    let root = h5i_core::browser_session::root()?;
    let session = cli.session.clone();
    let json_out = !cli.human;
    match &cli.command {
        Verb::Show { id, raw, body_to } => {
            let seq = sequence_of(id)?.parse::<u64>()?;
            let part = if id.starts_with("res_") {
                read::Part::Response
            } else if id.starts_with("req_") {
                read::Part::Request
            } else {
                read::Part::Both
            };
            return read::show(
                &root,
                session.as_deref(),
                seq,
                part,
                *raw,
                body_to.as_deref().map(std::path::Path::new),
                json_out,
            );
        }
        Verb::Diff { left, right } => {
            return read::diff(
                &root,
                session.as_deref(),
                sequence_of(left)?.parse::<u64>()?,
                sequence_of(right)?.parse::<u64>()?,
                json_out,
            );
        }
        Verb::Match {
            id,
            regex,
            contains,
            json_path,
            header,
            status,
            longer_than,
            shorter_than,
        } => {
            // `name=value` splits on the first `=`, like an edit does, so a
            // value containing one needs no escaping.
            let split = |spec: &str| -> (String, Option<String>) {
                match spec.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string())),
                    None => (spec.to_string(), None),
                }
            };
            let mut conditions = Vec::new();
            if let Some(pattern) = regex {
                conditions.push(read::Condition::Regex(pattern.clone()));
            }
            if let Some(text) = contains {
                conditions.push(read::Condition::Contains(text.clone()));
            }
            if let Some(spec) = json_path {
                let (path, value) = split(spec);
                conditions.push(read::Condition::Json { path, value });
            }
            if let Some(spec) = header {
                let (name, value) = split(spec);
                conditions.push(read::Condition::Header { name, value });
            }
            if let Some(status) = status {
                conditions.push(read::Condition::Status(*status));
            }
            if let Some(bytes) = longer_than {
                conditions.push(read::Condition::LongerThan(*bytes));
            }
            if let Some(bytes) = shorter_than {
                conditions.push(read::Condition::ShorterThan(*bytes));
            }
            return read::matches(
                &root,
                session.as_deref(),
                sequence_of(id)?.parse::<u64>()?,
                &conditions,
                json_out,
            );
        }
        Verb::Sitemap => return read::sitemap(&root, session.as_deref(), json_out),
        _ => {}
    }

    let mut argv: Vec<String> = vec!["browser".to_string()];
    fn push(argv: &mut Vec<String>, args: &[&str]) {
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
    }
    fn flag(argv: &mut Vec<String>, name: &str, value: Option<String>) {
        if let Some(value) = value {
            argv.push(format!("--{name}"));
            argv.push(value);
        }
    }

    match cli.command {
        Verb::Requests {
            method,
            url_contains,
            status,
            initiator,
            denied_only,
            limit,
        } => {
            push(&mut argv, &["requests"]);
            flag(&mut argv, "method", method);
            flag(&mut argv, "url-contains", url_contains);
            flag(&mut argv, "status", status.map(|s| s.to_string()));
            flag(&mut argv, "initiator", initiator);
            flag(&mut argv, "limit", limit.map(|s| s.to_string()));
            if denied_only {
                argv.push("--denied-only".into());
            }
        }
        // Handled above, in this process: these read the store rather than
        // send anything.
        Verb::Show { .. } | Verb::Diff { .. } | Verb::Match { .. } | Verb::Sitemap => {
            unreachable!("the reading verbs return before this")
        }
        Verb::Replay {
            id,
            set,
            set_file,
            unset,
            create,
            as_session,
            keep_credentials,
            repeat,
            race,
            no_follow,
            reset_budget,
            raw_target,
            raw_request,
            raw_headers,
            set_each,
        } => {
            let seq = sequence_of(&id)?;
            push(&mut argv, &["resend"]);
            argv.push(seq);
            for spec in set {
                argv.push("--set".into());
                argv.push(spec);
            }
            for spec in set_file {
                argv.push("--set-file".into());
                argv.push(spec);
            }
            for spec in unset {
                argv.push("--unset".into());
                argv.push(spec);
            }
            if create {
                argv.push("--create".into());
            }
            flag(&mut argv, "as", as_session);
            if keep_credentials {
                argv.push("--keep-credentials".into());
            }
            flag(&mut argv, "repeat", repeat.map(|n| n.to_string()));
            if race {
                argv.push("--race".into());
            }
            if no_follow {
                argv.push("--no-follow".into());
            }
            if reset_budget {
                argv.push("--reset-budget".into());
            }
            flag(&mut argv, "raw-target", raw_target);
            flag(&mut argv, "raw-request", raw_request);
            if raw_headers {
                push(&mut argv, &["--raw-headers"]);
            }
            flag(&mut argv, "set-each", set_each);
        }
        Verb::Socket {
            url,
            send,
            wait_ms,
        } => {
            push(&mut argv, &["socket"]);
            argv.push(url);
            for frame in send {
                argv.push("--send".into());
                argv.push(frame);
            }
            flag(&mut argv, "wait-ms", wait_ms.map(|ms| ms.to_string()));
        }
        Verb::Sequence {
            file,
            vars,
            keep_going,
        } => {
            push(&mut argv, &["sequence"]);
            argv.push(file);
            for var in vars {
                argv.push("--var".into());
                argv.push(var);
            }
            if keep_going {
                argv.push("--keep-going".into());
            }
        }
    }

    if let Some(session) = cli.session {
        argv.push("--session".into());
        argv.push(session);
    }
    // JSON unless asked otherwise. The caller here is usually a loop, and a
    // workbench whose default output has to be re-parsed out of prose is a
    // workbench nobody scripts. `--json` says the same thing out loud.
    let _ = cli.json;
    if !cli.human {
        argv.push("--json".into());
    }

    let status = Command::new(h5i())
        .args(&argv)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run h5i: {e}"))?;
    // The underlying verb's code, unchanged. `match` exits 1 for "did not
    // match" and 2 for "could not look", and flattening those here would break
    // every script built on them.
    std::process::exit(status.code().unwrap_or(2));
}

#[cfg(test)]
mod tests {
    use super::sequence_of;

    #[test]
    fn a_message_id_is_read_with_or_without_its_prefix() {
        assert_eq!(sequence_of("req_42").unwrap(), "42");
        assert_eq!(sequence_of("res_42").unwrap(), "42");
        assert_eq!(sequence_of("42").unwrap(), "42");
    }

    /// Parsing as far as it goes is how a loop tests the wrong request.
    #[test]
    fn something_that_is_not_an_id_is_refused_rather_than_salvaged() {
        for bad in ["42x", "req_", "", "req_-1", "one"] {
            assert!(sequence_of(bad).is_err(), "`{bad}` should be refused");
        }
    }
}
