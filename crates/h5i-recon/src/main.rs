//! `h5i recon`: what an application exposes, and how h5i knows.
//!
//! A plugin, not part of the default build, for the reason the workbench is
//! one: an install of a browser should not quietly include everything that can
//! be built on one (design-websec.md W21, design-recon.md N19).
//!
//! It holds no privilege of its own. It reads a session's request log, its
//! stored messages and its ledger, all files the caller could read anyway, and
//! it opens no socket. Anything that sends a request runs an `h5i browser` verb
//! in a subprocess, so the fetch is the engine's: policy decides it, the budget
//! pays for it, and the receipt is written before the bytes move (N13).

use std::path::Path;

use clap::{Parser, Subcommand};
use h5i_core::browser_session as bs;
use h5i_recon::{Endpoint, Inventory, Ledger, State};
use h5i_wire::record::RequestRecord;
use serde_json::{Value, json};

/// The h5i that launched this plugin.
///
/// Passed in rather than found on `$PATH`, so a plugin cannot compose verbs
/// from a different build than the one the user ran.
#[allow(dead_code)]
fn h5i() -> std::ffi::OsString {
    std::env::var_os("H5I_BIN").unwrap_or_else(|| std::ffi::OsString::from("h5i"))
}

#[derive(Parser)]
#[command(
    name = "h5i recon",
    version,
    about = "The endpoint ledger: what a target exposes, and how h5i knows."
)]
struct Cli {
    #[command(subcommand)]
    command: ReconCommands,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli.command) {
        // The JSON envelope on the way out too, so a caller parsing stdout is
        // never handed a bare string on stderr (design-websec.md W9).
        println!(
            "{}",
            json!({"error": {"code": "recon", "message": error.to_string()}})
        );
        std::process::exit(1);
    }
}

/// The JSON envelope every verb here answers in. Fields are added, never
/// repurposed; a removal is a new major (design-websec.md W9).
const SCHEMA: &str = "recon/1";

#[derive(Subcommand)]
enum ReconCommands {
    /// The endpoint ledger: what this session knows about, and how it knows.
    ///
    /// Reads the session's request log first, so the inventory never disagrees
    /// with the receipts beside it.
    Endpoints {
        /// Session name or id. Defaults to the default session.
        #[arg(long)]
        session: Option<String>,
        /// Only endpoints in this state: candidate, observed, confirmed,
        /// refused or gone.
        #[arg(long)]
        state: Option<String>,
        /// Only this origin, `scheme://host[:port]`.
        #[arg(long)]
        origin: Option<String>,
        /// Only what this identity observed. `anonymous` for a session with no
        /// identity of its own.
        #[arg(long)]
        identity: Option<String>,
        /// Only what changed after this cursor, as returned by a previous run.
        #[arg(long)]
        since: Option<u64>,
        #[arg(long)]
        json: bool,
    },

    /// Read what this session already fetched, and record what it disclosed.
    ///
    /// Sends nothing. It reads the stored messages, so it keeps working after
    /// the budget is spent, and everything it writes is a candidate: a URL in a
    /// bundle was not visited (design-recon.md N8).
    Extract {
        #[arg(long)]
        session: Option<String>,
        /// Only this message, as `req_42`. The default is every message the
        /// store holds.
        #[arg(long, value_name = "REQ")]
        from: Option<String>,
        /// Which readers to run: any of `html`, `js`, `json`, `headers`.
        #[arg(long, value_name = "LIST", default_value = "html,js,json,headers")]
        kind: String,
        #[arg(long)]
        json: bool,
    },

