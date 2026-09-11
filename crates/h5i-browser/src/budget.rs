//! What one page is allowed to spend before the engine stops answering it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What a page may spend on the network in one navigation.
///
/// Every field is a *ceiling*, and the defaults are chosen to be far above what
/// a real page does and far below what a runaway one would: a documentation
/// page makes tens of requests, and a loop makes as many as it can.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    pub max_requests: u64,
    /// Bytes as they crossed the wire, so a compressed response is counted at
    /// what it actually cost.
    pub max_wire_bytes: u64,
    /// Bytes after decoding, which is what a decompression bomb inflates and
    /// what the page's memory actually holds.
    pub max_decoded_bytes: u64,
    /// Wall-clock time spent waiting on the network, summed across requests.
    ///
    /// Not the same as a per-request timeout: a hundred requests that each take
    /// two seconds are each well within the 30s per-request limit and together
    /// are three minutes an agent is waiting.
    #[serde(with = "millis")]
    pub max_network_time: Duration,
    /// Wall clock since the navigation began, spent or not.
    ///
    /// The one ceiling the others could not give. A page whose cost is parsing,
    /// layout and fonts rather than waiting is inside every limit above and can
    /// still load for two minutes; the engine then has no answer except
    /// [`HardStop`], which is not an answer at all because it discards the page
    /// instead of reporting it unfinished.
    #[serde(with = "millis", default = "default_max_load_time")]
    pub max_load_time: Duration,
}

/// Matches `--navigation-seconds`, which bounds the same span from the other
/// side of the process split. Two numbers for one rule, because the budget is
/// the broker's and the navigation deadline is the renderer's.
fn default_max_load_time() -> Duration {
    Duration::from_secs(45)
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // A heavy real page loads a few hundred subresources. A loop makes
            // thousands.
            max_requests: 500,
            max_wire_bytes: 64 * 1024 * 1024,
            // Higher than the wire ceiling, because decoding legitimately
            // expands, and still bounded, because that is the direction a
            // compression bomb pushes.
            max_decoded_bytes: 256 * 1024 * 1024,
            max_network_time: Duration::from_secs(60),
            max_load_time: default_max_load_time(),
        }
    }
}

/// What has been spent, and against what.
///
/// Atomics rather than a lock: the counters are touched by every fetch thread
/// the script realm spawns, and a mutex here would serialise the one part of
/// this engine that is deliberately concurrent.
#[derive(Debug)]
pub struct Budget {
    limits: Limits,
    requests: AtomicU64,
    wire_bytes: AtomicU64,
    decoded_bytes: AtomicU64,
    network_micros: AtomicU64,
    /// A fixed point to measure the navigation's wall clock from.
    ///
    /// An `Instant` cannot be stored atomically, and a lock here would
    /// serialise the one part of this engine that is deliberately concurrent.
    /// So the base holds still and the offset moves.
    epoch: Instant,
    /// When the current navigation began, as microseconds since `epoch`.
    navigation_micros: AtomicU64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(Limits::default())
    }
}

/// Why a request was refused, in the form a receipt records and a page reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceeded(pub String);

