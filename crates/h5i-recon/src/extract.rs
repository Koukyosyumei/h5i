//! Endpoint candidates read out of stored bytes, so extraction keeps working
//! after the budget is spent (design-recon.md N8).
//!
//! The scanner is ours because a borrowed parser would sit where the target's
//! bytes arrive, in the process holding the jar (N19). It is a tag reader, not
//! an HTML parser: no tree, and no claim to agree with the engine's DOM.

use url::Url;

use crate::ledger::{Observation, Param, Source, State, Where};

/// The most candidates one document may contribute.
///
/// A generated page can carry a hundred thousand links, and the ledger is a
/// file on the operator's disk.
pub const MAX_PER_DOCUMENT: usize = 5_000;

/// The longest attribute value worth resolving as a URL.
const MAX_URL_BYTES: usize = 4 * 1024;

/// The tag sequence, hashed: one template's renderings share it whatever they
/// say, which is how a soft 404 is spotted (design-recon.md N11).
pub fn skeleton(html: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut tags = 0usize;
    for tag in Tags::new(html) {
        if tags >= 500 {
            break;
        }
        tags += 1;
        hasher.update(tag.name.as_bytes());
        hasher.update(b"/");
    }
    if tags == 0 {
        return String::new();
    }
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// One disclosed endpoint, before it becomes a ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Absolute, resolved against the document's base.
    pub url: Url,
    pub method: String,
    /// Names this disclosure named. A form's inputs, or a URL's own query.
    pub params: Vec<Param>,
    /// The element or header that disclosed it, for the reader who asks why.
    pub how: &'static str,
}

/// Candidates disclosed by a document's markup.
///
/// `base` is the response's *final* URL: resolving against the requested one
/// would attribute a redirected page's links to the origin it left.
pub fn from_html(base: &Url, html: &str) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let mut resolve_base = base.clone();
    let mut form: Option<OpenForm> = None;

    for tag in Tags::new(html) {
        if found.len() >= MAX_PER_DOCUMENT {
            break;
        }
        match tag.name.as_str() {
            "base" => {
                if let Some(href) = tag.attr("href")
                    && let Some(url) = resolve(&resolve_base, href)
                {
                    resolve_base = url;
                }
            }
            "a" | "area" => push(&mut found, &resolve_base, tag.attr("href"), "GET", "link"),
            "link" => push(&mut found, &resolve_base, tag.attr("href"), "GET", "link-rel"),
            "script" => push(&mut found, &resolve_base, tag.attr("src"), "GET", "script-src"),
            "iframe" | "frame" => push(&mut found, &resolve_base, tag.attr("src"), "GET", "frame"),
            "img" | "audio" | "video" | "source" | "embed" | "track" => {
                push(&mut found, &resolve_base, tag.attr("src"), "GET", "media");
                if let Some(srcset) = tag.attr("srcset") {
                    for candidate in srcset.split(',') {
                        let url = candidate.trim().split_whitespace().next();
                        push(&mut found, &resolve_base, url, "GET", "srcset");
                    }
                }
            }
            "form" => {
                // A form with no action posts back to the document it is in,
                // which is a real endpoint and the one most often missed.
                let action = tag.attr("action").unwrap_or("");
                let target = if action.trim().is_empty() {
                    Some(resolve_base.clone())
                } else {
                    resolve(&resolve_base, action)
                };
                let method = tag
                    .attr("method")
                    .map(|m| m.trim().to_ascii_uppercase())
                    .filter(|m| m == "POST" || m == "GET")
                    .unwrap_or_else(|| "GET".to_string());
                form = target.map(|url| OpenForm {
                    url,
                    method,
                    params: Vec::new(),
                });
            }
            "input" | "select" | "textarea" | "button" => {
                if let Some(open) = form.as_mut()
                    && let Some(name) = tag.attr("name")
                    && !name.is_empty()
                {
                    let at = if open.method == "GET" {
                        Where::Query
                    } else {
                        Where::Form
                    };
                    let param = Param {
                        name: name.to_string(),
                        at,
                    };
                    if !open.params.contains(&param) {
                        open.params.push(param);
                    }
                }
            }
            "/form" => {
                if let Some(open) = form.take() {
                    let mut params = open.params;
                    add_query_params(&open.url, &mut params);
                    found.push(Found {
                        url: open.url,
                        method: open.method,
                        params,
                        how: "form",
                    });
                }
            }
            "meta" => {
                // `<meta http-equiv=refresh content="0; url=/next">` is a
                // navigation the page performs on its own.
                let equiv = tag.attr("http-equiv").unwrap_or("").to_ascii_lowercase();
                if equiv == "refresh"
                    && let Some(content) = tag.attr("content")
                    && let Some((_, rest)) = content.split_once(';')
                {
                    let target = rest.trim().trim_start_matches("url=").trim_start_matches("URL=");
                    push(&mut found, &resolve_base, Some(target), "GET", "meta-refresh");
                }
            }
            _ => {}
        }
    }

    // A document that ends mid-form still disclosed the form.
    if let Some(open) = form.take() {
        let mut params = open.params;
        add_query_params(&open.url, &mut params);
        found.push(Found {
            url: open.url,
            method: open.method,
            params,
            how: "form",
        });
    }
    found
}

