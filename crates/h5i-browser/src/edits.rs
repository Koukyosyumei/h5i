//! Structured request edits. The engine rebuilds the message and framing.
//!
//! ```text
//! method=POST                     the verb
//! url=https://host/path?a=1       the whole target
//! path=/api/v2/users/456          the path, keeping the query
//! query.user_id=456               one query parameter
//! header.X-Forwarded-For=127.0.0.1
//! cookie.session=forged           one cookie, inside the Cookie header
//! json.user.role=admin            one field of a JSON body
//! form.username=admin             one field of a form body
//! body.raw=<bytes>                the whole body
//! ```
//!
//! Invalid edits fail instead of silently coercing or doing nothing. [`Applied`]
//! records effective changes. JSON values are parsed when valid and otherwise
//! treated as strings; quote a value to force a JSON string.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;
use url::form_urlencoded;

/// The part of a request an edit names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    /// The request method.
    Method,
    /// The whole URL, replaced.
    Url,
    /// The path, keeping the query that was there.
    Path,
    /// One query parameter, by name.
    Query(String),
    /// One header, by name. Replaces every copy of that name.
    Header(String),
    /// One cookie, inside the `Cookie` header.
    Cookie(String),
    /// One field of a JSON body, by dotted path (`user.role`, `items.0.id`).
    Json(String),
    /// One field of a form-encoded body.
    Form(String),
    /// One part of a `multipart/form-data` body, and which of its three
    /// interesting pieces.
    Multipart { field: String, piece: Piece },
    /// The whole body.
    BodyRaw,
}

/// Editable component of a multipart part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Piece {
    /// The bytes.
    Data,
    /// The declared filename. Where a traversal or a double extension goes.
    Filename,
    /// The declared `Content-Type`. What a type filter reads.
    ContentType,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Method => write!(f, "method"),
            Target::Url => write!(f, "url"),
            Target::Path => write!(f, "path"),
            Target::Query(name) => write!(f, "query.{name}"),
            Target::Header(name) => write!(f, "header.{name}"),
            Target::Cookie(name) => write!(f, "cookie.{name}"),
            Target::Json(path) => write!(f, "json.{path}"),
            Target::Form(name) => write!(f, "form.{name}"),
            Target::Multipart { field, piece } => match piece {
                Piece::Data => write!(f, "multipart.{field}"),
                Piece::Filename => write!(f, "multipart.{field}.filename"),
                Piece::ContentType => write!(f, "multipart.{field}.content_type"),
            },
            Target::BodyRaw => write!(f, "body.raw"),
        }
    }
}

/// One change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub target: Target,
    /// `None` removes targets that support removal.
    pub value: Option<Vec<u8>>,
}

/// What went wrong, in the terms the caller used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditError {
    pub target: String,
    pub message: String,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.target, self.message)
    }
}

impl EditError {
    fn new(target: impl fmt::Display, message: impl Into<String>) -> Self {
        Self {
            target: target.to_string(),
            message: message.into(),
        }
    }
}

/// One applied edit, including its previous value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub target: String,
    /// The new value, as text where it is text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Encoded value when it differs from `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded: Option<String>,
    /// What was there before, when there was something and it was text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
    /// Set when the target did not exist and was created.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
}

/// Editable request fields, kept separate from broker-owned [`crate::broker::Fetch`] fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Editable {
    pub method: String,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Editable {
    /// The `Content-Type`, which decides what a `json.` or `form.` edit means.
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
    }

    fn set_header(&mut self, name: &str, value: &str) -> Option<String> {
        let mut previous = None;
        let mut placed = false;
        // Preserve the first header's position and remove duplicate names.
        self.headers.retain_mut(|(existing, current)| {
            if !existing.eq_ignore_ascii_case(name) {
                return true;
            }
            if previous.is_none() {
                previous = Some(current.clone());
                *current = value.to_string();
                placed = true;
                return true;
            }
            false
        });
        if !placed {
            self.headers.push((name.to_string(), value.to_string()));
        }
        previous
    }

    fn remove_header(&mut self, name: &str) -> Option<String> {
        let mut previous = None;
        self.headers.retain(|(existing, current)| {
            if existing.eq_ignore_ascii_case(name) {
                if previous.is_none() {
                    previous = Some(current.clone());
                }
                return false;
            }
            true
        });
        previous
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Parse `target=value` as one edit.
///
/// The value is everything after the first `=`, so a value containing `=` (a
/// base64 payload, a JWT, a SQL clause) needs no escaping. That matters more
/// than it looks: the payloads this is for are exactly the strings that are
/// awkward in a shell.
pub fn parse_set(spec: &str) -> Result<Edit, EditError> {
    let (target, value) = spec
        .split_once('=')
        .ok_or_else(|| EditError::new(spec, "an edit is `target=value`, and this has no `=`"))?;
    Ok(Edit {
        target: parse_target(target)?,
        value: Some(value.as_bytes().to_vec()),
    })
}

/// The same edit, with a value that is bytes rather than text.
///
/// A file upload is the reason this exists. `Edit` has always carried a
/// `Vec<u8>`, but `--set` reaches it through the process arguments and then
/// through a JSON control message, and neither of those carries a JPEG's first
/// two bytes — `ff d8` is not text in any encoding. A magic-number check is a
/// filter worth testing, so the bytes have to arrive intact.
pub fn parse_set_bytes(target: &str, value: Vec<u8>) -> Result<Edit, EditError> {
    Ok(Edit {
        target: parse_target(target)?,
        value: Some(value),
    })
}

/// Parse a bare target, for a removal.
pub fn parse_unset(spec: &str) -> Result<Edit, EditError> {
    Ok(Edit {
        target: parse_target(spec)?,
        value: None,
    })
}

fn parse_target(spec: &str) -> Result<Target, EditError> {
    let spec = spec.trim();
    if let Some((kind, name)) = spec.split_once('.') {
        let name = name.trim();
        if name.is_empty() {
            return Err(EditError::new(spec, "names the kind but not which one"));
        }
        return match kind.trim().to_ascii_lowercase().as_str() {
            "query" => Ok(Target::Query(name.to_string())),
            "header" => Ok(Target::Header(name.to_string())),
            "cookie" => Ok(Target::Cookie(name.to_string())),
            "json" => {
                if name.contains('[') || name.contains(']') {
                    return Err(EditError::new(
                        spec,
                        "bracket array syntax is not supported and would be ambiguous. Use a \
                         dotted numeric index such as `json.items.0.name` for an existing array, \
                         or `--raw-request` to construct a new array",
                    ));
                }
                Ok(Target::Json(name.trim_start_matches("$.").to_string()))
            }
            "form" => Ok(Target::Form(name.to_string())),
            "body" if name.eq_ignore_ascii_case("raw") => Ok(Target::BodyRaw),
            "body" => Err(EditError::new(
                spec,
                "the only body target is `body.raw`; to change one field use `json.` or `form.`",
            )),
            "multipart" => {
                // `multipart.avatar`, `multipart.avatar.filename`,
                // `multipart.avatar.content_type`. The field name comes first
                // because that is how a person says it out loud.
                let (field, piece) = match name.rsplit_once('.') {
                    Some((field, "filename")) => (field, Piece::Filename),
                    Some((field, "content_type")) | Some((field, "content-type")) => {
                        (field, Piece::ContentType)
                    }
                    _ => (name, Piece::Data),
                };
                if field.is_empty() {
                    return Err(EditError::new(spec, "names a piece but not which field"));
                }
                Ok(Target::Multipart {
                    field: field.to_string(),
                    piece,
                })
            }
            other => Err(EditError::new(
                spec,
                format!(
                    "`{other}` is not something this engine can edit. \
                     Try method, url, path, query., header., cookie., json., form. or body.raw"
                ),
            )),
        };
    }
    match spec.to_ascii_lowercase().as_str() {
        "method" => Ok(Target::Method),
        "url" => Ok(Target::Url),
        "path" => Ok(Target::Path),
        other => Err(EditError::new(
            spec,
            format!(
                "`{other}` is not something this engine can edit. \
                 Try method, url, path, query., header., cookie., json., form. or body.raw"
            ),
        )),
    }
}

/// Whether a header name is one a request can carry: RFC 9110's token.
///
/// Checked here because the wire's refusal is "builder error", which names
/// neither the header nor the character.
fn header_name_is_sendable(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                        | b'^' | b'_' | b'`' | b'|' | b'~'
                )
        })
}

