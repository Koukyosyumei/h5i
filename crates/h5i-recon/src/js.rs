//! Endpoints a bundle discloses.
//!
//! A token scan, not a regex sweep: a comment is not code, a `//` inside a
//! string is not a comment, a template literal is a string. And it never
//! guesses: `"/api/" + id` is a [`Partial`], not a URL (design-recon.md N8).

use url::Url;

use crate::extract::{Found, MAX_PER_DOCUMENT};
use crate::ledger::Param;

/// A path this script builds at runtime, reported and not resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    /// The literal half, as written.
    pub prefix: String,
    /// What the script does with it, for the reader deciding whether to chase
    /// it: `concat` or a call site's name.
    pub how: &'static str,
}

/// What one script disclosed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Script {
    /// Resolvable endpoints. These become ledger candidates.
    pub found: Vec<Found>,
    /// Paths the script assembles. Reported to the caller, never stored as an
    /// endpoint: half a URL is not a place a request can go.
    pub partial: Vec<Partial>,
}

/// Read one script.
pub fn from_js(base: &Url, source: &str) -> Script {
    let tokens = tokenize(source);
    let mut script = Script::default();

    for (index, token) in tokens.iter().enumerate() {
        if script.found.len() >= MAX_PER_DOCUMENT {
            break;
        }
        let Token::Str(literal) = token else { continue };
        if !is_endpoint_shaped(literal) {
            continue;
        }

        // A literal glued to something else is half an endpoint.
        if matches!(tokens.get(index + 1), Some(Token::Punct('+')))
            || matches!(tokens.get(index.wrapping_sub(1)), Some(Token::Punct('+')) if index > 0)
        {
            let partial = Partial {
                prefix: literal.clone(),
                how: "concat",
            };
            if !script.partial.contains(&partial) {
                script.partial.push(partial);
            }
            continue;
        }

        let (method, how) = call_context(&tokens, index);
        let Some(url) = resolve(base, literal) else {
            continue;
        };
        let mut params = Vec::new();
        for (name, _) in url.query_pairs() {
            let param = Param {
                name: name.into_owned(),
                at: crate::ledger::Where::Query,
            };
            if !params.contains(&param) {
                params.push(param);
            }
        }
        let found = Found {
            url,
            method,
            params,
            how,
        };
        if !script.found.contains(&found) {
            script.found.push(found);
        }
    }
    script
}

/// The method and the reason, read from the tokens around this literal.
///
/// A few tokens back, not an expression parse: needing the whole grammar would
/// mean being a JavaScript engine, which this crate is not.
fn call_context(tokens: &[Token], index: usize) -> (String, &'static str) {
    let mut method = "GET".to_string();
    let mut how = "js-string";

    // `fetch("/x")`, `sendBeacon("/x")`, `new WebSocket("/x")`.
    if matches!(tokens.get(index.wrapping_sub(1)), Some(Token::Punct('(')) if index > 0)
        && index >= 2
        && let Some(Token::Ident(name)) = tokens.get(index - 2)
    {
        how = match name.as_str() {
            "fetch" => "js-fetch",
            "sendBeacon" => "js-beacon",
            "WebSocket" | "EventSource" => "js-socket",
            "get" | "post" | "put" | "patch" | "delete" | "head" | "request" => "js-client",
            _ => "js-call",
        };
        if let "post" | "put" | "patch" | "delete" | "head" = name.as_str() {
            method = name.to_ascii_uppercase();
        }
    }

    // `xhr.open("POST", "/x")`: the method is the argument before this one.
    if index >= 4
        && let (Some(Token::Str(first)), Some(Token::Punct(',')), Some(Token::Punct('('))) = (
            tokens.get(index - 2),
            tokens.get(index - 1),
            tokens.get(index - 3),
        )
        && let Some(Token::Ident(name)) = tokens.get(index - 4)
        && name == "open"
        && is_method(first)
    {
        method = first.to_ascii_uppercase();
        how = "js-xhr";
    }

    // `{ url: "/x", method: "POST" }` around it, or
    // `fetch("/x", { method: "POST" })` beside it in the same call.
    if let Some(declared) = object_method(tokens, index).or_else(|| options_method(tokens, index)) {
        method = declared;
        how = "js-options";
    }
    (method, how)
}