/// Candidates disclosed by a response's headers.
pub fn from_headers(base: &Url, headers: &[(String, String)]) -> Vec<Found> {
    let mut found = Vec::new();
    for (name, value) in headers {
        match name.to_ascii_lowercase().as_str() {
            "location" | "content-location" => {
                push(&mut found, base, Some(value), "GET", "location")
            }
            "link" => {
                for part in value.split(',') {
                    let target = part.trim().trim_start_matches('<');
                    if let Some((url, _)) = target.split_once('>') {
                        push(&mut found, base, Some(url), "GET", "link-header");
                    }
                }
            }
            "content-security-policy" | "content-security-policy-report-only" => {
                // `report-uri` names an endpoint that appears nowhere else.
                for directive in value.split(';') {
                    let directive = directive.trim();
                    if let Some(rest) = directive.strip_prefix("report-uri") {
                        for target in rest.split_whitespace() {
                            push(&mut found, base, Some(target), "POST", "csp-report-uri");
                        }
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// URL-shaped strings in a body that is not markup: absolute URLs and
/// root-relative paths only, because a ledger of guesses is worse than a short
/// one.
pub fn from_json(base: &Url, body: &str) -> Vec<Found> {
    let mut found = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() && found.len() < MAX_PER_DOCUMENT {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'"' {
            // A JSON escape cannot end a string.
            if bytes[end] == b'\\' {
                end += 1;
            }
            end += 1;
        }
        if end > bytes.len() {
            break;
        }
        let literal = &body[start..end.min(body.len())];
        if is_url_shaped(literal) {
            push(&mut found, base, Some(literal), "GET", "json-string");
        }
        i = end + 1;
    }
    found
}

/// Turn disclosures into ledger rows.
///
/// Always [`State::Candidate`]: something said this exists, and nothing has
/// been there. That is the line the whole ledger is arranged around.
pub fn candidates(found: &[Found], identity: &str, source: &Source) -> Vec<Observation> {
    found
        .iter()
        .map(|item| {
            let origin = crate::ingest::origin_of(&item.url);
            let mut params = item.params.clone();
            add_query_params(&item.url, &mut params);
            Observation::new(
                &origin,
                item.url.path(),
                &item.method,
                identity,
                State::Candidate,
                source.clone(),
            )
            .with_params(params)
        })
        .collect()
}

struct OpenForm {
    url: Url,
    method: String,
    params: Vec<Param>,
}

fn add_query_params(url: &Url, params: &mut Vec<Param>) {
    for (name, _) in url.query_pairs() {
        let param = Param {
            name: name.into_owned(),
            at: Where::Query,
        };
        if !params.contains(&param) {
            params.push(param);
        }
    }
}

fn push(found: &mut Vec<Found>, base: &Url, raw: Option<&str>, method: &str, how: &'static str) {
    let Some(raw) = raw else { return };
    let Some(url) = resolve(base, raw) else { return };
    let mut params = Vec::new();
    add_query_params(&url, &mut params);
    let item = Found {
        url,
        method: method.to_string(),
        params,
        how,
    };
    if !found.contains(&item) {
        found.push(item);
    }
}

/// Resolve one attribute value, or decline it. Fragments and `javascript:` are
/// not endpoints, and `candidate` has to keep meaning "a request could go
/// here".
fn resolve(base: &Url, raw: &str) -> Option<Url> {
    let raw = decode_entities(raw.trim());
    if raw.is_empty() || raw.starts_with('#') || raw.len() > MAX_URL_BYTES {
        return None;
    }
    if let Some((scheme, _)) = raw.split_once(':')
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !scheme.is_empty()
        && !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        && !raw.starts_with("//")
    {
        return None;
    }
    let mut url = base.join(&raw).ok()?;
    url.set_fragment(None);
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn is_url_shaped(literal: &str) -> bool {
    if literal.len() > MAX_URL_BYTES || literal.contains(char::is_whitespace) {
        return false;
    }
    literal.starts_with("http://")
        || literal.starts_with("https://")
        || (literal.starts_with('/') && !literal.starts_with("//") && literal.len() > 1)
}

/// The handful of entities that actually appear in a URL attribute.
///
/// `&amp;` above all: an unescaped `&` in an href is the common case, and a
/// candidate carrying `&amp;` in its query is a URL that does not exist.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" | "#X27" => Some('\''),
            other => other
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// One start tag, or one `</form>`.
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
}

impl Tag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A tag reader, not a parser. It knows the three things a regex does not: a
/// comment is not markup, a `<script>` body is not markup, and an attribute
/// value may be quoted, single-quoted or bare.
struct Tags<'a> {
    rest: &'a str,
}

impl<'a> Tags<'a> {
    fn new(html: &'a str) -> Self {
        Self { rest: html }
    }
}

impl Iterator for Tags<'_> {
    type Item = Tag;

    fn next(&mut self) -> Option<Tag> {
        loop {
            let at = self.rest.find('<')?;
            self.rest = &self.rest[at + 1..];

            if let Some(after) = self.rest.strip_prefix("!--") {
                // A comment can contain anything, including markup that is not
                // markup. Skip to its end, or to the end of the document.
                self.rest = after.split_once("-->").map(|(_, rest)| rest).unwrap_or("");
                continue;
            }
            if self.rest.starts_with('!') {
                self.rest = self.rest.split_once('>').map(|(_, rest)| rest).unwrap_or("");
                continue;
            }

            let closing = self.rest.starts_with('/');
            let body_start = if closing { 1 } else { 0 };
            let end = self.rest.find('>')?;
            let body = &self.rest[body_start..end];
            self.rest = &self.rest[end + 1..];

            let mut parts = body.splitn(2, |c: char| c.is_whitespace());
            let name = parts.next().unwrap_or("").trim_end_matches('/').to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            if closing {
                // Only `</form>` closes something this reader tracks.
                if name == "form" {
                    return Some(Tag {
                        name: "/form".to_string(),
                        attrs: Vec::new(),
                    });
                }
                continue;
            }
            let attrs = attributes(parts.next().unwrap_or(""));
            if name == "script" {
                // Whatever is inside is script, not markup. The JavaScript
                // reader is a separate pass over the same bytes.
                if let Some((_, after)) = split_case_insensitive(self.rest, "</script") {
                    self.rest = after.split_once('>').map(|(_, rest)| rest).unwrap_or("");
                }
            }
            return Some(Tag { name, attrs });
        }
    }
}

fn split_case_insensitive<'a>(haystack: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let lower = haystack.to_ascii_lowercase();
    let at = lower.find(needle)?;
    Some((&haystack[..at], &haystack[at + needle.len()..]))
}

/// Attribute values, quoted or bare.
fn attributes(text: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !matches!(bytes[i], b'=' | b'/' | b'>') && !(bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if name_start == i {
            i += 1;
            continue;
        }
        let name = text[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            attrs.push((name, String::new()));
            continue;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            attrs.push((name, String::new()));
            break;
        }
        let value = match bytes[i] {
            quote @ (b'"' | b'\'') => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let value = &text[start..i.min(text.len())];
                i += 1;
                value
            }
            _ => {
                let start = i;
                while i < bytes.len() && !(bytes[i] as char).is_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                &text[start..i.min(text.len())]
            }
        };
        attrs.push((name, value.to_string()));
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://target.test/app/index.html").expect("base")
    }

    fn urls(found: &[Found]) -> Vec<String> {
        found.iter().map(|f| f.url.to_string()).collect()
    }

    #[test]
    fn links_forms_and_scripts_are_all_disclosures() {
        let html = r#"
            <a href="/one">one</a>
            <a href='two.html'>two</a>
            <link rel=stylesheet href=/style.css>
            <script src="//cdn.target.test/app.js"></script>
            <img src="/img/logo.png" srcset="/img/logo@2x.png 2x, /img/logo@3x.png 3x">
        "#;
        let found = from_html(&base(), html);
        let urls = urls(&found);
        assert!(urls.contains(&"https://target.test/one".to_string()));
        assert!(
            urls.contains(&"https://target.test/app/two.html".to_string()),
            "a relative href resolves against the document, not the origin"
        );
        assert!(urls.contains(&"https://target.test/style.css".to_string()));
        assert!(urls.contains(&"https://cdn.target.test/app.js".to_string()));
        assert!(urls.contains(&"https://target.test/img/logo@2x.png".to_string()));
    }

    #[test]
    fn a_form_carries_its_method_and_its_input_names() {
        let html = r#"
            <form action="/login" method="POST">
              <input name="username"><input name="password" type=password>
              <button name="remember">go</button>
            </form>
        "#;
        let found = from_html(&base(), html);
        let form = found.iter().find(|f| f.how == "form").expect("the form");
        assert_eq!(form.url.as_str(), "https://target.test/login");
        assert_eq!(form.method, "POST");
        let names: Vec<&str> = form.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["username", "password", "remember"]);
        assert!(form.params.iter().all(|p| p.at == Where::Form));
    }

    #[test]
    fn a_form_with_no_action_posts_back_to_the_page_it_is_in() {
        let found = from_html(&base(), "<form method=post><input name=q></form>");
        let form = found.iter().find(|f| f.how == "form").expect("the form");
        assert_eq!(
            form.url.as_str(),
            "https://target.test/app/index.html",
            "the most-missed endpoint on any page"
        );
    }

    #[test]
    fn an_escaped_ampersand_is_decoded_because_the_other_url_does_not_exist() {
        let found = from_html(&base(), r#"<a href="/s?q=1&amp;page=2">go</a>"#);
        assert_eq!(urls(&found), vec!["https://target.test/s?q=1&page=2"]);
        let names: Vec<&str> = found[0].params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["q", "page"]);
    }

    #[test]
    fn what_is_not_an_endpoint_is_declined() {
        let html = r##"
            <a href="javascript:alert(1)">x</a>
            <a href="mailto:a@b.test">x</a>
            <a href="data:text/html,<b>">x</a>
            <a href="#section">x</a>
            <a href="">x</a>
        "##;
        assert!(
            from_html(&base(), html).is_empty(),
            "a candidate has to be somewhere a request could go"
        );
    }

    #[test]
    fn a_fragment_is_dropped_so_one_page_is_one_endpoint() {
        let found = from_html(&base(), r#"<a href="/docs#install">x</a>"#);
        assert_eq!(urls(&found), vec!["https://target.test/docs"]);
    }

    #[test]
    fn markup_inside_a_comment_or_a_script_is_not_markup() {
        let html = r#"
            <!-- <a href="/commented-out">no</a> -->
            <script>var s = '<a href="/from-a-string">no</a>';</script>
            <a href="/real">yes</a>
        "#;
        assert_eq!(
            urls(&from_html(&base(), html)),
            vec!["https://target.test/real"],
            "a string in a bundle is the JavaScript reader's business, and it says so"
        );
    }

    #[test]
    fn a_base_element_moves_where_relative_urls_resolve() {
        let html = r#"<base href="https://other.test/v2/"><a href="thing">x</a>"#;
        assert_eq!(urls(&from_html(&base(), html)), vec!["https://other.test/v2/thing"]);
    }

    #[test]
    fn one_document_cannot_flood_the_ledger() {
        let html = (0..MAX_PER_DOCUMENT + 500)
            .map(|n| format!("<a href=\"/p/{n}\">x</a>"))
            .collect::<String>();
        assert_eq!(from_html(&base(), &html).len(), MAX_PER_DOCUMENT);
    }

    #[test]
    fn headers_disclose_what_no_markup_names() {
        let headers = vec![
            ("Location".to_string(), "/after-login".to_string()),
            (
                "Link".to_string(),
                "</api/next>; rel=next, </api/last>; rel=last".to_string(),
            ),
            (
                "Content-Security-Policy".to_string(),
                "default-src 'self'; report-uri /csp/report".to_string(),
            ),
        ];
        let found = from_headers(&base(), &headers);
        let urls = urls(&found);
        assert!(urls.contains(&"https://target.test/after-login".to_string()));
        assert!(urls.contains(&"https://target.test/api/next".to_string()));
        assert!(urls.contains(&"https://target.test/api/last".to_string()));
        let report = found
            .iter()
            .find(|f| f.how == "csp-report-uri")
            .expect("the report endpoint");
        assert_eq!(report.method, "POST");
    }

    #[test]
    fn a_json_body_discloses_urls_and_not_every_string_with_a_slash() {
        let body = r#"{"next":"/api/v2/users","site":"https://target.test/help","when":"2026/09/07","note":"a/b c"}"#;
        let urls = urls(&from_json(&base(), body));
        assert!(urls.contains(&"https://target.test/api/v2/users".to_string()));
        assert!(urls.contains(&"https://target.test/help".to_string()));
        assert_eq!(urls.len(), 2, "a date is not an endpoint: {urls:?}");
    }

    #[test]
    fn a_disclosure_is_a_candidate_and_says_which_message_disclosed_it() {
        let found = from_html(&base(), r#"<a href="/admin?tab=users">x</a>"#);
        let source = Source::Page {
            req: "req_12".to_string(),
        };
        let rows = candidates(&found, "alice", &source);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, State::Candidate);
        assert_eq!(rows[0].path, "/admin");
        assert_eq!(rows[0].identity, "alice");
        assert_eq!(rows[0].source, source);
        assert!(
            rows[0].req.is_none(),
            "nothing has sent a request to it, and the row must not imply otherwise"
        );
    }
}