/// The character in a header value a request cannot carry, if there is one.
///
/// CR, LF and NUL only — anything narrower would refuse the payloads this is
/// for. Splitting a message deliberately is what `--raw-request` is for.
fn header_value_refuses(value: &str) -> Option<char> {
    value.chars().find(|c| matches!(c, '\r' | '\n' | '\0'))
}

/// The refusal both header edits share.
fn refuse_an_unsendable_header(
    target: &impl fmt::Display,
    name: &str,
    value: &str,
) -> Result<(), EditError> {
    if !header_name_is_sendable(name) {
        return Err(EditError::new(
            target,
            format!(
                "{name:?} is not a header name: a field name is a token, so it holds no \
                 space, colon or control character. A request whose framing is deliberately \
                 wrong goes out with `--raw-request`, which writes the bytes given and \
                 reports what it broke"
            ),
        ));
    }
    if let Some(bad) = header_value_refuses(value) {
        return Err(EditError::new(
            target,
            format!(
                "this value contains {bad:?}, which ends a header field rather than sitting \
                 in one: the request would not be the one it looks like. To send a message \
                 whose framing is deliberately wrong, use `--raw-request`, which writes the \
                 bytes given and reports which invariants it had to break"
            ),
        ));
    }
    Ok(())
}

/// Is this path segment a dot segment, in any of the spellings a URL parser
/// treats as one?
///
/// The URL standard resolves `.` and `..` away, and percent-decodes before
/// deciding — so `.%2e`, `%2e.` and `%2e%2e` are all "..", which is exactly what
/// the Apache traversal CVEs of 2021 are written with.
fn is_a_dot_segment(segment: &str) -> bool {
    let decoded = segment
        .replace("%2e", ".")
        .replace("%2E", ".")
        .replace("%2f", "/")
        .replace("%2F", "/");
    decoded == "." || decoded == ".."
}

/// Refuse a path the URL parser would resolve rather than send.
///
/// A workbench that quietly straightened `/cgi-bin/.%2e/.%2e/etc/passwd` into
/// `/etc/passwd` would send a request nobody asked for and report its 404 as
/// evidence about the request that was asked for. That is worse than not being
/// able to send it: a false negative that looks like a finding.
///
/// So this says what happened and stops. h5i builds every request from a parsed
/// `Url`, and the URL standard resolves dot segments before the bytes exist —
/// there is no layer here where the original spelling survives. Sending a
/// request-target the parser would rewrite is a capability this engine does not
/// have yet; see `docs/design/design-websec.md`.
fn refuse_a_resolved_traversal(
    target: &impl fmt::Display,
    asked: &str,
    resolved: &str,
) -> Result<(), EditError> {
    // Whatever was named — a path or a whole URL — only its path can hold a
    // dot segment.
    // The authority ends at the first separator, and `\` is one:
    // `https://app.test\..\..\x` has a path despite holding no `/`.
    let asked = match asked.split_once("://") {
        Some((_, rest)) => match rest.find(['/', '\\']) {
            Some(at) => &rest[at..],
            None => "/",
        },
        None => asked,
    };
    let asked_path = asked.split(['?', '#']).next().unwrap_or(asked);
    // The URL standard reads `\` as `/` in an http(s) URL, so
    // `/cgi-bin/..\..\etc/passwd` resolves to `/etc/passwd` before a request
    // exists — and even `/a\b` goes out as `/a/b`.
    if asked_path.contains('\\') {
        return Err(EditError::new(
            target,
            format!(
                "{asked_path:?} contains a backslash, and the URL standard reads that as a \
                 path separator in an http or https URL: this would have gone out as \
                 {resolved:?}. Percent-encode it as %5C to send the character itself."
            ),
        ));
    }
    if !asked_path.split(['/', '\\']).any(is_a_dot_segment) {
        return Ok(());
    }
    Err(EditError::new(
        target,
        format!(
            "{asked_path:?} contains a dot segment, and the URL standard resolves those \
             before a request exists: this would have gone out as {resolved:?}. h5i has no \
             way to send a request-target its URL parser would rewrite, so it refuses rather \
             than testing a different request and reporting the answer as yours."
        ),
    ))
}

