//! Read and compare captured HTTP messages without touching the network.
//!
//! h5i reads the store directly so credentials never pass through the renderer.
//! Stores on inaccessible boxed filesystems produce an explicit error.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use h5i_browser::capture::{Body, StoredRequest, StoredResponse, body_file};
// The store's readers live with the store's types, because the websec plugin
// reads the same bytes and must not link an engine to do it (W21).
use h5i_wire::read::{
    LOSSY_BODY_BYTES, MAX_PREVIEW_LINE, Text, body_bytes, body_text, json_at, preview_line,
    printable, push_body, raw_request, raw_response, read_json, sequences,
};
use h5i_core::browser_session as bs;
use serde_json::{json, Value};

/// Which half of a message to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Request,
    Response,
    Both,
}

/// Where a session's messages are, or why they cannot be read.
fn store_dir(root: &Path, selector: Option<&str>) -> anyhow::Result<(bs::Session, PathBuf)> {
    // Live or ended: a store outlives its engine.
    let session = super::browser::resolve_for_reading(root, selector)?;
    // A boxed session's record sits where boxed code can write it, and this id
    // becomes a path.
    if !bs::id_is_one_component(&session.id) {
        anyhow::bail!(
            "session record names `{}` as its id, which is not one this registry could \
             have minted. Nothing was read",
            session.id
        );
    }
    let dir = bs::dir(root, &session.id).join(bs::MESSAGES_DIR);
    if !dir.exists() {
        let placed = match &session.placement {
            bs::Placement::Box { name } => format!(
                "\n\n  This session runs in box `{name}`. If that box keeps its /tmp inside \
                 its image, its store is not on a filesystem this machine can read, and \
                 nothing here is missing."
            ),
            bs::Placement::Host => String::new(),
        };
        anyhow::bail!(
            "session {} kept no messages: it was opened without `--capture`, so only the \
             request log exists. `h5i browser requests` shows what it sent; \
             `h5i browser open <url> --capture` starts one that also keeps the messages.{placed}",
            session.id
        );
    }
    Ok((session, dir))
}













