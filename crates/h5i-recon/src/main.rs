//! `h5i recon`: what an application exposes, and how h5i knows.
//!
//! A plugin, for the reason the workbench is one: installing a browser should
//! not include everything buildable on one (W21, N19). It opens no socket. It
//! reads the session's log, store and ledger, and anything it sends is an `h5i
//! browser` verb in a subprocess, so the fetch is the engine's (N13).

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

    /// Which session, when more than one is open.
    #[arg(long, short = 's', global = true, value_name = "NAME")]
    session: Option<String>,

    /// Emit JSON. Every verb answers in the same envelope (`schema:
    /// "recon/1"`), errors included.
    #[arg(long, global = true)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let (session, json) = (cli.session.clone(), cli.json);
    if let Err(error) = run(cli.command, session.as_deref(), json) {
        // The envelope on stdout for the caller parsing it, the sentence on
        // stderr for the one reading it (design-websec.md W9). Exit 2 is what
        // the sibling plugin uses; 69 stays "the session is gone".
        println!(
            "{}",
            json!({"error": {"code": "recon", "message": error.to_string()}})
        );
        eprintln!("{error}");
        std::process::exit(2);
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
    },

    /// Read what this session already fetched, and record what it disclosed.
    ///
    /// Sends nothing, so it works after the budget is spent, and everything it
    /// writes is a candidate: a URL in a bundle was not visited (N8).
    Extract {
        /// Only this message, as `req_42`. The default is every message the
        /// store holds.
        #[arg(long, value_name = "REQ")]
        from: Option<String>,
        /// Which readers to run: any of `html`, `js`, `json`, `headers`.
        #[arg(long, value_name = "LIST", default_value = "html,js,json,headers")]
        kind: String,
    },

    /// Walk the application under this session's identity.
    ///
    /// Every request is an `h5i browser resend`, so it carries the session's
    /// cookies and its policy. Stops on frontier, allowance or login (N9).
    Crawl {
        /// Start here as well as from the ledger's candidates. Repeatable.
        #[arg(long, value_name = "URL")]
        seed: Vec<String>,
        /// How far from a seed to walk.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// How many requests this walk may spend, in total.
        #[arg(long = "max-requests", default_value_t = 200)]
        max_requests: usize,
        /// How many URLs of one path shape to visit: `/events/1` and
        /// `/events/2` are one endpoint twice.
        #[arg(long = "per-shape", default_value_t = 20)]
        per_shape: usize,
        /// Requests per second, per host. `0` means as fast as the engine will
        /// answer, which is rarely what an authorised engagement wants.
        #[arg(long, default_value_t = 4.0)]
        rate: f64,
        /// Re-check the login every N requests. `0` turns the check off.
        #[arg(long = "check-login-every", default_value_t = 25)]
        check_login_every: usize,
    },

    /// Ask for the files an application publishes about itself: robots.txt,
    /// sitemap.xml and its index chain, security.txt, OpenID discovery.
    ///
    /// `robots.txt` is read for candidates, never as permission (N8).
    Known {
        /// Which origin to ask. Defaults to the one this session has reached
        /// most recently.
        #[arg(long)]
        origin: Option<String>,
    },

    /// Sort what came back: calibrate a not-found baseline, then cluster.
    ///
    /// Confirmation happens here and nowhere else: `confirmed` means the answer
    /// differs from what this directory says about a path that is not there.
    Triage {
        /// Learn what each directory answers for a path that is not there,
        /// which costs a couple of requests per directory.
        #[arg(long)]
        calibrate: bool,
        /// How many probes per directory when calibrating.
        #[arg(long, default_value_t = 2)]
        probes: usize,
    },

    /// One endpoint: its sources, its evidence, and what it answered.
    Show {
        /// The `ep_…` id, as `h5i recon endpoints` prints it.
        id: String,
    },
}

