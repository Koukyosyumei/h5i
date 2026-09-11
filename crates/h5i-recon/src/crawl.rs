//! The frontier: what to ask for next, and when to stop (design-recon.md N9).
//!
//! The walk itself is in the plugin, which owns the sending. Here is the part
//! worth testing alone: what is somewhere new, what is the same page again,
//! and how a run notices it has been logged out.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use url::Url;

// The shape and its tolerance live in `h5i-wire`: recon compares responses and
// so does the workbench's experiment. What stays here is what recon does with
// one, which is decide that a login has expired.
pub use h5i_wire::triage::{Fingerprint, SIZE_TOLERANCE};

/// What bounds a crawl. Every one of these is named on the command line and
/// reported when the job ends: a crawl that chose its own limits would be a
/// scan with a friendly name.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// How far from a seed to walk.
    pub depth: usize,
    /// How many requests the whole walk may spend.
    pub max_requests: usize,
    /// How many URLs sharing one path shape to visit.
    ///
    /// The calendar rule: `/events/2026-01-01` through `/events/2031-12-31` are
    /// two thousand URLs and one endpoint.
    pub per_template: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            depth: 3,
            max_requests: 200,
            per_template: 20,
        }
    }
}

/// The shape of a path, with the parts that vary replaced.
///
/// Numbers, hex blobs and UUIDs are the three that generate URLs without
/// generating endpoints.
pub fn template_of(path: &str) -> String {
    let shaped: Vec<String> = path
        .split('/')
        .map(|segment| {
            let variable = !segment.is_empty()
                && (segment.chars().all(|c| c.is_ascii_digit())
                    || (segment.len() >= 8
                        && segment.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
                        && segment.chars().any(|c| c.is_ascii_digit())));
            if variable {
                "{v}".to_string()
            } else {
                segment.to_string()
            }
        })
        .collect();
    let joined = shaped.join("/");
    if joined.is_empty() {
        "/".to_string()
    } else {
        joined
    }
}

/// What a crawl has visited, and what it has left.
pub struct Frontier {
    bounds: Bounds,
    queue: VecDeque<(Url, usize)>,
    seen: BTreeSet<String>,
    templates: BTreeMap<String, usize>,
    spent: usize,
}

impl Frontier {
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            queue: VecDeque::new(),
            seen: BTreeSet::new(),
            templates: BTreeMap::new(),
            spent: 0,
        }
    }

    /// Offer a URL: too deep, already seen and same-shape are declined by
    /// name, so the caller can report the gap rather than hide it.
    pub fn offer(&mut self, url: &Url, depth: usize) -> Offer {
        if depth > self.bounds.depth {
            return Offer::TooDeep;
        }
        let key = normalise(url);
        if self.seen.contains(&key) {
            return Offer::Seen;
        }
        let template = template_of(url.path());
        let count = self.templates.entry(template).or_insert(0);
        if *count >= self.bounds.per_template {
            return Offer::SameShape;
        }
        *count += 1;
        self.seen.insert(key);
        self.queue.push_back((url.clone(), depth));
        Offer::Queued
    }

    /// The next URL to ask for, oldest first, or `None` when the walk is done.
    pub fn take_next(&mut self) -> Option<(Url, usize)> {
        if self.spent >= self.bounds.max_requests {
            return None;
        }
        let next = self.queue.pop_front()?;
        self.spent += 1;
        Some(next)
    }

    pub fn spent(&self) -> usize {
        self.spent
    }

    pub fn remaining(&self) -> usize {
        self.queue.len()
    }

    /// Whether the walk stopped because it ran out of allowance rather than
    /// out of pages. The difference is the difference between "that is the
    /// whole application" and "that is what 200 requests reached".
    pub fn exhausted(&self) -> bool {
        self.spent >= self.bounds.max_requests && !self.queue.is_empty()
    }
}

/// Why a URL was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Queued,
    Seen,
    TooDeep,
    SameShape,
}

/// One URL, in the form two visits to the same page share.
///
/// The fragment is gone (it never reaches the server) and query keys are
/// ordered, because `?a=1&b=2` and `?b=2&a=1` are one request twice.
pub fn normalise(url: &Url) -> String {
    let mut keys: Vec<String> = url
        .query_pairs()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    keys.sort();
    let query = if keys.is_empty() {
        String::new()
    } else {
        format!("?{}", keys.join("&"))
    };
    format!(
        "{}://{}{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.path(),
        query
    )
}

/// Whether a re-probed page changed shape, meaning the login is gone.
///
/// Continuing would inventory the login page under an identity that no longer
/// holds, which is worse than stopping short.
pub fn identity_lost(before: &Fingerprint, now: &Fingerprint) -> bool {
    !before.matches(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("url")
    }

    #[test]
    fn one_page_reached_two_ways_is_walked_once() {
        let mut frontier = Frontier::new(Bounds::default());
        assert_eq!(frontier.offer(&url("https://t.test/a?x=1&y=2"), 0), Offer::Queued);
        assert_eq!(
            frontier.offer(&url("https://t.test/a?y=2&x=1#top"), 0),
            Offer::Seen,
            "query order and a fragment do not make a second request"
        );
    }

    #[test]
    fn a_generated_url_space_is_walked_and_not_enumerated() {
        let bounds = Bounds {
            per_template: 3,
            ..Bounds::default()
        };
        let mut frontier = Frontier::new(bounds);
        let queued = (0..10)
            .filter(|n| frontier.offer(&url(&format!("https://t.test/events/{n}")), 0) == Offer::Queued)
            .count();
        assert_eq!(queued, 3, "ten dates are one endpoint");
        assert_eq!(
            frontier.offer(&url("https://t.test/events/new"), 0),
            Offer::Queued,
            "and a real path under the same parent is not one of them"
        );
    }

    #[test]
    fn depth_is_a_bound_and_says_so() {
        let bounds = Bounds {
            depth: 1,
            ..Bounds::default()
        };
        let mut frontier = Frontier::new(bounds);
        assert_eq!(frontier.offer(&url("https://t.test/a"), 1), Offer::Queued);
        assert_eq!(frontier.offer(&url("https://t.test/b"), 2), Offer::TooDeep);
    }

    #[test]
    fn a_walk_that_ran_out_of_allowance_is_not_a_walk_that_ran_out_of_pages() {
        let bounds = Bounds {
            max_requests: 1,
            ..Bounds::default()
        };
        let mut frontier = Frontier::new(bounds);
        frontier.offer(&url("https://t.test/a"), 0);
        frontier.offer(&url("https://t.test/b"), 0);
        assert!(frontier.take_next().is_some());
        assert!(frontier.take_next().is_none());
        assert!(frontier.exhausted(), "the caller has to be able to say which it was");
    }

    #[test]
    fn a_template_keeps_the_parts_that_are_not_ids() {
        assert_eq!(template_of("/users/42/orders/7"), "/users/{v}/orders/{v}");
        assert_eq!(
            template_of("/f/9f8c2b1e-0a4d-4c31-9d2f-5b7a1c6e8d40"),
            "/f/{v}"
        );
        assert_eq!(template_of("/about"), "/about");
    }

    #[test]
    fn a_page_that_changed_shape_means_the_login_is_gone() {
        let before = Fingerprint::of(Some(200), "text/html", 4096, None);
        let now = Fingerprint::of(Some(302), "text/html", 0, Some("https://t.test/login?next=/a"));
        assert!(identity_lost(&before, &now));

        let again = Fingerprint::of(Some(200), "text/html; charset=utf-8", 4102, None);
        assert!(
            !identity_lost(&before, &again),
            "a page carrying a timestamp is the same page"
        );
    }
}