/// `h5i browser message <seq>`.
pub fn show(
    root: &Path,
    selector: Option<&str>,
    seq: u64,
    part: Part,
    raw: bool,
    body_to: Option<&Path>,
    json_out: bool,
) -> anyhow::Result<()> {
    let (session, dir) = store_dir(root, selector)?;

    let request: Option<StoredRequest> = match part {
        Part::Response => None,
        _ => Some(read_json(&dir.join(format!("{seq}.request.json"))).map_err(|_| {
            let have = sequences(&dir);
            anyhow::anyhow!(
                "session {} has no stored request {seq}. It holds: {}",
                session.id,
                if have.is_empty() {
                    "nothing yet".to_string()
                } else {
                    have.iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )
        })?),
    };
    let response: Option<StoredResponse> = match part {
        Part::Request => None,
        // A request with no response is a real state: the connection failed, or
        // the process died mid-fetch. Reported as absence rather than as an
        // error, because the request half is still the evidence.
        _ => read_json(&dir.join(format!("{seq}.response.json"))).ok(),
    };

    let request_body = request.as_ref().map(|r| body_text(&dir, &r.body));
    let response_body = response.as_ref().map(|r| body_text(&dir, &r.body));
    // What the socket carried after the response ended. Nothing, for every
    // fetch that was not a desync.
    let trailing = response
        .as_ref()
        .and_then(|r| r.trailing.as_ref())
        .map(|body| body_text(&dir, body));

    let mut wrote: Option<Value> = None;
    // The bytes, before anything renders them. `--part both` means the
    // response's, because that is the half a caller asks to keep.
    if let Some(path) = body_to {
        let source = match part {
            Part::Request => request.as_ref().map(|r| &r.body),
            Part::Response => response.as_ref().map(|r| &r.body),
            Part::Both => response
                .as_ref()
                .map(|r| &r.body)
                .or(request.as_ref().map(|r| &r.body)),
        };
        let Some(body) = source else {
            anyhow::bail!("message {seq} has no such half to write out");
        };
        let bytes = body_bytes(&dir, body).ok_or_else(|| {
            anyhow::anyhow!(
                "message {seq}'s body is not in the store, so there is nothing to write"
            )
        })?;
        std::fs::write(path, &bytes)
            .map_err(|e| anyhow::anyhow!("{} could not be written: {e}", path.display()))?;
        // The exact byte channel every bounded view points at, so a body the
        // store cut has to say so rather than hand over a shorter file.
        let of_bytes = match body {
            Body::Stored {
                truncated: true,
                of_bytes,
                ..
            } => *of_bytes,
            _ => None,
        };
        let mut note = json!({"path": path.display().to_string(), "bytes": bytes.len()});
        if let Some(had) = of_bytes {
            note["of_bytes"] = json!(had);
            note["truncated"] = json!(true);
        }
        wrote = Some(note);
        if !json_out {
            match of_bytes {
                None => println!("  wrote    : {} bytes to {}", bytes.len(), path.display()),
                Some(had) => println!(
                    "  wrote    : {} bytes to {} — the head of a {had} byte body, which is \
                     all the store kept",
                    bytes.len(),
                    path.display()
                ),
            }
        }
    }

    // `--raw` outranks the JSON envelope, in both directions. A raw message is
    // bytes as they went on the wire, and there is no way to put those inside a
    // JSON document and still have them be those bytes. The alternative was to
    // keep ignoring the flag whenever the caller had not also typed `--human`,
    // which is the shape of silently sending something other than what was
    // asked for.
    if json_out && !raw {
        let mut value = json!({"seq": seq, "session": session.id});
        if let Some(wrote) = wrote {
            value["wrote"] = wrote;
        }
        if let (Some(request), Some(body)) = (&request, &request_body) {
            value["request"] = json!({
                "at": request.at,
                "method": request.method,
                "url": request.url,
                "headers": request.headers,
                "body": body.to_json(),
            });
        }
        if let (Some(response), Some(body)) = (&response, &response_body) {
            value["response"] = json!({
                "at": response.at,
                "url": response.url,
                "status": response.status,
                "headers": response.headers,
                "content_encoding": response.content_encoding,
                "wire_bytes": response.wire_bytes,
                "body": body.to_json(),
            });
            if let Some(trailing) = &trailing {
                value["response"]["trailing"] = trailing.to_json();
            }
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    if let (Some(request), Some(body)) = (&request, &request_body) {
        if raw {
            print!("{}", raw_request(request, body));
        } else {
            println!("  request  : {} {}", request.method, request.url);
            println!("  at       : {}", request.at);
            for (name, value) in &request.headers {
                println!("    {}: {}", preview_line(name), preview_line(value));
            }
            summarise_body(body);
        }
        if matches!(part, Part::Both) {
            println!();
        }
    }
    if let (Some(response), Some(body)) = (&response, &response_body) {
        if raw {
            print!("{}", raw_response(response, body));
            // Printed after the response and not inside it, because that is
            // where it arrived: a second message on the same connection,
            // answering a request this session never sent.
            if let Some(trailing) = &trailing {
                let mut after = String::from("\r\n");
                push_body(&mut after, trailing);
                print!("{after}");
            }
        } else {
            match response.status {
                Some(status) => println!("  response : {status}"),
                None => println!("  response : (none: the request did not complete)"),
            }
            for (name, value) in &response.headers {
                println!("    {}: {}", preview_line(name), preview_line(value));
            }
            summarise_body(body);
            if let Some(trailing) = &trailing {
                println!("  after    : the connection carried more once this response ended");
                summarise_body(trailing);
            }
        }
    } else if matches!(part, Part::Response) {
        println!("  response : not stored. The request half is at `--part request`.");
    }
    Ok(())
}



fn summarise_body(body: &Text) {
    match body {
        Text::Utf8(text) if text.is_empty() => println!("  body     : empty"),
        Text::Utf8(text) => {
            println!("  body     : {} bytes", text.len());
            let lines = text.lines().count();
            for line in text.lines().take(20) {
                println!("    {}", preview_line(line));
            }
            if lines > 20 {
                println!("    … {} more lines", lines - 20);
            }
        }
        Text::Cut { text, of_bytes } => {
            println!(
                "  body     : {of_bytes} bytes, of which the store kept {}",
                text.len()
            );
            for line in text.lines().take(20) {
                println!("    {}", preview_line(line));
            }
            println!("    … the rest of this body is not in the store");
        }
        Text::Binary {
            bytes,
            sha256,
            text,
        } => {
            println!("  body     : {bytes} bytes, not text (sha256 {sha256})");
            for line in text.lines().take(20) {
                println!("    {}", preview_line(line));
            }
        }
        Text::Missing(why) => println!("  body     : not stored ({why})"),
    }
}

/// Headers that make a request *that user's* request.
///
/// Stripped when a stored message is carried into another session, and this is
/// the whole meaning of `--as`. "Send Alice's request as Bob" means Bob's
/// session makes it: Bob's cookies, Bob's identity, Bob's policy. Carrying
/// Alice's `Cookie` header along would send a request that is neither Alice's
/// (it went through Bob's session) nor Bob's (it carried Alice's credential),
/// and the 200 it came back with would answer no question at all.
///
/// A caller who does want to send Alice's exact credential can read it with
/// `message` and set it with `--set header.Authorization=…`, which is a
/// deliberate act rather than a default.
fn header_is_the_users(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "cookie" | "authorization" | "proxy-authorization"
    )
}

/// The exact body to send again, or why there is not one.
///
/// The in-session resend's truncation rule, kept here so the two cannot
/// disagree: a body the store cut short is not the body that was sent.
fn carried_body(dir: &Path, seq: u64, body: &Body) -> anyhow::Result<Vec<u8>> {
    if let Body::Stored { truncated: true, .. } = body {
        anyhow::bail!(
            "request {seq} was too large to keep whole, so carrying it into another \
             session would send a request that is not the one recorded"
        );
    }
    match body_text(dir, body) {
        Text::Utf8(text) => Ok(text.into_bytes()),
        // Unreachable while the check above stands; spelled out so a change
        // to it fails here.
        Text::Cut { of_bytes, .. } => anyhow::bail!(
            "request {seq} carried {of_bytes} bytes and the store kept only its head, so \
             carrying it would send a request that is not the one recorded"
        ),
        Text::Binary { sha256, .. } => {
            let path = body_file(dir, &sha256)
                .ok_or_else(|| anyhow::anyhow!("{sha256:?} is not a body hash"))?;
            std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("the stored body could not be read: {e}"))
        }
        Text::Missing(why) => anyhow::bail!(
            "request {seq}'s body is not in the store ({why}), so it cannot be carried"
        ),
    }
}

/// One session's stored request, ready to hand to another session.
///
/// Returns the JSON the `resend` verb takes, and the names of the headers that
/// were dropped, so the caller can say what it did rather than doing it
/// quietly.
pub fn carry(
    root: &Path,
    from_session: Option<&str>,
    seq: u64,
    keep_credentials: bool,
) -> anyhow::Result<(Value, Vec<String>)> {
    let (session, dir) = store_dir(root, from_session)?;
    let stored: StoredRequest =
        read_json(&dir.join(format!("{seq}.request.json"))).map_err(|_| {
            anyhow::anyhow!(
                "session {} has no stored request {seq}. It holds: {}",
                session.id,
                sequences(&dir)
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let mut dropped = Vec::new();
    let headers: Vec<(String, String)> = stored
        .headers
        .into_iter()
        .filter(|(name, _)| {
            if !keep_credentials && header_is_the_users(name) {
                dropped.push(name.to_ascii_lowercase());
                return false;
            }
            true
        })
        .collect();

    let body = carried_body(&dir, seq, &stored.body)?;
    use base64::Engine as _;
    Ok((
        json!({
            "method": stored.method,
            "url": stored.url,
            "headers": headers,
            "body_base64": base64::engine::general_purpose::STANDARD.encode(&body),
        }),
        dropped,
    ))
}

/// How two responses differ, in the layers an agent branches on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Difference {
    /// Nothing differs: same status, same headers that matter, same body.
    pub same: bool,
    pub status: (Option<u16>, Option<u16>),
    pub status_changed: bool,
    /// Body length, and the difference between them.
    pub bytes: (u64, u64),
    pub length_delta: i64,
    /// 0.0 to 1.0 over the body. The number a blind-injection loop thresholds
    /// on, and the reason this verb is not just a printed diff: reading two
    /// HTML pages per candidate character through a model is the expensive way
    /// to answer "true page or false page".
    ///
    /// Only meaningful when [`Difference::bodies_compared`] is set.
    pub similarity: f64,
    /// Whether there were two bodies to compare at all.
    ///
    /// An unstored body reads as the empty string, so two of them compared as
    /// identical. Reachable on purpose: fill the store, or serve pages as
    /// `font/woff`, and every later comparison says "the same page".
    pub bodies_compared: bool,
    pub headers_added: Vec<String>,
    pub headers_removed: Vec<String>,
    pub headers_changed: Vec<String>,
    /// Changed body fields, when both bodies are JSON. Keyed by dotted path.
    pub json_changes: Vec<JsonChange>,
    /// How many there were, when the list above is only the first of them. A
    /// cap that says nothing is a partial diff shaped like a complete one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_changes_of: Option<usize>,
    /// Changed lines, when they are not.
    pub line_changes: Vec<LineChange>,
    /// How many there were. See [`Difference::json_changes_of`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_changes_of: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JsonChange {
    pub path: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LineChange {
    pub side: &'static str,
    pub line: usize,
    pub text: String,
}

/// Headers whose values differ on every response and mean nothing.
///
/// Comparing them makes every pair of responses "different", which is the same
/// as making the verb useless. Named rather than guessed at, so a header that
/// matters is never dropped for looking noisy.
fn header_is_noise(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "date" | "age" | "expires" | "last-modified" | "x-request-id" | "x-trace-id"
    )
}

/// How many lines to carry out of a body diff.
const MAX_LINE_CHANGES: usize = 60;

/// How many JSON fields to name.
const MAX_JSON_CHANGES: usize = 60;

/// Compare two stored responses.
pub fn compare(left: (&StoredResponse, &Text), right: (&StoredResponse, &Text)) -> Difference {
    let (a, a_body) = left;
    let (b, b_body) = right;

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let find = |set: &[(String, String)], name: &str| {
        set.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    for (name, value) in &b.headers {
        if header_is_noise(name) {
            continue;
        }
        match find(&a.headers, name) {
            None => added.push(name.clone()),
            Some(before) if &before != value => changed.push(name.clone()),
            Some(_) => {}
        }
    }
    for (name, _) in &a.headers {
        if !header_is_noise(name) && find(&b.headers, name).is_none() {
            removed.push(name.clone());
        }
    }

    // Two bodies, or none of this is a comparison. `Text::Missing` reads as the
    // empty string, which made "neither body was kept" indistinguishable from
    // "both bodies were empty".
    // Whole bodies, or none of this is a comparison: an unstored one reads as
    // empty and a previewed one as its own head.
    let bodies_compared = a_body.whole() && b_body.whole();
    let left_text = a_body.as_str();
    let right_text = b_body.as_str();
    let (json_changes, json_total, line_changes, line_total) =
        body_changes(a, b, left_text, right_text);

    // The bodies' own lengths, not the previews': `length_delta` read zero for
    // any two binary responses past the 64 KiB cap.
    let bytes = (
        a_body.len().unwrap_or_default(),
        b_body.len().unwrap_or_default(),
    );
    Difference {
        same: bodies_compared
            && a.status == b.status
            && added.is_empty()
            && removed.is_empty()
            && changed.is_empty()
            && left_text == right_text,
        status: (a.status, b.status),
        status_changed: a.status != b.status,
        bytes,
        length_delta: bytes.1 as i64 - bytes.0 as i64,
        // Zero rather than one, so a caller ignoring the flag looks again.
        similarity: if bodies_compared {
            similarity(left_text, right_text)
        } else {
            0.0
        },
        bodies_compared,
        headers_added: added,
        headers_removed: removed,
        headers_changed: changed,
        json_changes_of: (json_total > json_changes.len()).then_some(json_total),
        line_changes_of: (line_total > line_changes.len()).then_some(line_total),
        json_changes,
        line_changes,
    }
}

fn is_json(response: &StoredResponse) -> bool {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .is_some_and(|(_, value)| value.to_ascii_lowercase().contains("json"))
}

/// The JSON changes and the line changes, each with the number there were
/// before the cap took the rest.
fn body_changes(
    a: &StoredResponse,
    b: &StoredResponse,
    left: &str,
    right: &str,
) -> (Vec<JsonChange>, usize, Vec<LineChange>, usize) {
    // Both sides have to be JSON *and* parse. A body that claims JSON and is
    // not (a truncated answer, an error page served with the wrong type) falls
    // through to the line diff rather than reporting no changes at all.
    if is_json(a)
        && is_json(b)
        && let (Ok(left), Ok(right)) = (
            serde_json::from_str::<Value>(left),
            serde_json::from_str::<Value>(right),
        )
    {
        let mut changes = Vec::new();
        let mut total = 0usize;
        walk_json("", &left, &right, &mut changes, &mut total);
        return (changes, total, Vec::new(), 0);
    }
    let (lines, total) = line_changes(left, right);
    (Vec::new(), 0, lines, total)
}

/// Field-by-field, so a re-ordered object is not a difference.
/// Every difference between two documents, pushing at most [`MAX_JSON_CHANGES`]
/// and counting all of them in `total`. Stopping the walk at the cap made those
/// two numbers equal, so a capped diff could not say it had been capped.
fn walk_json(
    path: &str,
    left: &Value,
    right: &Value,
    out: &mut Vec<JsonChange>,
    total: &mut usize,
) {
    let note = |change: JsonChange, out: &mut Vec<JsonChange>, total: &mut usize| {
        *total += 1;
        if out.len() < MAX_JSON_CHANGES {
            out.push(change);
        }
    };
    let render = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for name in names {
                let next = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}.{name}")
                };
                match (a.get(name), b.get(name)) {
                    (Some(l), Some(r)) => walk_json(&next, l, r, out, total),
                    (Some(l), None) => note(
                        JsonChange { path: next, from: Some(render(l)), to: None },
                        out,
                        total,
                    ),
                    (None, Some(r)) => note(
                        JsonChange { path: next, from: None, to: Some(render(r)) },
                        out,
                        total,
                    ),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for index in 0..a.len().max(b.len()) {
                let next = format!("{path}.{index}");
                match (a.get(index), b.get(index)) {
                    (Some(l), Some(r)) => walk_json(&next, l, r, out, total),
                    (Some(l), None) => note(
                        JsonChange { path: next, from: Some(render(l)), to: None },
                        out,
                        total,
                    ),
                    (None, Some(r)) => note(
                        JsonChange { path: next, from: None, to: Some(render(r)) },
                        out,
                        total,
                    ),
                    (None, None) => {}
                }
            }
        }
        (l, r) if l != r => note(
            JsonChange {
                path: path.to_string(),
                from: Some(render(l)),
                to: Some(render(r)),
            },
            out,
            total,
        ),
        _ => {}
    }
}

/// Lines present on one side and not the other.
///
/// Set-based rather than a true longest-common-subsequence: what an agent asks
/// of a diff here is "what appeared and what vanished", and a page that moved a
/// line without changing it is not a finding. An LCS would also be O(n·m) over
/// two HTML documents, which is the wrong cost for a loop.
/// The changed lines, and how many there were before the cap.
///
/// Both sides are cut, not just the tail: additions came first and truncating
/// the end meant sixty new lines reported no removals at all.
fn line_changes(left: &str, right: &str) -> (Vec<LineChange>, usize) {
    let before: BTreeSet<&str> = left.lines().collect();
    let after: BTreeSet<&str> = right.lines().collect();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for (index, line) in right.lines().enumerate() {
        if !before.contains(line) {
            added.push(LineChange {
                side: "added",
                line: index + 1,
                text: line.chars().take(MAX_PREVIEW_LINE).collect(),
            });
        }
    }
    for (index, line) in left.lines().enumerate() {
        if !after.contains(line) {
            removed.push(LineChange {
                side: "removed",
                line: index + 1,
                text: line.chars().take(MAX_PREVIEW_LINE).collect(),
            });
        }
    }
    let total = added.len() + removed.len();
    if total > MAX_LINE_CHANGES {
        // Half each, with the shorter side's unused share going to the longer.
        let half = MAX_LINE_CHANGES / 2;
        let keep_added = if removed.len() < half {
            MAX_LINE_CHANGES - removed.len()
        } else {
            half.max(MAX_LINE_CHANGES - removed.len().min(MAX_LINE_CHANGES))
        };
        added.truncate(keep_added);
        removed.truncate(MAX_LINE_CHANGES - added.len());
    }
    added.extend(removed);
    (added, total)
}

/// How alike two bodies are, 0.0 to 1.0.
///
/// Token overlap (Jaccard over whitespace-separated tokens), which is cheap,
/// order-insensitive and good enough for the question it answers: is this the
/// same page with a different value in it, or a different page. Identical
/// bodies are 1.0 and two empty bodies are 1.0, because "nothing changed" is
/// the honest answer there.
pub fn similarity(left: &str, right: &str) -> f64 {
    if left == right {
        return 1.0;
    }
    let a: BTreeSet<&str> = left.split_whitespace().collect();
    let b: BTreeSet<&str> = right.split_whitespace().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let shared = a.intersection(&b).count() as f64;
    let total = a.union(&b).count() as f64;
    if total == 0.0 { 0.0 } else { shared / total }
}

/// `h5i browser diff <a> <b>`.
pub fn diff(
    root: &Path,
    selector: Option<&str>,
    left: u64,
    right: u64,
    json_out: bool,
) -> anyhow::Result<()> {
    let (session, dir) = store_dir(root, selector)?;
    let read = |seq: u64| -> anyhow::Result<(StoredResponse, Text)> {
        let stored: StoredResponse = read_json(&dir.join(format!("{seq}.response.json")))
            .map_err(|_| {
                anyhow::anyhow!(
                    "session {} has no stored response {seq}. It holds: {}",
                    session.id,
                    sequences(&dir)
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let body = body_text(&dir, &stored.body);
        Ok((stored, body))
    };
    let (a, a_body) = read(left)?;
    let (b, b_body) = read(right)?;
    let difference = compare((&a, &a_body), (&b, &b_body));

    // The division `match` makes: the status and headers are real and still
    // reported; only the exit code says the bodies could not be compared.
    let unanswerable = |seq: u64, body: &Text| match body {
        Text::Missing(reason) => Some(format!("{seq}'s body is not in the store ({reason})")),
        Text::Binary { bytes, .. } if !body.whole() => Some(format!(
            "{seq}'s body is {bytes} bytes of something that is not text, and only its \
             first {LOSSY_BODY_BYTES} are read back"
        )),
        _ => None,
    };
    let why: Vec<String> = [unanswerable(left, &a_body), unanswerable(right, &b_body)]
        .into_iter()
        .flatten()
        .collect();

    if json_out {
        println!("{}", serde_json::to_string_pretty(&difference)?);
        if !difference.bodies_compared {
            std::process::exit(EXIT_CANNOT_LOOK);
        }
        return Ok(());
    }

    if difference.same {
        println!("  {left} and {right} are the same response.");
        return Ok(());
    }
    if !difference.bodies_compared {
        println!("  bodies   : not compared. Response {}.", why.join("; and response "));
        println!(
            "             `message --body-to PATH` writes the exact bytes; a session \
             opened with `--capture` that has not run out of room keeps them whole."
        );
    }
    println!(
        "  status   : {} → {}",
        difference.status.0.map_or("none".to_string(), |s| s.to_string()),
        difference.status.1.map_or("none".to_string(), |s| s.to_string()),
    );
    println!(
        "  bytes    : {} → {} ({:+})",
        difference.bytes.0, difference.bytes.1, difference.length_delta
    );
    println!("  alike    : {:.3}", difference.similarity);
    for name in &difference.headers_added {
        println!("  + header : {}", preview_line(name));
    }
    for name in &difference.headers_removed {
        println!("  - header : {}", preview_line(name));
    }
    for name in &difference.headers_changed {
        println!("  ~ header : {}", preview_line(name));
    }
    for change in &difference.json_changes {
        let from = change.from.as_deref().unwrap_or("(absent)");
        let to = change.to.as_deref().unwrap_or("(absent)");
        println!(
            "  ~ {} : {} → {}",
            preview_line(&change.path),
            preview_line(from),
            preview_line(to)
        );
    }
    for change in &difference.line_changes {
        let mark = if change.side == "added" { '+' } else { '-' };
        println!("  {mark} {}", printable(&change.text));
    }
    // Said, not implied.
    if let Some(total) = difference.json_changes_of {
        println!(
            "  … {} more changed fields not listed",
            total - difference.json_changes.len()
        );
    }
    if let Some(total) = difference.line_changes_of {
        println!(
            "  … {} more changed lines not listed",
            total - difference.line_changes.len()
        );
    }
    if !difference.bodies_compared {
        std::process::exit(EXIT_CANNOT_LOOK);
    }
    Ok(())
}


/// The exit code a matcher answers with when nothing matched.
///
/// `grep`'s convention, not the sysexits scheme the rest of h5i uses for
/// session failures: 0 matched, 1 did not, 2 something went wrong. This verb is
/// a grep, it will be read in `if` and `&&` a thousand times more often than it
/// is read by a person, and a shell author already knows what 1 means from a
/// matcher. A distinct code is the whole point: "did not match" and "could not
/// look" must never be the same answer.
pub const EXIT_NO_MATCH: i32 = 1;

/// The exit code for a question that could not be asked at all.
///
/// A pattern that does not compile, a body that was never stored, a session
/// with no store. Distinct from [`EXIT_NO_MATCH`] on purpose and it is the
/// whole discipline of this verb: a loop that reads "did not match" when the
/// truth is "could not look" draws a conclusion from an absence, which is the
/// one mistake a workbench must not help anyone make.
pub const EXIT_CANNOT_LOOK: i32 = 2;

/// One thing a caller is asking about a response.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// A regular expression over the body.
    Regex(String),
    /// A literal substring of the body. The common case, and it needs no
    /// escaping, which matters when the thing being looked for is a payload.
    Contains(String),
    /// A dotted path into a JSON body, as `edits` spells them. Matches when the
    /// path exists; with a value, when it equals that value.
    Json { path: String, value: Option<String> },
    /// A header, by name, and optionally by value.
    Header { name: String, value: Option<String> },
    /// The status code.
    Status(u16),
    /// The body is longer than this many bytes.
    LongerThan(u64),
    /// ...or shorter.
    ShorterThan(u64),
}

/// What one condition found.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Found {
    pub kind: &'static str,
    pub expr: String,
    pub matched: bool,
    /// What the expression captured, when it captures. A regex hands back its
    /// groups, a JSON path its value, a header its value. This is the half of
    /// the verb that feeds the next request: an agent extracting a CSRF token
    /// is running a match and reading this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<String>,
    /// Whether this condition could be answered at all. `false` is never a
    /// "no", and `matches` turns it into the "could not look" exit.
    pub conclusive: bool,
}

fn evaluate(condition: &Condition, response: &StoredResponse, body: &Text) -> Found {
    let text = body.as_str();
    match condition {
        Condition::Regex(pattern) => match regex::Regex::new(pattern) {
            // A pattern that does not compile is not a response that does not
            // match. It comes back as an unmatched condition carrying the
            // parser's complaint, and `matches` turns that into an error exit
            // rather than a "no".
            Err(e) => Found {
                kind: "regex",
                expr: format!("{pattern} (not a regular expression: {e})"),
                matched: false,
                captures: Vec::new(),
                conclusive: false,
            },
            Ok(re) => {
                let found = re.captures(text);
                let hit = found.is_some();
                Found {
                    kind: "regex",
                    expr: pattern.clone(),
                    matched: hit,
                    // Groups when the pattern has them, the whole match when it
                    // does not. A pattern without a group is the ordinary way to
                    // ask "is this in there, and what was it", and handing back
                    // an empty list made the caller re-run the search itself.
                    // `extract_one` has always done this; the two must agree.
                    captures: found
                        .map(|caps| {
                            let groups: Vec<String> = caps
                                .iter()
                                .skip(1)
                                .flatten()
                                .map(|m| m.as_str().to_string())
                                .collect();
                            if groups.is_empty() {
                                caps.get(0)
                                    .map(|m| vec![m.as_str().to_string()])
                                    .unwrap_or_default()
                            } else {
                                groups
                            }
                        })
                        .unwrap_or_default(),
                    // A miss over part of a body is not a miss.
                    conclusive: hit || body.whole(),
                }
            }
        },
        Condition::Contains(needle) => {
            let matched = text.contains(needle.as_str());
            Found {
                kind: "contains",
                expr: needle.clone(),
                matched,
                captures: Vec::new(),
                conclusive: matched || body.whole(),
            }
        }
        Condition::Json { path, value } => {
            let found = serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|document| json_at(&document, path).cloned());
            let rendered = found.as_ref().map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            let matched = match (&rendered, value) {
                (Some(_), None) => true,
                (Some(have), Some(want)) => have == want,
                (None, _) => false,
            };
            Found {
                kind: "json",
                expr: match value {
                    Some(value) => format!("{path}={value}"),
                    None => path.clone(),
                },
                matched,
                captures: rendered.into_iter().collect(),
                conclusive: matched || body.whole(),
            }
        }
        Condition::Header { name, value } => {
            let have = response
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone());
            let matched = match (&have, value) {
                (Some(_), None) => true,
                (Some(have), Some(want)) => have == want,
                (None, _) => false,
            };
            Found {
                kind: "header",
                expr: match value {
                    Some(value) => format!("{name}={value}"),
                    None => name.clone(),
                },
                matched,
                captures: have.into_iter().collect(),
                // Headers are stored whole; only bodies are previewed.
                conclusive: true,
            }
        }
        Condition::Status(want) => Found {
            kind: "status",
            expr: want.to_string(),
            matched: response.status == Some(*want),
            captures: response.status.map(|s| s.to_string()).into_iter().collect(),
            conclusive: true,
        },
        // Off the body's own length, never the preview's. See [`Text::len`].
        Condition::LongerThan(bytes) => Found {
            kind: "longer-than",
            expr: bytes.to_string(),
            matched: body.len().is_some_and(|had| had > *bytes),
            captures: body.len().map(|had| had.to_string()).into_iter().collect(),
            conclusive: body.len().is_some(),
        },
        Condition::ShorterThan(bytes) => Found {
            kind: "shorter-than",
            expr: bytes.to_string(),
            matched: body.len().is_some_and(|had| had < *bytes),
            captures: body.len().map(|had| had.to_string()).into_iter().collect(),
            conclusive: body.len().is_some(),
        },
    }
}


/// `h5i browser match <seq>`.
///
/// Every condition has to hold. That is the useful default for the loop this
/// serves ("status 200 *and* the body has the flag"), and an `or` is a second
/// call in a shell that already has `||`.
pub fn matches(
    root: &Path,
    selector: Option<&str>,
    seq: u64,
    conditions: &[Condition],
    json_out: bool,
) -> anyhow::Result<()> {
    // The three answers are kept apart here rather than by the caller: `look`
    // says yes or no, and anything that stopped it from looking arrives as an
    // error and leaves by a different door.
    match look(root, selector, seq, conditions, json_out) {
        Ok(true) => Ok(()),
        Ok(false) => std::process::exit(EXIT_NO_MATCH),
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(EXIT_CANNOT_LOOK)
        }
    }
}