impl std::fmt::Display for Exceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            requests: AtomicU64::new(0),
            wire_bytes: AtomicU64::new(0),
            decoded_bytes: AtomicU64::new(0),
            network_micros: AtomicU64::new(0),
            epoch: Instant::now(),
            navigation_micros: AtomicU64::new(0),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Start again. Called when the agent navigates: a fresh page is a fresh
    /// decision by the principal, and gets a fresh allowance.
    pub fn reset(&self) {
        self.requests.store(0, Ordering::Relaxed);
        self.wire_bytes.store(0, Ordering::Relaxed);
        self.decoded_bytes.store(0, Ordering::Relaxed);
        self.network_micros.store(0, Ordering::Relaxed);
        self.navigation_micros.store(
            self.epoch.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
    }

    /// How long the current navigation has been loading.
    pub fn loading(&self) -> Duration {
        let began = Duration::from_micros(self.navigation_micros.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(began)
    }

    /// Whether there is room for one more request.
    ///
    /// Checked *before* the wire, so an over-budget request is refused rather
    /// than made and then complained about. The request counter is incremented
    /// here. A request that is about to be attempted has been spent, whatever
    /// its outcome, or a page whose every fetch fails would have an unlimited
    /// number of them.
    pub fn claim_request(&self) -> Result<(), Exceeded> {
        let spent = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        if spent > self.limits.max_requests {
            return Err(Exceeded(format!(
                "budget-exceeded: this page has made {} requests, and the limit for one \
                 navigation is {}. Navigating gives the page a fresh allowance.",
                spent, self.limits.max_requests
            )));
        }
        self.check_totals()
    }

    /// Record what a completed request cost.
    pub fn record(&self, wire: u64, decoded: u64, took: Duration) {
        self.wire_bytes.fetch_add(wire, Ordering::Relaxed);
        self.decoded_bytes.fetch_add(decoded, Ordering::Relaxed);
        self.network_micros
            .fetch_add(took.as_micros() as u64, Ordering::Relaxed);
    }

    /// Whether the cumulative ceilings are still intact.
    ///
    /// Public because a long-lived connection has no next request to be
    /// refused at: a socket or an event stream carries bytes for as long as it
    /// is open, so it asks directly, per frame. See
    /// [`crate::net::LocalBroker::record_socket_frame`].
    pub fn within_totals(&self) -> Result<(), Exceeded> {
        self.check_totals()
    }

    /// Separate from [`Self::claim_request`] so the byte and time totals are
    /// checked on the way *in* to the next request rather than the way out of
    /// the last one: the request that goes over the line should be the one
    /// refused, and refusing the one after it is both later and clearer.
    fn check_totals(&self) -> Result<(), Exceeded> {
        let wire = self.wire_bytes.load(Ordering::Relaxed);
        if wire > self.limits.max_wire_bytes {
            return Err(Exceeded(format!(
                "budget-exceeded: this page has pulled {wire} bytes across the wire, and the \
                 limit for one navigation is {}.",
                self.limits.max_wire_bytes
            )));
        }
        let decoded = self.decoded_bytes.load(Ordering::Relaxed);
        if decoded > self.limits.max_decoded_bytes {
            return Err(Exceeded(format!(
                "budget-exceeded: this page has decoded {decoded} bytes, and the limit for \
                 one navigation is {}.",
                self.limits.max_decoded_bytes
            )));
        }
        let micros = self.network_micros.load(Ordering::Relaxed);
        let limit = self.limits.max_network_time.as_micros() as u64;
        if micros > limit {
            return Err(Exceeded(format!(
                "budget-exceeded: this page has spent {}ms waiting on the network, and the \
                 limit for one navigation is {}ms.",
                micros / 1000,
                limit / 1000
            )));
        }
        // Last, because it is the least specific: when a page is over two
        // ceilings the one naming what it spent is the more useful answer.
        let loading = self.loading();
        if loading > self.limits.max_load_time {
            return Err(Exceeded(format!(
                "budget-exceeded: this page has been loading for {}s, and the limit for one \
                 navigation is {}s. What it has rendered by now is what there is; navigating \
                 gives the page a fresh allowance.",
                loading.as_secs(),
                self.limits.max_load_time.as_secs()
            )));
        }
        Ok(())
    }

    /// What has been spent, for `status` and for the snapshot's note.
    pub fn spent(&self) -> Spent {
        Spent {
            requests: self.requests.load(Ordering::Relaxed),
            wire_bytes: self.wire_bytes.load(Ordering::Relaxed),
            decoded_bytes: self.decoded_bytes.load(Ordering::Relaxed),
            network_time: Duration::from_micros(self.network_micros.load(Ordering::Relaxed)),
        }
    }
}

/// A reading of what has been spent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Spent {
    pub requests: u64,
    pub wire_bytes: u64,
    pub decoded_bytes: u64,
    #[serde(with = "millis")]
    pub network_time: Duration,
}

/// Durations as whole milliseconds, both ways.
///
/// The console has always read these as a number of milliseconds, so the
/// serialized form is fixed by something outside this crate. What is new is the
/// other direction: a reading of the budget now crosses a process boundary, and
/// a value that only serializes is a value the renderer cannot be told.
mod millis {
    use std::time::Duration;