/// A `method:` declared in an options object later in the same call.
///
/// `fetch(url, { method })` is the shape the platform documents, and there the
/// URL is a sibling of the object rather than inside it.
fn options_method(tokens: &[Token], index: usize) -> Option<String> {
    let mut depth = 0usize;
    let mut at = index;
    while at + 2 < tokens.len() {
        at += 1;
        match tokens.get(at) {
            Some(Token::Punct('(')) | Some(Token::Punct('{')) => depth += 1,
            Some(Token::Punct(')')) | Some(Token::Punct('}')) => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            Some(Token::Punct(';')) if depth == 0 => return None,
            Some(Token::Ident(name)) if name == "method" => {
                if let (Some(Token::Punct(':')), Some(Token::Str(value))) =
                    (tokens.get(at + 1), tokens.get(at + 2))
                    && is_method(value)
                {
                    return Some(value.to_ascii_uppercase());
                }
            }
            _ => {}
        }
    }
    None
}

/// A `method:` declared in the object literal this literal sits in.
fn object_method(tokens: &[Token], index: usize) -> Option<String> {
    let mut depth = 0usize;
    // Backwards to the enclosing `{`, then forwards over that object.
    let mut start = index;
    while start > 0 {
        start -= 1;
        match tokens.get(start) {
            Some(Token::Punct('}')) => depth += 1,
            Some(Token::Punct('{')) if depth == 0 => break,
            Some(Token::Punct('{')) => depth -= 1,
            _ => {}
        }
    }
    if !matches!(tokens.get(start), Some(Token::Punct('{'))) {
        return None;
    }
    let mut at = start;
    let mut depth = 0usize;
    while at + 2 < tokens.len() {
        at += 1;
        match tokens.get(at) {
            Some(Token::Punct('{')) => depth += 1,
            Some(Token::Punct('}')) if depth == 0 => break,
            Some(Token::Punct('}')) => depth -= 1,
            Some(Token::Ident(name)) if name == "method" && depth == 0 => {
                if let (Some(Token::Punct(':')), Some(Token::Str(value))) =
                    (tokens.get(at + 1), tokens.get(at + 2))
                    && is_method(value)
                {
                    return Some(value.to_ascii_uppercase());
                }
            }
            _ => {}
        }
    }
    None
}

fn is_method(text: &str) -> bool {
    matches!(
        text.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    )
}

/// Worth resolving: an absolute URL, or a root-relative path.
///
/// Deliberately narrow. `"index"` is a word, `"a/b"` is usually a key, and a
/// ledger full of those is worse than a short one.
fn is_endpoint_shaped(literal: &str) -> bool {
    if literal.is_empty() || literal.len() > 4096 || literal.contains(char::is_whitespace) {
        return false;
    }
    literal.starts_with("http://")
        || literal.starts_with("https://")
        || literal.starts_with("ws://")
        || literal.starts_with("wss://")
        || (literal.starts_with('/') && !literal.starts_with("//") && literal.len() > 1)
}

fn resolve(base: &Url, literal: &str) -> Option<Url> {
    let literal = literal.replace("ws://", "http://").replace("wss://", "https://");
    let mut url = base.join(&literal).ok()?;
    url.set_fragment(None);
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// What the scanner emits. Enough structure to read a call site, and no more.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Str(String),
    Ident(String),
    Punct(char),
}

/// Split source into strings, identifiers and the punctuation that joins them.
///
/// Comments, regex literals and template substitutions are skipped rather than
/// mis-read, which is the whole reason this is a scan and not a pattern match.
fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            '"' | '\'' | '`' => {
                let quote = bytes[i];
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != quote {
                    if bytes[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let literal = source
                    .get(start..j.min(source.len()))
                    .unwrap_or_default()
                    .to_string();
                // A template with a substitution is assembled at runtime, so
                // it is a prefix and not a URL. `${` is the marker.
                if quote == b'`' && literal.contains("${") {
                    let prefix = literal.split("${").next().unwrap_or_default().to_string();
                    tokens.push(Token::Str(prefix));
                    tokens.push(Token::Punct('+'));
                } else {
                    tokens.push(Token::Str(unescape(&literal)));
                }
                i = j + 1;
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i] as char;
                    if c.is_alphanumeric() || c == '_' || c == '$' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Ident(source[start..i].to_string()));
            }
            c if c.is_whitespace() => i += 1,
            c => {
                tokens.push(Token::Punct(c));
                i += 1;
            }
        }
    }
    tokens
}