fn look(
    root: &Path,
    selector: Option<&str>,
    seq: u64,
    conditions: &[Condition],
    json_out: bool,
) -> anyhow::Result<bool> {
    if conditions.is_empty() {
        anyhow::bail!(
            "match needs something to look for: --regex, --contains, --json, --header, \
             --status, --longer-than or --shorter-than"
        );
    }
    let (session, dir) = store_dir(root, selector)?;
    let stored: StoredResponse =
        read_json(&dir.join(format!("{seq}.response.json"))).map_err(|_| {
            anyhow::anyhow!(
                "session {} has no stored response {seq}. It holds: {}",
                session.id,
                sequences(&dir)
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let body = body_text(&dir, &stored.body);

    // A body that was never stored cannot be matched against, and answering
    // "no" would be a claim about the response rather than about the store.
    if let Text::Missing(why) = &body
        && conditions.iter().any(|c| {
            matches!(
                c,
                Condition::Regex(_)
                    | Condition::Contains(_)
                    | Condition::Json { .. }
                    | Condition::LongerThan(_)
                    | Condition::ShorterThan(_)
            )
        })
    {
        anyhow::bail!(
            "response {seq}'s body is not in the store ({why}), so a body condition cannot \
             be answered. Header and status conditions still can"
        );
    }

    let found: Vec<Found> = conditions
        .iter()
        .map(|condition| evaluate(condition, &stored, &body))
        .collect();
    // Read off the condition rather than sniffed out of its rendered text.
    let could_not_look = found.iter().any(|f| !f.conclusive);
    let matched = found.iter().all(|f| f.matched);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "seq": seq,
                "matched": matched,
                "conditions": found,
            }))?
        );
    } else {
        for one in &found {
            println!(
                "  {} {} : {}",
                if one.matched { "✔" } else { "✘" },
                one.kind,
                one.expr
            );
            for capture in &one.captures {
                // The target chooses how much page sits between a pattern's
                // anchors. `--json` still carries it whole, for tokens.
                println!("      {}", preview_line(capture));
            }
        }
    }
    if could_not_look {
        anyhow::bail!(
            "a condition could not be answered, so this is not a `no`. A body that is not \
             text is read back as a preview of its first {LOSSY_BODY_BYTES} bytes, and a \
             search that found nothing in the preview has said nothing about the rest; \
             `message --body-to PATH` writes the exact bytes"
        );
    }
    Ok(matched)
}



