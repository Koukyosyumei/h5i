//! Calibration and clustering: five thousand responses read as twenty rows,
//! with none of them lost (design-recon.md N11).
//!
//! A cluster is a view over stored messages. Every member still names the
//! `req_<n>` that `h5i websec show` reads exactly.

use std::collections::BTreeMap;

use crate::crawl::Fingerprint;

/// One response, as triage sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The endpoint this answered for.
    pub endpoint: String,
    pub path: String,
    /// The message it came from. Never dropped: the cluster is a view, the
    /// message is the evidence.
    pub req: String,
    pub fingerprint: Fingerprint,
    /// The document's tag shape, when it was a document.
    pub skeleton: String,
    /// What the document says, with the asked-for path taken out. Two pages
    /// from one template differ here and nowhere else.
    pub text: String,
}

/// What a document says, hashed, with the asked-for path taken out.
///
/// Tags, digits and whitespace go, because a template is not content and a
/// timestamp is not a difference. The path goes because a soft 404 that echoes
/// it would otherwise look like a different page every time.
pub fn text_digest(body: &str, path: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut text = String::with_capacity(body.len().min(64 * 1024));
    let mut inside_tag = false;
    for c in body.chars().take(64 * 1024) {
        match c {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if inside_tag => {}
            c if c.is_ascii_digit() => {}
            c if c.is_whitespace() => text.push(' '),
            c => text.push(c.to_ascii_lowercase()),
        }
    }
    // The path is normalised the same way before it is taken out, or the
    // digits stripped from the body would leave half of it behind.
    let needle: String = path
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let without_path = if needle.len() > 1 {
        text.replace(&needle, " ")
    } else {
        text
    };
    let words: Vec<&str> = without_path.split_whitespace().collect();
    let digest = Sha256::digest(words.join(" ").as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory a path belongs to, which is the unit calibration works in.
///
/// A soft 404 at the root and a real 404 under `/api` is the ordinary case, so
/// a single baseline for a whole host answers the wrong question.
pub fn directory_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(at) => path[..at].to_string(),
    }
}

/// What a directory answers for a path that is not there.
///
/// Several samples rather than one, because an application that echoes the
/// requested path answers a slightly different not-found every time.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Baseline {
    pub samples: Vec<Fingerprint>,
    /// The tag shape of the not-found document, when it is one.
    #[serde(default)]
    pub skeleton: String,
    /// What the not-found document says, with the probed path removed. A soft
    /// 404 that echoes the path says the same thing every time once it is.
    #[serde(default)]
    pub text: String,
}

impl Baseline {
    /// Whether a response is this directory's way of saying nothing is there.
    ///
    /// Not status alone: the case this exists for answers 200 with a "not
    /// found" page. But a matching shape is not enough either, because most
    /// sites build every page from one template. When the shape matches, what
    /// the page *says* decides, with the asked-for path removed so that a soft
    /// 404 echoing it still reads as the same sentence.
    pub fn says_nothing_here(&self, sample: &Sample) -> bool {
        if !self.skeleton.is_empty() && self.skeleton == sample.skeleton {
            return self.text.is_empty() || self.text == sample.text;
        }
        self.samples
            .iter()
            .any(|baseline| baseline.matches(&sample.fingerprint))
    }

    /// Whether this baseline is worth trusting.
    ///
    /// Two probes that disagreed mean the directory answers unpredictably, and
    /// a baseline built from that would confirm and un-confirm at random.
    pub fn is_stable(&self) -> bool {
        match self.samples.split_first() {
            Some((first, rest)) => rest.iter().all(|other| first.matches(other)),
            None => false,
        }
    }
}

/// A group of responses that look alike, and the one to read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Cluster {
    /// What the members share, in words a reader can act on.
    pub label: String,
    /// The member worth reading first.
    pub representative: String,
    pub count: usize,
    /// Every member's message id. The cluster summarises; it never replaces.
    pub members: Vec<String>,
    /// Paths in this cluster, capped for display.
    pub paths: Vec<String>,
}

/// The most paths one cluster lists before it starts counting instead.
pub const MAX_PATHS_SHOWN: usize = 10;