/// The escapes that change what a URL is. Everything else is left as written.
fn unescape(literal: &str) -> String {
    if !literal.contains('\\') {
        return literal.to_string();
    }
    let mut out = String::with_capacity(literal.len());
    let mut chars = literal.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('/') => out.push('/'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('n') => out.push('\n'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => out.push_str(&hex),
                }
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://target.test/app/main.js").expect("base")
    }

    fn found(script: &Script) -> Vec<(String, String, &'static str)> {
        script
            .found
            .iter()
            .map(|f| (f.url.to_string(), f.method.clone(), f.how))
            .collect()
    }

    #[test]
    fn a_fetch_call_is_an_endpoint_with_the_method_it_declares() {
        let source = r#"
            fetch("/api/v1/users");
            fetch("/api/v1/session", { method: "POST", body: b });
        "#;
        let script = from_js(&base(), source);
        let seen = found(&script);
        assert!(seen.contains(&(
            "https://target.test/api/v1/users".to_string(),
            "GET".to_string(),
            "js-fetch"
        )));
        assert!(
            seen.contains(&(
                "https://target.test/api/v1/session".to_string(),
                "POST".to_string(),
                "js-options"
            )),
            "{seen:?}"
        );
    }

    #[test]
    fn an_xhr_open_takes_its_method_from_the_argument_before_the_url() {
        let script = from_js(&base(), r#"xhr.open("DELETE", "/api/thing/1");"#);
        assert_eq!(
            found(&script),
            vec![(
                "https://target.test/api/thing/1".to_string(),
                "DELETE".to_string(),
                "js-xhr"
            )]
        );
    }

    #[test]
    fn a_websocket_url_is_an_endpoint_over_the_scheme_it_will_use() {
        let script = from_js(&base(), r#"new WebSocket("wss://target.test/live");"#);
        assert_eq!(script.found.len(), 1);
        assert_eq!(script.found[0].url.as_str(), "https://target.test/live");
    }

    #[test]
    fn a_built_path_is_reported_and_never_resolved() {
        let source = r#"
            const url = "/api/users/" + id;
            fetch(`/api/orders/${orderId}/items`);
        "#;
        let script = from_js(&base(), source);
        assert!(
            script.found.is_empty(),
            "guessing at the whole URL would put a place nobody can reach in the ledger: {:?}",
            script.found
        );
        let prefixes: Vec<&str> = script.partial.iter().map(|p| p.prefix.as_str()).collect();
        assert!(prefixes.contains(&"/api/users/"), "{prefixes:?}");
        assert!(prefixes.contains(&"/api/orders/"), "{prefixes:?}");
    }

    #[test]
    fn a_comment_is_not_code_and_a_slash_in_a_string_is_not_a_comment() {
        let source = r#"
            // fetch("/from-a-comment");
            /* fetch("/from-a-block"); */
            const note = "// not a comment";
            fetch("/real");
        "#;
        let script = from_js(&base(), source);
        assert_eq!(
            found(&script)
                .into_iter()
                .map(|(url, _, _)| url)
                .collect::<Vec<_>>(),
            vec!["https://target.test/real".to_string()]
        );
    }

    #[test]
    fn an_escaped_url_reads_as_the_url_it_is() {
        let script = from_js(&base(), r#"fetch("\/api\/escaped");"#);
        assert_eq!(script.found[0].url.as_str(), "https://target.test/api/escaped");
    }

    #[test]
    fn a_word_is_not_an_endpoint() {
        let source = r#"var a = "index"; var b = "a/b"; var c = "GET";"#;
        assert!(from_js(&base(), source).found.is_empty());
    }
}
