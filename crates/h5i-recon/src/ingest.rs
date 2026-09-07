//! The session's own request log, folded into ledger observations.
//!
//! This is the half of the inventory h5i is entitled to be certain about: not
//! what a bundle mentioned, but what this engine sent and what came back. Every
//! row it writes carries the `req_<n>` that produced it, so the workbench can
//! pick the endpoint up and resend it (design-recon.md N5, N6).

use h5i_wire::record::{Phase, RequestRecord};

use crate::ledger::{Observation, Param, Source, State, Where};

/// What the request log added, and how far it was read.
#[derive(Debug, Clone, Default)]
pub struct Ingested {
    pub observations: Vec<Observation>,
    /// The highest receipt `seq` whose request *and* response were both in
    /// hand. A fetch still in flight is not folded, so the next sync sees it.
    pub through_seq: u64,
}

/// `scheme://host[:port]`, the origin a policy grants and a ledger groups by.
pub fn origin_of(url: &url::Url) -> String {
    match url.host_str() {
        Some(host) => match url.port() {
            Some(port) => format!("{}://{host}:{port}", url.scheme()),
            None => format!("{}://{host}", url.scheme()),
        },
        None => url.scheme().to_string(),
    }
}

/// Fold receipts newer than `since` into observations.
///
/// Ordered by `seq` and stopped at the first fetch whose response has not been
/// written, so the caller's cursor never skips a request that was in flight
/// when it read.
pub fn from_receipts(records: &[RequestRecord], identity: &str, since: u64) -> Ingested {
    let mut by_seq: std::collections::BTreeMap<u64, (Option<&RequestRecord>, Option<&RequestRecord>)> =
        std::collections::BTreeMap::new();
    for record in records {
        if record.seq <= since {
            continue;
        }
        let slot = by_seq.entry(record.seq).or_default();
        match record.phase {
            Phase::Request => slot.0 = Some(record),
            Phase::Response => slot.1 = Some(record),
        }
    }

    let mut out = Ingested::default();
    for (seq, (request, response)) in by_seq {
        let (Some(request), Some(response)) = (request, response) else {
            // In flight. Stopping here rather than skipping is what keeps the
            // cursor honest: a gap it stepped over would never be revisited.
            break;
        };
        let Ok(url) = url::Url::parse(&request.url) else {
            out.through_seq = seq;
            continue;
        };

        let params: Vec<Param> = {
            let mut seen: Vec<Param> = Vec::new();
            for (name, _) in url.query_pairs() {
                let param = Param {
                    name: name.into_owned(),
                    at: Where::Query,
                };
                if !seen.contains(&param) {
                    seen.push(param);
                }
            }
            seen
        };

        let mut observation = Observation::new(
            &origin_of(&url),
            url.path(),
            &request.method,
            identity,
            State::Observed,
            Source::Receipt {
                req: format!("req_{seq}"),
            },
        )
        .with_req(format!("req_{seq}"))
        .with_params(params)
        .with_status(response.status);

        if !request.allowed {
            let reason = request
                .denied_reason
                .clone()
                .or_else(|| response.denied_reason.clone())
                .unwrap_or_else(|| "refused by policy".to_string());
            observation = observation.refused(reason);
            // A refusal never reached the wire, so it has no status to report.
            observation.status = None;
        }

        out.observations.push(observation);
        out.through_seq = seq;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use h5i_wire::record::Initiator;

    fn pair(seq: u64, method: &str, url: &str, status: u16) -> Vec<RequestRecord> {
        let request = RequestRecord::request(seq, Initiator::Navigation, method, url);
        let mut response = request.response();
        response.status = Some(status);
        vec![request, response]
    }

    #[test]
    fn a_fetch_becomes_an_observed_endpoint_that_names_its_message() {
        let records = pair(4, "GET", "https://target.test/api/users?id=7&page=2", 200);
        let ingested = from_receipts(&records, "alice", 0);

        assert_eq!(ingested.observations.len(), 1);
        let obs = &ingested.observations[0];
        assert_eq!(obs.origin, "https://target.test");
        assert_eq!(obs.path, "/api/users");
        assert_eq!(obs.state, State::Observed);
        assert_eq!(obs.req.as_deref(), Some("req_4"));
        assert_eq!(obs.status, Some(200));
        assert_eq!(obs.identity, "alice");
        assert_eq!(
            obs.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["id", "page"]
        );
        assert_eq!(ingested.through_seq, 4);
    }

    #[test]
    fn a_refused_request_is_kept_with_its_reason_and_no_status() {
        let mut records = pair(2, "GET", "https://elsewhere.test/", 200);
        records[0] = records[0]
            .clone()
            .denied("origin `https://elsewhere.test` is not in the allowlist");
        let ingested = from_receipts(&records, "anonymous", 0);

        let obs = &ingested.observations[0];
        assert_eq!(obs.state, State::Refused);
        assert!(obs.reason.as_deref().unwrap().contains("allowlist"));
        assert_eq!(obs.status, None, "nothing reached the wire, so nothing answered");
    }

    #[test]
    fn a_fetch_still_in_flight_stops_the_cursor_rather_than_being_stepped_over() {
        let mut records = pair(1, "GET", "https://target.test/a", 200);
        records.push(RequestRecord::request(
            2,
            Initiator::Subresource,
            "GET",
            "https://target.test/b",
        ));
        records.extend(pair(3, "GET", "https://target.test/c", 200));

        let ingested = from_receipts(&records, "anonymous", 0);
        assert_eq!(ingested.observations.len(), 1);
        assert_eq!(
            ingested.through_seq, 1,
            "seq 3 is complete, but folding it would leave seq 2 behind forever"
        );
    }

    #[test]
    fn a_second_sync_folds_only_what_is_new() {
        let mut records = pair(1, "GET", "https://target.test/a", 200);
        records.extend(pair(2, "POST", "https://target.test/login", 302));

        let first = from_receipts(&records, "anonymous", 0);
        assert_eq!(first.observations.len(), 2);
        let second = from_receipts(&records, "anonymous", first.through_seq);
        assert!(second.observations.is_empty());
        assert_eq!(second.through_seq, 0, "nothing new to report");
    }
}