    /// One endpoint: its sources, its evidence, and what it answered.
    Show {
        /// The `ep_…` id, as `h5i recon endpoints` prints it.
        id: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn run(action: ReconCommands) -> anyhow::Result<()> {
    let root = bs::root()?;
    let _ = bs::expire_due(&root);

    match action {
        ReconCommands::Endpoints {
            session,
            state,
            origin,
            identity,
            since,
            json,
        } => endpoints(
            &root,
            session.as_deref(),
            Filter {
                state: state.as_deref().map(parse_state).transpose()?,
                origin: origin.as_deref(),
                identity: identity.as_deref(),
                since: since.unwrap_or(0),
            },
            json,
        ),
        ReconCommands::Extract {
            session,
            from,
            kind,
            json,
        } => extract(&root, session.as_deref(), from.as_deref(), &kind, json),
        ReconCommands::Show { id, session, json } => show(&root, session.as_deref(), &id, json),
    }
}

/// The session a selector names, live or ended.
///
/// A ledger outlives the engine that filled it: the inventory of a finished run
/// is the one a reviewer wants.
fn resolve_for_reading(root: &Path, selector: Option<&str>) -> anyhow::Result<bs::Session> {
    match bs::resolve(root, selector) {
        Ok(session) => Ok(session),
        Err(bs::SessionGone::Ended { id, .. }) => Ok(bs::read(root, &id)?),
        Err(gone) => match selector.and_then(|name| bs::find_ended_by_name(root, name)) {
            Some(ended) => Ok(ended),
            None => {
                // 69 is what `browser_session::EXIT_SESSION_GONE` has always
                // been, and "could not ask" must never share a code with "no".
                eprintln!("{gone}");
                std::process::exit(bs::EXIT_SESSION_GONE);
            }
        },
    }
}

/// What a caller asked to be shown.
#[derive(Default)]
struct Filter<'a> {
    state: Option<State>,
    origin: Option<&'a str>,
    identity: Option<&'a str>,
    since: u64,
}

fn parse_state(name: &str) -> anyhow::Result<State> {
    match name {
        "candidate" => Ok(State::Candidate),
        "observed" => Ok(State::Observed),
        "confirmed" => Ok(State::Confirmed),
        "refused" => Ok(State::Refused),
        "gone" => Ok(State::Gone),
        other => anyhow::bail!(
            "`{other}` is not a state. The five are candidate (disclosed, never sent), \
             observed (a request answered), confirmed (distinguished from the not-found \
             baseline), refused (policy declined) and gone"
        ),
    }
}

/// Open the ledger and fold in whatever the request log has learned since.
///
/// Every read syncs, because an inventory that disagreed with the receipts
/// beside it would be answering a question nobody asked.
fn open_ledger(root: &Path, selector: Option<&str>) -> anyhow::Result<(bs::Session, Ledger)> {
    let session = resolve_for_reading(root, selector)?;
    if !bs::id_is_one_component(&session.id) {
        anyhow::bail!(
            "session record names `{}` as its id, which is not one this registry could \
             have minted. Nothing was read",
            session.id
        );
    }
    let dir = bs::dir(root, &session.id);
    let ledger = Ledger::open(&dir)?;
    ledger.sync_receipts(&receipts(&dir), identity_of(&session))?;
    Ok((session, ledger))
}

/// Who this session presents itself as. `anonymous` is a name, not an absence:
/// it is half of what the ledger's key separates.
fn identity_of(session: &bs::Session) -> &str {
    if session.identity.is_empty() {
        "anonymous"
    } else {
        &session.identity
    }
}

/// The session's request log, as records.
///
/// Off the log rather than out of the engine: the fold wants the whole run,
/// including the part that happened before this process existed, and a line
/// that will not parse is skipped rather than guessed at.
fn receipts(dir: &Path) -> Vec<RequestRecord> {
    let path = dir.join(bs::RECEIPTS_FILE);
    let (text, _cut) = bs::read_log_capped_saying(&path).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<RequestRecord>(line).ok())
        .collect()
}