/// Apply every edit, in order, and say what each one did.
///
/// Order is the caller's, and it is applied rather than optimised: setting a
/// body and then a field of it is a different instruction from the reverse, and
/// an engine that reordered them would answer a question nobody asked.
///
/// `create` allows an edit to add a target that is not there. Off by default
/// because a query parameter that does not exist is usually a typo, and a typo
/// that silently succeeds costs an agent a whole turn spent reading a response
/// that was never going to differ.
pub fn apply(
    request: &mut Editable,
    edits: &[Edit],
    create: bool,
) -> Result<Vec<Applied>, EditError> {
    let mut applied = Vec::with_capacity(edits.len());
    for edit in edits {
        applied.push(apply_one(request, edit, create)?);
    }
    Ok(applied)
}

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn apply_one(request: &mut Editable, edit: &Edit, create: bool) -> Result<Applied, EditError> {
    let target = edit.target.to_string();
    let removing = edit.value.is_none();
    let value = edit.value.clone().unwrap_or_default();

    match &edit.target {
        Target::Method => {
            if removing {
                return Err(EditError::new(&target, "a request without a method is not one"));
            }
            let was = std::mem::replace(&mut request.method, text(&value).trim().to_ascii_uppercase());
            Ok(Applied {
                target,
                encoded: None,
                value: Some(request.method.clone()),
                was: Some(was),
                created: false,
            })
        }

        Target::Url => {
            if removing {
                return Err(EditError::new(&target, "a request without a URL has nowhere to go"));
            }
            let raw = text(&value);
            let parsed = Url::parse(raw.trim())
                .map_err(|e| EditError::new(&target, format!("{raw:?} is not a URL: {e}")))?;
            refuse_a_resolved_traversal(&target, raw.trim(), parsed.path())?;
            let was = std::mem::replace(&mut request.url, parsed);
            Ok(Applied {
                target,
                encoded: None,
                value: Some(request.url.to_string()),
                was: Some(was.to_string()),
                created: false,
            })
        }

        Target::Path => {
            if removing {
                return Err(EditError::new(&target, "every URL has a path; set it to `/` instead"));
            }
            let asked = text(&value);
            let asked = asked.trim();
            // A `?` is not part of a path, and `set_path` would escape it to
            // `%3F` — which reaches the server as a path with a question mark
            // *in* it, a different request from the one that was typed and one
            // that usually 404s. Refused rather than encoded, and named: this
            // edit sets the path and keeps the query, so a caller who wants
            // both means `url=`, and a caller who wants one parameter means
            // `query.<name>=`.
            if let Some(at) = asked.find('?') {
                return Err(EditError::new(
                    &target,
                    format!(
                        "a path has no query in it, and `{}` would be sent as part of the \
                         path. Set the whole target with `url=` or one parameter with \
                         `query.<name>=`",
                        &asked[at..]
                    ),
                ));
            }
            let was = request.url.path().to_string();
            let mut candidate = request.url.clone();
            candidate.set_path(asked);
            refuse_a_resolved_traversal(&target, asked, candidate.path())?;
            request.url = candidate;
            Ok(Applied {
                target,
                encoded: None,
                value: Some(request.url.path().to_string()),
                was: Some(was),
                created: false,
            })
        }

        Target::Query(name) => {
            // Edited as the pieces it was written as. Decoding and re-encoding
            // rewrote parameters nobody named: `?debug` came back as `debug=`,
            // `x[]=1` as `x%5B%5D=1`.
            let raw = request.url.query().unwrap_or_default().to_string();
            let pieces: Vec<&str> = if raw.is_empty() {
                Vec::new()
            } else {
                raw.split('&').collect()
            };
            let named: Vec<(String, String)> = pieces
                .iter()
                .map(|piece| (query_name(piece), query_value(piece)))
                .collect();
            let was = named
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone());
            if was.is_none() && !create && !removing {
                return Err(missing(&target, "query parameter", &named));
            }
            let mut replaced = false;
            let mut next: Vec<String> = Vec::with_capacity(pieces.len() + 1);
            for (piece, (k, _)) in pieces.iter().zip(&named) {
                if k == name {
                    if removing {
                        continue;
                    }
                    // Only the first copy is replaced; a repeated parameter is a
                    // list, and collapsing it would change the request in a way
                    // nobody asked for.
                    if !replaced {
                        next.push(format!("{}={}", encode_query(k), encode_query(&text(&value))));
                        replaced = true;
                        continue;
                    }
                }
                next.push((*piece).to_string());
            }
            if !replaced && !removing {
                next.push(format!(
                    "{}={}",
                    encode_query(name),
                    encode_query(&text(&value))
                ));
            }
            if next.is_empty() {
                request.url.set_query(None);
            } else {
                request.url.set_query(Some(&next.join("&")));
            }
            Ok(Applied {
                target,
                encoded: None,
                value: (!removing).then(|| text(&value)),
                created: was.is_none() && !removing,
                was,
            })
        }

        Target::Header(name) => {
            if crate::net::header_is_the_clients(name) {
                return Err(EditError::new(
                    &target,
                    "this header frames the message and is the client's to compute. \
                     Setting it would describe a request other than the one that goes out",
                ));
            }
            if removing {
                let was = request.remove_header(name);
                return Ok(Applied {
                    target,
                    value: None,
                    encoded: None,
                    was,
                    created: false,
                });
            }
            refuse_an_unsendable_header(&target, name, &text(&value))?;
            let was = request.set_header(name, &text(&value));
            Ok(Applied {
                target,
                encoded: None,
                value: Some(text(&value)),
                created: was.is_none(),
                was,
            })
        }

        Target::Cookie(name) => {
            // A cookie is a header once written, so the same three characters
            // end it. Checked before anything is rebuilt.
            if !removing {
                refuse_an_unsendable_header(&target, "Cookie", &text(&value))?;
                refuse_a_reframed_cookie(&target, name, &text(&value))?;
            }
            // As the pieces it was written as: parsing into pairs dropped
            // every piece with no `=`, so a bare token was lost on any edit.
            let current = request.header("cookie").unwrap_or_default().to_string();
            let pieces: Vec<&str> = if current.is_empty() {
                Vec::new()
            } else {
                current.split(';').collect()
            };
            let named: Vec<(String, String)> =
                pieces.iter().map(|piece| cookie_name_and_value(piece)).collect();
            let was = named.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            if was.is_none() && !create && !removing {
                return Err(missing(&target, "cookie", &named));
            }
            let mut replaced = false;
            let mut next: Vec<String> = Vec::with_capacity(pieces.len() + 1);
            for (piece, (k, _)) in pieces.iter().zip(&named) {
                if k == name {
                    if removing {
                        continue;
                    }
                    if !replaced {
                        // The piece's own leading space, so the header reads
                        // as it did.
                        let lead = &piece[..piece.len() - piece.trim_start().len()];
                        next.push(format!("{lead}{name}={}", text(&value)));
                        replaced = true;
                        continue;
                    }
                }
                next.push((*piece).to_string());
            }
            if !replaced && !removing {
                let lead = if next.is_empty() { "" } else { " " };
                next.push(format!("{lead}{name}={}", text(&value)));
            }
            if next.is_empty() {
                request.remove_header("cookie");
            } else {
                request.set_header("Cookie", &next.join(";"));
            }
            Ok(Applied {
                target,
                encoded: None,
                value: (!removing).then(|| text(&value)),
                created: was.is_none() && !removing,
                was,
            })
        }

        Target::Json(path) => {
            // An empty body with `--create` is a body to *build*, the way a
            // multipart one is. A request that carries no JSON yet is the
            // ordinary starting point for an API call the page never makes, and
            // refusing it sent callers to `body.raw` to hand-write the JSON that
            // this edit exists to maintain.
            // Built here and committed at the end: starting the body off as
            // `{}` left a refused edit having already changed the request.
            let building = request.body.is_empty() && create;
            let kind = request.content_type().unwrap_or("").to_string();
            let mut document: serde_json::Value = if building {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_slice(&request.body).map_err(|e| {
                    EditError::new(
                        &target,
                        format!(
                            "this request's body is not JSON ({e}); its Content-Type is {}. \
                             Use `form.` for a form body, or `body.raw` for anything else",
                            if kind.is_empty() { "unset" } else { &kind }
                        ),
                    )
                })?
            };
            let was = json_at(&document, path).map(render_json);
            if was.is_none() && !create && !removing {
                return Err(EditError::new(
                    &target,
                    format!(
                        "this body has no `{path}`. Pass --create to add it, \
                         or check the field name against the stored request"
                    ),
                ));
            }
            let mut encoded = None;
            if removing {
                json_remove(&mut document, path)
                    .map_err(|why| EditError::new(&target, why))?;
            } else {
                let typed = text(&value);
                let parsed = serde_json::from_slice::<serde_json::Value>(&value)
                    .unwrap_or_else(|_| serde_json::Value::String(typed.clone()));
                refuse_a_number_that_loses_its_digits(&target, &typed, &parsed)?;
                // Record coercions so the receipt matches the sent body.
                if render_json(&parsed) != typed {
                    encoded = Some(parsed.to_string());
                }
                json_set(&mut document, path, parsed)
                    .map_err(|why| EditError::new(&target, why))?;
            }
            request.body = serde_json::to_vec(&document)
                .map_err(|e| EditError::new(&target, format!("could not rebuild the body: {e}")))?;
            if building && request.content_type().is_none() {
                request.set_header("Content-Type", "application/json");
            }
            Ok(Applied {
                target,
                encoded,
                value: (!removing).then(|| text(&value)),
                created: was.is_none() && !removing,
                was,
            })
        }

        Target::Form(name) => {
            // As the pieces it was written as, for the reason `query.` is:
            // re-serialising rewrote fields nobody named.
            let pieces: Vec<&[u8]> = if request.body.is_empty() {
                Vec::new()
            } else {
                request.body.split(|b| *b == b'&').collect()
            };
            let named: Vec<(String, String)> = pieces
                .iter()
                .map(|piece| form_name_and_value(piece))
                .collect();
            let was = named.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            if was.is_none() && !create && !removing {
                return Err(missing(&target, "form field", &named));
            }
            let encoded = format!(
                "{}={}",
                encode_query(name),
                encode_query(&text(&value))
            );
            let mut replaced = false;
            let mut next: Vec<Vec<u8>> = Vec::with_capacity(pieces.len() + 1);
            for (piece, (k, _)) in pieces.iter().zip(&named) {
                if k == name {
                    if removing {
                        continue;
                    }
                    if !replaced {
                        next.push(encoded.clone().into_bytes());
                        replaced = true;
                        continue;
                    }
                }
                next.push(piece.to_vec());
            }
            if !replaced && !removing {
                next.push(encoded.into_bytes());
            }
            request.body = next.join(&b'&');
            if request.content_type().is_none() {
                request.set_header("Content-Type", "application/x-www-form-urlencoded");
            }
            Ok(Applied {
                target,
                encoded: None,
                value: (!removing).then(|| text(&value)),
                created: was.is_none() && !removing,
                was,
            })
        }

        Target::Multipart { field, piece } => {
            let declared = request.content_type().unwrap_or("").to_string();
            let mut parts = match crate::multipart::boundary_of(&declared) {
                // Already multipart: take it apart, or say it is not the shape
                // it claims rather than quietly replacing it.
                Some(boundary) => crate::multipart::parse(&request.body, &boundary)
                    .ok_or_else(|| {
                        EditError::new(
                            &target,
                            "this request says it is multipart and its body does not match \
                             the boundary it declares, so there is nothing to edit safely",
                        )
                    })?,
                None if request.body.is_empty() || create => {
                    // Not multipart yet. With `--create` this *builds* an
                    // upload, which is the common case: a file-upload test
                    // usually has no recorded upload to start from, because the
                    // engine does not post files itself.
                    Vec::new()
                }
                None => {
                    return Err(EditError::new(
                        &target,
                        format!(
                            "this request's body is not multipart; its Content-Type is {}. \
                             Pass --create to build a multipart body instead",
                            if declared.is_empty() { "unset" } else { &declared }
                        ),
                    ));
                }
            };

            let at = parts.iter().position(|part| &part.name == field);
            let was = at.and_then(|at| match piece {
                Piece::Data => Some(String::from_utf8_lossy(&parts[at].data).into_owned()),
                Piece::Filename => parts[at].filename.clone(),
                Piece::ContentType => parts[at].content_type.clone(),
            });
            if at.is_none() && !create && !removing {
                let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
                return Err(EditError::new(
                    &target,
                    format!(
                        "no such part, so nothing would change. This request has: {}. \
                         Pass --create to add it",
                        if names.is_empty() {
                            "none".to_string()
                        } else {
                            names.join(", ")
                        }
                    ),
                ));
            }

            if removing {
                if let Some(at) = at {
                    match piece {
                        Piece::Data => {
                            parts.remove(at);
                        }
                        Piece::Filename => parts[at].filename = None,
                        Piece::ContentType => parts[at].content_type = None,
                    }
                }
            } else {
                let at = match at {
                    Some(at) => at,
                    None => {
                        parts.push(crate::multipart::Part {
                            name: field.clone(),
                            ..Default::default()
                        });
                        parts.len() - 1
                    }
                };
                match piece {
                    Piece::Data => parts[at].data = value.clone(),
                    Piece::Filename => parts[at].filename = Some(text(&value)),
                    Piece::ContentType => parts[at].content_type = Some(text(&value)),
                }
            }

            // The engine's own boundary, always. Re-using the one that arrived
            // means a caller who puts that string in a part's data splits the
            // message, and sends something other than what was asked for.
            let boundary = crate::multipart::fresh_boundary();
            request.body = crate::multipart::serialize(&parts, &boundary);
            request.set_header(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            );
            Ok(Applied {
                target,
                encoded: None,
                value: (!removing).then(|| text(&value)),
                created: was.is_none() && !removing,
                was,
            })
        }

        Target::BodyRaw => {
            let was = request.body.len();
            request.body = value.clone();
            Ok(Applied {
                target,
                encoded: None,
                value: Some(format!("{} bytes", request.body.len())),
                was: Some(format!("{was} bytes")),
                created: false,
            })
        }
    }
}