/// The middle sample, and how far the samples sit from it.
///
/// Median and median absolute deviation rather than mean and standard
/// deviation, because one scheduling hiccup on a loaded machine moves a mean by
/// more than a blind injection's signal and moves a median not at all. The pair
/// answers the only question a timing test asks: is this run *reliably* slower
/// than that one, or did it just get unlucky once.
pub fn median_and_deviation(samples: &[u64]) -> Option<(u64, u64)> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let mut spread: Vec<u64> = sorted
        .iter()
        .map(|value| value.abs_diff(median))
        .collect();
    spread.sort_unstable();
    Some((median, spread[spread.len() / 2]))
}

/// Summarise a replay's samples for a person, and for a script.
///
/// The caveat is part of the answer rather than a footnote: a session inside a
/// box pays a proxy hop and a network namespace, so its absolute latency is not
/// the host's. Comparisons within one session are sound; comparisons across
/// placements are not.
pub fn timing_summary(samples: &[Value]) -> Option<Value> {
    if samples.len() < 2 {
        return None;
    }
    // Only the sends a server answered. One refused by policy or budget still
    // carries a clock, and it is the refusal's — near zero — which drags the
    // median down until a three-second delay reads as none.
    let answered: Vec<&Value> = samples
        .iter()
        .filter(|s| s.get("status").is_some_and(|status| !status.is_null()))
        .collect();
    let field = |name: &str| -> Vec<u64> {
        answered
            .iter()
            .filter_map(|s| s.get(name).and_then(Value::as_u64))
            .collect()
    };
    let unanswered = samples.len() - answered.len();
    if answered.len() < 2 {
        return Some(json!({
            "sends": samples.len(),
            "measured": answered.len(),
            "unanswered": unanswered,
            "note": "too few of these sends were answered to take a median. A send that \
                     never reached the wire has a clock, and it is the refusal's, not the \
                     server's",
        }));
    }
    let (ttfb, ttfb_spread) = median_and_deviation(&field("ttfb_ms"))?;
    let (total, total_spread) = median_and_deviation(&field("total_ms"))?;
    let mut summary = json!({
        "sends": samples.len(),
        "measured": answered.len(),
        "ttfb_ms": {"median": ttfb, "deviation": ttfb_spread},
        "total_ms": {"median": total, "deviation": total_spread},
        "note": "medians over the sends this session got an answer to. A session in a box \
                 pays a proxy hop and a namespace, so compare within one session rather \
                 than across placements",
    });
    if unanswered > 0 {
        summary["unanswered"] = json!(unanswered);
    }
    Some(summary)
}


// ── the site map ─────────────────────────────────────────────────────────────

/// One endpoint, as the session saw it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Endpoint {
    pub path: String,
    /// Methods seen, in the order first seen.
    pub methods: Vec<String>,
    /// Status codes seen.
    pub statuses: Vec<u16>,
    /// Query parameter *names* observed. Never values: a session id in a query
    /// string is still a session id, and a map is the kind of thing that gets
    /// pasted into a report.
    pub params: Vec<String>,
    /// How many requests this endpoint accounted for, both phases counted once.
    pub hits: usize,
    /// Whether anything reached it by navigation rather than as a subresource.
    pub navigated: bool,
}

/// One origin's endpoints.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Origin {
    pub origin: String,
    pub endpoints: Vec<Endpoint>,
}

/// What a session reached, and what it was refused.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Map {
    pub origins: Vec<Origin>,
    /// URLs the policy refused. Part of the map because "this session tried to
    /// reach that and was not allowed" is a fact about the application, not
    /// just about the session.
    pub denied: Vec<String>,
}

/// Fold a request log into a map.
///
/// Only what was *reached*. A URL scraped out of a JavaScript bundle was not
/// visited, and a map that blurred the two would answer "what did this session
/// reach" with a guess, which is the one question the receipts exist to answer
/// exactly. Disclosed-but-unvisited candidates belong in a separate verb that
/// says so, and that verb is not built.
pub fn map_of(records: &[Value]) -> Map {
    use std::collections::BTreeMap;
    let mut origins: BTreeMap<String, BTreeMap<String, Endpoint>> = BTreeMap::new();
    let mut denied: Vec<String> = Vec::new();

    for record in records {
        let url = record.get("url").and_then(Value::as_str).unwrap_or_default();
        let Ok(parsed) = url::Url::parse(url) else {
            continue;
        };
        let phase = record.get("phase").and_then(Value::as_str).unwrap_or("");
        if record.get("allowed").and_then(Value::as_bool) == Some(false) {
            if phase == "request" && !denied.contains(&url.to_string()) {
                denied.push(url.to_string());
            }
            continue;
        }

        let origin = match parsed.host_str() {
            Some(host) => match parsed.port() {
                Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
                None => format!("{}://{host}", parsed.scheme()),
            },
            None => parsed.scheme().to_string(),
        };
        let slot = origins.entry(origin).or_default();
        let endpoint = slot.entry(parsed.path().to_string()).or_insert(Endpoint {
            path: parsed.path().to_string(),
            methods: Vec::new(),
            statuses: Vec::new(),
            params: Vec::new(),
            hits: 0,
            navigated: false,
        });

        if phase == "request" {
            endpoint.hits += 1;
            if let Some(method) = record.get("method").and_then(Value::as_str)
                && !endpoint.methods.iter().any(|m| m == method)
            {
                endpoint.methods.push(method.to_string());
            }
            if record.get("initiator").and_then(Value::as_str) == Some("navigation") {
                endpoint.navigated = true;
            }
            for (name, _) in parsed.query_pairs() {
                let name = name.into_owned();
                if !endpoint.params.contains(&name) {
                    endpoint.params.push(name);
                }
            }
        } else if let Some(status) = record.get("status").and_then(Value::as_u64) {
            let status = status as u16;
            if !endpoint.statuses.contains(&status) {
                endpoint.statuses.push(status);
            }
        }
    }

    Map {
        origins: origins
            .into_iter()
            .map(|(origin, endpoints)| Origin {
                origin,
                endpoints: endpoints.into_values().collect(),
            })
            .collect(),
        denied,
    }
}

