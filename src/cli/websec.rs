//! Read and compare captured HTTP messages without touching the network.
//!
//! h5i reads the store directly so credentials never pass through the renderer.
//! Stores on inaccessible boxed filesystems produce an explicit error.

use std::path::{Path, PathBuf};

use h5i_browser::capture::{Body, StoredRequest, StoredResponse, body_file};
// The store's readers live with the store's types, because the websec plugin
// reads the same bytes and must not link an engine to do it (W21).
use h5i_wire::read::{
    EXIT_CANNOT_LOOK, LOSSY_BODY_BYTES, Text, body_text, json_at, preview_line, read_json,
    sequences,
};
use h5i_core::browser_session as bs;
use serde_json::{json, Value};


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
    // Read directly: what these exercise is the shared reader, and the module
    // above no longer names them.
    use h5i_wire::read::{MAX_PREVIEW_LINE, body_bytes, printable};

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







    fn json_response(body: &str) -> (StoredResponse, Text) {
        (
            response(200, "application/json"),
            Text::Utf8(body.to_string()),
        )
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
