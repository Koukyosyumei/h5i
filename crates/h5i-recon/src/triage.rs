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
}

impl Baseline {
    /// Whether a response is this directory's way of saying nothing is there.
    ///
    /// Not status alone: the case this exists for answers 200 with a "not
    /// found" page, so a matching skeleton is enough.
    pub fn says_nothing_here(&self, sample: &Sample) -> bool {
        if !self.skeleton.is_empty() && self.skeleton == sample.skeleton {
            return true;
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
        }
    }

    #[test]
    fn a_soft_404_is_recognised_by_shape_rather_than_by_status() {
        let baseline = Baseline {
            samples: vec![Fingerprint::of(Some(200), "text/html", 2000, None)],
            skeleton: "aaaa".to_string(),
        };
        // The application answers 200 for everything, and the sizes differ
        // because the page echoes the path.
        assert!(baseline.says_nothing_here(&sample("/nope", "req_1", 200, 3500, "aaaa")));
        assert!(!baseline.says_nothing_here(&sample("/admin", "req_2", 200, 3500, "bbbb")));
    }

    #[test]
    fn an_unstable_baseline_is_not_trusted() {
        let steady = Baseline {
            samples: vec![
                Fingerprint::of(Some(404), "text/html", 500, None),
                Fingerprint::of(Some(404), "text/html", 520, None),
            ],
            skeleton: String::new(),
        };
        assert!(steady.is_stable());

        let noisy = Baseline {
            samples: vec![
                Fingerprint::of(Some(404), "text/html", 500, None),
                Fingerprint::of(Some(200), "text/html", 9000, None),
            ],
            skeleton: String::new(),
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
