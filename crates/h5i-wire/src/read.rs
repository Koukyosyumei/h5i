//! Reading a stored message: its bytes, its head, and the states a body can be
//! in when it is not there.
//!
//! Shared because two programs read this store: the binary, whose `resend` and
//! `sequence` bind values out of a response, and the websec plugin, which is
//! where reading it is a verb (design-websec.md W21).

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Value, json};

use crate::message::{Body, StoredRequest, StoredResponse, body_file};

/// Every sequence number the store holds, in order.
pub fn sequences(dir: &Path) -> Vec<u64> {
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some((seq, _)) = name.split_once('.')
                && let Ok(seq) = seq.parse::<u64>()
            {
                seen.insert(seq);
            }
        }
    }
    seen.into_iter().collect()
}

/// A condition that did not match, in grep's own code.
///
/// `match` is a grep, so it answers like one: 0 matched, 1 did not, 2 could not
/// look. Shared because two binaries answer with it.
pub const EXIT_NO_MATCH: i32 = 1;

/// A question that could not be asked: a pattern that will not compile, a body
/// that was never stored. Never the same code as "no".
pub const EXIT_CANNOT_LOOK: i32 = 2;

/// One stored message file, or the reason it could not be read.
pub fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("{} could not be read: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{} is not readable: {e}", path.display()))
}

/// A body represented for inspection.
#[derive(Debug, Clone, PartialEq)]
pub enum Text {
    /// Decoded, and safe to compare line by line.
    Utf8(String),
    /// Decoded, and only the head of what the response carried.
    ///
    /// Its own state because every verb here turns on the difference: a search
    /// over the head says nothing about the rest, and its length is the cap's
    /// number rather than the body's.
    Cut {
        text: String,
        /// How many bytes the response actually carried.
        of_bytes: u64,
    },
    /// Binary data with an inspection-only lossy preview.
    Binary {
        bytes: u64,
        sha256: String,
        text: String,
    },
    /// Not in the store, and why.
    Missing(String),
}

impl Text {
    /// How many bytes the body actually had.
    ///
    /// Not `as_str().len()`: one invalid byte puts a response on the preview
    /// path, and a length read off a 64 KiB preview is a number the target
    /// chose. `None` when the body is not stored, which is not a length of zero.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> Option<u64> {
        match self {
            Text::Utf8(text) => Some(text.len() as u64),
            Text::Cut { of_bytes, .. } => Some(*of_bytes),
            Text::Binary { bytes, .. } => Some(*bytes),
            Text::Missing(_) => None,
        }
    }

    /// Whether [`Text::as_str`] is the whole body or only its head. A search
    /// over part of a body cannot answer "not there".
    pub fn whole(&self) -> bool {
        match self {
            Text::Utf8(_) => true,
            Text::Cut { .. } => false,
            Text::Binary { bytes, .. } => *bytes <= LOSSY_BODY_BYTES as u64,
            Text::Missing(_) => false,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Text::Utf8(text) => text,
            Text::Cut { text, .. } => text,
            // Let match and diff inspect the lossy preview.
            Text::Binary { text, .. } => text,
            Text::Missing(_) => "",
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Text::Utf8(text) => json!({"kind": "text", "text": text}),
            Text::Cut { text, of_bytes } => {
                json!({"kind": "text", "text": text, "truncated": true, "of_bytes": of_bytes})
            }
            Text::Binary {
                bytes,
                sha256,
                text,
            } => {
                json!({"kind": "binary", "bytes": bytes, "sha256": sha256, "text": text})
            }
            Text::Missing(why) => json!({"kind": "absent", "why": why}),
        }
    }
}