/// Group responses that look alike: status, content type, redirect target and
/// document shape must match, then size within tolerance of the group's first
/// member, which is what keeps two pages of one template apart.
pub fn cluster(samples: &[Sample]) -> Vec<Cluster> {
    let mut groups: BTreeMap<String, Vec<&Sample>> = BTreeMap::new();
    for sample in samples {
        let key = format!(
            "{}|{}|{}|{}",
            sample
                .fingerprint
                .status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string()),
            sample.fingerprint.content_type,
            sample.fingerprint.location.as_deref().unwrap_or(""),
            sample.skeleton,
        );
        groups.entry(key).or_default().push(sample);
    }

    let mut clusters = Vec::new();
    for (key, members) in groups {
        // Within a group, size still separates: a listing of one item and a
        // listing of a thousand share a template and are not the same answer.
        let mut buckets: Vec<Vec<&Sample>> = Vec::new();
        for sample in members {
            match buckets
                .iter_mut()
                .find(|bucket| bucket[0].fingerprint.matches(&sample.fingerprint))
            {
                Some(bucket) => bucket.push(sample),
                None => buckets.push(vec![sample]),
            }
        }
        for bucket in buckets {
            let first = bucket[0];
            clusters.push(Cluster {
                label: format!(
                    "{} {} ~{} bytes",
                    key.split('|').next().unwrap_or("-"),
                    if first.fingerprint.content_type.is_empty() {
                        "-"
                    } else {
                        &first.fingerprint.content_type
                    },
                    first.fingerprint.size
                ),
                representative: first.req.clone(),
                count: bucket.len(),
                members: bucket.iter().map(|s| s.req.clone()).collect(),
                paths: bucket
                    .iter()
                    .take(MAX_PATHS_SHOWN)
                    .map(|s| s.path.clone())
                    .collect(),
            });
        }
    }
    // Biggest first: the large cluster is the noise an agent wants folded away,
    // and the singletons at the bottom are what it came for.
    clusters.sort_by(|a, b| b.count.cmp(&a.count).then(a.label.cmp(&b.label)));
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(path: &str, req: &str, status: u16, bytes: u64, skeleton: &str) -> Sample {
        Sample {
            endpoint: format!("ep_{path}"),
            path: path.to_string(),
            req: req.to_string(),
            fingerprint: Fingerprint::of(Some(status), "text/html", bytes, None),
            skeleton: skeleton.to_string(),
            text: String::new(),
        }
    }

    #[test]
    fn a_soft_404_is_recognised_by_shape_and_by_what_it_says() {
        let missing = "<html><body><div><span>no such page: %s</span></div></body></html>";
        let baseline = Baseline {
            samples: vec![Fingerprint::of(Some(200), "text/html", 2000, None)],
            skeleton: "aaaa".to_string(),
            text: text_digest(&missing.replace("%s", "/h5i-not-here-1"), "/h5i-not-here-1"),
        };

        // The application answers 200 for everything, and echoes the path, so
        // no two of its refusals are the same length.
        let mut echoed = sample("/nope", "req_1", 200, 3500, "aaaa");
        echoed.text = text_digest(&missing.replace("%s", "/nope"), "/nope");
        assert!(baseline.says_nothing_here(&echoed));

        // A real page built from the same template is not a refusal. This is
        // the ordinary case on any site: every page shares a shape.
        let mut real = sample("/admin", "req_2", 200, 3400, "aaaa");
        real.text = text_digest(
            "<html><body><div><span>user accounts</span></div></body></html>",
            "/admin",
        );
        assert!(
            !baseline.says_nothing_here(&real),
            "a shared template is not a missing page"
        );
    }

    #[test]
    fn a_page_whose_only_difference_is_its_words_is_still_a_page() {
        // The case the benchmark found: a one-line page and a one-line 404,
        // same tags, sizes within a few bytes of each other.
        let baseline = Baseline {
            samples: vec![Fingerprint::of(Some(404), "text/html", 48, None)],
            skeleton: "aaaa".to_string(),
            text: text_digest("<html><body><p>not found</p></body></html>", "/h5i-not-here"),
        };
        let mut home = sample("/", "req_0", 200, 52, "aaaa");
        home.text = text_digest("<html><body><p>nothing to see</p></body></html>", "/");
        assert!(!baseline.says_nothing_here(&home));
    }

    #[test]
    fn an_unstable_baseline_is_not_trusted() {
        let steady = Baseline {
            samples: vec![
                Fingerprint::of(Some(404), "text/html", 500, None),
                Fingerprint::of(Some(404), "text/html", 520, None),
            ],
            skeleton: String::new(),
            text: String::new(),
        };
        assert!(steady.is_stable());

        let noisy = Baseline {
            samples: vec![
                Fingerprint::of(Some(404), "text/html", 500, None),
                Fingerprint::of(Some(200), "text/html", 9000, None),
            ],
            skeleton: String::new(),
            text: String::new(),
        };
        assert!(
            !noisy.is_stable(),
            "a directory that answers unpredictably would confirm at random"
        );
    }

    #[test]
    fn the_noise_folds_and_the_exceptions_stay_visible() {
        let mut samples: Vec<Sample> = (0..50)
            .map(|n| sample(&format!("/x{n}"), &format!("req_{n}"), 200, 1200, "aaaa"))
            .collect();
        samples.push(sample("/admin", "req_900", 200, 8000, "bbbb"));
        samples.push(sample("/backup.zip", "req_901", 200, 100000, "cccc"));

        let clusters = cluster(&samples);
        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].count, 50, "the noise is one row");
        assert!(clusters[1..].iter().all(|c| c.count == 1));
        assert_eq!(
            clusters[0].members.len(),
            50,
            "and every member still names its message: the cluster summarises, it does not replace"
        );
    }

    #[test]
    fn a_directory_is_where_calibration_happens() {
        assert_eq!(directory_of("/api/v1/users"), "/api/v1");
        assert_eq!(directory_of("/about"), "/");
        assert_eq!(directory_of("/"), "/");
    }
}