/// `h5i browser sitemap`.
pub fn sitemap(root: &Path, selector: Option<&str>, json_out: bool) -> anyhow::Result<()> {
    // From the engine while there is one, and off the log once there is not:
    // the map of a finished run is the one a reviewer wants, and each line of
    // that log is already the record `map_of` reads.
    let session = super::browser::resolve_for_reading(root, selector)?;
    let owned: Vec<Value>;
    let answer;
    let records: &[Value] = if session.state.is_live() {
        answer = super::browser::ask_session(
            root,
            selector,
            vec!["requests".to_string()],
            false,
        )?;
        answer
            .get("requests")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    } else {
        let path = bs::dir(root, &session.id).join(bs::RECEIPTS_FILE);
        let (text, cut) = bs::read_log_capped_saying(&path).unwrap_or_default();
        if cut {
            // A map built from the head of a log covers part of the run.
            eprintln!(
                "  note     : this session's request log is larger than the {} bytes read \
                 back, so this map covers the start of the run and not all of it",
                8 * 1024 * 1024
            );
        }
        owned = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect();
        &owned
    };
    let map = map_of(records);

    if json_out {
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(());
    }
    if map.origins.is_empty() && map.denied.is_empty() {
        println!("  this session has reached nothing yet");
        return Ok(());
    }
    for origin in &map.origins {
        println!("  {}", origin.origin);
        for endpoint in &origin.endpoints {
            let methods = endpoint.methods.join(",");
            let statuses = endpoint
                .statuses
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let params = if endpoint.params.is_empty() {
                String::new()
            } else {
                format!("  ?{}", preview_line(&endpoint.params.join("&")))
            };
            let mark = if endpoint.navigated { "*" } else { " " };
            println!(
                "  {mark} {:<32} {:<8} {:<12} x{}{params}",
                preview_line(&endpoint.path),
                methods,
                statuses,
                endpoint.hits
            );
        }
    }
    if !map.denied.is_empty() {
        println!();
        println!("  refused by policy:");
        for url in &map.denied {
            println!("    {}", preview_line(url));
        }
    }
    Ok(())
}

// ── sequences ────────────────────────────────────────────────────────────────

/// One request to send, as the caller wants it sent.
///
/// A struct rather than eight arguments, for the reason
/// `h5i_browser::capture::Response` is one: what goes out is exactly this,
/// named in one place, and nobody can pass a create flag where a
/// keep-credentials flag was meant.
#[derive(Debug, Clone, Copy)]
pub struct Sending<'a> {
    /// The stored request to send again.
    pub from: u64,
    pub set: &'a [String],
    pub unset: &'a [String],
    pub create: bool,
    /// Send it from this session instead, carrying only what is not a
    /// credential. See `header_is_the_users`.
    pub as_session: Option<&'a str>,
    /// Carry the source session's credentials across anyway.
    pub keep_credentials: bool,
}

/// One step of a sequence, as written in the file.
///
/// A step is a `resend` plus what to pull out of its answer. The page verbs are
/// deliberately not here: a sequence is an HTTP-level thing, and a flow that
/// needs a click to happen first should drive the browser to that point and then
/// start the sequence from the request it produced. Mixing the two would make
/// the file a second scripting language beside `browser script`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Step {
    /// The stored request to send again.
    pub resend: u64,
    /// Edits, as `target=value` with `${name}` for anything bound earlier.
    #[serde(default)]
    pub set: Vec<String>,
    /// Targets to remove.
    #[serde(default)]
    pub unset: Vec<String>,
    /// Send it from another session, as `resend --as` does.
    #[serde(default, rename = "as")]
    pub as_session: Option<String>,
    /// Add targets that are not there.
    #[serde(default)]
    pub create: bool,
    /// What to pull out of the answer, by name.
    ///
    /// `"csrf": "regex:name=\"csrf\" value=\"([^\"]+)\""`, or `json:`, or
    /// `header:`, or `status`. A binding that does not resolve stops the
    /// sequence, because a step acting on a token the step before it failed to
    /// produce is acting somewhere the sequence never described.
    #[serde(default)]
    pub extract: std::collections::BTreeMap<String, String>,
    /// A human-readable name for the step, for the report.
    #[serde(default)]
    pub name: Option<String>,
}

/// A sequence file.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Sequence {
    pub steps: Vec<Step>,
}

/// What one step did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Ran {
    pub step: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub resend: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u64>,
    /// What this step bound, for the steps after it.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bound: std::collections::BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Replace `${name}` with what an earlier step bound.