/// The decoded name of one `k=v` piece of a query string. A piece with no `=`
/// is a name with no value: `?debug` and `?debug=` are not the same request.
fn query_name(piece: &str) -> String {
    let raw = piece.split_once('=').map(|(k, _)| k).unwrap_or(piece);
    decode_query(raw)
}

/// The decoded value of one piece, empty when it has none.
fn query_value(piece: &str) -> String {
    piece.split_once('=').map(|(_, v)| decode_query(v)).unwrap_or_default()
}

fn decode_query(raw: &str) -> String {
    form_urlencoded::parse(raw.as_bytes())
        .next()
        .map(|(k, _)| k.into_owned())
        .unwrap_or_default()
}

fn encode_query(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// One piece of a `Cookie` header, as name and value.
///
/// A piece with no `=` is a bare token, which servers do read and which this
/// therefore has to keep: it is a cookie with an empty value, not an absence.
fn cookie_name_and_value(piece: &str) -> (String, String) {
    match piece.split_once('=') {
        Some((name, value)) => (name.trim().to_string(), value.trim().to_string()),
        None => (piece.trim().to_string(), String::new()),
    }
}

/// Refuse a cookie edit that would write more than one cookie.
///
/// `;` separates pairs and `=` separates a pair, so either inside a name — or a
/// `;` in a value — writes something other than the cookie named. Refused
/// rather than escaped; `header.Cookie=` takes the whole header.
fn refuse_a_reframed_cookie(
    target: &impl fmt::Display,
    name: &str,
    value: &str,
) -> Result<(), EditError> {
    let bad_name = name.contains(';') || name.contains('=');
    if bad_name || value.contains(';') {
        return Err(EditError::new(
            target,
            "`;` separates one cookie from the next and `=` separates a name from its \
             value, so this edit would write more than the one cookie it names. Set the \
             whole header with `header.Cookie=` when that is the request you mean",
        ));
    }
    // Every reader strips whitespace around a pair, so a value that begins or
    // ends with one goes out saying something it is not read as. Inside, it stays.
    if value != value.trim() || name != name.trim() {
        return Err(EditError::new(
            target,
            "a `Cookie` header allows whitespace around each pair and every reader strips \
             it, so a name or value that begins or ends with a space would not arrive as \
             the one asked for. Set the whole header with `header.Cookie=` to send those \
             bytes exactly",
        ));
    }
    Ok(())
}

/// One `k=v` piece of a form body, decoded.
fn form_name_and_value(piece: &[u8]) -> (String, String) {
    let at = piece.iter().position(|b| *b == b'=');
    let (raw_name, raw_value) = match at {
        Some(at) => (&piece[..at], &piece[at + 1..]),
        None => (piece, &[][..]),
    };
    (decode_form(raw_name), decode_form(raw_value))
}

fn decode_form(raw: &[u8]) -> String {
    form_urlencoded::parse(raw)
        .next()
        .map(|(k, _)| k.into_owned())
        .unwrap_or_default()
}

/// The error for a target that is not there, carrying what is.
///
/// The names, not a bare refusal: nine times out of ten the caller is one
/// spelling away, and the list is the answer. Values are never included, since
/// the thing being listed may be a session cookie.
fn missing(target: &str, kind: &str, have: &[(String, String)]) -> EditError {
    let names: Vec<&str> = have.iter().map(|(k, _)| k.as_str()).collect();
    let known = if names.is_empty() {
        "this request has none".to_string()
    } else {
        format!("this request has: {}", names.join(", "))
    };
    EditError::new(
        target,
        format!("no such {kind}, so nothing would change. {known}. Pass --create to add it"),
    )
}

/// Refuse integer literals that JSON parsing would round.
fn refuse_a_number_that_loses_its_digits(
    target: &str,
    typed: &str,
    parsed: &serde_json::Value,
) -> Result<(), EditError> {
    let serde_json::Value::Number(number) = parsed else {
        return Ok(());
    };
    // Fractions and exponents are not integer literals.
    let digits = typed.trim();
    let unsigned = digits.strip_prefix('-').unwrap_or(digits);
    if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(());
    }
    // Falling back to a float loses integer precision.
    if number.is_i64() || number.is_u64() {
        return Ok(());
    }
    Err(EditError::new(
        target,
        format!(
            "{digits} is too many digits for a JSON number: it would be sent as {parsed}, \
             which is a different value. Quote it (`{target}=\"{digits}\"`) to send the text, \
             or use `body.raw` to write a body this cannot build"
        ),
    ))
}