/// Target text with what a terminal would act on made visible instead.
///
/// The human view prints strings the target wrote; an escape in one repaints
/// the report of what the target did. Escaped rather than dropped, so it stays
/// visible. Bidi controls go too: they are not `is_control` and reorder the
/// line around them. `--raw`, `--body-to` and `--json` are the byte channels
/// and are untouched.
pub fn printable(text: &str) -> String {
    if !text
        .chars()
        .any(|c| c.is_control() || is_bidi_control(c))
    {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() || is_bidi_control(ch) {
            out.extend(ch.escape_debug());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Bidirectional formatting characters, which reorder the text *around* them.
/// Overrides, embeddings and isolates only, as `snapshot.rs` drops from page text.
fn is_bidi_control(c: char) -> bool {
    matches!(c,
        '\u{200E}' | '\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}'
    )
}

/// Exact stored body bytes, or `None` when unavailable.
pub fn body_bytes(dir: &Path, body: &Body) -> Option<Vec<u8>> {
    match body {
        Body::Empty => Some(Vec::new()),
        Body::Skipped { .. } => None,
        Body::Stored { sha256, .. } => std::fs::read(body_file(dir, sha256)?).ok(),
    }
}

/// How much of a body that is not text to read back anyway.
pub const LOSSY_BODY_BYTES: usize = 64 * 1024;

/// Pull a body out of the store.
pub fn body_text(dir: &Path, body: &Body) -> Text {
    match body {
        Body::Empty => Text::Utf8(String::new()),
        Body::Skipped { reason, bytes } => Text::Missing(match (reason, bytes) {
            (reason, Some(bytes)) => format!("{reason:?} ({bytes} bytes)").to_lowercase(),
            (reason, None) => format!("{reason:?}").to_lowercase(),
        }),
        Body::Stored {
            sha256,
            bytes,
            of_bytes,
            truncated,
        } => {
            let Some(path) = body_file(dir, sha256) else {
                return Text::Missing(format!(
                    "{sha256:?} is not a body hash, so the store has nothing under it"
                ));
            };
            match std::fs::read(&path) {
                Err(e) => Text::Missing(format!("the stored body could not be read: {e}")),
                Ok(raw) => match String::from_utf8(raw) {
                    // A cut on a character boundary — every cut, for ASCII —
                    // decodes cleanly, so the head used to look like a body.
                    Ok(text) if *truncated => Text::Cut {
                        of_bytes: of_bytes.unwrap_or(text.len() as u64),
                        text,
                    },
                    Ok(text) => Text::Utf8(text),
                    Err(e) => Text::Binary {
                        bytes: *bytes,
                        sha256: sha256.clone(),
                        text: {
                            let raw = e.into_bytes();
                            // Capped, because a real image would otherwise fill
                            // the reply with replacement characters. The digest
                            // above is what says how much there was.
                            let head = &raw[..raw.len().min(LOSSY_BODY_BYTES)];
                            String::from_utf8_lossy(head).into_owned()
                        },
                    },
                },
            }
        }
    }
}

/// Render a request the way it went out.
/// The request half, as an HTTP message.
///
/// CRLF, because this output is not only for reading: `resend --raw-request`
/// takes a file holding a whole request and writes it to the socket with
/// nothing recomputed, and the obvious way to get such a file is to dump the
/// request that is already stored. A dump that ended its lines with a bare LF
/// would be a message a server may refuse, produced by the one command whose
/// job is to hand back exactly what went out.
pub fn raw_request(stored: &StoredRequest, body: &Text) -> String {
    let mut out = String::new();
    let target = url::Url::parse(&stored.url)
        .map(|u| {
            let mut target = u.path().to_string();
            if let Some(query) = u.query() {
                target.push('?');
                target.push_str(query);
            }
            target
        })
        .unwrap_or_else(|_| stored.url.clone());
    out.push_str(&format!("{} {} HTTP/1.1\r\n", stored.method, target));
    if let Ok(url) = url::Url::parse(&stored.url)
        && let Some(host) = url.host_str()
    {
        // The client computes this one, so it is not in the stored set; showing
        // the message without it would be showing something that is not a
        // request.
        match url.port() {
            Some(port) => out.push_str(&format!("host: {host}:{port}\r\n")),
            None => out.push_str(&format!("host: {host}\r\n")),
        }
    }
    for (name, value) in &stored.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    push_body(&mut out, body);
    out
}

/// Render a response the way it arrived.
pub fn raw_response(stored: &StoredResponse, body: &Text) -> String {
    let mut out = String::new();
    match stored.status {
        Some(status) => out.push_str(&format!("HTTP/1.1 {status}\n")),
        None => out.push_str("HTTP/1.1 (no status: the request did not complete)\n"),
    }
    for (name, value) in &stored.headers {
        out.push_str(&format!("{name}: {value}\n"));
    }
    out.push('\n');
    push_body(&mut out, body);
    out
}

pub fn push_body(out: &mut String, body: &Text) {
    match body {
        Text::Utf8(text) => out.push_str(text),
        Text::Cut { text, of_bytes } => {
            out.push_str(text);
            out.push_str(&format!(
                "\n[the store kept {} of this body's {of_bytes} bytes]\n",
                text.len()
            ));
        }
        Text::Binary {
            bytes,
            sha256,
            text,
        } => {
            out.push_str(&format!("[{bytes} bytes, not text — sha256 {sha256}]\n"));
            out.push_str(text);
        }
        Text::Missing(why) => out.push_str(&format!("[no body: {why}]")),
    }
}

/// How much of one line a preview shows.
///
/// The line count was bounded and the length was not, so a minified page — one
/// line, megabytes of it — printed whole. The same number the body diff cuts at.
pub const MAX_PREVIEW_LINE: usize = 400;

/// One line of a body, bounded and inert.
pub fn preview_line(line: &str) -> String {
    let mut shown: String = line.chars().take(MAX_PREVIEW_LINE).collect();
    if shown.chars().count() < line.chars().count() {
        shown.push_str(&format!(
            " … [{} more characters on this line]",
            line.chars().count() - MAX_PREVIEW_LINE
        ));
    }
    printable(&shown)
}

/// Walk a dotted path, the way `edits` does. Kept to the same spelling so a
/// path that names a field for an edit names the same field for a match.
pub fn json_at<'a>(document: &'a Value, path: &str) -> Option<&'a Value> {
    let mut at = document;
    for segment in path.trim_start_matches("$.").split('.').filter(|s| !s.is_empty()) {
        at = match at {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(at)
}
