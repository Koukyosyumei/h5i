//! Files other tools produced, read as candidates (design-recon.md N17, N19).
//!
//! h5i does not run those tools, and never records what one saw: their claims
//! are testimony, and `candidate` is the ledger's state for testimony.

use url::Url;

use crate::extract::Found;
use crate::ledger::{Param, Where};

/// The most bytes one import reads.
pub const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;

/// The most rows one import contributes.
pub const MAX_ROWS: usize = 50_000;

/// What a file was produced by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One URL per line: gau, waybackurls, a list somebody kept.
    Urls,
    /// Katana's JSONL, or its plain output.
    Katana,
    /// Subfinder's hosts, JSONL or plain. A host becomes its root URL.
    Subfinder,
    /// httpx's JSONL. The URL is kept; what httpx saw is not.
    Httpx,
    /// An OpenAPI document, JSON only.
    Openapi,
}

impl Format {
    pub fn parse(name: &str) -> Option<Format> {
        match name.trim().to_ascii_lowercase().as_str() {
            "urls" | "gau" | "waybackurls" | "list" => Some(Format::Urls),
            "katana" => Some(Format::Katana),
            "subfinder" => Some(Format::Subfinder),
            "httpx" => Some(Format::Httpx),
            "openapi" | "swagger" => Some(Format::Openapi),
            _ => None,
        }
    }

    /// The name a ledger row carries: `import:<tool>`.
    pub fn tool(self) -> &'static str {
        match self {
            Format::Urls => "urls",
            Format::Katana => "katana",
            Format::Subfinder => "subfinder",
            Format::Httpx => "httpx",
            Format::Openapi => "openapi",
        }
    }
}

/// What an import read, and what it could not.
#[derive(Debug, Clone, Default)]
pub struct Imported {
    pub found: Vec<Found>,
    /// Lines that were not a URL, a host or a record. Counted, never guessed.
    pub unreadable: u64,
    /// Whether the file was longer than [`MAX_ROWS`] rows.
    pub truncated: bool,
}

/// Read one file's worth of candidates.
///
/// `base` resolves anything relative, which OpenAPI paths and some katana
/// output are.
pub fn read(format: Format, base: &Url, text: &str) -> Imported {
    match format {
        Format::Openapi => openapi(base, text),
        other => lines(other, base, text),
    }
}

