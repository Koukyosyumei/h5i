//! The request log's row: the decision written before the wire, and the
//! outcome written after it.
//!
//! Data only. The sinks that durably write these rows, and the fail-closed rule
//! that a fetch stops when the write does, stay with the engine in
//! `h5i_browser::receipt`.

use serde::{Deserialize, Serialize};

/// Whether this record was written before the wire or after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The decision: what was asked for, and whether policy permitted it.
    /// Written before any bytes move.
    Request,
    /// The outcome: status, size, duration, or the error that ended it.
    Response,
}

/// Why the engine asked for this URL. A proxy sees only the request; the
/// engine knows whether it was the page the user named, a stylesheet the
/// document pulled in, or a redirect hop, and that distinction is most of
/// what makes the record worth reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Initiator {
    /// The top-level document the caller asked to open.
    Navigation,
    /// A subresource the document referenced (stylesheet, image, font).
    Subresource,
    /// A frame's document, fetched to be flattened into the page (§B21).
    ///
    /// Its own name in the receipt rather than `subresource`, because an
    /// auditor asking "did this page pull in another *document*" is asking a
    /// different question from "did it load its stylesheet". A frame is the
    /// one subresource whose content is someone else's whole page.
    Frame,
    /// A hop the server asked for via `Location`.
    Redirect,
    /// The agent sending a message again, through `resend`.
    ///
    /// Its own name because a replay recorded as a navigation said the browser
    /// went somewhere it never went. This is the field a reviewer reads to tell
    /// what the application did from what the tester did.
    Replay,
}

/// The engine's clock, RFC3339 with microseconds.
///
/// Public because the capture store stamps its messages from the same
/// clock: two artifacts written about one fetch that disagreed about when
/// it happened would be two artifacts nobody could join.
///
/// Microseconds because a page's subresource fetches land inside the same
/// second, and a log an audit sorts by needs a total order within one verb.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// One line of the request log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    pub seq: u64,
    /// When the engine wrote this row, RFC3339.
    ///
    /// The engine's own claim, not an observation. A reader outside the box
    /// has no way to check the box's clock, so this is what orders the engine's
    /// two logs against each other and what a host-side reader labels as a
    /// claim when it puts them beside rows h5i wrote itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    pub phase: Phase,
    pub initiator: Initiator,
    pub method: String,
    pub url: String,
    /// `true` when policy permitted the request. A denied request never
    /// reaches the wire, so it has a `Request` record and a `Response` record
    /// describing the refusal, and no bytes between them.
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The body's size *after* decoding, which is what the page received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// What actually crossed the wire, when that is a different number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_bytes: Option<u64>,
    /// How the body was encoded on the wire, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    /// The whole fetch: asked to body in hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Time to the response's *headers*, which is a different question and the
    /// one a timing test asks.
    ///
    /// A server that decides slowly and then streams a large body answers late
    /// with a fast total; a fast decision followed by a slow transfer answers
    /// early with a slow one. Blind injection is timing the decision, so
    /// collapsing the two into a single duration would hide the signal under
    /// the size of the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How many cookies this request carried. A count, never a value. The
    /// log is read by people and shipped in exports, and a credential in a
    /// receipt is a credential in a bug report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies_sent: Option<usize>,
    /// How many the response stored, after the jar refused what it refuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies_stored: Option<usize>,
    /// How many bytes the connection carried *after* this response ended.
    ///
    /// A count, like everything else here. Absent for every ordinary fetch,
    /// because one request gets one response; present when a raw send
    /// desynchronised a proxy from its backend and the socket answered twice.
    /// The bytes themselves are in the message store, which is where content
    /// lives; this is the line in the log that says to go and look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_bytes: Option<u64>,
    /// Headers the caller asked for and this engine computed instead.
    ///
    /// Names, never values, like everything else here. Three headers frame the
    /// message and are the client's to compute (`content-length`,
    /// `transfer-encoding`, `connection`); a caller setting one is told so here
    /// rather than left to believe a request went out shaped the way it asked.
    /// Empty for every request nobody tried to reshape, which is nearly all of
    /// them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers_overridden: Vec<String>,
}

impl RequestRecord {
    /// The decision record, written before the wire.
    pub fn request(seq: u64, initiator: Initiator, method: &str, url: &str) -> Self {
        Self {
            seq,
            at: now_rfc3339(),
            phase: Phase::Request,
            initiator,
            method: method.to_string(),
            url: url.to_string(),
            allowed: true,
            denied_reason: None,
            status: None,
            bytes: None,
            wire_bytes: None,
            content_encoding: None,
            duration_ms: None,
            ttfb_ms: None,
            error: None,
            cookies_sent: None,
            cookies_stored: None,
            trailing_bytes: None,
            headers_overridden: Vec::new(),
        }
    }

    pub fn denied(mut self, reason: &str) -> Self {
        self.allowed = false;
        self.denied_reason = Some(reason.to_string());
        self
    }

    /// The outcome record, written after the wire (or instead of it, when the
    /// request was refused).
    pub fn response(&self) -> Self {
        let mut next = self.clone();
        next.phase = Phase::Response;
        // Its own time, not the request's: the gap between them is how long the
        // fetch took, and copying the request's stamp would erase it.
        next.at = now_rfc3339();
        next
    }

    /// Render as the one-line form the CLI prints and a viewer pane shows.
    pub fn render(&self) -> String {
        if !self.allowed {
            let why = self.denied_reason.as_deref().unwrap_or("denied");
            return format!("DENIED {} {} — {why}", self.method, self.url);
        }
        match (self.status, self.error.as_deref()) {
            (_, Some(err)) => format!("ERROR  {} {} — {err}", self.method, self.url),
            (Some(status), None) => {
                let bytes = self.bytes.unwrap_or(0);
                let ms = self.duration_ms.unwrap_or(0);
                // Both numbers when they differ, because "184 KB" and "43 KB
                // on the wire" are two facts a reader wants and one line that
                // showed either alone would be answering the other's question.
                let size = match (self.wire_bytes, self.content_encoding.as_deref()) {
                    (Some(wire), Some(encoding)) => {
                        format!("{bytes} bytes, {wire} on the wire {encoding}")
                    }
                    (None, Some(encoding)) => format!("{bytes} bytes {encoding}"),
                    _ => format!("{bytes} bytes"),
                };
                format!("{status:>6} {} {} ({size}, {ms}ms)", self.method, self.url)
            }
            (None, None) => format!("       {} {}", self.method, self.url),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_record_becomes_a_response_record_without_losing_identity() {
        let req = RequestRecord::request(7, Initiator::Navigation, "GET", "https://example.com/");
        let mut resp = req.response();
        resp.status = Some(200);
        resp.bytes = Some(1234);

        assert_eq!(resp.seq, 7, "the pair must be joinable by seq");
        assert_eq!(resp.url, req.url);
        assert_eq!(req.phase, Phase::Request);
        assert_eq!(resp.phase, Phase::Response);
    }

    #[test]
    fn denial_is_carried_on_the_record_not_inferred_from_a_missing_status() {
        let rec = RequestRecord::request(1, Initiator::Subresource, "GET", "https://tracker.test/p")
            .denied("origin `https://tracker.test` is not in the allowlist");
        assert!(!rec.allowed);
        assert!(rec.render().starts_with("DENIED"));
        assert!(rec.render().contains("not in the allowlist"));
    }
}