/// Render a JSON value the way a person reads it: strings bare, the rest as JSON.
fn render_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Walk a dotted path. A numeric segment indexes an array.
fn json_at<'a>(document: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut at = document;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        at = match at {
            serde_json::Value::Object(map) => map.get(segment)?,
            serde_json::Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(at)
}

fn json_set(
    document: &mut serde_json::Value,
    path: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let Some((last, parents)) = segments.split_last() else {
        *document = value;
        return Ok(());
    };
    let mut at = document;
    for segment in parents {
        at = match at {
            serde_json::Value::Object(map) => map
                .entry(segment.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new())),
            serde_json::Value::Array(items) => {
                let index: usize = segment
                    .parse()
                    .map_err(|_| format!("`{segment}` is not an index, and that step is an array"))?;
                items
                    .get_mut(index)
                    .ok_or_else(|| format!("this body's array has no index {index}"))?
            }
            _ => return Err(format!("`{segment}` is a scalar, so it has no fields")),
        };
    }
    match at {
        serde_json::Value::Object(map) => {
            map.insert(last.to_string(), value);
            Ok(())
        }
        serde_json::Value::Array(items) => {
            let index: usize = last
                .parse()
                .map_err(|_| format!("`{last}` is not an index, and that step is an array"))?;
            match items.get_mut(index) {
                Some(slot) => {
                    *slot = value;
                    Ok(())
                }
                None => Err(format!("this body's array has no index {index}")),
            }
        }
        _ => Err(format!("`{last}` sits under a scalar, which has no fields")),
    }
}