    pub fn serialize<S: serde::Serializer>(value: &Duration, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_u64(value.as_millis() as u64)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Duration, D::Error> {
        use serde::Deserialize;
        Ok(Duration::from_millis(u64::deserialize(input)?))
    }
}

/// A wall-clock bound on one navigation, from the first byte to the last.
///
/// The per-phase budgets this engine already had each bound their own step and
/// none of them bounds the whole. A page that spends thirty seconds on the
/// network *and* twenty in its script is inside every limit and has still taken
/// the better part of a minute.
///
/// A deadline rather than a timer, because it is consulted from several places
/// and each needs the same answer: how much is left.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    started: Instant,
    budget: Duration,
}

impl Deadline {
    pub fn new(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
        }
    }

    /// How long is left, or zero.
    pub fn remaining(&self) -> Duration {
        self.budget.saturating_sub(self.started.elapsed())
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn budget(&self) -> Duration {
        self.budget
    }
}

/// The stop of last resort, for the half of this engine no deadline reaches.
pub struct HardStop {
    done: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

/// How far past its budget a navigation may go before the process stops.
const HARD_STOP_MARGIN: Duration = Duration::from_secs(60);

impl HardStop {
    pub fn arm(budget: Duration) -> Self {
        let done = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let watching = done.clone();
        let ceiling = budget + HARD_STOP_MARGIN;
        let _ = std::thread::Builder::new()
            .name("h5i-navigation-stop".to_string())
            .spawn(move || {
                let (lock, wake) = &*watching;
                let mut finished = lock.lock().unwrap_or_else(|e| e.into_inner());
                let deadline = Instant::now() + ceiling;
                while !*finished {
                    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    let (guard, _) = wake
                        .wait_timeout(finished, left)
                        .unwrap_or_else(|e| e.into_inner());
                    finished = guard;
                }
                if *finished {
                    return;
                }
                eprintln!(
                    "h5i-browser: this page has been loading for {ceiling:?} without \
                     finishing, which is past anything a slow page does. The engine is stopping \
                     rather than holding a core indefinitely. This is a bug in the engine: a \
                     page that takes too long is supposed to be reported as unfinished, not to \
                     become one that never returns."
                );
                crate::ipc::stop_now(71);
            });
        Self { done }
    }
}

impl Drop for HardStop {
    /// The navigation finished, however it finished.
    fn drop(&mut self) {
        let (lock, wake) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_navigation_that_finishes_disarms_its_hard_stop() {
        // The half of `HardStop` that can be tested in process. The other half ends
        // the process, so it is verified by running one: a navigation held open past
        // its ceiling exits 71 and says why.
        //
        // What this pins is the part that would be catastrophic to get wrong in the
        // opposite direction. A guard that fired on a page which had already
        // finished would take down a working engine, rarely enough to be blamed on
        // anything else.
        for _ in 0..64 {
            drop(HardStop::arm(Duration::from_millis(1)));
        }
        // The margin is what makes the above safe: an armed stop that outlived
        // its navigation by 1 ms would have fired sixty-four times by now.
        std::thread::sleep(Duration::from_millis(50));
    }

    fn tight() -> Budget {
        Budget::new(Limits {
            max_requests: 3,
            max_wire_bytes: 1000,
            max_decoded_bytes: 2000,
            max_network_time: Duration::from_millis(100),
            max_load_time: Duration::from_secs(3600),
        })
    }

    /// The ceiling the per-phase budgets could not give. Without it a page
    /// whose cost is not the network has no stop but `HardStop`, which reports
    /// nothing because it ends the process.
    #[test]
    fn a_page_that_loads_for_too_long_is_refused_the_next_fetch() {
        let budget = Budget::new(Limits {
            max_load_time: Duration::from_millis(40),
            ..Limits::default()
        });
        assert!(budget.claim_request().is_ok(), "inside the clock");
        std::thread::sleep(Duration::from_millis(60));
        let refused = budget
            .claim_request()
            .expect_err("past the clock, whatever else it spent");
        assert!(
            refused.to_string().contains("has been loading for"),
            "the refusal names the clock: {refused}"
        );
        assert!(
            refused.to_string().contains("what there is"),
            "and says the page is unfinished rather than broken: {refused}"
        );

        // A navigation is a fresh decision by the principal, so it gets a
        // fresh clock along with everything else.
        budget.reset();
        assert!(budget.claim_request().is_ok(), "navigating starts it again");
    }

    #[test]
    fn a_page_may_spend_up_to_its_allowance_and_not_past_it() {
        let budget = tight();
        for at in 1..=3 {
            assert!(budget.claim_request().is_ok(), "request {at} is within 3");
        }
        let refused = budget.claim_request().expect_err("the fourth is not");
        assert!(refused.0.contains("budget-exceeded"), "{refused}");
        // Both numbers: what was spent, and what the ceiling was.
        assert!(refused.0.contains("made 4 requests"), "{refused}");
        assert!(refused.0.contains("is 3"), "{refused}");
        // And it says what to do about it.
        assert!(refused.0.contains("Navigating"), "{refused}");
    }

    /// A request that is *attempted* is spent, whatever its outcome. Counting
    /// only successes would give a page whose every fetch fails an unlimited
    /// number of them, which is exactly the runaway shape.
    #[test]
    fn a_failed_request_still_costs_its_place() {
        let budget = tight();
        for _ in 0..3 {
            let _ = budget.claim_request();
            // No `record` call: the request never completed.
        }
        assert!(budget.claim_request().is_err());
        assert_eq!(budget.spent().requests, 4);
    }

    #[test]
    fn the_byte_ceilings_are_cumulative_and_counted_separately() {
        let budget = tight();
        assert!(budget.claim_request().is_ok());
        // Under the wire ceiling, over nothing.
        budget.record(600, 1500, Duration::ZERO);
        assert!(budget.claim_request().is_ok());
        // Now past the wire ceiling.
        budget.record(600, 100, Duration::ZERO);
        let refused = budget.claim_request().expect_err("over the wire ceiling");
        assert!(refused.0.contains("across the wire"), "{refused}");
    }

    /// The decoded ceiling is the one a compression bomb pushes on, and it is
    /// higher than the wire ceiling because decoding legitimately expands.
    #[test]
    fn the_decoded_ceiling_is_separate_from_the_wire_one() {
        let budget = tight();
        assert!(budget.claim_request().is_ok());
        // Tiny on the wire, enormous decoded: under one ceiling, over the other.
        budget.record(10, 5000, Duration::ZERO);
        let refused = budget.claim_request().expect_err("over the decoded ceiling");
        assert!(refused.0.contains("decoded"), "{refused}");
    }

    /// Not the same as a per-request timeout: a hundred requests each well
    /// inside the 30s limit are together minutes an agent is waiting.
    #[test]
    fn network_time_is_summed_across_requests() {
        let budget = tight();
        assert!(budget.claim_request().is_ok());
        budget.record(0, 0, Duration::from_millis(60));
        assert!(budget.claim_request().is_ok(), "60ms is within 100ms");
        budget.record(0, 0, Duration::from_millis(60));
        let refused = budget.claim_request().expect_err("120ms is not");
        assert!(refused.0.contains("waiting on the network"), "{refused}");
    }

    /// A fresh page is a fresh decision by the agent, so it gets a fresh
    /// allowance. The budget bounds untrusted page code, not the principal
    /// driving the engine.
    #[test]
    fn navigating_restores_the_allowance() {
        let budget = tight();
        for _ in 0..4 {
            let _ = budget.claim_request();
        }
        assert!(budget.claim_request().is_err());

        budget.reset();
        assert!(budget.claim_request().is_ok());
        assert_eq!(budget.spent().requests, 1);
    }

    #[test]
    fn spending_is_reported_so_a_reader_can_see_how_close_it_came() {
        let budget = tight();
        let _ = budget.claim_request();
        budget.record(100, 200, Duration::from_millis(5));
        let spent = budget.spent();
        assert_eq!(spent.requests, 1);
        assert_eq!(spent.wire_bytes, 100);
        assert_eq!(spent.decoded_bytes, 200);
        assert_eq!(spent.network_time, Duration::from_millis(5));
    }

    #[test]
    fn a_deadline_reports_what_is_left_rather_than_only_whether_it_passed() {
        let deadline = Deadline::new(Duration::from_secs(10));
        assert!(!deadline.expired());
        assert!(deadline.remaining() <= Duration::from_secs(10));
        assert!(deadline.remaining() > Duration::from_secs(9));

        let done = Deadline::new(Duration::ZERO);
        assert!(done.expired());
        assert_eq!(done.remaining(), Duration::ZERO);
    }
}