fn run(action: ReconCommands, session: Option<&str>, json: bool) -> anyhow::Result<()> {
    let root = bs::root()?;
    let _ = bs::expire_due(&root);

    match action {
        ReconCommands::Endpoints {
            state,
            origin,
            identity,
            since,
        } => endpoints(
            &root,
            session,
            Filter {
                state: state.as_deref().map(parse_state).transpose()?,
                origin: origin.as_deref(),
                identity: identity.as_deref(),
                since: since.unwrap_or(0),
            },
            json,
        ),
        ReconCommands::Extract { from, kind } => {
            extract(&root, session, from.as_deref(), &kind, json)
        }
        ReconCommands::Crawl {
            seed,
            depth,
            max_requests,
            per_shape,
            rate,
            check_login_every,
        } => crawl(
            &root,
            session,
            &seed,
            h5i_recon::crawl::Bounds {
                depth,
                max_requests,
                per_template: per_shape,
            },
            rate,
            check_login_every,
            json,
        ),
        ReconCommands::Triage { calibrate, probes } => {
            triage(&root, session, calibrate, probes, json)
        }
        ReconCommands::Known { origin } => known(&root, session, origin.as_deref(), json),
        ReconCommands::Show { id } => show(&root, session, &id, json),
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
fn open_ledger(root: &Path, selector: Option<&str>) -> anyhow::Result<(bs::Session, Ledger, bool)> {
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
    let (records, capped) = receipts(&dir);
    ledger.sync_receipts(&records, identity_of(&session))?;
    Ok((session, ledger, capped))
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

/// The session's request log, and whether the read stopped at the cap.
///
/// The cap reads the *head* of the log, so a long run's later requests are not
/// there and never will be. A caller that did not say so would report an
/// inventory of the first part of a run as the inventory.
fn receipts(dir: &Path) -> (Vec<RequestRecord>, bool) {
    let path = dir.join(bs::RECEIPTS_FILE);
    let (text, capped) = bs::read_log_capped_saying(&path).unwrap_or_default();
    let records = text
        .lines()
        .filter_map(|line| serde_json::from_str::<RequestRecord>(line).ok())
        .collect();
    (records, capped)
}

/// Said once, wherever a verb read a log that was too long to read whole.
fn note_capped(capped: bool) {
    if capped {
        eprintln!(
            "  note     : this session's request log is longer than the cap on reading it, \
             so this covers the start of the run and not all of it"
        );
    }
}

fn endpoints(
    root: &Path,
    selector: Option<&str>,
    filter: Filter<'_>,
    json_out: bool,
) -> anyhow::Result<()> {
    let (_session, ledger, capped) = open_ledger(root, selector)?;
    note_capped(capped);
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
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope(&inventory, &shown, capped))?
        );
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

/// One request, as the engine sent it.
struct Sent {
    seq: u64,
    status: Option<u16>,
    error: Option<String>,
}

/// Send a stored request again with its target replaced.
///
/// Through `h5i browser resend`, the verb a person types: the fetch is the
/// engine's, and this plugin has no other route to the network.
fn probe(selector: Option<&str>, from: u64, target: &str, method: &str) -> anyhow::Result<Sent> {
    let mut command = std::process::Command::new(h5i());
    command
        .arg("browser")
        .arg("resend")
        .arg(from.to_string())
        // The method is named rather than inherited: a seed that happened to
        // be a POST would ask for `/robots.txt` with a body, and the answer to
        // that is about the method, not about the file.
        .arg("--set")
        .arg(format!("method={method}"))
        .arg("--raw-target")
        .arg(target)
        .arg("--json");
    if let Some(name) = selector {
        command.arg("--session").arg(name);
    }
    let output = command
        .output()
        .map_err(|e| anyhow::anyhow!("`h5i browser resend` could not be run: {e}"))?;
    let reply: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        anyhow::anyhow!(
            "`h5i browser resend` answered something this could not read: {}",
            String::from_utf8_lossy(&output.stdout).chars().take(200).collect::<String>()
        )
    })?;
    if reply.get("ok").and_then(Value::as_bool) == Some(false) {
        anyhow::bail!(
            "{}",
            reply
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the engine refused the request")
        );
    }
    Ok(Sent {
        seq: reply.get("seq").and_then(Value::as_u64).unwrap_or_default(),
        status: reply
            .pointer("/response/status")
            .and_then(Value::as_u64)
            .map(|s| s as u16),
        error: reply
            .pointer("/response/error")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The stored request a probe is built from, so it inherits real authority,
/// cookies and headers.
///
/// The newest `GET` wins: a `POST` seed carries a body, and the answer would
/// be about the body rather than the path.
fn seed(store: &Path, origin: Option<&str>) -> Option<(u64, String)> {
    let mut fallback = None;
    for seq in h5i_recon::store::sequences(store).into_iter().rev() {
        let Some(message) = h5i_recon::store::read(store, seq) else {
            continue;
        };
        let Ok(url) = url::Url::parse(&message.request.url) else {
            continue;
        };
        let seen = h5i_recon::ingest::origin_of(&url);
        if origin.is_some_and(|wanted| wanted != seen) {
            continue;
        }
        if message.request.method.eq_ignore_ascii_case("GET") {
            return Some((seq, seen));
        }
        fallback.get_or_insert((seq, seen));
    }
    fallback
}

/// What one probe found out, folded into the ledger and the frontier.
struct Visited {
    fingerprint: h5i_recon::crawl::Fingerprint,
    observations: Vec<h5i_recon::Observation>,
    /// Candidates this page disclosed, to offer to the frontier.
    disclosed: Vec<url::Url>,
}

/// Ask for one URL and read what came back, with the same readers `extract`
/// uses: a crawl with its own parser would be a second opinion.
fn visit(
    store: &Path,
    selector: Option<&str>,
    from: u64,
    url: &url::Url,
    identity: &str,
) -> anyhow::Result<Visited> {
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let origin = h5i_recon::ingest::origin_of(url);
    let sent = probe(selector, from, &target, "GET")?;
    let req = format!("req_{}", sent.seq);

    let mut visited = Visited {
        fingerprint: h5i_recon::crawl::Fingerprint::of(sent.status, "", 0, None),
        observations: vec![
            h5i_recon::Observation::new(
                &origin,
                url.path(),
                "GET",
                identity,
                h5i_recon::State::Observed,
                h5i_recon::Source::Receipt { req: req.clone() },
            )
            .with_req(req.clone())
            .with_status(sent.status),
        ],
        disclosed: Vec::new(),
    };

    let Some(message) = h5i_recon::store::read(store, sent.seq) else {
        return Ok(visited);
    };
    let content_type = message.content_type().unwrap_or_default().to_string();
    let location = message.header("location").map(str::to_string);
    let body = h5i_recon::store::body_text(store, &message.response.body).unwrap_or_default();
    visited.fingerprint = h5i_recon::crawl::Fingerprint::of(
        sent.status,
        &content_type,
        body.len() as u64,
        location.as_deref(),
    );

    let Ok(base) = url::Url::parse(&message.response.url) else {
        return Ok(visited);
    };
    let mut found = h5i_recon::extract::from_headers(&base, &message.response.headers);
    let source = if content_type.to_ascii_lowercase().contains("html") {
        found.extend(h5i_recon::extract::from_html(&base, &body));
        h5i_recon::Source::Page { req: req.clone() }
    } else if is_script(&content_type.to_ascii_lowercase()) {
        found.extend(h5i_recon::js::from_js(&base, &body).found);
        h5i_recon::Source::Script { req: req.clone() }
    } else if content_type.to_ascii_lowercase().contains("json") {
        found.extend(h5i_recon::extract::from_json(&base, &body));
        h5i_recon::Source::Json { req: req.clone() }
    } else {
        h5i_recon::Source::Header { req: req.clone() }
    };
    visited.disclosed = found.iter().map(|item| item.url.clone()).collect();
    visited
        .observations
        .extend(h5i_recon::extract::candidates(&found, identity, &source));
    Ok(visited)
}

/// `h5i recon crawl`.
fn crawl(
    root: &Path,
    selector: Option<&str>,
    seeds: &[String],
    bounds: h5i_recon::crawl::Bounds,
    rate: f64,
    check_login_every: usize,
    json_out: bool,
) -> anyhow::Result<()> {
    let (session, ledger, capped) = open_ledger(root, selector)?;
    note_capped(capped);
    let store = bs::dir(root, &session.id).join(bs::MESSAGES_DIR);
    let Some((from, origin)) = seed(&store, None) else {
        anyhow::bail!(
            "this session has no stored request to walk from. `h5i browser open <url> \
             --capture` first: a crawl is a series of edits of a real request, so it \
             carries that request's authority and cookies rather than inventing them"
        );
    };
    let identity = identity_of(&session).to_string();

    let mut frontier = h5i_recon::crawl::Frontier::new(bounds);
    let mut queued = 0usize;
    for seed_url in seeds {
        let url = url::Url::parse(seed_url)
            .map_err(|_| anyhow::anyhow!("`{seed_url}` is not a URL a crawl could start from"))?;
        if frontier.offer(&url, 0) == h5i_recon::crawl::Offer::Queued {
            queued += 1;
        }
    }
    // Everything the ledger already knows about and nothing has visited. This
    // is why `extract` and `known` come first: they fill the frontier without
    // spending a request.
    for endpoint in &ledger.read()?.endpoints {
        if endpoint.state != h5i_recon::State::Candidate || endpoint.method != "GET" {
            continue;
        }
        let Ok(url) = url::Url::parse(&format!("{}{}", endpoint.origin, endpoint.path)) else {
            continue;
        };
        if frontier.offer(&url, 0) == h5i_recon::crawl::Offer::Queued {
            queued += 1;
        }
    }
    if queued == 0 {
        anyhow::bail!(
            "nothing to walk. `h5i recon extract` reads what this session already fetched \
             and `h5i recon known` asks for robots.txt and the sitemap; either fills the \
             frontier without spending a request. `--seed <url>` starts from one you name"
        );
    }

    let pause = if rate > 0.0 {
        std::time::Duration::from_secs_f64(1.0 / rate)
    } else {
        std::time::Duration::ZERO
    };
    let mut observations = Vec::new();
    let mut walked: Vec<Value> = Vec::new();
    let mut login: Option<(url::Url, h5i_recon::crawl::Fingerprint)> = None;
    let mut stopped: Option<String> = None;
    let mut since_check = 0usize;
    let mut checks = 0usize;

    while let Some((url, depth)) = frontier.next() {
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
        let visited = match visit(&store, selector, from, &url, &identity) {
            Ok(visited) => visited,
            Err(why) => {
                // Refused, or the engine could not send it. Either way it is a
                // fact about this walk and goes in the ledger as one.
                observations.push(
                    h5i_recon::Observation::new(
                        &h5i_recon::ingest::origin_of(&url),
                        url.path(),
                        "GET",
                        &identity,
                        h5i_recon::State::Candidate,
                        h5i_recon::Source::Receipt {
                            req: format!("req_{from}"),
                        },
                    )
                    .refused(why.to_string()),
                );
                walked.push(json!({"url": url.to_string(), "refused": why.to_string()}));
                continue;
            }
        };
        walked.push(json!({
            "url": url.to_string(),
            "depth": depth,
            "status": visited.fingerprint.status,
            "disclosed": visited.disclosed.len(),
        }));
        for disclosed in &visited.disclosed {
            if h5i_recon::ingest::origin_of(disclosed) == origin {
                frontier.offer(disclosed, depth + 1);
            }
        }
        observations.extend(visited.observations);
        login.get_or_insert_with(|| (url.clone(), visited.fingerprint.clone()));

        since_check += 1;
        if check_login_every > 0 && since_check >= check_login_every {
            since_check = 0;
            let Some((probe_url, before)) = &login else {
                continue;
            };
            // The check is a request like any other, so it is counted and it
            // stops when the allowance does.
            checks += 1;
            match visit(&store, selector, from, probe_url, &identity) {
                Ok(now) if h5i_recon::crawl::identity_lost(before, &now.fingerprint) => {
                    stopped = Some(format!(
                        "the page this walk started from answers differently now, so the                          session is no longer logged in as `{identity}`. Nothing after this                          point would be that identity's answer. Log in again and re-run"
                    ));
                    break;
                }
                Ok(_) => {}
                Err(why) => {
                    stopped = Some(format!("the login check could not be sent: {why}"));
                    break;
                }
            }
        }
    }
    if stopped.is_none() && frontier.exhausted() {
        stopped = Some(format!(
            "spent its allowance of {} requests with {} URLs still queued. This is what              {} requests reached, not the whole application",
            frontier.spent(),
            frontier.remaining(),
            frontier.spent()
        ));
    }

    let written = ledger.append(&observations)?;
    let inventory = ledger.read()?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA,
                "origin": origin,
                "requests": frontier.spent() + checks,
                "login_checks": checks,
                "queued": frontier.remaining(),
                "walked": walked,
                "written": written,
                "stopped": stopped,
                "cursor": inventory.cursor,
            }))?
        );
        return Ok(());
    }
    println!("  origin   : {origin}");
    println!(
        "  requests : {} ({} of them login checks)",
        frontier.spent() + checks,
        checks
    );
    println!("  queued   : {} left unwalked", frontier.remaining());
    println!("  written  : {written} row(s)");
    if let Some(why) = &stopped {
        println!("  stopped  : {why}");
    }
    println!("  cursor   : {}", inventory.cursor);
    Ok(())
}