fn endpoints(
    root: &Path,
    selector: Option<&str>,
    filter: Filter<'_>,
    json_out: bool,
) -> anyhow::Result<()> {
    let (_session, ledger) = open_ledger(root, selector)?;
    let inventory = ledger.read()?;
    let shown: Vec<&Endpoint> = inventory
        .endpoints
        .iter()
        .filter(|e| filter.state.is_none_or(|state| e.state == state))
        .filter(|e| filter.origin.is_none_or(|origin| e.origin == origin))
        .filter(|e| filter.identity.is_none_or(|who| e.identity == who))
        .filter(|e| e.line > filter.since)
        .collect();

    if json_out {
        println!("{}", serde_json::to_string_pretty(&envelope(&inventory, &shown))?);
        return Ok(());
    }

    if shown.is_empty() {
        println!("  nothing in this session's ledger matches");
        println!("  cursor   : {}", inventory.cursor);
        return Ok(());
    }

    let mut origin = String::new();
    for endpoint in &shown {
        if endpoint.origin != origin {
            origin = endpoint.origin.clone();
            println!("  {origin}");
        }
        let status = endpoint
            .status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        let params = if endpoint.params.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = endpoint.params.iter().map(|p| p.name.as_str()).collect();
            format!("  ?{}", names.join("&"))
        };
        println!(
            "  {:<9} {:<32} {:<7} {:<5} {:<10} {}{params}",
            state_word(endpoint.state),
            preview(&endpoint.path),
            endpoint.method,
            status,
            endpoint.identity,
            endpoint.id,
        );
    }
    println!();
    println!("  cursor   : {}", inventory.cursor);
    if inventory.unreadable > 0 {
        println!(
            "  note     : {} ledger line(s) could not be read and were skipped",
            inventory.unreadable
        );
    }
    Ok(())
}

/// `h5i recon extract`.
fn extract(
    root: &Path,
    selector: Option<&str>,
    from: Option<&str>,
    kinds: &str,
    json_out: bool,
) -> anyhow::Result<()> {
    let (session, ledger) = open_ledger(root, selector)?;
    let store = bs::dir(root, &session.id).join(bs::MESSAGES_DIR);
    if !store.is_dir() {
        anyhow::bail!(
            "session {} kept no messages, so there is nothing to read. Extraction reads \
             stored bytes rather than fetching them again: open a session with \
             `h5i browser open <url> --capture` and the store is there to scan",
            session.id
        );
    }
    let wanted: Vec<&str> = kinds.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    for kind in &wanted {
        if !matches!(*kind, "html" | "js" | "json" | "headers") {
            anyhow::bail!("`{kind}` is not a reader. The four are html, js, json and headers");
        }
    }
    let only = from
        .map(|name| {
            name.trim_start_matches("req_")
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("`{name}` is not a message id. They look like `req_42`"))
        })
        .transpose()?;

    let identity = identity_of(&session).to_string();
    let mut observations = Vec::new();
    let mut partial: Vec<Value> = Vec::new();
    let mut scanned = 0u64;

    for seq in h5i_recon::store::sequences(&store) {
        if only.is_some_and(|wanted| wanted != seq) {
            continue;
        }
        let Some(message) = h5i_recon::store::read(&store, seq) else {
            continue;
        };
        let Ok(base) = url::Url::parse(&message.response.url) else {
            continue;
        };
        scanned += 1;
        let req = format!("req_{seq}");

        if wanted.contains(&"headers") {
            let found = h5i_recon::extract::from_headers(&base, &message.response.headers);
            observations.extend(h5i_recon::extract::candidates(
                &found,
                &identity,
                &h5i_recon::Source::Header { req: req.clone() },
            ));
        }

        let content_type = message.content_type().unwrap_or_default().to_ascii_lowercase();
        let Some(body) = h5i_recon::store::body_text(&store, &message.response.body) else {
            continue;
        };
        if content_type.contains("html") && wanted.contains(&"html") {
            let found = h5i_recon::extract::from_html(&base, &body);
            observations.extend(h5i_recon::extract::candidates(
                &found,
                &identity,
                &h5i_recon::Source::Page { req: req.clone() },
            ));
        } else if is_script(&content_type) && wanted.contains(&"js") {
            let script = h5i_recon::js::from_js(&base, &body);
            observations.extend(h5i_recon::extract::candidates(
                &script.found,
                &identity,
                &h5i_recon::Source::Script { req: req.clone() },
            ));
            for item in script.partial {
                // Reported, never stored. Half a URL is not a place a request
                // can go, and the ledger holds places (N8).
                partial.push(json!({"req": req, "prefix": item.prefix, "how": item.how}));
            }
        } else if content_type.contains("json") && wanted.contains(&"json") {
            let found = h5i_recon::extract::from_json(&base, &body);
            observations.extend(h5i_recon::extract::candidates(
                &found,
                &identity,
                &h5i_recon::Source::Json { req: req.clone() },
            ));
        }
    }

    let written = ledger.append(&observations)?;
    let inventory = ledger.read()?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA,
                "scanned": scanned,
                "disclosed": observations.len(),
                "written": written,
                "partial": partial,
                "cursor": inventory.cursor,
            }))?
        );
        return Ok(());
    }

    println!("  scanned  : {scanned} stored message(s)");
    println!("  disclosed: {} candidate(s)", observations.len());
    if written != observations.len() {
        println!(
            "  refused  : {} row(s) too large to keep",
            observations.len() - written
        );
    }
    for item in &partial {
        println!(
            "  partial  : {} built at runtime ({})",
            preview(item["prefix"].as_str().unwrap_or_default()),
            item["req"].as_str().unwrap_or_default()
        );
    }
    println!("  cursor   : {}", inventory.cursor);
    Ok(())
}