fn json_remove(document: &mut serde_json::Value, path: &str) -> Result<(), String> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let Some((last, parents)) = segments.split_last() else {
        return Err("there is nothing to remove without a path".to_string());
    };
    let mut at = document;
    for segment in parents {
        at = match at {
            serde_json::Value::Object(map) => map
                .get_mut(*segment)
                .ok_or_else(|| format!("this body has no `{segment}`"))?,
            serde_json::Value::Array(items) => {
                let index: usize = segment
                    .parse()
                    .map_err(|_| format!("`{segment}` is not an index, and that step is an array"))?;
                items
                    .get_mut(index)
                    .ok_or_else(|| format!("this body's array has no index {index}"))?
            }
            _ => return Err(format!("`{segment}` is a scalar, so it has no fields")),
        };
    }
    match at {
        serde_json::Value::Object(map) => {
            // `shift_remove`: with `preserve_order` the plain one is a swap.
            map.shift_remove(*last);
            Ok(())
        }
        serde_json::Value::Array(items) => {
            let index: usize = last
                .parse()
                .map_err(|_| format!("`{last}` is not an index, and that step is an array"))?;
            if index < items.len() {
                items.remove(index);
            }
            Ok(())
        }
        _ => Err(format!("`{last}` sits under a scalar, which has no fields")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Editable {
        Editable {
            method: "GET".to_string(),
            url: Url::parse("https://app.test/api/users?user_id=123&page=2").unwrap(),
            headers: vec![
                ("Cookie".to_string(), "session=abc; theme=dark".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            body: Vec::new(),
        }
    }

    fn set(spec: &str) -> Edit {
        parse_set(spec).expect("parses")
    }

    #[test]
    fn bracket_array_syntax_is_refused_instead_of_becoming_a_literal_key() {
        let error = parse_set("json.api_keys[0].name=pwn").expect_err("brackets are refused");
        assert!(error.message.contains("bracket array syntax"), "{error}");
        assert!(error.message.contains("json.items.0.name"), "{error}");
        assert!(error.message.contains("--raw-request"), "{error}");
    }

    #[test]
    fn a_query_parameter_is_replaced_in_place() {
        let mut request = request();
        let applied = apply(&mut request, &[set("query.user_id=456")], false).expect("applies");
        assert_eq!(request.url.as_str(), "https://app.test/api/users?user_id=456&page=2");
        assert_eq!(applied[0].was.as_deref(), Some("123"));
        assert!(!applied[0].created);
    }

    /// The failure that costs the most time: a typo that changes nothing and
    /// returns a response that looks like an answer.
    #[test]
    fn a_parameter_that_is_not_there_is_an_error_naming_the_ones_that_are() {
        let mut refused = request();
        let error = apply(&mut refused, &[set("query.userid=456")], false).expect_err("refused");
        assert!(error.message.contains("user_id"), "{}", error.message);
        assert!(error.message.contains("page"), "{}", error.message);
        // ...and it is a different request only when the caller says so.
        let mut created = request();
        let applied = apply(&mut created, &[set("query.userid=456")], true).expect("created");
        assert!(applied[0].created);
        assert!(created.url.as_str().contains("userid=456"));
    }

    #[test]
    fn one_cookie_changes_and_the_others_stay() {
        let mut request = request();
        apply(&mut request, &[set("cookie.session=forged")], false).expect("applies");
        assert_eq!(
            request.header("cookie"),
            Some("session=forged; theme=dark"),
            "the rest of the jar's header survives"
        );
    }

    #[test]
    fn a_json_field_is_typed_the_way_it_reads() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api/user").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"user":{"id":1,"role":"user"},"active":false}"#.to_vec(),
        };
        apply(
            &mut request,
            &[set("json.user.role=admin"), set("json.user.id=99"), set("json.active=true")],
            false,
        )
        .expect("applies");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["user"]["role"], "admin", "bare text stays text");
        assert_eq!(body["user"]["id"], 99, "a number reads as a number");
        assert_eq!(body["active"], true);
    }

    /// Receipts include coerced values.
    #[test]
    fn a_json_value_that_reads_as_a_number_says_what_it_became() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/login").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"password":"x"}"#.to_vec(),
        };
        let applied = apply(
            &mut request,
            &[set("json.password=0e830400451993494058024219903391")],
            false,
        )
        .expect("applies");
        assert_eq!(
            applied[0].value.as_deref(),
            Some("0e830400451993494058024219903391"),
            "the text that was typed is still reported"
        );
        assert_eq!(
            applied[0].encoded.as_deref(),
            Some("0.0"),
            "and so is what the body now carries"
        );
        assert_eq!(request.body, br#"{"password":0.0}"#.to_vec());
    }

    /// Unchanged encodings stay omitted.
    #[test]
    fn a_json_value_that_survives_as_written_says_nothing_extra() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"role":"user","id":1,"active":false}"#.to_vec(),
        };
        let applied = apply(
            &mut request,
            &[set("json.role=admin"), set("json.id=99"), set("json.active=true")],
            false,
        )
        .expect("applies");
        for one in &applied {
            assert_eq!(one.encoded, None, "{} reported an encoding", one.target);
        }
    }

    /// Oversized integers are refused.
    #[test]
    fn an_integer_too_big_for_a_double_is_refused_rather_than_rounded() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"id":1}"#.to_vec(),
        };
        let error = apply(
            &mut request,
            &[set("json.id=123456789012345678901234567890")],
            false,
        )
        .expect_err("refused");
        let spelling = error.to_string();
        assert!(spelling.contains("too many digits"), "{spelling}");
        assert!(spelling.contains("body.raw"), "{spelling}");
        assert_eq!(request.body, br#"{"id":1}"#.to_vec(), "and the body is untouched");
    }

    /// Exact integers remain numbers.
    #[test]
    fn an_integer_a_double_can_hold_is_still_a_number() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"id":1}"#.to_vec(),
        };
        apply(&mut request, &[set("json.id=9007199254740993")], false).expect("applies");
        assert_eq!(request.body, br#"{"id":9007199254740993}"#.to_vec());
    }

    /// Quoting preserves large values as strings.
    #[test]
    fn a_quoted_json_value_is_the_string_it_was_written_as() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/login").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"password":"x"}"#.to_vec(),
        };
        apply(
            &mut request,
            &[set("json.password=\"0e830400451993494058024219903391\"")],
            false,
        )
        .expect("applies");
        assert_eq!(
            request.body,
            br#"{"password":"0e830400451993494058024219903391"}"#.to_vec(),
            "the magic hash reaches the wire as the string it is"
        );
    }

    /// The API call a page never makes has to be composable, not hand-written.
    #[test]
    fn a_json_body_can_be_built_where_there_was_none() {
        let mut request = Editable {
            method: "GET".to_string(),
            url: Url::parse("https://app.test/check").unwrap(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        apply(
            &mut request,
            &[set("method=POST"), set("json.service_name=-t custom \"id\"")],
            true,
        )
        .expect("builds");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.content_type(),
            Some("application/json"),
            "and it declares what it now is"
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
        assert_eq!(body["service_name"], "-t custom \"id\"");
    }

    /// Without `--create` an empty body is still an empty body: the caller
    /// did not ask for one to be invented.
    #[test]
    fn an_empty_body_is_not_quietly_turned_into_json() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/check").unwrap(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert!(apply(&mut request, &[set("json.a=1")], false).is_err());
    }

    #[test]
    fn a_json_edit_against_a_body_that_is_not_json_names_what_it_is() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/upload").unwrap(),
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: b"hello".to_vec(),
        };
        let error = apply(&mut request, &[set("json.a=1")], false).expect_err("refused");
        assert!(error.message.contains("text/plain"), "{}", error.message);
    }

    #[test]
    fn a_form_field_round_trips_through_encoding() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/login").unwrap(),
            headers: vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: b"username=alice&password=hunter2".to_vec(),
        };
        apply(&mut request, &[set("form.username=admin' OR '1'='1")], false).expect("applies");
        let pairs: Vec<(String, String)> =
            form_urlencoded::parse(&request.body).into_owned().collect();
        assert_eq!(pairs[0].1, "admin' OR '1'='1", "the payload survives encoding");
        assert_eq!(pairs[1].1, "hunter2");
    }

    /// The file-upload test, built from nothing: the engine never posts a file
    /// itself, so there is usually no recorded upload to start from.
    #[test]
    fn an_upload_can_be_built_where_there_was_no_body() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/upload").unwrap(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        apply(
            &mut request,
            &[
                set("multipart.file=<?php system($_GET[0]); ?>"),
                set("multipart.file.filename=shell.php.png"),
                set("multipart.file.content_type=image/png"),
            ],
            true,
        )
        .expect("builds");

        let kind = request.content_type().expect("a content type was set");
        let boundary = crate::multipart::boundary_of(kind).expect("with a boundary");
        let parts = crate::multipart::parse(&request.body, &boundary).expect("well formed");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "file");
        assert_eq!(parts[0].filename.as_deref(), Some("shell.php.png"));
        assert_eq!(parts[0].content_type.as_deref(), Some("image/png"));
        assert_eq!(parts[0].data, b"<?php system($_GET[0]); ?>");
    }

    /// The three pieces are edited apart, because a server checks them apart.
    #[test]
    fn one_piece_of_an_upload_changes_and_the_rest_stays() {
        let boundary = "----abc";
        let original = crate::multipart::serialize(
            &[
                crate::multipart::Part {
                    name: "csrf".to_string(),
                    data: b"tok_1".to_vec(),
                    ..Default::default()
                },
                crate::multipart::Part {
                    name: "avatar".to_string(),
                    filename: Some("cat.png".to_string()),
                    content_type: Some("image/png".to_string()),
                    data: b"\x89PNG data".to_vec(),
                    ..Default::default()
                },
            ],
            boundary,
        );
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/upload").unwrap(),
            headers: vec![(
                "Content-Type".to_string(),
                format!("multipart/form-data; boundary={boundary}"),
            )],
            body: original,
        };

        let applied = apply(
            &mut request,
            &[set("multipart.avatar.filename=../../etc/passwd")],
            false,
        )
        .expect("applies");
        assert_eq!(applied[0].was.as_deref(), Some("cat.png"));

        let kind = request.content_type().unwrap().to_string();
        let boundary = crate::multipart::boundary_of(&kind).unwrap();
        let parts = crate::multipart::parse(&request.body, &boundary).unwrap();
        assert_eq!(parts[0].data, b"tok_1", "the other field is untouched");
        assert_eq!(parts[1].filename.as_deref(), Some("../../etc/passwd"));
        assert_eq!(parts[1].content_type.as_deref(), Some("image/png"));
        assert_eq!(parts[1].data, b"\x89PNG data", "the file itself is untouched");
    }

    /// `.name` is not `.filename`, and the difference has to be visible.
    #[test]
    fn a_near_miss_on_a_piece_name_is_caught_by_the_create_rule() {
        let boundary = "----abc";
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/upload").unwrap(),
            headers: vec![(
                "Content-Type".to_string(),
                format!("multipart/form-data; boundary={boundary}"),
            )],
            body: crate::multipart::serialize(
                &[crate::multipart::Part {
                    name: "file".to_string(),
                    filename: Some("cat.png".to_string()),
                    data: b"data".to_vec(),
                    ..Default::default()
                }],
                boundary,
            ),
        };
        let error = apply(&mut request, &[set("multipart.file.name=shell.php")], false)
            .expect_err("refused");
        assert!(
            error.message.contains("This request has: file"),
            "the error names the part that does exist: {}",
            error.message
        );
    }

    #[test]
    fn a_multipart_edit_on_a_body_that_is_not_one_says_so() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/upload").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: b"{}".to_vec(),
        };
        let error = apply(&mut request, &[set("multipart.file=x")], false).expect_err("refused");
        assert!(error.message.contains("application/json"), "{}", error.message);
    }

    #[test]
    fn a_framing_header_is_refused_here_too() {
        let mut request = request();
        let error = apply(&mut request, &[set("header.Content-Length=999")], false)
            .expect_err("refused");
        assert!(error.message.contains("client's to compute"), "{}", error.message);
    }

    #[test]
    fn a_value_may_contain_the_separator() {
        let edit = set("query.q=a=b=c");
        assert_eq!(edit.value.as_deref(), Some(b"a=b=c".as_slice()));
    }

    #[test]
    fn an_unknown_target_says_what_the_targets_are() {
        let error = parse_set("headers.X=1").expect_err("refused");
        assert!(error.message.contains("header."), "{}", error.message);
        // `multipart.<field>` where the field itself contains a dot is legal, so
        // only `filename` and `content_type` are read as pieces. A near miss
        // like `.name` is a field name, and the create rule is what catches it:
        // see `a_near_miss_on_a_piece_name_is_caught_by_the_create_rule`.
        let edit = parse_set("multipart.file.name=x").expect("parses as a field");
        assert_eq!(
            edit.target,
            Target::Multipart {
                field: "file.name".to_string(),
                piece: Piece::Data
            }
        );
    }

    #[test]
    fn edits_apply_in_the_order_they_were_given() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api").unwrap(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: b"{}".to_vec(),
        };
        apply(
            &mut request,
            &[
                Edit {
                    target: Target::BodyRaw,
                    value: Some(br#"{"role":"user"}"#.to_vec()),
                },
                set("json.role=admin"),
            ],
            false,
        )
        .expect("applies");
        assert_eq!(request.body, br#"{"role":"admin"}"#.to_vec());
    }

    /// A traversal payload that the URL parser resolves is a request nobody
    /// asked for. Sending it and reporting its answer would be a false
    /// negative wearing the shape of evidence.
    /// `path=/reset.php?token=…` is a natural thing to type and a different
    /// request from the one meant: `?` is not in a path, so it is escaped, and
    /// the server is asked for a file whose name contains a question mark.
    #[test]
    fn a_query_inside_a_path_edit_is_refused_and_the_right_target_is_named() {
        let mut request = request();
        let error = apply(&mut request, &[set("path=/reset.php?token=abc")], false)
            .expect_err("a path with a query in it is refused");
        assert!(error.to_string().contains("query.<name>="), "{error}");
        assert!(error.to_string().contains("url="), "{error}");
        assert_eq!(
            request.url.as_str(),
            "https://app.test/api/users?user_id=123&page=2",
            "a refused edit leaves the request alone"
        );
    }

    #[test]
    fn a_path_the_url_parser_would_resolve_is_refused_rather_than_straightened() {
        for spelling in [
            "path=/cgi-bin/.%2e/.%2e/etc/passwd",
            "path=/cgi-bin/%2e%2e/%2e%2e/etc/passwd",
            "path=/a/../etc/passwd",
            "path=/a/./b",
            "url=https://app.test/cgi-bin/.%2E/.%2E/etc/passwd",
        ] {
            let mut request = request();
            let error = apply(&mut request, &[set(spelling)], false)
                .expect_err("`{spelling}` should be refused");
            assert!(
                error.to_string().contains("dot segment"),
                "{spelling}: {error}"
            );
            assert_eq!(
                request.url.as_str(),
                "https://app.test/api/users?user_id=123&page=2",
                "a refused edit leaves the request alone"
            );
        }
    }

    /// The check above splits on `/`, and `..\..\etc` holds none — so
    /// `path=/cgi-bin/..\..\etc/passwd` went out as `/etc/passwd`.
    #[test]
    fn a_backslash_is_a_separator_too_and_is_refused_with_it() {
        for spelling in [
            "path=/cgi-bin/..\\..\\etc/passwd",
            "path=/a\\b",
            "url=https://app.test/cgi-bin/..\\..\\etc/passwd",
            "url=https://app.test\\..\\..\\x",
        ] {
            let mut request = request();
            let error = apply(&mut request, &[set(spelling)], false)
                .expect_err("a backslash the parser rewrites is refused");
            assert!(error.to_string().contains("backslash"), "{spelling}: {error}");
            assert_eq!(
                request.url.as_str(),
                "https://app.test/api/users?user_id=123&page=2",
                "a refused edit leaves the request alone"
            );
        }
    }

    /// A CR or LF in a header value came back as "builder error", which names
    /// neither the header nor the character.
    #[test]
    fn a_header_that_could_not_be_sent_is_refused_by_name() {
        for spelling in [
            "header.X-A=one\r\nX-Injected: yes",
            "header.X-A=one\nX-Injected: yes",
            "cookie.sid=a\r\nX-Injected: yes",
        ] {
            let mut request = request();
            let error = apply(&mut request, &[set(spelling)], true)
                .expect_err("a header that cannot be sent is refused");
            assert!(error.to_string().contains("--raw-request"), "{spelling}: {error}");
            assert_eq!(
                request.header("cookie"),
                Some("session=abc; theme=dark"),
                "a refused edit leaves the request alone"
            );
        }
        // A name that is not a token goes the same way, and says which.
        let mut request = request();
        let error = apply(&mut request, &[set("header.X A=1")], true)
            .expect_err("a name with a space in it is not a header name");
        assert!(error.to_string().contains("not a header name"), "{error}");
    }

    /// Replay claims nothing else changed. Re-encoding the query broke that
    /// for every parameter the caller did not name: `?debug` went out as
    /// `debug=`, `x[]=1` as `x%5B%5D=1`.
    #[test]
    fn editing_one_query_parameter_leaves_the_others_byte_for_byte() {
        let mut request = Editable {
            method: "GET".to_string(),
            url: Url::parse("https://app.test/a?debug&next=%2Fadmin&q=a+b&x[]=1&keep=A")
                .expect("a url"),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let applied = apply(&mut request, &[set("query.keep=B")], false).expect("applies");
        assert_eq!(applied[0].was.as_deref(), Some("A"));
        assert_eq!(
            request.url.query(),
            Some("debug&next=%2Fadmin&q=a+b&x[]=1&keep=B")
        );
    }

    /// A parameter that is not there is still added, and a removal still takes
    /// every copy.
    #[test]
    fn a_query_parameter_is_added_and_removed_without_touching_the_rest() {
        let mut request = Editable {
            method: "GET".to_string(),
            url: Url::parse("https://app.test/a?debug&id=1&id=2").expect("a url"),
            headers: Vec::new(),
            body: Vec::new(),
        };
        apply(&mut request, &[set("query.role=admin")], true).expect("adds");
        assert_eq!(request.url.query(), Some("debug&id=1&id=2&role=admin"));
        apply(&mut request, &[parse_unset("query.id").expect("parses")], false)
            .expect("removes");
        assert_eq!(request.url.query(), Some("debug&role=admin"));
    }

    /// The query's rule, for the same reason: a body differing in a field the
    /// caller never named is a different request.
    #[test]
    fn editing_one_form_field_leaves_the_others_byte_for_byte() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/login").expect("a url"),
            headers: vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: b"remember&next=%2Fadmin&tags[]=a&user=alice".to_vec(),
        };
        let applied = apply(&mut request, &[set("form.user=admin")], false).expect("applies");
        assert_eq!(applied[0].was.as_deref(), Some("alice"));
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            "remember&next=%2Fadmin&tags[]=a&user=admin"
        );
    }

    /// Parsing into pairs threw away every bare token, so editing one cookie
    /// deleted another the caller never named — a logout inside a measurement.
    #[test]
    fn a_bare_cookie_survives_an_edit_to_a_different_one() {
        let mut request = Editable {
            method: "GET".to_string(),
            url: Url::parse("https://app.test/a").expect("a url"),
            headers: vec![(
                "Cookie".to_string(),
                "session=abc; beta; theme=dark".to_string(),
            )],
            body: Vec::new(),
        };
        apply(&mut request, &[set("cookie.theme=light")], false).expect("applies");
        assert_eq!(
            request.header("cookie"),
            Some("session=abc; beta; theme=light")
        );
    }

    /// And an edit naming one cookie but writing two is refused, not escaped.
    #[test]
    fn a_cookie_edit_that_would_write_two_is_refused() {
        let mut request = request();
        let error = apply(&mut request, &[set("cookie.session=a; admin=1")], false)
            .expect_err("a `;` in a cookie value writes a second cookie");
        assert!(error.to_string().contains("header.Cookie="), "{error}");
        assert_eq!(request.header("cookie"), Some("session=abc; theme=dark"));
    }

    /// A JSON edit rewrites the body, and `serde_json`'s default map sorts the
    /// keys — so a signed body was replayed as a different one, and the 401
    /// that came back read as the finding.
    #[test]
    fn a_json_edit_keeps_the_fields_in_the_order_they_were_in() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api").expect("a url"),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"user":"alice","role":"user","nonce":"z9"}"#.to_vec(),
        };
        apply(&mut request, &[set("json.role=\"admin\"")], false).expect("applies");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            r#"{"user":"alice","role":"admin","nonce":"z9"}"#
        );
    }

    /// And a removal shifts the rest along rather than swapping in the last.
    #[test]
    fn removing_a_json_field_leaves_the_others_in_order() {
        let mut request = Editable {
            method: "POST".to_string(),
            url: Url::parse("https://app.test/api").expect("a url"),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: br#"{"a":1,"b":2,"c":3,"d":4}"#.to_vec(),
        };
        apply(
            &mut request,
            &[parse_unset("json.b").expect("parses")],
            false,
        )
        .expect("removes");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            r#"{"a":1,"c":3,"d":4}"#
        );
    }

    /// The rule every refusal here is written to: an edit that errors changed
    /// nothing, or the next attempt edits a half-edited request.
    #[test]
    fn an_edit_that_is_refused_leaves_the_request_exactly_as_it_was() {
        let targets = [
            "method", "url", "path", "query.a", "query.", "header.X", "header.X A",
            "cookie.s", "cookie.s;x", "json.a", "json.a.b", "json.", "form.f",
            "multipart.f", "multipart.f.filename", "multipart.f.content_type",
            "body.raw", "body.other", "nonsense", "", ".", "..", "query.a.b",
        ];
        let values = [
            "", "1", "..", "/a/../b", "\\\\..\\\\", "https://app.test/../x", "not a url",
            "a\r\nb", "a; b=c", "{\"a\":1}", "%2e%2e", "@", "=", "a=b",
        ];
        let bodies: [&[u8]; 4] = [b"", b"{\"a\":{\"b\":1}}", b"a=1&b=2", b"\xff\xd8not text"];

        for body in bodies {
            for target in targets {
                for value in values {
                    for create in [false, true] {
                        for removing in [false, true] {
                            let mut request = request();
                            request.body = body.to_vec();
                            let before = request.clone();
                            let edit = if removing {
                                parse_unset(target)
                            } else {
                                parse_set(&format!("{target}={value}"))
                            };
                            let Ok(edit) = edit else { continue };
                            if apply(&mut request, &[edit], create).is_err() {
                                assert_eq!(
                                    request, before,
                                    "`{target}` = `{value}` (create={create}, \
                                     removing={removing}) was refused and still \
                                     changed the request"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// The editors slice strings a target or caller chose. Multi-byte
    /// characters, lone separators and empty pieces are where that goes wrong.
    #[test]
    fn no_arrangement_of_these_edits_panics() {
        let targets = [
            "query.a", "query.é", "query.", "form.f", "form.é", "cookie.s", "cookie.é",
            "json.a", "body.raw", "header.X", "method", "path", "url",
        ];
        let values = [
            "", "=", "&", ";", " ", "é", "🙂", "a=b&c", "a; b", "\u{a0}x", "%", "%zz",
            "\u{feff}", "..", "/a/../b",
        ];
        // Bodies and URLs built out of the same sharp pieces.
        let queries = ["", "?a", "?a=1", "?é=1&a", "?=1", "?&&", "?a=%", "?🙂=1"];
        let bodies: [&[u8]; 6] = [
            b"",
            b"a=1&b",
            b"=1",
            b"&&",
            "é=1&🙂=2".as_bytes(),
            b"\xff\xfe not text",
        ];
        let cookies = ["", "s=1", " s = 1 ; t", ";;", "é=1", "s"];

        let mut cases = 0usize;
        for query in queries {
            for body in bodies {
                for cookie in cookies {
                    for target in targets {
                        for value in values {
                            for create in [false, true] {
                                let mut request = Editable {
                                    method: "POST".to_string(),
                                    url: Url::parse(&format!("https://app.test/p{query}"))
                                        .expect("a url"),
                                    headers: vec![(
                                        "Cookie".to_string(),
                                        cookie.to_string(),
                                    )],
                                    body: body.to_vec(),
                                };
                                let spec = format!("{target}={value}");
                                if let Ok(edit) = parse_set(&spec) {
                                    let applied = apply(&mut request, &[edit], create);
                                    cases += 1;
                                    // An edit that succeeded put the value
                                    // where it said it did — the half a panic
                                    // hunt misses.
                                    if applied.is_ok() {
                                        if let Some(name) = target.strip_prefix("query.") {
                                            assert!(
                                                request
                                                    .url
                                                    .query_pairs()
                                                    .any(|(k, v)| k == name && v == value),
                                                "`{spec}` did not read back: {:?}",
                                                request.url.query()
                                            );
                                        }
                                        if let Some(name) = target.strip_prefix("form.") {
                                            assert!(
                                                form_urlencoded::parse(&request.body)
                                                    .any(|(k, v)| k == name && v == value),
                                                "`{spec}` did not read back: {:?}",
                                                String::from_utf8_lossy(&request.body)
                                            );
                                        }
                                        if let Some(name) = target.strip_prefix("cookie.") {
                                            let header =
                                                request.header("cookie").unwrap_or_default();
                                            assert!(
                                                header.split(';').any(|piece| {
                                                    cookie_name_and_value(piece)
                                                        == (name.to_string(), value.to_string())
                                                }),
                                                "`{spec}` did not read back: {header:?}"
                                            );
                                        }
                                    }
                                }
                                if let Ok(edit) = parse_unset(target) {
                                    let _ = apply(&mut request, &[edit], create);
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(cases > 10_000, "the sweep actually ran: {cases}");
    }

    /// Every reader strips whitespace around a pair, so `cookie.s= ` went out
    /// as `s= ` and arrived as `s=`. Found by the sweep below.
    #[test]
    fn a_cookie_value_the_header_cannot_carry_is_refused() {
        for spec in ["cookie.s= ", "cookie.s=  a", "cookie.s=a "] {
            let mut request = request();
            let error = apply(&mut request, &[set(spec)], true)
                .expect_err("a value the header cannot carry is refused");
            assert!(error.to_string().contains("header.Cookie="), "{spec}: {error}");
            assert_eq!(request.header("cookie"), Some("session=abc; theme=dark"));
        }
        // Whitespace inside the value is ordinary and survives.
        let mut request = request();
        apply(&mut request, &[set("cookie.theme=a b")], false).expect("applies");
        assert_eq!(request.header("cookie"), Some("session=abc; theme=a b"));
    }

    /// And an ordinary path still goes through, dots in a filename included.
    #[test]
    fn a_path_with_no_dot_segment_is_set_as_asked() {
        let mut request = request();
        apply(&mut request, &[set("path=/static/app.min.js")], false).expect("applies");
        assert_eq!(request.url.path(), "/static/app.min.js");
    }

    /// The bytes a command line cannot carry, which is the whole reason
    /// `--set-file` exists.
    #[test]
    fn a_byte_valued_edit_carries_bytes_that_are_not_text() {
        let mut request = request();
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, b'x'];
        let edit = parse_set_bytes("body.raw", jpeg.clone()).expect("parses");
        apply(&mut request, &[edit], false).expect("applies");
        assert_eq!(request.body, jpeg);
    }
}