/// A path that cannot exist, for calibration.
///
/// Long, random and ordinary-looking: a name a real application would not have
/// and a filter would not treat specially.
fn improbable(n: usize) -> String {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(n as u64 * 1442695040888963407);
    format!("h5i-not-here-{seq:016x}", seq = seed)
}

/// `h5i recon triage`.
fn triage(
    root: &Path,
    selector: Option<&str>,
    calibrate: bool,
    probes: usize,
    json_out: bool,
) -> anyhow::Result<()> {
    let (session, ledger, capped) = open_ledger(root, selector)?;
    note_capped(capped);
    let store = bs::dir(root, &session.id).join(bs::MESSAGES_DIR);
    let identity = identity_of(&session).to_string();
    let inventory = ledger.read()?;
    let mut progress = ledger.progress();

    // What came back, read off the stored messages rather than sent again.
    let mut samples: Vec<h5i_recon::triage::Sample> = Vec::new();
    for endpoint in &inventory.endpoints {
        let Some(req) = endpoint.evidence.last() else {
            continue;
        };
        let Some(seq) = req.trim_start_matches("req_").parse::<u64>().ok() else {
            continue;
        };
        let Some(message) = h5i_recon::store::read(&store, seq) else {
            continue;
        };
        let content_type = message.content_type().unwrap_or_default().to_string();
        let location = message.header("location").map(str::to_string);
        let body = h5i_recon::store::body_text(&store, &message.response.body).unwrap_or_default();
        samples.push(h5i_recon::triage::Sample {
            endpoint: endpoint.id.clone(),
            path: endpoint.path.clone(),
            req: req.clone(),
            fingerprint: h5i_recon::crawl::Fingerprint::of(
                message.response.status,
                &content_type,
                body.len() as u64,
                location.as_deref(),
            ),
            skeleton: if content_type.to_ascii_lowercase().contains("html") {
                h5i_recon::extract::skeleton(&body)
            } else {
                String::new()
            },
        });
    }

    let mut calibrated: Vec<Value> = Vec::new();
    let mut probes_sent: Vec<h5i_recon::Observation> = Vec::new();
    if calibrate {
        let Some((from, _origin)) = seed(&store, None) else {
            anyhow::bail!("this session has no stored request to calibrate from");
        };
        let mut directories: Vec<String> = samples
            .iter()
            .map(|sample| h5i_recon::triage::directory_of(&sample.path))
            .collect();
        directories.sort();
        directories.dedup();

        for directory in directories {
            let mut baseline = h5i_recon::triage::Baseline::default();
            for n in 0..probes.max(1) {
                let target = format!("{}/{}", directory.trim_end_matches('/'), improbable(n));
                let Ok(sent) = probe(selector, from, &target, "GET") else {
                    continue;
                };
                let Some(message) = h5i_recon::store::read(&store, sent.seq) else {
                    continue;
                };
                let content_type = message.content_type().unwrap_or_default().to_string();
                let body =
                    h5i_recon::store::body_text(&store, &message.response.body).unwrap_or_default();
                if baseline.skeleton.is_empty() && content_type.to_ascii_lowercase().contains("html")
                {
                    baseline.skeleton = h5i_recon::extract::skeleton(&body);
                }
                baseline.samples.push(h5i_recon::crawl::Fingerprint::of(
                    message.response.status,
                    &content_type,
                    body.len() as u64,
                    message.header("location"),
                ));
                // The probe is in the receipts either way. Naming it here is
                // what keeps a reader from wondering why the inventory holds a
                // path nobody would have.
                let req = format!("req_{}", sent.seq);
                probes_sent.push(
                    h5i_recon::Observation::new(
                        &h5i_recon::ingest::origin_of(
                            &url::Url::parse(&message.response.url)
                                .unwrap_or_else(|_| url::Url::parse("http://invalid.invalid").unwrap()),
                        ),
                        &target,
                        "GET",
                        &identity,
                        h5i_recon::State::Observed,
                        h5i_recon::Source::Calibration { req: req.clone() },
                    )
                    .with_req(req)
                    .with_status(message.response.status),
                );
            }
            calibrated.push(json!({
                "directory": directory,
                "probes": baseline.samples.len(),
                "status": baseline.samples.first().and_then(|f| f.status),
                "stable": baseline.is_stable(),
            }));
            progress.baselines.insert(directory, baseline);
        }
        ledger.set_progress(&progress)?;
        ledger.append(&probes_sent)?;
    }

    // Confirm what the baseline says is really there, and retire what is not.
    let mut verdicts = Vec::new();
    let mut confirmed = 0usize;
    let mut nothing_here = 0usize;
    let clusters = h5i_recon::triage::cluster(&samples);
    for sample in &samples {
        let directory = h5i_recon::triage::directory_of(&sample.path);
        let Some(baseline) = progress.baselines.get(&directory) else {
            continue;
        };
        if !baseline.is_stable() {
            // An unstable baseline confirms at random, so it confirms nothing.
            continue;
        }
        let Some(endpoint) = inventory.endpoints.iter().find(|e| e.id == sample.endpoint) else {
            continue;
        };
        let cluster = clusters
            .iter()
            .find(|cluster| cluster.members.contains(&sample.req))
            .map(|cluster| cluster.label.clone());

        let (state, counted) = if baseline.says_nothing_here(sample) {
            // Only something already confirmed can be gone. A path that never
            // existed has not stopped existing.
            match endpoint.state {
                h5i_recon::State::Confirmed => (Some(h5i_recon::State::Gone), &mut nothing_here),
                _ => (None, &mut nothing_here),
            }
        } else {
            (Some(h5i_recon::State::Confirmed), &mut confirmed)
        };
        *counted += 1;
        let Some(state) = state else { continue };
        let mut observation = h5i_recon::Observation::new(
            &endpoint.origin,
            &endpoint.path,
            &endpoint.method,
            &identity,
            state,
            h5i_recon::Source::Receipt {
                req: sample.req.clone(),
            },
        )
        .with_req(sample.req.clone())
        .with_status(sample.fingerprint.status);
        observation.cluster = cluster;
        verdicts.push(observation);
    }
    let written = ledger.append(&verdicts)?;
    let after = ledger.read()?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA,
                "calibrated": calibrated,
                "read": samples.len(),
                "confirmed": confirmed,
                "not_found_like": nothing_here,
                "written": written,
                "clusters": clusters,
                "cursor": after.cursor,
            }))?
        );
        return Ok(());
    }

    for item in &calibrated {
        println!(
            "  baseline : {:<24} {} probe(s), status {}{}",
            item["directory"].as_str().unwrap_or_default(),
            item["probes"].as_u64().unwrap_or(0),
            item["status"]
                .as_u64()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string()),
            if item["stable"].as_bool() == Some(true) {
                ""
            } else {
                ", unstable: it confirms nothing here"
            }
        );
    }
    println!("  read     : {} response(s)", samples.len());
    println!("  confirmed: {confirmed}");
    println!("  not there: {nothing_here} answered like a path that is not there");
    println!();
    for cluster in &clusters {
        println!("  x{:<5} {}", cluster.count, cluster.label);
        for path in &cluster.paths {
            println!("           {}", preview(path));
        }
        if cluster.count > cluster.paths.len() {
            println!("           … and {} more", cluster.count - cluster.paths.len());
        }
        println!("           read one: h5i websec show {}", cluster.representative);
    }
    println!("  cursor   : {}", after.cursor);
    Ok(())
}