///
/// An unbound name is an error rather than an empty string. A request that goes
/// out with `X-CSRF-Token: ` instead of a token gets a 403 that looks exactly
/// like the finding somebody is hunting for, which is the worst way for this to
/// fail.
fn substitute(
    text: &str,
    bound: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            anyhow::bail!("`{text}` opens `${{` and never closes it");
        };
        let name = &after[..end];
        match bound.get(name) {
            Some(value) => out.push_str(value),
            None => anyhow::bail!(
                "`{text}` uses ${{{name}}}, which no earlier step bound. \
                 Bound so far: {}",
                if bound.is_empty() {
                    "nothing".to_string()
                } else {
                    bound.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            ),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Pull one value out of a step's answer.
fn extract_one(spec: &str, response: &StoredResponse, body: &Text) -> anyhow::Result<String> {
    let (kind, rest) = spec.split_once(':').unwrap_or((spec, ""));
    let found = match kind.trim() {
        "regex" => {
            let re = regex::Regex::new(rest)
                .map_err(|e| anyhow::anyhow!("`{rest}` is not a regular expression: {e}"))?;
            re.captures(body.as_str()).and_then(|caps| {
                // The first group, or the whole match when the pattern has no
                // group. A pattern with a group nearly always means "this bit".
                caps.get(1).or_else(|| caps.get(0)).map(|m| m.as_str().to_string())
            })
        }
        "json" => serde_json::from_str::<Value>(body.as_str())
            .ok()
            .and_then(|document| json_at(&document, rest).cloned())
            .map(|value| match value {
                Value::String(s) => s,
                other => other.to_string(),
            }),
        "header" => response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(rest.trim()))
            .map(|(_, value)| value.clone()),
        "status" => response.status.map(|s| s.to_string()),
        other => anyhow::bail!(
            "`{other}` is not an extractor. Use regex:, json:, header: or status"
        ),
    };
    found.ok_or_else(|| {
        // "Found nothing" is a claim about the response, and only true when
        // the whole response was there to look at.
        let searched_the_body = matches!(kind.trim(), "regex" | "json");
        if searched_the_body && !body.whole() {
            match body {
                Text::Missing(why) => anyhow::anyhow!(
                    "`{spec}` could not be answered: this response's body is not in the \
                     store ({why})"
                ),
                _ => anyhow::anyhow!(
                    "`{spec}` found nothing in the first {LOSSY_BODY_BYTES} bytes of this \
                     response, which is all of a body that is not text that is read back. \
                     Whether it is in the rest is not something this can answer; \
                     `message --body-to PATH` writes the exact bytes"
                ),
            }
        } else {
            anyhow::anyhow!("`{spec}` found nothing in this response")
        }
    })
}

/// Why a step failed, out of wherever the reply put it: a refusal carries
/// `code` and `message`, a send that failed at the wire puts it under
/// `response.error`. A sequence stops at the first failure, so this string is
/// all its author gets.
fn why_a_step_failed(answer: &Value) -> String {
    let text = |value: &Value| value.as_str().map(str::to_string);
    let refusal = answer.get("message").and_then(text).map(|message| {
        match answer.get("code").and_then(text) {
            Some(code) => format!("{code}: {message}"),
            None => message,
        }
    });
    refusal
        .or_else(|| {
            answer
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(text)
        })
        .unwrap_or_else(|| "the step failed, and the reply said nothing about why".to_string())
}

/// `h5i browser sequence <file>`.
///
/// Stops at the first failure. A sequence is a chain, and a step that runs after
/// the one before it failed is acting on a state the file never described: the
/// login that did not happen, the token that was never issued. `--keep-going`
/// exists for reading a whole file's worth of failures at once and is not the
/// default for the same reason `browser replay` does not continue by default.
pub fn sequence(
    root: &Path,
    selector: Option<&str>,
    file: &Path,
    vars: &[(String, String)],
    keep_going: bool,
    json_out: bool,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("{} could not be read: {e}", file.display()))?;
    let plan: Sequence = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} is not a sequence file: {e}", file.display()))?;
    if plan.steps.is_empty() {
        anyhow::bail!("{} has no steps", file.display());
    }

    let mut bound: std::collections::BTreeMap<String, String> =
        vars.iter().cloned().collect();
    let mut ran: Vec<Ran> = Vec::new();
    let mut failed = false;

    for (index, step) in plan.steps.iter().enumerate() {
        let mut record = Ran {
            step: index,
            name: step.name.clone(),
            resend: step.resend,
            ok: false,
            seq: None,
            status: None,
            bound: Default::default(),
            error: None,
        };

        let mut sets: Vec<String> = Vec::with_capacity(step.set.len());
        let mut bad: Option<String> = None;
        for spec in &step.set {
            match substitute(spec, &bound) {
                Ok(spec) => sets.push(spec),
                Err(e) => {
                    bad = Some(e.to_string());
                    break;
                }
            }
        }

        if let Some(why) = bad {
            record.error = Some(why);
            ran.push(record);
            failed = true;
            if !keep_going {
                break;
            }
            continue;
        }

        // Through the same command a person would type, so a sequence cannot
        // reach a session by a path a typed verb could not.
        let answer = super::browser::resend_step(
            root,
            selector,
            &Sending {
                from: step.resend,
                set: &sets,
                unset: &step.unset,
                create: step.create,
                as_session: step.as_session.as_deref(),
                keep_credentials: false,
            },
        )?;
        let ok = answer.get("ok").and_then(Value::as_bool).unwrap_or(false);
        record.ok = ok;
        record.seq = answer.get("seq").and_then(Value::as_u64);
        record.status = answer
            .get("response")
            .and_then(|r| r.get("status"))
            .and_then(Value::as_u64);
        if !ok {
            record.error = Some(why_a_step_failed(&answer));
            ran.push(record);
            failed = true;
            if !keep_going {
                break;
            }
            continue;
        }

        // The bindings, read from what this step stored rather than from the
        // reply: the reply carries the status and the headers, and an extractor
        // usually wants the body.
        if !step.extract.is_empty() {
            let target = step.as_session.as_deref().or(selector);
            let (_, dir) = store_dir(root, target)?;
            // The step's own answer, or nothing. Defaulting to zero bound the
            // token out of message 0 — the navigation that opened the session.
            let Some(seq) = record.seq else {
                record.error = Some(format!(
                    "step {index} came back without a sequence number, so there is no \
                     answer of its own to extract from"
                ));
                record.ok = false;
                failed = true;
                ran.push(record);
                if !keep_going {
                    break;
                }
                continue;
            };
            let stored: StoredResponse =
                read_json(&dir.join(format!("{seq}.response.json"))).map_err(|_| {
                    anyhow::anyhow!("step {index} left no stored response {seq} to extract from")
                })?;
            let body = body_text(&dir, &stored.body);
            for (name, spec) in &step.extract {
                match extract_one(spec, &stored, &body) {
                    Ok(value) => {
                        record.bound.insert(name.clone(), value.clone());
                        bound.insert(name.clone(), value);
                    }
                    Err(e) => {
                        record.error = Some(e.to_string());
                        record.ok = false;
                        failed = true;
                        break;
                    }
                }
            }
        }
        let stop = !record.ok && !keep_going;
        ran.push(record);
        if stop {
            break;
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": !failed,
                "ran": ran.len(),
                "of": plan.steps.len(),
                "steps": ran,
            }))?
        );
    } else {
        for step in &ran {
            let label = step.name.clone().unwrap_or_else(|| format!("resend {}", step.resend));
            match (&step.error, step.status) {
                (Some(why), _) => println!("  ✘ {label}: {}", preview_line(why)),
                (None, Some(status)) => {
                    let bound = if step.bound.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " · bound {}",
                            step.bound.keys().cloned().collect::<Vec<_>>().join(", ")
                        )
                    };
                    println!("  ✔ {label}: {status}{bound}");
                }
                (None, None) => println!("  ✔ {label}"),
            }
        }
        if failed {
            println!("  stopped after {} of {} steps", ran.len(), plan.steps.len());
        }
    }
    if failed {
        std::process::exit(EXIT_CANNOT_LOOK);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boxed session's store is writable by boxed code, so the hash in a
    /// sidecar is target input: joined unchecked, `../` read a host file.
    #[test]
    fn a_body_hash_that_is_not_one_names_nothing_in_the_store() {
        let dir = std::env::temp_dir().join(format!(
            "h5i-websec-hash-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("bodies")).expect("a store");
        std::fs::write(dir.join("outside.txt"), b"host secret").expect("a file beside it");
        let escaped = Body::Stored {
            sha256: "../outside.txt".to_string(),
            bytes: 11,
            of_bytes: None,
            truncated: false,
        };
        match body_text(&dir, &escaped) {
            Text::Missing(why) => assert!(why.contains("not a body hash"), "{why}"),
            read => panic!("read outside the store: {read:?}"),
        }
        assert_eq!(body_bytes(&dir, &escaped), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The in-session resend refuses a body the store cut short; `--as` read
    /// the same store and did not, so it sent a shortened body as a replay.
    #[test]
    fn a_truncated_body_is_not_carried_into_another_session() {
        let dir = std::env::temp_dir().join(format!(
            "h5i-websec-cut-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("bodies")).expect("a store");
        let hash = "a".repeat(64);
        std::fs::write(dir.join("bodies").join(&hash), b"the head of it").expect("a body");
        let cut = Body::Stored {
            sha256: hash,
            bytes: 14,
            of_bytes: Some(9_000_000),
            truncated: true,
        };
        let refused = carried_body(&dir, 42, &cut).expect_err("a cut body is not replayable");
        assert!(refused.to_string().contains("not the one recorded"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The human view prints strings the target wrote. `\r` alone rewrites the
    /// line naming which request this was; `ESC[2J` clears what came before it.
    #[test]
    fn what_a_terminal_would_act_on_is_shown_rather_than_obeyed() {
        let hostile = "ok\u{1b}[2J\rHTTP/1.1 200 forged\u{202e}";
        let safe = printable(hostile);
        assert!(!safe.contains('\u{1b}'), "{safe:?}");
        assert!(!safe.contains('\r'), "{safe:?}");
        assert!(!safe.contains('\u{202e}'), "{safe:?}");
        assert!(safe.contains("forged"), "the evidence survives: {safe:?}");
        assert!(safe.contains("u{1b}"), "and says what was there: {safe:?}");
        assert_eq!(printable("ordinary text"), "ordinary text");
    }

    /// An unstored body reads as the empty string, so two of them compared as
    /// identical. A target reaches that state on purpose by filling the store
    /// or serving `font/woff`, and the oracle then says "false page" forever.
    #[test]
    fn two_bodies_that_were_never_kept_are_not_the_same_page() {
        let left = response(200, "text/html");
        let right = response(200, "text/html");
        let absent = Text::Missing("store-full (2000000 bytes)".to_string());
        let difference = compare((&left, &absent), (&right, &absent));
        assert!(!difference.bodies_compared, "there was nothing to compare");
        assert!(!difference.same, "an absence is not a match");
        assert!(
            difference.similarity < 0.5,
            "a number nobody could measure is not 1.0: {}",
            difference.similarity
        );
    }

    /// An empty body is a real body, and two of them are still the same page.
    #[test]
    fn two_empty_bodies_are_still_compared() {
        let left = response(204, "text/html");
        let right = response(204, "text/html");
        let empty = Text::Utf8(String::new());
        let difference = compare((&left, &empty), (&right, &empty));
        assert!(difference.bodies_compared);
        assert!(difference.same);
    }

    /// One invalid byte puts a response on the 64 KiB preview path, so a
    /// length read off the preview is a number the target picked.
    #[test]
    fn a_length_condition_measures_the_body_and_not_its_preview() {
        let stored = response(200, "application/octet-stream");
        let big = Text::Binary {
            bytes: 5_000_000,
            sha256: "b".repeat(64),
            text: "x".repeat(64 * 1024),
        };
        let shorter = evaluate(&Condition::ShorterThan(100_000), &stored, &big);
        assert!(!shorter.matched, "5 MB is not shorter than 100 kB");
        assert_eq!(shorter.captures, vec!["5000000".to_string()]);
        let longer = evaluate(&Condition::LongerThan(1_000_000), &stored, &big);
        assert!(longer.matched, "and it is longer than 1 MB");
    }

    /// `length_delta` is a headline field, and two binary responses past the
    /// cap both measured 64 KiB: no change at all.
    #[test]
    fn a_length_delta_is_over_the_bodies_not_the_previews() {
        let stored = response(200, "application/octet-stream");
        let preview = "x".repeat(64 * 1024);
        let small = Text::Binary {
            bytes: 1_000_000,
            sha256: "a".repeat(64),
            text: preview.clone(),
        };
        let large = Text::Binary {
            bytes: 5_000_000,
            sha256: "b".repeat(64),
            text: preview,
        };
        let difference = compare((&stored, &small), (&stored, &large));
        assert_eq!(difference.bytes, (1_000_000, 5_000_000));
        assert_eq!(difference.length_delta, 4_000_000);
    }

    /// A search over a preview that finds nothing has said nothing about the
    /// rest, and one invalid byte puts every body search on that path.
    #[test]
    fn a_miss_over_part_of_a_body_is_not_a_miss() {
        let stored = response(200, "application/octet-stream");
        let partial = Text::Binary {
            bytes: 5_000_000,
            sha256: "b".repeat(64),
            text: "nothing interesting".to_string(),
        };
        for condition in [
            Condition::Contains("FLAG{".to_string()),
            Condition::Regex("FLAG\\{.*\\}".to_string()),
        ] {
            let found = evaluate(&condition, &stored, &partial);
            assert!(!found.matched);
            assert!(!found.conclusive, "{:?} claimed to be a real no", found.kind);
        }
        // A hit is still a hit: finding it in the head proves it is there.
        let hit = Text::Binary {
            bytes: 5_000_000,
            sha256: "b".repeat(64),
            text: "FLAG{here}".to_string(),
        };
        let found = evaluate(&Condition::Contains("FLAG{".to_string()), &stored, &hit);
        assert!(found.matched && found.conclusive);
    }

    /// And a body small enough to be previewed whole answers for real.
    #[test]
    fn a_miss_over_a_whole_body_is_a_miss() {
        let stored = response(200, "application/octet-stream");
        let whole = Text::Binary {
            bytes: 19,
            sha256: "b".repeat(64),
            text: "nothing interesting".to_string(),
        };
        let found = evaluate(&Condition::Contains("FLAG{".to_string()), &stored, &whole);
        assert!(!found.matched && found.conclusive);
    }

    /// A cap that says nothing turns a partial diff into a complete-looking
    /// one — and additions came first, so sixty new lines hid every removal.
    #[test]
    fn a_capped_diff_says_how_much_it_left_out_and_keeps_both_sides() {
        let stored = response(200, "text/html");
        let left = Text::Utf8((0..100).map(|n| format!("old line {n}\n")).collect());
        let right = Text::Utf8((0..100).map(|n| format!("new line {n}\n")).collect());
        let difference = compare((&stored, &left), (&stored, &right));

        assert_eq!(difference.line_changes.len(), MAX_LINE_CHANGES);
        assert_eq!(difference.line_changes_of, Some(200));
        assert!(
            difference.line_changes.iter().any(|c| c.side == "added"),
            "the additions survive the cap"
        );
        assert!(
            difference.line_changes.iter().any(|c| c.side == "removed"),
            "and so do the removals"
        );
    }

    /// A diff that fits says nothing about a cap, because there was none.
    #[test]
    fn a_small_diff_carries_no_truncation_note() {
        let stored = response(200, "text/html");
        let difference = compare(
            (&stored, &Text::Utf8("a\nb\n".to_string())),
            (&stored, &Text::Utf8("a\nc\n".to_string())),
        );
        assert_eq!(difference.line_changes_of, None);
        assert_eq!(difference.line_changes.len(), 2);
    }

    /// The JSON walk stopped at the cap, so the count and the listed count
    /// were equal and the diff could not say it had been capped.
    #[test]
    fn a_capped_json_diff_counts_the_changes_it_did_not_list() {
        let stored = response(200, "application/json");
        let left: String = format!(
            "{{{}}}",
            (0..200)
                .map(|n| format!("\"k{n}\":\"a\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let right: String = format!(
            "{{{}}}",
            (0..200)
                .map(|n| format!("\"k{n}\":\"b\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let difference = compare(
            (&stored, &Text::Utf8(left)),
            (&stored, &Text::Utf8(right)),
        );
        assert_eq!(difference.json_changes.len(), MAX_JSON_CHANGES);
        assert_eq!(difference.json_changes_of, Some(200));
    }

    /// A failed binding stops the sequence, so its reason is what the author
    /// acts on: "found nothing" sent them to the target when the truth was
    /// that the body was never searched.
    #[test]
    fn a_binding_that_could_not_look_says_so_rather_than_saying_it_is_not_there() {
        let stored = response(200, "application/octet-stream");

        let absent = Text::Missing("store-full (900000 bytes)".to_string());
        let why = extract_one("regex:FLAG\\{(.+)\\}", &stored, &absent)
            .expect_err("nothing to search");
        assert!(why.to_string().contains("not in the store"), "{why}");

        let partial = Text::Binary {
            bytes: 5_000_000,
            sha256: "b".repeat(64),
            text: "nothing here".to_string(),
        };
        let why = extract_one("regex:FLAG\\{(.+)\\}", &stored, &partial)
            .expect_err("only the head was searched");
        assert!(why.to_string().contains("is all of a body"), "{why}");

        // And a body that really was searched whole still says it plainly.
        let whole = Text::Utf8("nothing here".to_string());
        let why = extract_one("regex:FLAG\\{(.+)\\}", &stored, &whole)
            .expect_err("searched, and not there");
        assert!(why.to_string().contains("found nothing in this response"), "{why}");

        // A header extractor is not a body search, so a missing body does not
        // change what it can say.
        let why = extract_one("header:X-Nope", &stored, &absent).expect_err("no such header");
        assert!(why.to_string().contains("found nothing in this response"), "{why}");
    }

    /// A cut at the store's 8 MiB cap lands on a character boundary — every
    /// cut, for ASCII — and still decodes, so the head arrived as ordinary
    /// text and every verb treated it as the whole body.
    #[test]
    fn a_body_the_store_cut_is_not_a_whole_body() {
        let cut = Text::Cut {
            text: "the first eight megabytes".to_string(),
            of_bytes: 20_000_000,
        };
        assert!(!cut.whole());
        assert_eq!(cut.len(), Some(20_000_000), "the body's length, not the head's");

        let stored = response(200, "text/html");
        let found = evaluate(&Condition::Contains("FLAG{".to_string()), &stored, &cut);
        assert!(!found.matched);
        assert!(!found.conclusive, "a search over the head is not a no");

        let difference = compare((&stored, &cut), (&stored, &cut));
        assert!(
            !difference.bodies_compared,
            "two heads matching is not two pages matching"
        );
        assert!(!difference.same);
    }

    /// The line count was bounded and the length was not, so a minified page —
    /// one line, megabytes of it — printed whole. The target picks the newlines.
    #[test]
    fn one_enormous_line_is_bounded_like_every_other() {
        let shown = preview_line(&"A".repeat(2_000_000));
        assert!(
            shown.chars().count() < MAX_PREVIEW_LINE + 80,
            "a line is bounded: {} characters",
            shown.chars().count()
        );
        assert!(shown.contains("more characters on this line"), "and says so");

        // A line that fits is untouched, and still inert.
        assert_eq!(preview_line("<p>hello</p>"), "<p>hello</p>");
        assert!(!preview_line("a\u{1b}[2Jb").contains('\u{1b}'));
    }

    /// The same bound wherever target text reaches a terminal. The target
    /// chooses how much page sits between a pattern's anchors.
    #[test]
    fn a_capture_over_a_whole_page_is_bounded_in_the_human_view() {
        let stored = response(200, "text/html");
        let page = format!("flag={}", "A".repeat(2_000_000));
        let body = Text::Utf8(page);
        let found = evaluate(&Condition::Regex("flag=(.*)".to_string()), &stored, &body);
        assert!(found.matched);
        // Whole in the machine channel, which is where a token is read from.
        assert_eq!(found.captures[0].len(), 2_000_000);
        // Bounded on the way to a terminal.
        assert!(preview_line(&found.captures[0]).chars().count() < MAX_PREVIEW_LINE + 80);
    }

    /// The site map's `*` means the browser went here. Replays were recorded
    /// as navigations, which is the mark a reviewer reads to tell what the
    /// application did from what the tester did.
    #[test]
    fn a_replay_is_not_a_page_somebody_went_to() {
        let map = map_of(&[
            json!({"seq":0,"phase":"request","initiator":"navigation","method":"GET",
                   "url":"https://app.test/users","allowed":true}),
            json!({"seq":1,"phase":"request","initiator":"replay","method":"GET",
                   "url":"https://app.test/admin","allowed":true}),
        ]);
        let endpoints = &map.origins[0].endpoints;
        let users = endpoints.iter().find(|e| e.path == "/users").expect("users");
        let admin = endpoints.iter().find(|e| e.path == "/admin").expect("admin");
        assert!(users.navigated, "the browser did go here");
        assert!(!admin.navigated, "and it never went here: a replay is not a visit");
        assert_eq!(admin.hits, 1, "but it is still on the map");
    }

    /// A step that failed at the wire carries no `message`, so reading only
    /// that reported "the step failed" while the reply said why.
    #[test]
    fn a_failed_step_reports_the_reason_the_reply_carried() {
        assert_eq!(
            why_a_step_failed(&json!({
                "ok": false,
                "code": "bad-edit",
                "message": "path: a path has no query in it"
            })),
            "bad-edit: path: a path has no query in it"
        );
        assert_eq!(
            why_a_step_failed(&json!({
                "ok": false,
                "response": {"error": "error sending request for url (http://a.test/)"}
            })),
            "error sending request for url (http://a.test/)"
        );
        assert!(why_a_step_failed(&json!({"ok": false})).contains("said nothing about why"));
    }

    fn response(status: u16, kind: &str) -> StoredResponse {
        StoredResponse {
            seq: 0,
            at: "2026-09-02T00:00:00.000000Z".to_string(),
            url: "https://app.test/api".to_string(),
            status: Some(status),
            headers: vec![
                ("content-type".to_string(), kind.to_string()),
                ("date".to_string(), "whenever".to_string()),
            ],
            content_encoding: None,
            wire_bytes: None,
            body: Body::Empty,
            trailing: None,
        }
    }

    #[test]
    fn one_changed_json_field_is_named_and_the_rest_is_not() {
        let a = response(200, "application/json");
        let b = response(200, "application/json");
        let left = Text::Utf8(r#"{"id":1,"name":"alice","role":"user"}"#.to_string());
        let right = Text::Utf8(r#"{"id":2,"name":"alice","role":"user"}"#.to_string());
        let difference = compare((&a, &left), (&b, &right));
        assert!(!difference.same);
        assert_eq!(difference.json_changes.len(), 1);
        assert_eq!(difference.json_changes[0].path, "id");
        assert_eq!(difference.json_changes[0].from.as_deref(), Some("1"));
        assert_eq!(difference.json_changes[0].to.as_deref(), Some("2"));
    }

    /// The header that changes on every response must not make every pair of
    /// responses differ, or the verb answers "different" always and says
    /// nothing.
    #[test]
    fn a_clock_header_is_not_a_difference() {
        let mut a = response(200, "text/html");
        let mut b = response(200, "text/html");
        a.headers[1].1 = "Mon, 01 Jan 2026 00:00:00 GMT".to_string();
        b.headers[1].1 = "Tue, 02 Jan 2026 00:00:00 GMT".to_string();
        let body = Text::Utf8("<p>hello</p>".to_string());
        let difference = compare((&a, &body), (&b, &body));
        assert!(difference.same, "{difference:?}");
    }

    #[test]
    fn a_status_change_is_the_headline() {
        let a = response(200, "text/html");
        let b = response(403, "text/html");
        let body = Text::Utf8("<p>hello</p>".to_string());
        let difference = compare((&a, &body), (&b, &body));
        assert!(difference.status_changed);
        assert_eq!(difference.status, (Some(200), Some(403)));
        assert!(!difference.same);
    }

    /// The number a blind-injection loop reads instead of the body.
    ///
    /// The property that matters is the *ordering*, not any particular value:
    /// the same page scores above a page with one value changed, which scores
    /// above a different page. Thresholds belong to the caller, who knows how
    /// long its pages are, because this is Jaccard over unique tokens and a
    /// short body moves a long way per word.
    #[test]
    fn similarity_orders_the_same_page_above_a_changed_one_above_a_different_one() {
        let page = "<html><body><h1>Welcome back</h1><p>You have 3 new messages</p></body></html>";
        let changed = "<html><body><h1>Welcome back</h1><p>You have 4 new messages</p></body></html>";
        let other = "<html><body><h1>Login required</h1><p>Please sign in</p></body></html>";

        assert_eq!(similarity(page, page), 1.0, "identical is exactly 1.0");
        let near = similarity(page, changed);
        let far = similarity(page, other);
        assert!(near > far, "one value changed ({near}) must read closer than another page ({far})");
        assert!(near > 0.6, "the true/false pair of a blind test stays recognisable: {near}");
        assert!(far < 0.4, "a different page is plainly different: {far}");
    }

    /// A short body is sharper than a long one, and a caller thresholding on
    /// this number should know that rather than discover it.
    #[test]
    fn a_short_body_moves_further_per_word() {
        let short = similarity("the quick brown fox", "the quick brown cat");
        let long = similarity(
            "the quick brown fox jumps over the lazy dog again and again and again",
            "the quick brown cat jumps over the lazy dog again and again and again",
        );
        assert!(short < long, "short {short} should be further from 1.0 than long {long}");
    }

    #[test]
    fn a_body_that_is_not_text_is_reported_rather_than_mangled() {
        let a = response(200, "image/png");
        let body = Text::Binary {
            bytes: 12,
            sha256: "beef".to_string(),
            text: "\u{fffd}\u{fffd}png".to_string(),
        };
        let difference = compare((&a, &body), (&a, &body));
        assert!(difference.same, "identical binary bodies are identical");
        assert!(difference.line_changes.is_empty());
    }

    fn json_response(body: &str) -> (StoredResponse, Text) {
        (
            response(200, "application/json"),
            Text::Utf8(body.to_string()),
        )
    }

    #[test]
    fn a_regex_hands_back_what_it_captured() {
        let (stored, body) = json_response(r#"{"csrf":"tok_9f8e","user":"alice"}"#);
        let found = evaluate(
            &Condition::Regex("\"csrf\":\"([a-z0-9_]+)\"".to_string()),
            &stored,
            &body,
        );
        assert!(found.matched);
        assert_eq!(found.captures, vec!["tok_9f8e".to_string()]);
    }

    /// The binding an agent chains into the next request.
    #[test]
    fn a_json_path_captures_the_value_it_found() {
        let (stored, body) = json_response(r#"{"session":{"token":"abc123"}}"#);
        let found = evaluate(
            &Condition::Json {
                path: "session.token".to_string(),
                value: None,
            },
            &stored,
            &body,
        );
        assert!(found.matched);
        assert_eq!(found.captures, vec!["abc123".to_string()]);

        let wrong = evaluate(
            &Condition::Json {
                path: "session.token".to_string(),
                value: Some("nope".to_string()),
            },
            &stored,
            &body,
        );
        assert!(!wrong.matched, "a value that differs does not match");
        assert_eq!(wrong.captures, vec!["abc123".to_string()], "and still says what it found");
    }

    /// A pattern that does not compile is not a response that does not match.
    /// A pattern with no group still says what it found.
    #[test]
    fn a_pattern_without_a_group_captures_the_whole_match() {
        let (stored, body) = json_response(r#"{"note":"FLAG{abc123} is here"}"#);
        let found = evaluate(
            &Condition::Regex(r"FLAG\{[a-z0-9]+\}".to_string()),
            &stored,
            &body,
        );
        assert!(found.matched);
        assert_eq!(found.captures, vec!["FLAG{abc123}".to_string()]);
    }

    #[test]
    fn a_broken_pattern_is_not_a_negative_answer() {
        let (stored, body) = json_response("{}");
        let found = evaluate(&Condition::Regex("([unclosed".to_string()), &stored, &body);
        assert!(!found.matched);
        assert!(
            found.expr.contains("not a regular expression"),
            "the caller has to be able to tell this from a clean miss: {}",
            found.expr
        );
    }

    #[test]
    fn status_and_length_read_off_the_stored_response() {
        let (stored, body) = json_response(r#"{"a":1}"#);
        assert!(evaluate(&Condition::Status(200), &stored, &body).matched);
        assert!(!evaluate(&Condition::Status(403), &stored, &body).matched);
        assert!(evaluate(&Condition::LongerThan(3), &stored, &body).matched);
        assert!(!evaluate(&Condition::LongerThan(100), &stored, &body).matched);
        assert!(evaluate(&Condition::ShorterThan(100), &stored, &body).matched);
    }

    #[test]
    fn a_binding_is_substituted_and_an_unbound_name_is_refused() {
        let mut bound = std::collections::BTreeMap::new();
        bound.insert("csrf".to_string(), "tok_123".to_string());
        assert_eq!(
            substitute("header.X-CSRF-Token=${csrf}", &bound).unwrap(),
            "header.X-CSRF-Token=tok_123"
        );
        // The failure that matters: an empty token produces a 403 that looks
        // exactly like the finding somebody is hunting for.
        let error = substitute("header.X-CSRF-Token=${nonce}", &bound).unwrap_err();
        assert!(error.to_string().contains("nonce"), "{error}");
        assert!(error.to_string().contains("csrf"), "it says what is bound: {error}");
    }

    #[test]
    fn extractors_read_the_four_places_a_token_hides() {
        let (stored, body) = json_response(r#"{"session":{"token":"abc123"}}"#);
        assert_eq!(
            extract_one("json:session.token", &stored, &body).unwrap(),
            "abc123"
        );
        assert_eq!(extract_one("status", &stored, &body).unwrap(), "200");
        assert_eq!(
            extract_one("header:content-type", &stored, &body).unwrap(),
            "application/json"
        );

        let html = Text::Utf8(
            r#"<input name="csrf" value="tok_9f8e">"#.to_string(),
        );
        assert_eq!(
            extract_one(r#"regex:name="csrf" value="([^"]+)""#, &stored, &html).unwrap(),
            "tok_9f8e"
        );
        // A pattern that finds nothing is an error, not an empty binding.
        assert!(extract_one("regex:nothing-here", &stored, &html).is_err());
    }

    /// One hiccup must not move the answer, which is why this is a median.
    #[test]
    fn a_single_outlier_does_not_move_the_median() {
        let steady = [100u64, 102, 99, 101, 100];
        let (median, spread) = median_and_deviation(&steady).unwrap();
        assert_eq!(median, 100);
        assert!(spread <= 1, "a steady run has a tight spread: {spread}");

        // The same run with one scheduling stall in it.
        let hiccup = [100u64, 102, 99, 101, 100, 4000];
        let (median, _) = median_and_deviation(&hiccup).unwrap();
        assert!(
            (99..=102).contains(&median),
            "one 4-second sample must not become the answer: {median}"
        );
        // A mean would have said ~750.
    }

    /// A burst can stop reaching the wire partway through, and those sends
    /// carry the refusal's clock — a millisecond or two — which pulls the
    /// median down until a three-second delay reports as none.
    #[test]
    fn sends_that_never_reached_the_wire_are_not_part_of_the_timing() {
        let answered = |ms: u64| json!({"status": 200, "ttfb_ms": ms, "total_ms": ms});
        let refused = json!({"status": Value::Null, "ttfb_ms": 0, "total_ms": 0});
        let samples = vec![
            answered(3000),
            answered(3010),
            answered(2990),
            refused.clone(),
            refused.clone(),
            refused.clone(),
            refused,
        ];
        let summary = timing_summary(&samples).expect("a summary");
        assert_eq!(summary["sends"], json!(7));
        assert_eq!(summary["measured"], json!(3));
        assert_eq!(summary["unanswered"], json!(4));
        let median = summary["ttfb_ms"]["median"].as_u64().expect("a median");
        assert!(
            (2990..=3010).contains(&median),
            "the delay is the answer, not the refusals: {median}"
        );
    }

    /// And a burst that mostly did not happen says so rather than taking a
    /// median of one.
    #[test]
    fn a_burst_that_almost_never_answered_reports_that_instead_of_a_number() {
        let samples = vec![
            json!({"status": 200, "ttfb_ms": 3000, "total_ms": 3000}),
            json!({"status": Value::Null, "ttfb_ms": 0, "total_ms": 0}),
            json!({"status": Value::Null, "ttfb_ms": 0, "total_ms": 0}),
        ];
        let summary = timing_summary(&samples).expect("a summary");
        assert_eq!(summary["measured"], json!(1));
        assert!(summary.get("ttfb_ms").is_none(), "{summary}");
    }

    /// A blind test's whole signal: one payload is reliably slower.
    #[test]
    fn a_real_delay_moves_the_median() {
        let fast = median_and_deviation(&[100, 101, 99, 100]).unwrap().0;
        let slow = median_and_deviation(&[2100, 2098, 2101, 2099]).unwrap().0;
        assert!(slow > fast * 10);
    }

    #[test]
    fn a_map_folds_a_log_into_endpoints_and_keeps_the_refusals() {
        let records = vec![
            json!({"seq":0,"phase":"request","initiator":"navigation","method":"GET",
                   "url":"https://app.test/users?id=1&page=2","allowed":true}),
            json!({"seq":0,"phase":"response","url":"https://app.test/users?id=1&page=2",
                   "allowed":true,"status":200}),
            json!({"seq":1,"phase":"request","initiator":"subresource","method":"GET",
                   "url":"https://app.test/style.css","allowed":true}),
            json!({"seq":1,"phase":"response","url":"https://app.test/style.css",
                   "allowed":true,"status":200}),
            json!({"seq":2,"phase":"request","initiator":"navigation","method":"POST",
                   "url":"https://app.test/users?id=9","allowed":true}),
            json!({"seq":2,"phase":"response","url":"https://app.test/users?id=9",
                   "allowed":true,"status":403}),
            json!({"seq":3,"phase":"request","initiator":"subresource","method":"GET",
                   "url":"https://tracker.example/beacon","allowed":false}),
        ];
        let map = map_of(&records);

        assert_eq!(map.origins.len(), 1, "the refused origin is not an endpoint");
        let app = &map.origins[0];
        assert_eq!(app.origin, "https://app.test");
        assert_eq!(app.endpoints.len(), 2);

        let users = app.endpoints.iter().find(|e| e.path == "/users").unwrap();
        assert_eq!(users.methods, vec!["GET", "POST"], "both methods, once each");
        assert_eq!(users.statuses, vec![200, 403]);
        assert_eq!(users.params, vec!["id", "page"], "names, never values");
        assert_eq!(users.hits, 2);
        assert!(users.navigated);

        let css = app.endpoints.iter().find(|e| e.path == "/style.css").unwrap();
        assert!(!css.navigated, "a subresource is not a page somebody went to");

        assert_eq!(map.denied, vec!["https://tracker.example/beacon".to_string()]);
    }

    #[test]
    fn a_raw_request_reads_as_an_http_message() {
        let stored = StoredRequest {
            seq: 3,
            at: "2026-09-02T00:00:00.000000Z".to_string(),
            method: "POST".to_string(),
            url: "https://app.test/login?next=/home".to_string(),
            headers: vec![("cookie".to_string(), "session=abc".to_string())],
            body: Body::Empty,
        };
        let rendered = raw_request(&stored, &Text::Utf8("user=alice".to_string()));
        // CRLF: this is the file `resend --raw-request` reads back.
        assert!(rendered.starts_with("POST /login?next=/home HTTP/1.1\r\n"), "{rendered}");
        assert!(rendered.contains("host: app.test\r\n"), "{rendered}");
        assert!(rendered.contains("cookie: session=abc\r\n"), "{rendered}");
        assert!(rendered.ends_with("\r\n\r\nuser=alice"), "{rendered}");
    }

    /// A body with two undecodable bytes is still evidence. Hiding it behind a
    /// digest loses the answer in exactly the case the store exists for.
    #[test]
    fn a_body_that_is_not_utf8_is_still_readable() {
        let mut raw = vec![0xff, 0xd8];
        raw.extend_from_slice(b"FLAG{deadbeef}");
        let sha = "d0";
        let body = Text::Binary {
            bytes: raw.len() as u64,
            sha256: sha.to_string(),
            text: String::from_utf8_lossy(&raw).into_owned(),
        };
        assert!(
            body.as_str().contains("FLAG{deadbeef}"),
            "match and diff have to be able to see it: {:?}",
            body.as_str()
        );
        let json = body.to_json();
        assert_eq!(json["kind"], "binary", "and it still says it was not text");
        assert_eq!(json["bytes"], 16);
        assert_eq!(json["sha256"], sha);
    }
}
