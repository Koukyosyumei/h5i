//! The files an application publishes about itself, read as sources of
//! candidates.
//!
//! `robots.txt` is not an authorisation oracle: a `Disallow` says where
//! something is, not that h5i may go there. Scope is the session's policy.

use url::Url;

use crate::extract::Found;

/// What recon asks for, in order. Short on purpose: each one is a request, and
/// a list that grew would be a scan.
pub const WELL_KNOWN: &[&str] = &[
    "/robots.txt",
    "/sitemap.xml",
    "/.well-known/security.txt",
    "/.well-known/openid-configuration",
];

/// The most nested sitemaps one run will follow from an index.
pub const MAX_SITEMAP_DEPTH: usize = 3;

/// The most locations one sitemap contributes.
pub const MAX_LOCATIONS: usize = 5_000;

/// Paths named by a `robots.txt`, and the sitemaps it points at.
///
/// A wildcard rule discloses only its literal head: `/admin/*` gives `/admin/`,
/// `/*.bak` gives nothing.
pub fn from_robots(base: &Url, text: &str) -> (Vec<Found>, Vec<Url>) {
    let mut found = Vec::new();
    let mut sitemaps = Vec::new();
    for line in text.lines().take(10_000) {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match field.trim().to_ascii_lowercase().as_str() {
            "disallow" | "allow" => {
                let literal = value.split(['*', '$']).next().unwrap_or_default();
                if literal.len() > 1
                    && literal.starts_with('/')
                    && let Ok(url) = base.join(literal)
                {
                    push(&mut found, url, "robots");
                }
            }
            "sitemap" => {
                if let Ok(url) = base.join(value)
                    && matches!(url.scheme(), "http" | "https")
                {
                    sitemaps.push(url);
                }
            }
            _ => {}
        }
    }
    (found, sitemaps)
}

/// Locations in a `sitemap.xml`, and the nested sitemaps of an index.
///
/// A tag reader for the reason the HTML one is: `<loc>` is all this needs, and
/// a whole XML parser would be one more thing reading hostile bytes (N19).
pub fn from_sitemap(base: &Url, xml: &str) -> (Vec<Found>, Vec<Url>) {
    let is_index = xml.contains("<sitemapindex") || xml.contains(":sitemapindex");
    let mut found = Vec::new();
    let mut nested = Vec::new();

    let mut rest = xml;
    while let Some(at) = rest.find("<loc") {
        rest = &rest[at + 4..];
        let Some(open) = rest.find('>') else { break };
        rest = &rest[open + 1..];
        let Some(close) = rest.find("</loc") else { break };
        let raw = decode_xml(rest[..close].trim());
        rest = &rest[close..];

        let Ok(url) = base.join(&raw) else { continue };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        if is_index {
            if nested.len() < MAX_SITEMAP_DEPTH * 32 {
                nested.push(url);
            }
        } else if found.len() < MAX_LOCATIONS {
            push(&mut found, url, "sitemap");
        }
    }
    (found, nested)
}

fn push(found: &mut Vec<Found>, url: Url, how: &'static str) {
    let mut params = Vec::new();
    for (name, _) in url.query_pairs() {
        let param = crate::ledger::Param {
            name: name.into_owned(),
            at: crate::ledger::Where::Query,
        };
        if !params.contains(&param) {
            params.push(param);
        }
    }
    let item = Found {
        url,
        method: "GET".to_string(),
        params,
        how,
    };
    if !found.contains(&item) {
        found.push(item);
    }
}

/// The five entities XML defines. A `&amp;` in a `<loc>` is the common case.
fn decode_xml(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://target.test/robots.txt").expect("base")
    }

    fn urls(found: &[Found]) -> Vec<String> {
        found.iter().map(|f| f.url.to_string()).collect()
    }

    #[test]
    fn robots_discloses_the_paths_it_asks_crawlers_to_avoid() {
        let text = "User-agent: *\nDisallow: /admin/\nDisallow: /internal/reports\nAllow: /public\n\
                    Sitemap: https://target.test/sitemap.xml\n";
        let (found, sitemaps) = from_robots(&base(), text);
        let urls = urls(&found);
        assert!(urls.contains(&"https://target.test/admin/".to_string()));
        assert!(urls.contains(&"https://target.test/internal/reports".to_string()));
        assert!(urls.contains(&"https://target.test/public".to_string()));
        assert_eq!(sitemaps, vec![Url::parse("https://target.test/sitemap.xml").unwrap()]);
    }

    #[test]
    fn a_rule_that_names_no_place_discloses_nothing() {
        let (found, _) = from_robots(&base(), "Disallow: /\nDisallow: /*.bak$\nDisallow:\n");
        assert!(
            found.is_empty(),
            "`/` is every path and `/*.bak` is a pattern: {:?}",
            urls(&found)
        );
    }

    #[test]
    fn a_comment_is_not_a_rule() {
        let (found, _) = from_robots(&base(), "# Disallow: /secret\nDisallow: /real # why\n");
        assert_eq!(urls(&found), vec!["https://target.test/real".to_string()]);
    }

    #[test]
    fn a_sitemap_yields_locations() {
        let xml = r#"<?xml version="1.0"?>
            <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
              <url><loc>https://target.test/a</loc><lastmod>2026-01-01</lastmod></url>
              <url><loc>https://target.test/s?q=1&amp;p=2</loc></url>
            </urlset>"#;
        let (found, nested) = from_sitemap(&base(), xml);
        assert_eq!(
            urls(&found),
            vec![
                "https://target.test/a".to_string(),
                "https://target.test/s?q=1&p=2".to_string()
            ]
        );
        assert!(nested.is_empty());
    }

    #[test]
    fn an_index_yields_sitemaps_rather_than_endpoints() {
        let xml = r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
              <sitemap><loc>https://target.test/sitemap-1.xml</loc></sitemap>
              <sitemap><loc>https://target.test/sitemap-2.xml</loc></sitemap>
            </sitemapindex>"#;
        let (found, nested) = from_sitemap(&base(), xml);
        assert!(found.is_empty(), "an index names sitemaps, not pages");
        assert_eq!(nested.len(), 2);
    }
}