/// `h5i recon known`.
fn known(
    root: &Path,
    selector: Option<&str>,
    origin: Option<&str>,
    json_out: bool,
) -> anyhow::Result<()> {
    let (session, ledger, capped) = open_ledger(root, selector)?;
    note_capped(capped);
    let store = bs::dir(root, &session.id).join(bs::MESSAGES_DIR);
    let Some((from, origin)) = seed(&store, origin) else {
        anyhow::bail!(
            "this session has no stored request to send again. `h5i browser open <url> \
             --capture` first: a probe is an edit of a real request, so it carries that \
             request's authority and cookies rather than inventing them"
        );
    };
    let identity = identity_of(&session).to_string();

    let mut observations = Vec::new();
    let mut asked: Vec<Value> = Vec::new();
    let mut queue: Vec<String> = h5i_recon::known::WELL_KNOWN
        .iter()
        .map(|path| (*path).to_string())
        .collect();
    let mut followed = 0usize;
    let mut done: Vec<String> = Vec::new();

    while let Some(target) = queue.pop() {
        if done.contains(&target) {
            continue;
        }
        done.push(target.clone());
        let sent = match probe(selector, from, &target, "GET") {
            Ok(sent) => sent,
            Err(why) => {
                // A refusal is a fact about the scope, not a reason to stop.
                asked.push(json!({"path": target, "refused": why.to_string()}));
                observations.push(
                    h5i_recon::Observation::new(
                        &origin,
                        &target,
                        "GET",
                        &identity,
                        h5i_recon::State::Candidate,
                        h5i_recon::Source::KnownFile {
                            req: format!("req_{from}"),
                        },
                    )
                    .refused(why.to_string()),
                );
                continue;
            }
        };
        let req = format!("req_{}", sent.seq);
        asked.push(json!({
            "path": target,
            "req": req,
            "status": sent.status,
            "error": sent.error,
        }));
        observations.push(
            h5i_recon::Observation::new(
                &origin,
                &target,
                "GET",
                &identity,
                h5i_recon::State::Observed,
                h5i_recon::Source::KnownFile { req: req.clone() },
            )
            .with_req(req.clone())
            .with_status(sent.status),
        );

        // What the file says, read from the store rather than from the reply:
        // the bytes are already written down, and reading them twice would be
        // two answers that can disagree.
        if sent.status.is_none_or(|status| !(200..300).contains(&status)) {
            continue;
        }
        let Some(message) = h5i_recon::store::read(&store, sent.seq) else {
            continue;
        };
        let Some(body) = h5i_recon::store::body_text(&store, &message.response.body) else {
            continue;
        };
        let Ok(base) = url::Url::parse(&message.response.url) else {
            continue;
        };
        let source = h5i_recon::Source::KnownFile { req };
        let (found, sitemaps) = if target.ends_with("robots.txt") {
            h5i_recon::known::from_robots(&base, &body)
        } else if body.contains("<urlset") || body.contains("<sitemapindex") {
            h5i_recon::known::from_sitemap(&base, &body)
        } else {
            (Vec::new(), Vec::new())
        };
        observations.extend(h5i_recon::extract::candidates(&found, &identity, &source));

        for sitemap in sitemaps {
            if followed >= h5i_recon::known::MAX_SITEMAP_DEPTH {
                break;
            }
            if h5i_recon::ingest::origin_of(&sitemap) != origin {
                // Another origin is another policy decision. It goes into the
                // ledger as a candidate and is not chased from here.
                observations.extend(h5i_recon::extract::candidates(
                    &[h5i_recon::Found {
                        url: sitemap,
                        method: "GET".to_string(),
                        params: Vec::new(),
                        how: "sitemap-index",
                    }],
                    &identity,
                    &source,
                ));
                continue;
            }
            followed += 1;
            queue.push(sitemap.path().to_string());
        }
    }

    let written = ledger.append(&observations)?;
    let inventory = ledger.read()?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA,
                "origin": origin,
                "asked": asked,
                "written": written,
                "cursor": inventory.cursor,
            }))?
        );
        return Ok(());
    }
    println!("  origin   : {origin}");
    for item in &asked {
        match item.get("refused").and_then(Value::as_str) {
            Some(why) => println!(
                "  refused  : {} — {}",
                item["path"].as_str().unwrap_or_default(),
                preview(why)
            ),
            None => println!(
                "  {:<8} {:<36} {}",
                item["status"]
                    .as_u64()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                item["path"].as_str().unwrap_or_default(),
                item["req"].as_str().unwrap_or_default()
            ),
        }
    }
    println!("  written  : {written} row(s)");
    println!("  cursor   : {}", inventory.cursor);
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
    let (session, ledger, capped) = open_ledger(root, selector)?;
    note_capped(capped);
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
    let (_session, ledger, capped) = open_ledger(root, selector)?;
    note_capped(capped);
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

fn envelope(inventory: &Inventory, shown: &[&Endpoint], capped: bool) -> Value {
    json!({
        "schema": SCHEMA,
        "cursor": inventory.cursor,
        "unreadable": inventory.unreadable,
        // Two different partial reads, and a caller has to be able to tell
        // them apart: the ledger was too long, or the request log was.
        "ledger_truncated": inventory.truncated,
        "log_truncated": capped,
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
        Calibration { req } => format!("calibration {req}"),
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