fn lines(format: Format, base: &Url, text: &str) -> Imported {
    let mut out = Imported::default();
    for line in text.lines() {
        if out.found.len() >= MAX_ROWS {
            out.truncated = true;
            break;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let record = serde_json::from_str::<serde_json::Value>(line).ok();
        let (raw, method) = match (&record, format) {
            (Some(json), Format::Katana) => (
                json.pointer("/request/endpoint")
                    .or_else(|| json.get("endpoint"))
                    .or_else(|| json.get("url"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                json.pointer("/request/method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GET")
                    .to_ascii_uppercase(),
            ),
            (Some(json), Format::Subfinder) => (
                json.get("host")
                    .and_then(|v| v.as_str())
                    .map(|host| format!("https://{host}/")),
                "GET".to_string(),
            ),
            (Some(json), _) => (
                json.get("url")
                    .or_else(|| json.get("endpoint"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                "GET".to_string(),
            ),
            (None, Format::Subfinder) => {
                // A bare host, which is what subfinder prints without `-json`.
                (Some(format!("https://{line}/")), "GET".to_string())
            }
            (None, _) => (Some(line.to_string()), "GET".to_string()),
        };
        let Some(raw) = raw else {
            out.unreadable += 1;
            continue;
        };
        // A line with a space in it is prose, not a URL. Joining it against the
        // base would percent-encode a sentence into an endpoint.
        if raw.contains(char::is_whitespace) {
            out.unreadable += 1;
            continue;
        }
        match resolve(base, &raw) {
            Some(url) => push(&mut out.found, url, method, "import"),
            None => out.unreadable += 1,
        }
    }
    out
}

/// Paths, methods and named inputs from an OpenAPI document.
///
/// JSON only: YAML would need a parser this crate will not carry (N19), and
/// converting it is one `yq` away.
fn openapi(base: &Url, text: &str) -> Imported {
    let mut out = Imported::default();
    let Ok(document) = serde_json::from_str::<serde_json::Value>(text) else {
        out.unreadable += 1;
        return out;
    };
    // `servers[0].url` when it is absolute, so an imported path lands on the
    // origin the document names rather than the one this session happens to be
    // on.
    let root = document
        .pointer("/servers/0/url")
        .and_then(|v| v.as_str())
        .and_then(|server| resolve(base, server))
        .unwrap_or_else(|| base.clone());

    let Some(paths) = document.get("paths").and_then(|v| v.as_object()) else {
        out.unreadable += 1;
        return out;
    };
    for (path, item) in paths {
        let Some(operations) = item.as_object() else {
            continue;
        };
        for (method, operation) in operations {
            let method = method.to_ascii_uppercase();
            if !matches!(
                method.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            ) {
                continue;
            }
            // Joined by hand: `servers[0].url` usually ends in a base path
            // (`/v2`) with no trailing slash, and `Url::join` would drop that
            // last segment rather than keep it.
            let joined = format!(
                "{}/{}",
                root.as_str().trim_end_matches('/'),
                path.trim_start_matches('/')
            );
            let Some(url) = resolve(&root, &joined) else {
                out.unreadable += 1;
                continue;
            };
            let mut params = Vec::new();
            for parameter in operation
                .get("parameters")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let Some(name) = parameter.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let at = match parameter.get("in").and_then(|v| v.as_str()) {
                    Some("query") => Where::Query,
                    Some("header") => Where::Header,
                    Some("cookie") => Where::Cookie,
                    // A path parameter is part of the path, and a body is a
                    // schema rather than a name.
                    _ => Where::Declared,
                };
                let param = Param {
                    name: name.to_string(),
                    at,
                };
                if !params.contains(&param) {
                    params.push(param);
                }
            }
            if out.found.len() >= MAX_ROWS {
                out.truncated = true;
                return out;
            }
            let item = Found {
                url,
                method,
                params,
                how: "openapi",
            };
            if !out.found.contains(&item) {
                out.found.push(item);
            }
        }
    }
    out
}

fn resolve(base: &Url, raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    let mut url = base.join(raw).ok()?;
    url.set_fragment(None);
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn push(found: &mut Vec<Found>, url: Url, method: String, how: &'static str) {
    let mut params = Vec::new();
    for (name, _) in url.query_pairs() {
        let param = Param {
            name: name.into_owned(),
            at: Where::Query,
        };
        if !params.contains(&param) {
            params.push(param);
        }
    }
    let item = Found {
        url,
        method,
        params,
        how,
    };
    if !found.contains(&item) {
        found.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://target.test/").expect("base")
    }

    fn urls(imported: &Imported) -> Vec<String> {
        imported.found.iter().map(|f| f.url.to_string()).collect()
    }

    #[test]
    fn a_url_list_is_the_common_case() {
        let text = "# from gau\nhttps://target.test/a\n/b?x=1\n\nnot a url at all\n";
        let imported = read(Format::Urls, &base(), text);
        assert_eq!(
            urls(&imported),
            vec![
                "https://target.test/a".to_string(),
                "https://target.test/b?x=1".to_string()
            ]
        );
        assert_eq!(imported.unreadable, 1, "a line that is not a URL is counted");
        assert_eq!(imported.found[1].params[0].name, "x");
    }

    #[test]
    fn katana_carries_the_method_it_saw() {
        let text = r#"{"timestamp":"t","request":{"method":"POST","endpoint":"https://target.test/api/login"}}
{"request":{"method":"GET","endpoint":"/api/me"}}"#;
        let imported = read(Format::Katana, &base(), text);
        assert_eq!(imported.found[0].method, "POST");
        assert_eq!(imported.found[1].url.as_str(), "https://target.test/api/me");
    }

    #[test]
    fn a_host_becomes_a_root_url_and_nothing_more() {
        let text = "admin.target.test\n{\"host\":\"api.target.test\"}\n";
        let imported = read(Format::Subfinder, &base(), text);
        assert_eq!(
            urls(&imported),
            vec![
                "https://admin.target.test/".to_string(),
                "https://api.target.test/".to_string()
            ]
        );
    }

    #[test]
    fn what_another_tool_saw_is_not_recorded_as_ours() {
        let text = r#"{"url":"https://target.test/admin","status_code":200,"title":"Admin"}"#;
        let imported = read(Format::Httpx, &base(), text);
        assert_eq!(urls(&imported), vec!["https://target.test/admin".to_string()]);
        // The row carries a URL and a method. Nothing here can carry a status,
        // and that is the point: an h5i request has to answer for it.
        assert_eq!(imported.found[0].method, "GET");
    }

    #[test]
    fn an_openapi_document_names_paths_methods_and_inputs() {
        let text = r#"{
          "servers": [{"url": "https://api.target.test/v2"}],
          "paths": {
            "/users": {
              "get": {"parameters": [{"name": "page", "in": "query"}]},
              "post": {},
              "summary": "not a method"
            },
            "/users/{id}": {"delete": {"parameters": [{"name": "id", "in": "path"}]}}
          }
        }"#;
        let imported = read(Format::Openapi, &base(), text);
        let mut seen: Vec<(String, String)> = imported
            .found
            .iter()
            .map(|f| (f.url.path().to_string(), f.method.clone()))
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("/v2/users".to_string(), "GET".to_string()),
                ("/v2/users".to_string(), "POST".to_string()),
                ("/v2/users/%7Bid%7D".to_string(), "DELETE".to_string()),
            ],
            "`summary` is not a method"
        );
        let get = imported
            .found
            .iter()
            .find(|f| f.method == "GET")
            .expect("the GET");
        assert_eq!(get.params[0].name, "page");
        assert_eq!(get.params[0].at, Where::Query);
    }

    #[test]
    fn a_document_that_is_not_openapi_is_counted_rather_than_guessed_at() {
        assert_eq!(read(Format::Openapi, &base(), "swagger: 2.0\npaths:\n").unreadable, 1);
        assert_eq!(read(Format::Openapi, &base(), "{}").unreadable, 1);
    }
}