/// A body the JavaScript reader should read.
fn is_script(content_type: &str) -> bool {
    content_type.contains("javascript") || content_type.contains("ecmascript")
}

fn show(root: &Path, selector: Option<&str>, id: &str, json_out: bool) -> anyhow::Result<()> {
    let (_session, ledger) = open_ledger(root, selector)?;
    let inventory = ledger.read()?;
    let Some(endpoint) = inventory.endpoints.iter().find(|e| e.id == id) else {
        anyhow::bail!(
            "no endpoint `{id}` in this session's ledger. `h5i recon endpoints` lists what \
             is there"
        );
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA,
                "endpoint": endpoint,
            }))?
        );
        return Ok(());
    }

    println!("  {} {}{}", endpoint.method, endpoint.origin, endpoint.path);
    println!("  id       : {}", endpoint.id);
    println!("  state    : {}", state_word(endpoint.state));
    println!("  identity : {}", endpoint.identity);
    if let Some(status) = endpoint.status {
        println!("  status   : {status}");
    }
    if let Some(reason) = &endpoint.reason {
        println!("  refused  : {reason}");
    }
    if !endpoint.params.is_empty() {
        for param in &endpoint.params {
            println!("  param    : {} ({:?})", preview(&param.name), param.at);
        }
    }
    for source in &endpoint.sources {
        println!("  source   : {}", source_word(source));
    }
    if endpoint.evidence.is_empty() {
        // The distinction the whole ledger is arranged around, said plainly.
        println!("  evidence : none. Nothing has sent a request to this yet");
    } else {
        println!("  evidence : {}", endpoint.evidence.join(", "));
    }
    println!("  seen     : {} to {}", endpoint.first_seen, endpoint.last_seen);
    for note in &endpoint.notes {
        println!("  note     : {}", preview(note));
    }
    Ok(())
}

fn envelope(inventory: &Inventory, shown: &[&Endpoint]) -> Value {
    json!({
        "schema": SCHEMA,
        "cursor": inventory.cursor,
        "unreadable": inventory.unreadable,
        "truncated": inventory.truncated,
        "endpoints": shown,
    })
}

fn state_word(state: State) -> &'static str {
    match state {
        State::Candidate => "candidate",
        State::Observed => "observed",
        State::Confirmed => "confirmed",
        State::Refused => "refused",
        State::Gone => "gone",
    }
}

fn source_word(source: &h5i_recon::Source) -> String {
    use h5i_recon::Source::*;
    match source {
        Page { req } => format!("page {req}"),
        Script { req } => format!("script {req}"),
        Json { req } => format!("json {req}"),
        Header { req } => format!("header {req}"),
        KnownFile { req } => format!("known-file {req}"),
        Receipt { req } => format!("receipt {req}"),
        Wordlist { list } => format!("wordlist {}", preview(list)),
        Openapi { at } => format!("openapi {}", preview(at)),
        Import { tool } => format!("import {}", preview(tool)),
        Manual => "manual".to_string(),
    }
}

/// One line of target-written text, bounded. Everything in the ledger came
/// from somewhere else, and a terminal is not a safe place to paste it whole.
fn preview(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .take(120)
        .collect();
    if text.chars().count() > 120 {
        format!("{cleaned}…")
    } else {
        cleaned
    }
}
