//! The broker: the only way bytes enter this engine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use h5i_error::H5iError;
use url::Url;

use crate::policy::Policy;
use crate::receipt::{Initiator, RequestRecord, Sink};

/// The agent string [`crate::identity::native`] presents.
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; h5i-browser/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/h5i-dev/h5i)"
);

/// What `native` asks for, kept as a constant so the one test that pins the
/// default's wire bytes has something to pin them against.
///
/// Not read when a request is built. The header is derived from
/// [`crate::identity::Locale::accept_language`], off the same list
/// `navigator.languages` reports, because the two used to be written separately
/// and *had already drifted*: this string offered `en` and the script realm's
/// array did not, so a server that content-negotiates on the header while its
/// script reads the array saw two different browsers.
pub const NATIVE_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// What this engine will accept compressed, and can decode.
///
/// Kept beside the decoder deliberately: a header that advertises an encoding
/// `decode_capped` does not handle is a promise the engine cannot keep, and the
/// failure would be a page of binary rather than an error.
const ACCEPT_ENCODING: &str = "gzip, br, deflate";

/// What this engine will take, by what asked for it.
///
/// Not cosmetic: crates.io answered *404* to a request with no `Accept` at
/// all, and the corpus recorded an empty page with no error. A server that
/// content-negotiates cannot serve a client that never says what it wants.
fn accept_for(initiator: Initiator) -> &'static str {
    match initiator {
        // A frame fetch is a document fetch: it negotiates like a navigation,
        // because the server on the other end is serving a page.
        Initiator::Navigation | Initiator::Redirect | Initiator::Frame => {
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        }
        Initiator::Subresource => "*/*",
        // A replay carries its own `Accept`; this is for a composed request
        // that named none.
        Initiator::Replay => "*/*",
    }
}


/// What a fetch produced. A denied or failed fetch is still an outcome, with
/// an empty body and a reason. Never an absence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FetchOutcome {
    /// The receipt sequence number this request was recorded under.
    ///
    /// Carried out of the broker so a caller can say *which* receipt a page's
    /// action produced. `None` when the request never reached the point of
    /// being recorded, which is the same thing as there being no receipt to
    /// name.
    pub seq: Option<u64>,
    /// Response headers, in arrival order. Carried because `Response.headers`
    /// is how a page learns content type, pagination links and rate limits, and
    /// returning `null` from `headers.get` made every one of those look absent
    /// rather than unsupported.
    pub headers: Vec<(String, String)>,
    pub final_url: Url,
    /// Carried beside the message rather than inside it when this crosses a
    /// process boundary: a page's images are megabytes, and base64 in JSON
    /// would pay a third again for the privilege of being unreadable.
    #[serde(skip)]
    pub body: Vec<u8>,
    pub status: Option<u16>,
    pub error: Option<String>,
    /// Set when this was a cross-origin `no-cors` request.
    ///
    /// The body and headers are already empty and the status is already zero,
    /// which is what an opaque response *is*. The flag exists so the caller can
    /// say so rather than presenting a page with an empty 0 that looks like a
    /// failure. A page checking `response.type === "opaque"` is doing the
    /// right thing and should get the right answer.
    pub opaque: bool,
}

impl FetchOutcome {
    /// An outcome that never reached the wire. Public because the script realm
    /// needs to answer a request it could not even start, rather than leaving
    /// the page's promise pending forever.
    pub fn refused(url: Url, error: String) -> Self {
        Self::failed(url, error)
    }

    pub(crate) fn failed(url: Url, error: String) -> Self {
        Self::failed_at(url, error, None)
    }

    pub(crate) fn failed_at(url: Url, error: String, seq: Option<u64>) -> Self {
        Self {
            seq,
            headers: Vec::new(),
            final_url: url,
            body: Vec::new(),
            status: None,
            error: Some(error),
            opaque: false,
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Policy plus receipts plus a client, in that order of importance.
#[derive(Default)]
struct Pinned {
    by_host: std::sync::Mutex<std::collections::HashMap<String, Vec<std::net::SocketAddr>>>,
}

impl Pinned {
    /// Remember the addresses a host was approved for.
    fn set(&self, host: &str, addrs: Vec<std::net::SocketAddr>) {
        if let Ok(mut map) = self.by_host.lock() {
            map.insert(host.to_ascii_lowercase(), addrs);
        }
    }

    fn get(&self, host: &str) -> Option<Vec<std::net::SocketAddr>> {
        self.by_host.lock().ok()?.get(&host.to_ascii_lowercase()).cloned()
    }
}

impl reqwest::dns::Resolve for Pinned {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let found = self.get(name.as_str());
        Box::pin(std::future::ready(match found {
            Some(addrs) => {
                let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
                Ok(iter)
            }
            // Not an error worth dressing up: every request this client makes
            // goes through the broker, which pins before it sends. Arriving
            // here means something reached the wire without a decision, and
            // connecting anyway is the one outcome that must not happen.
            None => Err(format!(
                "`{}` was not resolved through the policy, so this engine will not connect \
                 to it",
                name.as_str()
            )
            .into()),
        }))
    }
}

/// Is this a header the engine owns rather than the page?
///
/// Two groups. The Fetch spec's forbidden request-header names, which a page
/// may not set because they describe the connection rather than the request
/// (`Host`, `Connection`, `Content-Length`) or because they are what the
/// boundary is decided on (`Cookie`, `Origin`, `Referer`, the
/// `Access-Control-Request-*` pair a preflight is made of). And
/// `Accept-Encoding`, which this engine sets because it is the one that has to
/// decode the answer.
fn header_is_the_engines(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "referer"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
    ) || n.starts_with("proxy-")
        || n.starts_with("sec-")
}

/// The three headers a client must compute for itself.
///
/// They frame the message: a `Content-Length` that disagrees with the body, or a
/// `Transfer-Encoding` this client is not using, describes a request other than
/// the one that goes out. A caller setting one is refused and told, which is a
/// different answer from being quietly obeyed by a client that then computes the
/// real value anyway. Request smuggling is a real thing to test for, and an
/// engine that pretended to support it here would be lying about what it sent.
pub fn header_is_the_clients(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "content-length" | "transfer-encoding" | "connection"
    )
}

/// Headers that carry a credential, and so do not cross an origin boundary.
///
/// A caller's headers ride along a redirect chain, because a chain is one
/// request as far as the caller is concerned. These stop at the first hop that
/// changes origin, which is the rule browsers apply to `Authorization` and the
/// reason they apply it: otherwise any server in a chain can harvest the
/// credential by bouncing the request to itself.
fn header_is_a_credential(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "proxy-authorization"
    )
}

/// What a script request needs the broker to know about its origin.
///
/// Only script requests carry one. A navigation has no document behind it, and
/// the absence of this is what says so.
struct CorsContext {
    /// `None` for a document with an opaque origin (a `file:` page, say)
    /// which is same-origin with nothing and so may read nothing cross-origin.
    document: Option<crate::cors::Origin>,
    headers: Vec<(String, String)>,
    mode: crate::cors::Mode,
    credentials: crate::cors::Credentials,
}

/// Who a session presents itself as, reduced to the two values a request carries and computed
/// once.
pub struct Presented {
    user_agent: String,
    accept_language: String,
    /// The whole identity, for the renderer to answer `navigator` from.
    ///
    /// Only the script realm reads this, and only a build with the feature has
    /// a realm that can. The wire needs the two fields above and nothing else.
    #[cfg(feature = "identity")]
    declared: Arc<crate::identity::Identity>,
}

impl Presented {
    /// The honest one: this engine, under its own name.
    ///
    /// Built from the constants rather than from `identity::native()` so that a
    /// build without the feature has one at all. The two agree, and
    /// `identity::tests::native_is_exactly_what_this_engine_already_sent` is
    /// what holds them together.
    pub fn native() -> Result<Self, H5iError> {
        Self::from_parts(
            USER_AGENT.to_string(),
            NATIVE_ACCEPT_LANGUAGE.to_string(),
            #[cfg(feature = "identity")]
            Arc::new(crate::identity::native()),
        )
    }

    /// The one a session was opened with.
    #[cfg(feature = "identity")]
    pub fn declared(identity: Arc<crate::identity::Identity>) -> Result<Self, H5iError> {
        Self::from_parts(
            identity.browser.user_agent.clone(),
            identity.locale.accept_language(),
            identity.clone(),
        )
    }

    fn from_parts(
        user_agent: String,
        accept_language: String,
        #[cfg(feature = "identity")] declared: Arc<crate::identity::Identity>,
    ) -> Result<Self, H5iError> {
        // Validated while the session is being built rather than when a page is
        // waiting on the first request, and the parse is thrown away: what is
        // kept is the string, because that is what a request wants. A session
        // whose languages cannot be a header should refuse at its door, and
        // `Identity::incoherences` already rejects the values that would get
        // here. This is the belt to that brace, and the only check the
        // constants ever need.
        reqwest::header::HeaderValue::from_str(&accept_language).map_err(|e| {
            H5iError::Metadata(format!("that is not a sendable Accept-Language: {e}"))
        })?;
        Ok(Self {
            user_agent,
            accept_language,
            #[cfg(feature = "identity")]
            declared,
        })
    }
}

pub struct LocalBroker {
    /// A handle to itself, for the two operations that hand out something with a
    /// life of its own.
    ///
    /// A WebSocket's reader thread receipts every frame for as long as the
    /// connection is open, so it holds the broker rather than borrowing it, and
    /// [`crate::broker::Broker::open_socket`] takes `&self`, because a trait
    /// method taking `Arc<Self>` could not be called through `dyn Broker`.
    /// `Arc::new_cyclic` closes that.
    me: std::sync::Weak<LocalBroker>,
    policy: Policy,
    sink: Arc<dyn Sink>,
    /// Every record, kept in memory as well as sent to the sink.
    ///
    /// The engine's own copy, and the only one a renderer can ask for. It is
    /// here rather than threaded in beside the sink because the record-keeper
    /// and the thing being asked "what did you record" have to be the same
    /// component, or the answer is somebody's report about it.
    log: Arc<crate::receipt::MemorySink>,
    /// Credentials the agent may use and may not read.
    ///
    /// Read here and nowhere else. This is the process that holds `H5I_SECRET_*`;
    /// the renderer's environment is scrubbed of them, so a compromised parser
    /// reads the values that were substituted into a field it was told to fill
    /// and no others.
    ///
    /// Read once, when the broker is built, so a later `setenv` cannot widen what
    /// the session can reach. See [`crate::secrets`].
    secrets: crate::secrets::Secrets,
    client: reqwest::blocking::Client,
    seq: AtomicU64,
    /// The session's cookies. Attached here rather than by the HTTP client so
    /// that sending one is a decision this broker makes and records, like every
    /// other thing it does with the wire. `reqwest`'s own cookie store would
    /// have done the matching for us and taken that with it.
    jar: crate::cookies::Jar,
    /// Whether requests route through an egress proxy.
    ///
    /// Kept because the socket client has to know: it cannot use one, a raw
    /// `TcpStream` does not go through what `reqwest` was configured with, so
    /// it refuses any non-loopback address while one is set, rather than
    /// stepping around the allowlist that proxy enforces.
    proxied: bool,
    /// What this page may still spend on the network.
    ///
    /// Per navigation, reset by the factory when the agent moves. See
    /// [`crate::budget`] for why the ceiling bounds a page rather than a
    /// session: a loop is untrusted code the engine cannot otherwise stop, and
    /// an agent navigating is the principal exercising its own authority.
    budget: crate::budget::Budget,
    /// Addresses already approved, and the client's only source of them.
    ///
    /// `None` when an egress proxy is configured: the proxy resolves the name
    /// itself and this engine never sees an address, so pinning one would be a
    /// claim it cannot support. The proxy is the enforcement point there, which
    /// is the same division of labour the socket client already follows.
    pinned: Option<Arc<Pinned>>,
    /// Who this session says it is, on the wire and in the page.
    ///
    /// Held by the broker rather than read from a constant because the broker
    /// is the half that puts bytes on the wire, and because it is the same
    /// object the renderer's script realm answers `navigator` from. One
    /// identity, two layers, no second copy to drift: see [`Presented`].
    presented: Presented,
    /// Where the messages themselves are kept, when a session was opened with
    /// somewhere to keep them.
    ///
    /// `None` is the ordinary session and the default: the receipt still
    /// records every decision, and no header or body is written anywhere. The
    /// store lives here rather than in the renderer because this is the half
    /// that holds the request as built and the response as received, and
    /// because the renderer is the untrusted parser. See [`crate::capture`].
    capture: Option<Arc<crate::capture::Capture>>,
}

impl LocalBroker {
    /// Build a broker.
    ///
    /// `proxy` is h5i's egress proxy (`H5I_EGRESS_PROXY`). It is not required,
    /// the engine is useful on a bare host, but inside a box it is how the
    /// sandbox's own allowlist stays in the path. Loopback bypasses it, because
    /// the dev server is not egress.
    pub fn new(
        policy: Policy,
        sink: Arc<dyn Sink>,
        proxy: Option<&str>,
    ) -> Result<Arc<Self>, H5iError> {
        Self::build(
            policy,
            sink,
            proxy,
            crate::budget::Limits::default(),
            crate::secrets::Secrets::from_env(),
            Presented::native()?,
            None,
        )
    }

    /// The same, with the credentials named rather than read from the
    /// environment. For a caller that resolves them somewhere else, and for
    /// tests, which must not depend on what is exported around them.
    pub fn with_secrets(
        policy: Policy,
        sink: Arc<dyn Sink>,
        proxy: Option<&str>,
        secrets: crate::secrets::Secrets,
    ) -> Result<Arc<Self>, H5iError> {
        Self::build(
            policy,
            sink,
            proxy,
            crate::budget::Limits::default(),
            secrets,
            Presented::native()?,
            None,
        )
    }

    /// The same, with the page ceiling the caller wants, and with the message
    /// store the session was opened with, if it was opened with one.
    ///
    /// Separate from [`Self::new`] because the limits have to be in place
    /// before the broker is shared: it is handed out as an `Arc`, a socket's
    /// reader thread holds one for the life of the connection, and there is no
    /// later moment at which it can be borrowed mutably.
    pub fn with_limits(
        policy: Policy,
        sink: Arc<dyn Sink>,
        proxy: Option<&str>,
        limits: crate::budget::Limits,
        capture: Option<Arc<crate::capture::Capture>>,
    ) -> Result<Arc<Self>, H5iError> {
        Self::build(
            policy,
            sink,
            proxy,
            limits,
            crate::secrets::Secrets::from_env(),
            Presented::native()?,
            capture,
        )
    }

    /// The same, presenting the identity the session was opened with.
    ///
    /// A separate constructor rather than a setter, and that is not ceremony: the
    /// agent string is handed to the HTTP client when the client is built, and the
    /// broker is shared as an `Arc` the moment it exists. An identity that could
    /// be changed afterwards would be one that changed mid-session, which is the
    /// single thing a coherent identity must never do.
    #[cfg(feature = "identity")]
    pub fn with_identity(
        policy: Policy,
        sink: Arc<dyn Sink>,
        proxy: Option<&str>,
        limits: crate::budget::Limits,
        identity: Arc<crate::identity::Identity>,
        capture: Option<Arc<crate::capture::Capture>>,
    ) -> Result<Arc<Self>, H5iError> {
        Self::build(
            policy,
            sink,
            proxy,
            limits,
            crate::secrets::Secrets::from_env(),
            Presented::declared(identity)?,
            capture,
        )
    }

    /// Who this session says it is.
    ///
    /// Read by the renderer through the broker, so the half that answers
    /// `navigator` and the half that writes the headers answer from one object
    /// rather than from two copies of one file.
    #[cfg(feature = "identity")]
    pub fn identity(&self) -> &Arc<crate::identity::Identity> {
        &self.presented.declared
    }

    fn build(
        policy: Policy,
        sink: Arc<dyn Sink>,
        proxy: Option<&str>,
        limits: crate::budget::Limits,
        secrets: crate::secrets::Secrets,
        presented: Presented,
        capture: Option<Arc<crate::capture::Capture>>,
    ) -> Result<Arc<Self>, H5iError> {
        let mut builder = reqwest::blocking::Client::builder()
            // Redirects are followed by hand so each hop is a policy decision
            // and a receipt line. Letting the client follow them would hide
            // exactly the hops most worth seeing.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(presented.user_agent.clone());

        let mut proxied = false;
        if let Some(proxy_url) = proxy.filter(|p| !p.trim().is_empty()) {
            proxied = true;
            let no_proxy = reqwest::NoProxy::from_string("localhost,127.0.0.1,::1");
            let proxy = reqwest::Proxy::all(proxy_url)
                .map_err(|e| {
                    H5iError::Metadata(format!("egress proxy `{proxy_url}` is not usable: {e}"))
                })?
                .no_proxy(no_proxy);
            builder = builder.proxy(proxy);
        }

        // With a proxy in the path the name is resolved at the far end and this
        // engine never sees an address, so there is nothing to pin and the
        // proxy is the enforcement point. Without one, every connection goes to
        // an address this broker has already decided about.
        let pinned = if proxied {
            None
        } else {
            let pinned = Arc::new(Pinned::default());
            builder = builder.dns_resolver(pinned.clone());
            Some(pinned)
        };

        let client = builder
            .build()
            .map_err(|e| H5iError::Metadata(format!("failed to build the http client: {e}")))?;

        Ok(Arc::new_cyclic(|me| Self {
            me: me.clone(),
            policy,
            sink,
            log: Arc::new(crate::receipt::MemorySink::new()),
            secrets,
            client,
            budget: crate::budget::Budget::new(limits),
            pinned,
            presented,
            seq: AtomicU64::new(0),
            jar: crate::cookies::Jar::new(),
            proxied,
            capture,
        }))
    }

    /// This broker's own copy of the record, for the broker process's use.
    ///
    /// Not on [`crate::broker::Broker`], which offers `records()`. A reading,
    /// not the sink. Appending is not something a renderer gets to ask for:
    /// the whole claim is that the log is written by the component that made
    /// the decision.
    pub fn log(&self) -> &crate::receipt::MemorySink {
        &self.log
    }

    /// Append to the sink, and to this broker's own copy.
    ///
    /// One method rather than two calls at thirty sites, and the order matters:
    /// the sink is the one that can refuse, and a refusal is what stops the
    /// request. The in-memory copy never fails, so writing it first cannot
    /// change whether a fetch happens.
    fn append(&self, record: &RequestRecord) -> Result<(), H5iError> {
        let _ = self.log.append(record);
        self.sink.append(record)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// What this page has spent, and what it may. The live object, for the
    /// broker's own use; [`crate::broker::Broker::budget`] hands callers a
    /// reading of it instead.
    pub fn spending(&self) -> &crate::budget::Budget {
        &self.budget
    }


    /// Ask a server, before the real request, whether it will accept one.
    fn preflight(
        &self,
        url: &Url,
        ask: &crate::cors::Preflight,
        document: Option<&Url>,
    ) -> Result<(), String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        // The same allowlist as any other request. A preflight is a request to
        // the origin being asked about, so it is subject to the same grant.
        if let Some(reason) = self.policy.check_from(url, document).reason() {
            let record = RequestRecord::request(seq, Initiator::Subresource, "OPTIONS", url.as_str())
                .denied(reason);
            let _ = self.record_pair(&record);
            return Err(format!("the preflight was denied by policy: {reason}"));
        }
        if let Err(reason) = self.pin_addresses(url) {
            let record = RequestRecord::request(seq, Initiator::Subresource, "OPTIONS", url.as_str())
                .denied(&reason);
            let _ = self.record_pair(&record);
            return Err(format!("the preflight was denied by policy: {reason}"));
        }

        // A preflight is a request in its own right, and the budget is the one
        // place that had not been told. It sat *before* the claim in
        // `send_with_cors`, so a page issuing non-simple cross-origin requests
        // whose preflights the server refuses made unlimited round trips while
        // the allowance recorded none of them. The real request never happened,
        // so the request that was counted never happened either.
        if let Err(over) = self.budget.claim_request() {
            let record = RequestRecord::request(seq, Initiator::Subresource, "OPTIONS", url.as_str())
                .denied(&over.0);
            let _ = self.record_pair(&record);
            return Err(over.0);
        }

        let record = RequestRecord::request(seq, Initiator::Subresource, "OPTIONS", url.as_str());
        if let Err(e) = self.append(&record) {
            return Err(format!(
                "refusing to preflight: the receipt could not be written: {e}"
            ));
        }

        let started = Instant::now();
        let mut request = self
            .client
            .request(reqwest::Method::OPTIONS, url.clone())
            .header("origin", ask.origin.clone())
            .header("access-control-request-method", ask.method.clone());
        if !ask.headers.is_empty() {
            request = request.header("access-control-request-headers", ask.headers.join(", "));
        }
        // Never. A preflight is a question about whether a credentialed request
        // would be allowed, and sending the credential to ask would defeat the
        // asking.
        let response = request.send();
        let elapsed = started.elapsed().as_millis() as u64;

        let response = match response {
            Ok(response) => response,
            Err(e) => {
                let mut outcome = record.response();
                outcome.duration_ms = Some(elapsed);
                outcome.error = Some(e.to_string());
                let _ = self.append(&outcome);
                return Err(format!("the preflight could not be sent: {e}"));
            }
        };

        let status = response.status();
        let header = |name: &str| -> Option<String> {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let acao = header("access-control-allow-origin");
        let acac = header("access-control-allow-credentials");
        let methods = header("access-control-allow-methods");
        let headers = header("access-control-allow-headers");

        let mut outcome = record.response();
        outcome.status = Some(status.as_u16());
        outcome.duration_ms = Some(elapsed);
        let _ = self.append(&outcome);
        // What it cost. A preflight is small on the wire and real on the clock,
        // and the clock is the half a page can spend without trying.
        self.budget
            .record(0, 0, Duration::from_millis(elapsed));

        if !status.is_success() {
            return Err(format!(
                "the preflight was answered with {}, so the request was not made.",
                status.as_u16()
            ));
        }

        crate::cors::check_preflight(
            ask,
            acao.as_deref(),
            acac.as_deref(),
            methods.as_deref(),
            headers.as_deref(),
        )
    }

    /// The header set this request will actually go out with.
    ///
    /// Not `built.headers()` alone: `reqwest` merges the client's defaults —
    /// here, the `User-Agent` — at *execute* time, so the store held a request
    /// that was not the one sent. `message --raw` feeds `--raw-request`, and
    /// that round trip was going out with no `User-Agent` at all. Merged the
    /// way `reqwest` does: the request's own value wins.
    fn headers_as_sent(&self, built: &reqwest::blocking::Request) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = built
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            headers.push((
                "user-agent".to_string(),
                self.presented.user_agent.clone(),
            ));
        }
        headers
    }

    /// Resolve a URL's host, check every address it answers with, and pin the
    /// result for the connection that follows. `Ok(())` when there is nothing to
    /// do (a proxy in the path, or a URL with no host) so the caller has one
    /// branch rather than three.
    ///
    /// *Every* address is checked, not the first. A name that answers with one
    /// public address and one loopback address is refused: which one gets
    /// connected to is the client's choice among them, and approving a set while
    /// objecting to a member of it would leave the outcome to chance.
    fn pin_addresses(&self, url: &Url) -> Result<(), String> {
        let Some(pinned) = self.pinned.as_ref() else {
            // A proxy resolves the name at the far end; there is no address
            // here to check, and none to pin.
            return Ok(());
        };
        // `ws`/`wss` too, and that omission was the hole: the socket client
        // reached `TcpStream::connect((host, port))` with a name nothing had
        // resolved, so `Policy::check_address`, the rebinding and
        // private-space guard, never ran on the one path that does its own
        // connecting. A name on the allowlist that answers `10.0.0.1` was a
        // WebSocket into private space with a receipt that said otherwise.
        if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
            return Ok(());
        }
        let Some(host) = url.host_str() else {
            return Ok(());
        };

        let port = url
            .port_or_known_default()
            .unwrap_or(if matches!(url.scheme(), "https" | "wss") { 443 } else { 80 });
        // IPv6 arrives from `host_str` with its brackets, which the resolver
        // does not want.
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);

        use std::net::ToSocketAddrs;
        let resolved: Vec<std::net::SocketAddr> = match (bare, port).to_socket_addrs() {
            Ok(addrs) => addrs.collect(),
            Err(e) => return Err(format!("`{host}` could not be resolved: {e}")),
        };
        if resolved.is_empty() {
            return Err(format!("`{host}` resolved to no addresses"));
        }
        for addr in &resolved {
            if let Some(reason) = self.policy.check_address(url, addr.ip()).reason() {
                return Err(reason.to_string());
            }
        }
        pinned.set(host, resolved);
        Ok(())
    }

    /// The addresses a host was approved for, for a caller that does its own connecting.
    pub fn approved_addresses(&self, host: &str) -> Option<Vec<std::net::SocketAddr>> {
        self.pinned.as_ref()?.get(host)
    }

    /// The session's jar, for the broker's own use.
    ///
    /// Not on [`crate::broker::Broker`], and that is the point of §B18.6's
    /// hardest case: this returns a live reference, which cannot cross a
    /// process boundary and, in one process, put the session in reach of the
    /// parsers. Callers get three operations instead (`document_cookie`,
    /// `store_cookie`, `keep_only_origin`) each of which enforces `HttpOnly`
    /// and origin scoping on the way through.
    pub fn jar(&self) -> &crate::cookies::Jar {
        &self.jar
    }

    /// The four public entry points are [`crate::broker::Broker`]'s, and they
    /// all arrive here: check policy, record the decision, then use the wire,
    /// following redirects by hand so every hop is a decision of its own.
    #[allow(clippy::too_many_arguments)]
    fn send_with_cors(
        &self,
        url: &Url,
        initiator: Initiator,
        method: &str,
        body: &[u8],
        content_type: Option<&str>,
        caller_headers: &[(String, String)],
        max_redirects: Option<usize>,
        document: Option<&Url>,
        cors: Option<&CorsContext>,
    ) -> FetchOutcome {
        let mut current = url.clone();
        // Split once rather than per hop: what the client owns does not depend
        // on where a redirect went.
        let mut overridden: Vec<String> = Vec::new();
        let caller_headers: Vec<(String, String)> = caller_headers
            .iter()
            .filter(|(name, _)| {
                if header_is_the_clients(name) {
                    overridden.push(name.trim().to_ascii_lowercase());
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        // Where the chain began, for the credential rule below.
        let started_at = crate::cors::Origin::of(url);
        // The caller may ask for fewer hops than the policy allows, never more:
        // the policy's number is a ceiling on what this session may do, and a
        // request that could raise it would be a request that sets its own
        // limits.
        let hop_limit = max_redirects
            .unwrap_or_else(|| self.policy.max_redirects())
            .min(self.policy.max_redirects());
        // What a caller may see of the answer. Recomputed per hop, because a
        // redirect can change the origin the answer comes from.
        let mut cors_exposure = crate::cors::Exposure::Full;
        // Set once a redirect has crossed an origin, which makes the request's
        // own origin opaque from there on: a server must not be able to launder
        // a cross-origin read by bouncing it somewhere that says `*`.
        let mut origin_tainted = false;
        // What originally asked, kept separate from the per-hop initiator: a
        // redirect chain that began as a navigation is still asking for a
        // document, and a subresource that redirects is still a subresource.
        let asked_as = initiator;
        let mut initiator = initiator;
        let mut method = method.to_ascii_uppercase();
        let mut body = body.to_vec();

        for hop in 0..=hop_limit {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);

            // 1. Policy. A denial is recorded as a pair like any other request,
            //    so the log shows what was attempted, not only what succeeded.
            let verdict = self.policy.check_from(&current, document);
            if let Some(reason) = verdict.reason() {
                let record = RequestRecord::request(seq, initiator, &method, current.as_str())
                    .denied(reason);
                if let Err(e) = self.record_pair(&record) {
                    return FetchOutcome::failed(current, format!("receipt sink refused: {e}"));
                }
                // A refused *redirect* is a different fact from a refused
                // request, and the difference is actionable. vitejs.dev moved to
                // vite.dev; the corpus reported only "origin is not in the
                // allowlist" and an agent had no way to learn the site had
                // moved. Following it automatically is not the fix, that would
                // let any server route us out of the allowlist, but saying so
                // is.
                let message = if hop == 0 {
                    format!("denied by policy: {reason}")
                } else if cors.is_some() {
                    // A page asked, and where a server redirected it is not the page's to
                    // learn.
                    "the request was redirected somewhere this engine is not allowed to \
                     follow, so it was not followed"
                        .to_string()
                } else {
                    // The agent asked. A redirect that left the allowlist is
                    // the most actionable thing this engine can say. Vitejs.dev
                    // moved to vite.dev and the corpus reported only "origin is
                    // not in the allowlist", leaving no way to learn the site
                    // had moved. Following it automatically is not the fix, that
                    // would let any server route us out of the allowlist, but
                    // saying so is.
                    format!(
                        "the site redirected to {current}, which is not allowed: {reason}. \
                         This engine will not follow a redirect out of the allowlist, because \
                         a server could then choose where we go; allow that host if you meant \
                         to follow it."
                    )
                };
                return FetchOutcome::failed_at(current, message, Some(seq));
            }

            // 1b. Where that name actually goes. The check above decided about
            //     a *name*; this decides about the address behind it and pins
            //     the answer, so the bytes cannot reach somewhere the decision
            //     never saw. Recorded as a denial like any other, because "the
            //     name was allowed and the address was not" is exactly what a
            //     reader of the log would want to find.
            if let Err(reason) = self.pin_addresses(&current) {
                let record = RequestRecord::request(seq, initiator, &method, current.as_str())
                    .denied(&reason);
                if let Err(e) = self.record_pair(&record) {
                    return FetchOutcome::failed(current, format!("receipt sink refused: {e}"));
                }
                return FetchOutcome::failed_at(
                    current,
                    format!("denied by policy: {reason}"),
                    Some(seq),
                );
            }

            // 1c. The same-origin policy. The checks above decided whether
            //     this engine may *connect*; this decides whether the document
            //     that asked may *read the answer*, which is a different
            //     question and was going unasked. See `crate::cors`.
            let mut cors_plan: Option<crate::cors::Plan> = None;
            if let Some(context) = cors {
                // Tainted by a cross-origin redirect, or a document with no
                // origin of its own: both are opaque, and an opaque requester
                // is same-origin with nothing. Distinct from `Agent`, which is
                // also origin-less and is the *opposite* case. See
                // `cors::Requester`.
                let requester = match (origin_tainted, context.document.as_ref()) {
                    (false, Some(origin)) => crate::cors::Requester::Document(origin),
                    _ => crate::cors::Requester::Opaque,
                };
                let plan = crate::cors::plan(
                    requester,
                    &current,
                    &method,
                    &context.headers,
                    context.mode,
                    context.credentials,
                    self.policy.cors_stance(),
                );

                if let crate::cors::Plan::Blocked(why) = &plan {
                    let record =
                        RequestRecord::request(seq, initiator, &method, current.as_str())
                            .denied(why);
                    if let Err(e) = self.record_pair(&record) {
                        return FetchOutcome::failed(
                            current,
                            format!("receipt sink refused: {e}"),
                        );
                    }
                    return FetchOutcome::failed_at(
                        current,
                        format!("blocked by the same-origin policy: {why}"),
                        Some(seq),
                    );
                }

                // A request that is not simple asks permission first, and the
                // preflight is a request like any other: policy-checked and
                // receipted, so it appears in the log rather than arriving
                // from nowhere.
                if let crate::cors::Plan::Send {
                    preflight: Some(ask),
                    ..
                } = &plan
                    && let Err(why) = self.preflight(&current, ask, document)
                {
                    return FetchOutcome::failed_at(
                        current,
                        format!("blocked by the same-origin policy: {why}"),
                        Some(seq),
                    );
                }
                cors_plan = Some(plan);
            }

            // 1d. What this page has left to spend. Every limit before this
            //     one is *per request* (a size cap, a redirect count, a
            //     timeout) and none of them bounds a page that makes many.
            //     A refusal here is recorded like any other, because "the page
            //     ran out of allowance" is exactly what a reader of the log
            //     needs to see rather than a request that silently stopped.
            if let Err(over) = self.budget.claim_request() {
                let record = RequestRecord::request(seq, initiator, &method, current.as_str())
                    .denied(&over.0);
                if let Err(e) = self.record_pair(&record) {
                    return FetchOutcome::failed(current, format!("receipt sink refused: {e}"));
                }
                return FetchOutcome::failed_at(current, over.0, Some(seq));
            }

            // 2. The decision record, before any bytes move. If this cannot be
            //    written, the fetch does not happen. This is the fail-closed
            //    guarantee, and it is why `Sink::append` returns a Result.
            let mut record = RequestRecord::request(seq, initiator, &method, current.as_str());
            record.headers_overridden = overridden.clone();
            if let Err(e) = self.append(&record) {
                return FetchOutcome::failed(
                    current,
                    format!("refusing to fetch: the receipt could not be written: {e}"),
                );
            }

            // 3. The wire. Cookies are attached here, after the policy check
            //    and after the record: a request that policy refuses must never
            //    have carried a credential anywhere, not even into a log line.
            let started = Instant::now();
            let verb = reqwest::Method::from_bytes(method.as_bytes())
                .unwrap_or(reqwest::Method::GET);
            // The page's own headers, on the request `cors::plan` approved.
            //
            // They were used to decide whether a preflight was needed and what
            // it asked permission for, and then never sent: the server answered
            // a question about a request that did not happen, and
            // `fetch(u, {headers: {Authorization: …}})` lost the header without
            // saying so. Forbidden names are dropped (see
            // [`header_is_the_engines`]), because a page choosing `Host`,
            // `Cookie` or `Origin` would be choosing what the CORS decision was
            // made about.
            let page_headers: Vec<(&str, &str)> = cors
                .map(|c| c.headers.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|(name, _)| !header_is_the_engines(name))
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let page_sets = |n: &str| page_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));

            // The caller's own headers, for this hop. A chain that has changed
            // origin no longer carries the credential the caller set for the
            // origin it named: see `header_is_a_credential`.
            let crossed = crate::cors::Origin::of(&current) != started_at;
            let caller_now: Vec<&(String, String)> = caller_headers
                .iter()
                .filter(|(name, _)| !(crossed && header_is_a_credential(name)))
                .collect();
            let caller_sets =
                |n: &str| caller_now.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));

            let mut request = self.client.request(verb, current.clone());
            // Every header the engine sets by default has to stand aside for a
            // caller that set it, and this one is the reason the rule is
            // written as a loop rather than as three ifs: a replay hands back
            // the *stored* header set, which already contains the engine's own
            // defaults, so any default added unconditionally arrives twice. It
            // did, and `message --raw` showed two `accept-encoding` lines on
            // every replayed request.
            if !caller_sets("accept-encoding") {
                request = request.header(reqwest::header::ACCEPT_ENCODING, ACCEPT_ENCODING);
            }
            // Ours only where neither the page nor the caller said: `reqwest`'s
            // builder appends rather than replaces, so setting both would send
            // both, and a request carrying two `Accept` headers is not the
            // request either of them asked for.
            if !page_sets("accept") && !caller_sets("accept") {
                request = request.header(reqwest::header::ACCEPT, accept_for(asked_as));
            }
            if !page_sets("accept-language") && !caller_sets("accept-language") {
                request = request.header(
                    reqwest::header::ACCEPT_LANGUAGE,
                    self.presented.accept_language.as_str(),
                );
            }
            for (name, value) in &page_headers {
                request = request.header(*name, *value);
            }
            for (name, value) in &caller_now {
                request = request.header(name.as_str(), value.as_str());
            }
            if !body.is_empty() {
                // The caller's own `Content-Type` wins, and the engine's is not
                // sent beside it. A replayed request carries the stored header
                // set *and* names its content type, so without this check the
                // message went out with the header twice and servers answered
                // 400 to a request that looked perfectly good in the log.
                if let Some(kind) = content_type
                    && !page_sets("content-type")
                    && !caller_sets("content-type")
                {
                    request = request.header(reqwest::header::CONTENT_TYPE, kind);
                }
                request = request.body(body.clone());
            }
            // Cookies, and whether this request may carry them at all.
            //
            // The gate is the whole point of the credentials mode: a cross-origin
            // `fetch` defaults to sending none, so a script on one allowlisted
            // origin cannot read another origin's pages *as the logged-in user*.
            // Before this the jar was attached to every request unconditionally,
            // which turned the missing same-origin policy from an unauthenticated
            // cross-origin read into an authenticated one.
            let may_use_cookies = match &cors_plan {
                Some(crate::cors::Plan::Send { send_cookies, .. }) => *send_cookies,
                // No CORS context: the agent asked, and its own requests carry
                // its own session.
                _ => true,
            };
            let mut cookies_sent = 0;
            if may_use_cookies
                // A caller that named `Cookie` itself is testing what that
                // exact value does, and the jar's copy beside it would make the
                // request neither what was asked for nor what the session
                // holds. The receipt's count then honestly reads zero: the jar
                // sent nothing.
                && !caller_sets("cookie")
                && let Some((header, count)) = self.jar.header_for(&current)
            {
                request = request.header(reqwest::header::COOKIE, header);
                cookies_sent = count;
            }
            // Cross-origin requests announce themselves, which is how a server
            // knows to answer the CORS question at all.
            if let Some(crate::cors::Plan::Send {
                origin_header: Some(origin),
                ..
            }) = &cors_plan
            {
                request = request.header("origin", origin.clone());
            }
            // Built rather than sent, which is the same two steps `send`
            // takes. The difference is that the finished request can be read:
            // the header set here is the one that went, engine defaults, jar
            // cookie, identity and all, and a store that recorded what the
            // caller asked for instead would be recording an intention.
            let response = request.build().and_then(|built| {
                if let Some(capture) = &self.capture {
                    capture.request(
                        seq,
                        built.method().as_str(),
                        built.url().as_str(),
                        self.headers_as_sent(&built),
                        &body,
                        content_type,
                    );
                }
                self.client.execute(built)
            });
            // The headers, not the body: `reqwest` returns as soon as the
            // status line and headers have arrived. That is time to first byte,
            // and it is the number a timing test wants.
            let ttfb = started.elapsed().as_millis() as u64;

            let response = match response {
                Ok(response) => response,
                Err(e) => {
                    let mut outcome_record = record.response();
                    outcome_record.duration_ms = Some(ttfb);
                    outcome_record.ttfb_ms = Some(ttfb);
                    outcome_record.cookies_sent = Some(cookies_sent);
                    outcome_record.error = Some(e.to_string());
                    let _ = self.append(&outcome_record);
                    return FetchOutcome::failed_at(current, e.to_string(), Some(seq));
                }
            };

            let status = response.status();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value.to_str().ok().map(|v| (name.as_str().to_string(), v.to_string()))
                })
                .collect();

            // Before the redirect branch, deliberately: a login flow sets its
            // session cookie on the 302 itself, so a jar that only looked at
            // final responses would never see the thing it exists to hold.
            //
            // Gated by the same flag that decided whether to *send* one: the
            // credentials mode governs both directions. Storing unconditionally
            // let an attacker page write another allowlisted origin's cookies
            // into the shared jar with a `no-cors`, `credentials: omit` request
            // whose answer it was told nothing about — session fixation driven
            // from a response nobody was allowed to read.
            let cookies_stored = if may_use_cookies {
                self.jar.store(
                    &current,
                    response
                        .headers()
                        .get_all(reqwest::header::SET_COOKIE)
                        .iter()
                        .filter_map(|v| v.to_str().ok()),
                )
            } else {
                0
            };

            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|loc| current.join(loc).ok());

                let mut outcome_record = record.response();
                outcome_record.status = Some(status.as_u16());
                outcome_record.duration_ms = Some(started.elapsed().as_millis() as u64);
                outcome_record.ttfb_ms = Some(ttfb);
                outcome_record.cookies_sent = Some(cookies_sent);
                outcome_record.cookies_stored = Some(cookies_stored);
                if location.is_none() {
                    outcome_record.error = Some("redirect without a usable Location".to_string());
                }
                let _ = self.append(&outcome_record);
                // A hop is a message like any other, and the one whose headers
                // matter most: `Location` is the whole content of a redirect
                // and `Set-Cookie` on a 302 is how a login lands.
                if let Some(capture) = &self.capture {
                    capture.response(crate::capture::Response {
                        seq,
                        url: current.as_str(),
                        status: Some(status.as_u16()),
                        headers: headers.clone(),
                        content_encoding: None,
                        wire_bytes: None,
                        body: crate::capture::Received::NotRead,
                        trailing: &[],
                    });
                }

                match location {
                    Some(next) if hop < hop_limit => {
                        // A redirect that crosses an origin makes the request's
                        // own origin opaque from here on, so a server cannot
                        // launder a cross-origin read by bouncing it somewhere
                        // that answers `*`. Checked against the *previous* hop,
                        // because that is the boundary being crossed.
                        if cors.is_some()
                            && crate::cors::Origin::of(&next) != crate::cors::Origin::of(&current)
                        {
                            origin_tainted = true;
                        }
                        current = next;
                        initiator = Initiator::Redirect;
                        // 303 always, and 301/302 by universal practice, turn
                        // the follow-up into a bodyless GET. Carrying a form
                        // body onward would replay a password to whatever the
                        // server named next.
                        if matches!(status.as_u16(), 301..=303) {
                            method = "GET".to_string();
                            body.clear();
                        }
                        continue;
                    }
                    Some(next) => {
                        // Out of hops. With a limit of zero that is not a
                        // failure, it is the answer: the caller asked to see the
                        // redirect rather than follow it, and the outcome
                        // carries the status, the headers and the `Location`.
                        if hop_limit == 0 {
                            let exposure = cors_exposure.clone();
                            return FetchOutcome {
                                seq: Some(seq),
                                headers: crate::cors::filter_headers(&headers, &exposure),
                                final_url: current,
                                body: Vec::new(),
                                status: Some(status.as_u16()),
                                error: None,
                                opaque: false,
                            };
                        }
                        let _ = next;
                        return FetchOutcome::failed(
                            current,
                            format!("too many redirects (limit {hop_limit})"),
                        );
                    }
                    None => {
                        return FetchOutcome::failed(
                            current,
                            "redirect without a usable Location".to_string(),
                        );
                    }
                }
            }

            // The answer to the question the `Origin` header asked. A response
            // that does not name this origin back is not handed to the caller,
            // which is the entire point, and the reason the body is not even
            // read below when this fails.
            if let Some(crate::cors::Plan::Send {
                check_response: true,
                send_cookies,
                ..
            }) = &cors_plan
            {
                let header = |name: &str| -> Option<&str> {
                    headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.as_str())
                };
                let origin = match &cors_plan {
                    Some(crate::cors::Plan::Send {
                        origin_header: Some(origin),
                        ..
                    }) => origin.clone(),
                    _ => "null".to_string(),
                };
                if let Err(why) = crate::cors::check_response(
                    header("access-control-allow-origin"),
                    header("access-control-allow-credentials"),
                    &origin,
                    *send_cookies,
                ) {
                    let mut refused = record.response();
                    refused.status = Some(status.as_u16());
                    refused.duration_ms = Some(started.elapsed().as_millis() as u64);
                    refused.ttfb_ms = Some(ttfb);
                    refused.cookies_sent = Some(cookies_sent);
                    refused.cookies_stored = Some(cookies_stored);
                    refused.error = Some(format!("blocked by the same-origin policy: {why}"));
                    let _ = self.append(&refused);
                    if let Some(capture) = &self.capture {
                        capture.response(crate::capture::Response {
                            seq,
                            url: current.as_str(),
                            status: Some(status.as_u16()),
                            headers: headers.clone(),
                            content_encoding: None,
                            wire_bytes: None,
                            body: crate::capture::Received::NotRead,
                            trailing: &[],
                        });
                    }
                    return FetchOutcome::failed_at(
                        current,
                        format!("blocked by the same-origin policy: {why}"),
                        Some(seq),
                    );
                }
                // Allowed. What of the headers the caller may see is the
                // server's to widen, and `*` does not widen a credentialed
                // response.
                cors_exposure = crate::cors::exposure_from(
                    header("access-control-expose-headers"),
                    *send_cookies,
                );
            } else if let Some(crate::cors::Plan::Send {
                exposure: crate::cors::Exposure::Opaque,
                ..
            }) = &cors_plan
            {
                cors_exposure = crate::cors::Exposure::Opaque;
            }

            // What crossed the wire, before `reqwest` decodes it. Read from the
            // response's own headers rather than measured, because the decoding
            // happens inside the client and the compressed bytes are gone by the
            // time a body is in hand.
            //
            // `Content-Length` is the *compressed* length when the body is encoded,
            // which is exactly the number wanted here. Absent under chunked
            // transfer, and absent is the honest answer: recording the decoded size
            // under `wire_bytes` would be a guess wearing a measurement's name.
            let encoding = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
                .map(|(_, value)| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty() && value != "identity");

            // Read compressed, then decode. Both sizes are *measured* rather
            // than read off a header, so they are right under chunked transfer
            // and cannot be lied about by a `Content-Length` that disagrees
            // with the body.
            let body = self.read_capped(response).and_then(|raw| {
                let wire = raw.len() as u64;
                match &encoding {
                    None => Ok((raw, wire, false)),
                    Some(encoding) => self
                        .decode_capped(&raw, encoding)
                        .map(|decoded| (decoded, wire, true)),
                }
            });

            let mut outcome_record = record.response();
            outcome_record.status = Some(status.as_u16());
            // Now that the body is in hand: the whole fetch, where `ttfb` was
            // the decision.
            outcome_record.duration_ms = Some(started.elapsed().as_millis() as u64);
            outcome_record.ttfb_ms = Some(ttfb);
            outcome_record.cookies_sent = Some(cookies_sent);
            outcome_record.cookies_stored = Some(cookies_stored);
            let body = match body {
                Ok((decoded, wire, was_encoded)) => {
                    if was_encoded {
                        outcome_record.wire_bytes = Some(wire);
                        outcome_record.content_encoding = encoding.clone();
                    }
                    // What this one cost, against the page's allowance. Both
                    // sizes, because a compressed response costs the wire
                    // little and the page's memory a great deal.
                    self.budget.record(
                        wire,
                        decoded.len() as u64,
                        std::time::Duration::from_millis(
                            outcome_record.duration_ms.unwrap_or(ttfb),
                        ),
                    );
                    Ok(decoded)
                }
                Err(e) => {
                    // A failed read still cost the time it took.
                    self.budget.record(
                        0,
                        0,
                        std::time::Duration::from_millis(
                            outcome_record.duration_ms.unwrap_or(ttfb),
                        ),
                    );
                    Err(e)
                }
            };

            return match body {
                Ok(body) => {
                    outcome_record.bytes = Some(body.len() as u64);
                    let _ = self.append(&outcome_record);
                    // Stored as received, before the CORS filter below. The
                    // filter answers what a *page's script* may read, and the
                    // reader of this store is the agent, which could have asked
                    // for this URL itself under the same policy. Storing the
                    // filtered view would mean an agent's own evidence was
                    // narrowed by a rule written about somebody else.
                    if let Some(capture) = &self.capture {
                        capture.response(crate::capture::Response {
                            seq,
                            url: current.as_str(),
                            status: Some(status.as_u16()),
                            headers: headers.clone(),
                            content_encoding: outcome_record.content_encoding.clone(),
                            wire_bytes: outcome_record.wire_bytes,
                            body: crate::capture::Received::Bytes(&body),
                            trailing: &[],
                        });
                    }
                    // What the caller may see of this. Same-origin sees
                    // everything; a cross-origin CORS response is filtered to
                    // the safelist plus whatever the server exposed; a no-cors
                    // response is opaque, which is the whole reason it was
                    // allowed to be sent without asking.
                    let exposure = cors_exposure.clone();
                    let opaque = matches!(exposure, crate::cors::Exposure::Opaque);
                    FetchOutcome {
                        seq: Some(seq),
                        headers: crate::cors::filter_headers(&headers, &exposure),
                        final_url: current,
                        body: if opaque { Vec::new() } else { body },
                        status: if opaque { Some(0) } else { Some(status.as_u16()) },
                        error: None,
                        opaque,
                    }
                }
                Err(e) => {
                    outcome_record.error = Some(e.to_string());
                    let _ = self.append(&outcome_record);
                    // The response arrived and the body did not survive being
                    // read: too large for the cap, or a decoder that refused it.
                    // The headers are still evidence, and often the answer.
                    if let Some(capture) = &self.capture {
                        capture.response(crate::capture::Response {
                            seq,
                            url: current.as_str(),
                            status: Some(status.as_u16()),
                            headers: headers.clone(),
                            content_encoding: outcome_record.content_encoding.clone(),
                            wire_bytes: outcome_record.wire_bytes,
                            body: crate::capture::Received::NotRead,
                            trailing: &[],
                        });
                    }
                    FetchOutcome::failed_at(current, e.to_string(), Some(seq))
                }
            };
        }

        FetchOutcome::failed(
            current,
            format!("too many redirects (limit {})", self.policy.max_redirects()),
        )
    }

    /// Read at most `max_response_bytes`, so one hostile response cannot become this process's
    /// memory ceiling.
    fn decode_capped(&self, raw: &[u8], encoding: &str) -> Result<Vec<u8>, H5iError> {
        use std::io::Read;
        let cap = self.policy.max_response_bytes();

        // One encoding, or a refusal. This makes a single pass, and a stacked
        // `gzip, br` needs two of them in order; taking the *last* name and
        // decoding with it is not half the answer, it is the wrong one under a
        // message that sends the reader somewhere else ("the br response could
        // not be decoded", over gzip bytes). `identity` is a no-op in the list
        // and does not count as a layer.
        let mut layers = encoding
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("identity"));
        let name = layers.next().unwrap_or("");
        if layers.next().is_some() {
            return Err(H5iError::Metadata(format!(
                "the response stacks encodings (`{}`), which this engine decodes one layer at \
                 a time. It asked for {ACCEPT_ENCODING}.",
                encoding.trim()
            )));
        }
        let mut out = Vec::new();
        let read = match name {
            "gzip" | "x-gzip" => flate2::read::GzDecoder::new(raw)
                .take(cap + 1)
                .read_to_end(&mut out),
            // `deflate` is specified as zlib and sent as raw by enough servers
            // that a browser has to try both. The bare form is the fallback,
            // which is what every other engine does here.
            "deflate" => flate2::read::ZlibDecoder::new(raw)
                .take(cap + 1)
                .read_to_end(&mut out)
                .or_else(|_| {
                    out.clear();
                    flate2::read::DeflateDecoder::new(raw)
                        .take(cap + 1)
                        .read_to_end(&mut out)
                }),
            "br" => brotli::Decompressor::new(raw, 4096)
                .take(cap + 1)
                .read_to_end(&mut out),
            other => {
                return Err(H5iError::Metadata(format!(
                    "the response is `{other}`-encoded, which this engine cannot decode. It \
                     asked for {ACCEPT_ENCODING}."
                )))
            }
        };
        read.map_err(|e| {
            H5iError::Metadata(format!("the {name} response could not be decoded: {e}"))
        })?;

        if out.len() as u64 > cap {
            return Err(H5iError::Metadata(format!(
                "the response decompresses past the {cap} byte cap, so it was not read. A \
                 small response that expands without limit is how a page exhausts the \
                 memory of whatever is reading it."
            )));
        }
        Ok(out)
    }

    fn read_capped(&self, response: reqwest::blocking::Response) -> Result<Vec<u8>, H5iError> {
        use std::io::Read;

        let cap = self.policy.max_response_bytes();
        let mut buf = Vec::new();
        let mut reader = response.take(cap + 1);
        reader
            .read_to_end(&mut buf)
            .map_err(|e| H5iError::Metadata(format!("failed to read the response body: {e}")))?;

        if buf.len() as u64 > cap {
            return Err(H5iError::Metadata(format!(
                "response exceeds the {cap} byte cap"
            )));
        }
        Ok(buf)
    }

    /// Whether an egress proxy is in the path.
    ///
    /// Read by the socket client, which cannot go through one: a raw
    /// `TcpStream` bypasses whatever `reqwest` was configured with, and inside
    /// a box that proxy is how the sandbox's allowlist stays in the path.
    pub fn has_proxy(&self) -> bool {
        self.proxied
    }

    /// Authorise a long-lived connection and record the decision.
    ///
    /// The front half of [`crate::broker::Broker::send_from`] (policy, then the record,
    /// *then* the caller may dial) for a connection that has no single body to
    /// read and so cannot use the rest of that loop. Returns the sequence the
    /// handshake was recorded under.
    ///
    /// Same rule, same order: no receipt, no connection.
    pub fn authorise_socket(&self, url: &Url, document: Option<&Url>) -> Result<u64, String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        let verdict = self.policy.check_from(url, document);
        if let Some(reason) = verdict.reason() {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "WS-OPEN", url.as_str())
                    .denied(reason);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(format!("denied by policy: {reason}"));
        }

        // And where that name goes, on the same rule as every other request.
        if let Err(reason) = self.pin_addresses(url) {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "WS-OPEN", url.as_str())
                    .denied(&reason);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(format!("denied by policy: {reason}"));
        }

        // Against the page's allowance like any other request. A connection is
        // one request that then carries an unbounded number of frames (the
        // frames are charged in `record_socket_frame`, and this is the handshake)
        // so a page could otherwise open as many as it liked and spend
        // nothing to do it.
        if let Err(over) = self.budget.claim_request() {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "WS-OPEN", url.as_str())
                    .denied(&over.0);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(over.0);
        }

        let record = RequestRecord::request(seq, Initiator::Subresource, "WS-OPEN", url.as_str());
        if let Err(e) = self.append(&record) {
            return Err(format!(
                "refusing to connect: the receipt could not be written: {e}"
            ));
        }
        Ok(seq)
    }

    /// Record one frame on an open connection.
    pub fn record_socket_frame(
        &self,
        url: &Url,
        direction: crate::wsclient::Direction,
        bytes: u64,
    ) -> Result<(), H5iError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let record = RequestRecord::request(
            seq,
            Initiator::Subresource,
            direction.as_method(),
            url.as_str(),
        );
        self.append(&record)?;
        let mut outcome = record.response();
        outcome.bytes = Some(bytes);
        // No status. 101 is the WebSocket upgrade's, and stamping it on every
        // frame said "switching protocols" four hundred times on one
        // connection, and said it on event streams, which never switched
        // anything. A frame is not an exchange with a status of its own.
        self.append(&outcome)?;

        // And charged, which it was not.
        self.budget
            .record(bytes, bytes, std::time::Duration::ZERO);
        self.budget.within_totals().map_err(|over| {
            H5iError::Metadata(format!(
                "{} A long-lived connection is charged per frame, so this is the page's \
                 whole allowance rather than this frame's.",
                over.0
            ))
        })
    }

    /// Authorise and begin an event stream, handing back the open response.
    pub fn begin_event_stream(
        &self,
        url: &Url,
        document: Option<&Url>,
    ) -> Result<reqwest::blocking::Response, String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        let verdict = self.policy.check_from(url, document);
        if let Some(reason) = verdict.reason() {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "SSE-OPEN", url.as_str())
                    .denied(reason);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(format!("denied by policy: {reason}"));
        }

        // And where that name goes, on the same rule as every other request.
        if let Err(reason) = self.pin_addresses(url) {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "SSE-OPEN", url.as_str())
                    .denied(&reason);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(format!("denied by policy: {reason}"));
        }

        // Who is asking, and so whether the answer may be read. `None` is the
        // agent naming a URL and is unrestricted; a document is subject to the
        // origin boundary like any other request it makes.
        let plan = match document {
            None => None,
            Some(doc) => {
                let origin = crate::cors::Origin::of(doc);
                let requester = match origin.as_ref() {
                    Some(origin) => crate::cors::Requester::Document(origin),
                    None => crate::cors::Requester::Opaque,
                };
                Some(crate::cors::plan(
                    requester,
                    url,
                    "GET",
                    &[],
                    crate::cors::Mode::Cors,
                    // `withCredentials` is not exposed, so this is the
                    // default: cookies same-origin, never across.
                    crate::cors::Credentials::SameOrigin,
                    self.policy.cors_stance(),
                ))
            }
        };
        if let Some(crate::cors::Plan::Blocked(why)) = &plan {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "SSE-OPEN", url.as_str())
                    .denied(why);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(format!("blocked by the same-origin policy: {why}"));
        }
        let (origin_header, send_cookies, check_response) = match &plan {
            Some(crate::cors::Plan::Send {
                origin_header,
                send_cookies,
                check_response,
                ..
            }) => (origin_header.clone(), *send_cookies, *check_response),
            _ => (None, true, false),
        };

        // Budgeted like the socket handshake, and for the same reason: opening
        // a stream is one request that then carries frames, each of which
        // spends the allowance in `record_socket_frame`. Without this a page
        // could open as many streams as it liked, each one a thread, and the
        // allowance would say it had spent nothing.
        if let Err(over) = self.budget.claim_request() {
            let record =
                RequestRecord::request(seq, Initiator::Subresource, "SSE-OPEN", url.as_str())
                    .denied(&over.0);
            if let Err(e) = self.record_pair(&record) {
                return Err(format!("receipt sink refused: {e}"));
            }
            return Err(over.0);
        }

        let record = RequestRecord::request(seq, Initiator::Subresource, "SSE-OPEN", url.as_str());
        if let Err(e) = self.append(&record) {
            return Err(format!(
                "refusing to connect: the receipt could not be written: {e}"
            ));
        }

        let mut request = self
            .client
            .request(reqwest::Method::GET, url.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(
                reqwest::header::ACCEPT_LANGUAGE,
                self.presented.accept_language.as_str(),
            )
            // The client carries a 30s timeout for ordinary requests, which is
            // exactly wrong for a stream that is *supposed* to stay open and
            // quiet. Cleared for this one request only.
            .timeout(Duration::from_secs(60 * 60));
        // `header_for` reports the value and how many cookies went with it;
        // only the value goes on the wire, and the count is what a receipt is
        // allowed to carry.
        if send_cookies
            && let Some((cookies, _count)) = self.jar.header_for(url)
        {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        if let Some(origin) = &origin_header {
            request = request.header("origin", origin.clone());
        }

        match request.send() {
            Ok(response) => {
                // The response half, on success too. Every other path here
                // writes both phases, and a reader that pairs them (the
                // console's request/response linkage, `h5i box watch`) showed
                // an `SSE-OPEN` request that never completed for the life of
                // the session. It records that the connection was *established*;
                // what flows after it is receipted per event.
                let mut outcome = record.response();
                outcome.status = Some(response.status().as_u16());

                // The answer to the question the `Origin` header asked. A
                // stream is a body like any other, and a cross-origin one may
                // not be read unless the server named this origin back.
                if check_response {
                    let header = |name: &str| -> Option<String> {
                        response
                            .headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    let asked = origin_header.clone().unwrap_or_else(|| "null".to_string());
                    if let Err(why) = crate::cors::check_response(
                        header("access-control-allow-origin").as_deref(),
                        header("access-control-allow-credentials").as_deref(),
                        &asked,
                        send_cookies,
                    ) {
                        outcome.error =
                            Some(format!("blocked by the same-origin policy: {why}"));
                        let _ = self.append(&outcome);
                        return Err(format!("blocked by the same-origin policy: {why}"));
                    }
                }

                // And that it really is a stream. A browser fails an
                // `EventSource` whose answer is not `text/event-stream`, and
                // the rule earns its place beyond conformance: without it the
                // line parser is a reader for *any* body, and every line
                // beginning `data:` in someone else's document becomes a
                // message the page receives.
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                if content_type != "text/event-stream" {
                    let named = if content_type.is_empty() {
                        "(no content type)"
                    } else {
                        content_type.as_str()
                    };
                    let why = format!(
                        "the answer is `{named}`, not `text/event-stream`, so it is not an \
                         event stream and was not read."
                    );
                    outcome.error = Some(why.clone());
                    let _ = self.append(&outcome);
                    return Err(why);
                }

                if let Err(e) = self.append(&outcome) {
                    return Err(format!(
                        "refusing to stream: the receipt could not be written: {e}"
                    ));
                }
                Ok(response)
            }
            Err(error) => {
                let mut outcome = record.response();
                outcome.error = Some(error.to_string());
                let _ = self.append(&outcome);
                Err(format!("could not open the event stream: {error}"))
            }
        }
    }

    /// Write both phases for a request that never reaches the wire.
    fn record_pair(&self, record: &RequestRecord) -> Result<(), H5iError> {
        self.append(record)?;
        self.append(&record.response())
    }
}

impl crate::broker::Broker for LocalBroker {
    #[cfg(feature = "identity")]
    fn identity(&self) -> Arc<crate::identity::Identity> {
        self.presented.declared.clone()
    }

    fn send(&self, fetch: &crate::broker::Fetch) -> FetchOutcome {
        let context = fetch.cors.as_ref().map(|ask| CorsContext {
            document: crate::cors::Origin::of(&ask.document),
            headers: ask.headers.clone(),
            mode: ask.mode,
            credentials: ask.credentials,
        });
        self.send_with_cors(
            &fetch.url,
            fetch.initiator,
            &fetch.method,
            &fetch.body,
            fetch.content_type.as_deref(),
            &fetch.headers,
            fetch.max_redirects,
            fetch.document.as_ref(),
            context.as_ref(),
        )
    }

    fn capture(&self) -> Option<crate::capture::Health> {
        self.capture.as_ref().map(|capture| capture.health())
    }

    fn send_edited(
        &self,
        from: u64,
        edits: &[crate::edits::Edit],
        create: bool,
        plan: crate::broker::Sends,
    ) -> Result<crate::broker::Edited, crate::broker::SendError> {
        use crate::broker::SendError;
        let Some(store) = &self.capture else {
            return Err(SendError::new(
                "no-capture",
                "this session was not opened with `--capture`, so it kept no request to send \
                 again",
            ));
        };
        let stored = store.read_request(from).map_err(|_| {
            SendError::new(
                "no-such-request",
                format!(
                    "there is no stored request {from} in this session. \
                     `requests` lists the sequence numbers this session has"
                ),
            )
        })?;
        let url = Url::parse(&stored.url).map_err(|e| {
            SendError::new(
                "no-such-request",
                format!("stored request {from} has an unusable URL: {e}"),
            )
        })?;
        // The body as it was sent, out of the store rather than out of the
        // record: a truncated or skipped body cannot be replayed faithfully, and
        // saying so is better than sending a request that is quietly not the one
        // being replayed.
        let body = match &stored.body {
            crate::capture::Body::Empty => Vec::new(),
            crate::capture::Body::Stored {
                sha256, truncated, ..
            } => {
                if *truncated {
                    return Err(SendError::new(
                        "unreplayable-body",
                        format!(
                            "stored request {from} was too large to keep whole, so replaying \
                             it would send a request that is not the one recorded"
                        ),
                    ));
                }
                store.read_body(sha256).map_err(|e| {
                    SendError::new(
                        "unreplayable-body",
                        format!("stored request {from}'s body is not readable: {e}"),
                    )
                })?
            }
            crate::capture::Body::Skipped { reason, .. } => {
                return Err(SendError::new(
                    "unreplayable-body",
                    format!(
                        "stored request {from}'s body was not kept ({reason:?}), so there is \
                         nothing to replay"
                    ),
                ));
            }
        };

        self.edit_then_send(
            crate::edits::Editable {
                method: stored.method.clone(),
                url,
                headers: stored.headers.clone(),
                body,
            },
            edits,
            create,
            plan,
        )
    }

    fn send_given(
        &self,
        request: crate::broker::Given,
        edits: &[crate::edits::Edit],
        create: bool,
        plan: crate::broker::Sends,
    ) -> Result<crate::broker::Edited, crate::broker::SendError> {
        self.edit_then_send(
            crate::edits::Editable {
                method: request.method,
                url: request.url,
                headers: request.headers,
                body: request.body,
            },
            edits,
            create,
            plan,
        )
    }

    fn send_raw(&self, req: &crate::broker::RawRequest) -> FetchOutcome {
        let url = &req.url;
        // Show the authority and the target that was actually sent.
        let display = format!("{}{}", scheme_authority(url), req.target);

        let denied = |broker: &Self, seq: u64, reason: &str| -> FetchOutcome {
            let record =
                RequestRecord::request(seq, Initiator::Replay, &req.method, &display)
                    .denied(reason);
            if let Err(e) = broker.record_pair(&record) {
                return FetchOutcome::failed(url.clone(), format!("receipt sink refused: {e}"));
            }
            FetchOutcome::failed_at(url.clone(), reason.to_string(), Some(seq))
        };

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        // 1. Check policy and record any refusal.
        if let Some(reason) = self.policy.check_from(url, None).reason() {
            return denied(self, seq, reason);
        }

        // 2. Raw sockets cannot bypass the proxy except for loopback.
        if self.proxied && !crate::rawsock::is_loopback(url) {
            return denied(
                self,
                seq,
                "a raw request is a socket this session's egress proxy does not carry, so it is \
                 refused off loopback rather than sent around the allowlist the proxy enforces",
            );
        }

        // 3. Pin and check addresses before dialing.
        if let Err(reason) = self.pin_addresses(url) {
            return denied(self, seq, &reason);
        }

        // 4. Claim the request budget.
        if let Err(over) = self.budget.claim_request() {
            return denied(self, seq, &over.0);
        }

        // 5. Record the decision before sending. Fail closed if this fails.
        let mut record =
            RequestRecord::request(seq, Initiator::Replay, &req.method, &display);
        record.headers_overridden = req.broke.clone();
        if let Err(e) = self.append(&record) {
            return FetchOutcome::failed(
                url.clone(),
                format!("refusing to fetch: the receipt could not be written: {e}"),
            );
        }

        // Capture the outgoing request when a store is enabled.
        if let Some(capture) = &self.capture {
            let body = req.wire.get(req.body_at..).unwrap_or(&[]);
            let content_type = req
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.as_str());
            capture.request(seq, &req.method, &display, req.headers.clone(), body, content_type);
        }

        // 6. Send the request.
        let started = Instant::now();
        let approved = url.host_str().and_then(|host| self.approved_addresses(host));
        let mut wire = match crate::rawsock::dial(url, approved) {
            Ok(wire) => wire,
            Err(e) => {
                let mut outcome = record.response();
                outcome.duration_ms = Some(started.elapsed().as_millis() as u64);
                outcome.error = Some(e.clone());
                let _ = self.append(&outcome);
                return FetchOutcome::failed_at(url.clone(), e, Some(seq));
            }
        };
        // Do not wait forever for a silent server.
        wire.set_read_timeout(Some(Duration::from_secs(30)));

        if let Err(e) = wire.write_all(&req.wire) {
            wire.shutdown();
            let mut outcome = record.response();
            outcome.duration_ms = Some(started.elapsed().as_millis() as u64);
            outcome.error = Some(format!("writing the request failed: {e}"));
            let _ = self.append(&outcome);
            return FetchOutcome::failed_at(
                url.clone(),
                format!("writing the request failed: {e}"),
                Some(seq),
            );
        }

        let cap = self.policy.max_response_bytes() as usize;
        let response = crate::rawsock::read_http_response(&mut wire, cap);
        let ttfb = started.elapsed().as_millis() as u64;
        // Only the raw path asks this, and only the raw path has a reason to:
        // an ordinary fetch is one request and one response, while a request
        // written byte for byte may have been two requests as far as the server
        // was concerned. What comes back after the first response is the
        // smuggled request's answer, and dropping it would leave the caller
        // holding a successful desync with no way to see that it worked.
        let followed = match &response {
            Ok(resp) => {
                crate::rawsock::read_whatever_follows(&mut wire, resp.leftover.clone(), cap)
            }
            Err(_) => Vec::new(),
        };
        wire.shutdown();

        match response {
            Err(e) => {
                let mut outcome = record.response();
                outcome.duration_ms = Some(ttfb);
                outcome.ttfb_ms = Some(ttfb);
                outcome.error = Some(e.clone());
                let _ = self.append(&outcome);
                FetchOutcome::failed_at(url.clone(), e, Some(seq))
            }
            Ok(resp) => {
                // Store cookies from raw responses in the session jar.
                let cookies_stored = self.jar.store(
                    url,
                    resp.headers
                        .iter()
                        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                        .map(|(_, value)| value.as_str()),
                );

                let mut outcome = record.response();
                outcome.status = resp.status;
                outcome.duration_ms = Some(ttfb);
                outcome.ttfb_ms = Some(ttfb);
                outcome.bytes = Some(resp.body.len() as u64);
                outcome.cookies_stored = Some(cookies_stored);
                if !followed.is_empty() {
                    outcome.trailing_bytes = Some(followed.len() as u64);
                }
                let _ = self.append(&outcome);
                self.budget.record(
                    resp.body.len() as u64,
                    resp.body.len() as u64,
                    Duration::from_millis(ttfb),
                );

                if let Some(capture) = &self.capture {
                    capture.response(crate::capture::Response {
                        seq,
                        url: &display,
                        status: resp.status,
                        headers: resp.headers.clone(),
                        content_encoding: None,
                        wire_bytes: None,
                        body: crate::capture::Received::Bytes(&resp.body),
                        trailing: &followed,
                    });
                }

                FetchOutcome {
                    seq: Some(seq),
                    headers: resp.headers,
                    final_url: url.clone(),
                    body: resp.body,
                    status: resp.status,
                    error: None,
                    opaque: false,
                }
            }
        }
    }


    fn records(&self) -> Vec<RequestRecord> {
        self.log.records()
    }

    fn budget(&self) -> crate::broker::Allowance {
        crate::broker::Allowance {
            spent: self.budget.spent(),
            limits: self.budget.limits().clone(),
        }
    }

    fn reset_budget(&self) {
        self.budget.reset();
    }

    fn cookie_count(&self) -> usize {
        self.jar.len()
    }

    fn document_cookie(&self, url: &Url) -> String {
        self.jar.document_cookie(url)
    }

    fn store_cookie(&self, url: &Url, header: &str) -> usize {
        self.jar.store_from_script(url, header)
    }

    fn keep_only_origin(&self, origin: &Url) -> bool {
        self.jar.retain_origin(origin)
    }

    fn open_socket(
        &self,
        url: &Url,
        document: Option<&Url>,
    ) -> Result<Arc<dyn crate::broker::Channel>, String> {
        let me = self.me.upgrade().ok_or("the broker is no longer running")?;
        Ok(Arc::new(crate::wsclient::Socket::open(me, url, document)?))
    }

    fn open_event_stream(
        &self,
        url: &Url,
        document: Option<&Url>,
    ) -> Result<Arc<dyn crate::broker::Channel>, String> {
        let me = self.me.upgrade().ok_or("the broker is no longer running")?;
        Ok(Arc::new(crate::sse::EventStream::open(me, url, document)?))
    }

    fn secret_names(&self) -> Vec<String> {
        self.secrets.names().into_iter().map(str::to_string).collect()
    }

    fn substitute(&self, text: &str) -> crate::secrets::Resolved {
        self.secrets.substitute(text)
    }

    fn redact(&self, text: &str) -> String {
        self.secrets.redact(text)
    }

    fn redact_all(&self, texts: &[String]) -> Vec<String> {
        self.secrets.redact_all(texts)
    }

    fn has_redactions(&self) -> bool {
        self.secrets.has_redactable()
    }
}


/// The longest a rate may hold one send back: an hour.
const SLOWEST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// The slowest rate that means anything, in sends per second.
const SLOWEST_RATE: f64 = 1.0 / 3600.0;

/// Holds a caller to a number of sends per second.
///
/// Measured from the last send rather than by sleeping a fixed amount between
/// them, so a slow response spends the interval it was going to wait: a rate is
/// a ceiling on what the target sees, not an extra delay on top of it.
struct Pace {
    interval: Option<std::time::Duration>,
    last: Option<std::time::Instant>,
}

impl Pace {
    fn new(rate: Option<f64>) -> Self {
        Self {
            interval: rate
                .filter(|rate| *rate > 0.0 && rate.is_finite())
                // `from_secs_f64` panics on an interval it cannot hold, and a
                // panic here is a request that never answers rather than a
                // refusal. The caller-facing check above rejects these; this
                // one is here because the control channel is JSON and a
                // sender's arithmetic must not depend on it.
                .map(|rate| {
                    std::time::Duration::try_from_secs_f64(1.0 / rate)
                        .unwrap_or(SLOWEST_INTERVAL)
                        .min(SLOWEST_INTERVAL)
                }),
            last: None,
        }
    }

    /// Wait, if the previous send was more recent than the interval allows.
    fn hold(&mut self) {
        if let Some(interval) = self.interval {
            if let Some(last) = self.last
                && let Some(remaining) = interval.checked_sub(last.elapsed())
            {
                std::thread::sleep(remaining);
            }
            self.last = Some(std::time::Instant::now());
        }
    }
}

impl LocalBroker {
    /// One send, and what it cost.
    fn send_once(
        &self,
        fetch: &crate::broker::Fetch,
    ) -> (crate::broker::Timing, FetchOutcome) {
        let started = std::time::Instant::now();
        let outcome = crate::broker::Broker::send(self, fetch);
        let total_ms = started.elapsed().as_millis() as u64;
        // The engine's own reading of the hop, which knows where the headers
        // stopped and the body began. Falls back to the wall clock around the
        // call for a fetch that never got that far.
        let ttfb_ms = outcome
            .seq
            .and_then(|seq| {
                self.log
                    .records()
                    .into_iter()
                    .find(|r| r.seq == seq && r.phase == crate::receipt::Phase::Response)
                    .and_then(|r| r.ttfb_ms)
            })
            .unwrap_or(total_ms);
        (
            crate::broker::Timing {
                seq: outcome.seq,
                status: outcome.status,
                value: None,
                ttfb_ms,
                total_ms,
                bytes: outcome.body.len() as u64,
                body_preview: crate::broker::body_preview(&outcome.body),
                body_truncated: outcome.body.len() > crate::broker::BODY_PREVIEW_BYTES,
            },
            outcome,
        )
    }

    /// The same request from several threads, released together.
    ///
    /// The barrier is the whole point: without it the first thread is already
    /// waiting on the network before the last one has been spawned, and a
    /// check-then-act window measured in milliseconds closes in between. With
    /// it, every thread has its request built and is blocked on the same
    /// rendezvous, so they leave within the cost of waking a thread.
    ///
    /// Every send is a receipt and a stored message like any other. A race is
    /// not a special mode; it is `count` ordinary requests that happen to
    /// overlap, and the audit should read that way afterwards.
    fn send_together(
        &self,
        fetch: &crate::broker::Fetch,
        count: u32,
    ) -> Result<(Vec<crate::broker::Timing>, FetchOutcome), crate::broker::SendError> {
        let me = self.me.upgrade().ok_or_else(|| {
            crate::broker::SendError::new("no-broker", "the broker is no longer running")
        })?;
        let barrier = Arc::new(std::sync::Barrier::new(count as usize));
        let mut threads = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let broker = me.clone();
            let barrier = barrier.clone();
            let fetch = fetch.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                broker.send_once(&fetch)
            }));
        }
        let mut samples = Vec::with_capacity(count as usize);
        let mut last = None;
        for thread in threads {
            match thread.join() {
                Ok((sample, outcome)) => {
                    samples.push(sample);
                    last = Some(outcome);
                }
                // A panicked sender is a gap in the burst, and a burst that
                // silently sent fewer requests than asked for would make a race
                // that did not reproduce look like a race that does not exist.
                Err(_) => {
                    return Err(crate::broker::SendError::new(
                        "send-failed",
                        "one of the parallel sends panicked, so the burst was not the size \
                         it was asked for",
                    ));
                }
            }
        }
        Ok((samples, last.expect("at least one send")))
    }

    /// Apply the edits and put it on the wire.
    ///
    /// The one path both replay verbs end at, so a stored request and a
    /// composed one cannot be sent under two slightly different sets of rules.
    fn edit_then_send(
        &self,
        mut editable: crate::edits::Editable,
        edits: &[crate::edits::Edit],
        create: bool,
        plan: crate::broker::Sends,
    ) -> Result<crate::broker::Edited, crate::broker::SendError> {
        use crate::broker::SendError;
        let applied = crate::edits::apply(&mut editable, edits, create)
            .map_err(|e| SendError::new("bad-edit", e.to_string()))?;

        // Raw requests cannot vary per send.
        if plan.each.is_some()
            && (plan.raw_request.is_some() || plan.raw_target.is_some() || plan.raw_headers)
        {
            return Err(SendError::new(
                "bad-each",
                "a walk edits the request once per value, and a raw send writes bytes that were \
                 already decided. Walk with `--set` alone, or write the values into a sequence \
                 file"
                    .to_string(),
            ));
        }
        // `--repeat` sends one request N times; a walk sends N different ones.
        // Silently taking the walk was a caller asking for 5 passes over 100
        // values and getting one.
        if plan.count > 1 && plan.each.is_some() {
            return Err(SendError::new(
                "bad-each",
                "`--repeat` sends one request again and a walk sends a different request each \
                 time. Pick one"
                    .to_string(),
            ));
        }
        if plan.together && plan.each.is_some() {
            return Err(SendError::new(
                "bad-each",
                "`--race` releases the same request together; a walk sends a different request \
                 each time. Pick one"
                    .to_string(),
            ));
        }
        // A burst meets at a barrier and then sends; a rate holds each send
        // back. Together they are neither, and the race would not reproduce.
        if plan.together && plan.rate.is_some() {
            return Err(SendError::new(
                "bad-rate",
                "`--race` releases every request at one moment and `--rate` holds them apart. \
                 Pick one"
                    .to_string(),
            ));
        }
        if plan.rate.is_some_and(|rate| rate <= 0.0 || !rate.is_finite()) {
            return Err(SendError::new(
                "bad-rate",
                "`--rate` is sends per second and has to be a positive number".to_string(),
            ));
        }
        // A rate this small is a typo, not a plan: one send an hour is already
        // slower than any engagement asks for, and the interval it implies is
        // what `Duration` cannot hold.
        if plan.rate.is_some_and(|rate| rate < SLOWEST_RATE) {
            return Err(SendError::new(
                "bad-rate",
                format!(
                    "`--rate {}` is slower than one send an hour, which is not a rate anybody \
                     meant to ask for",
                    plan.rate.unwrap_or_default()
                ),
            ));
        }

        // Build raw requests after edits so `--set` still applies.
        if plan.raw_request.is_some() || plan.raw_target.is_some() || plan.raw_headers {
            let raw = build_raw_request(&editable, &plan)
                .map_err(|e| SendError::new("bad-raw", e))?;
            let sent = crate::broker::Sent {
                method: raw.method.clone(),
                url: format!("{}{}", scheme_authority(&raw.url), raw.target),
                header_names: raw.headers.iter().map(|(name, _)| name.clone()).collect(),
                body_bytes: (raw.wire.len().saturating_sub(raw.body_at)) as u64,
            };
            let outcome = crate::broker::Broker::send_raw(self, &raw);
            return Ok(crate::broker::Edited {
                seq: outcome.seq,
                applied,
                sent,
                samples: Vec::new(),
                outcome,
            });
        }

        let follow = plan.no_follow.then_some(0);
        // Replays are agent requests, not document subresources.
        let to_fetch = |one: &crate::edits::Editable| crate::broker::Fetch {
            url: one.url.clone(),
            initiator: Initiator::Replay,
            method: one.method.clone(),
            body: one.body.clone(),
            content_type: one.content_type().map(str::to_string),
            headers: one.headers.clone(),
            max_redirects: follow,
            document: None,
            cors: None,
        };

        // Apply each step's edits after the shared ones.
        if let Some(each) = &plan.each {
            let mut samples = Vec::with_capacity(each.steps.len());
            let mut last: Option<(FetchOutcome, crate::broker::Sent)> = None;
            let mut pace = Pace::new(plan.rate);
            for step in &each.steps {
                let mut one = editable.clone();
                let mut walked = Vec::with_capacity(step.set.len());
                for spec in &step.set {
                    walked.push(
                        crate::edits::parse_set(spec)
                            .map_err(|e| SendError::new("bad-edit", e.to_string()))?,
                    );
                }
                crate::edits::apply(&mut one, &walked, create)
                    .map_err(|e| SendError::new("bad-edit", e.to_string()))?;
                let fetch = to_fetch(&one);
                let sent = crate::broker::Sent {
                    method: fetch.method.clone(),
                    url: fetch.url.to_string(),
                    header_names: fetch.headers.iter().map(|(name, _)| name.clone()).collect(),
                    body_bytes: fetch.body.len() as u64,
                };
                pace.hold();
                let (mut sample, outcome) = self.send_once(&fetch);
                sample.value = Some(step.label.clone());
                samples.push(sample);
                last = Some((outcome, sent));
            }
            let (outcome, sent) = last.ok_or_else(|| {
                SendError::new("bad-each", "there were no values to walk".to_string())
            })?;
            return Ok(crate::broker::Edited {
                seq: outcome.seq,
                applied,
                sent,
                samples,
                outcome,
            });
        }

        let fetch = to_fetch(&editable);
        let sent = crate::broker::Sent {
            method: fetch.method.clone(),
            url: fetch.url.to_string(),
            header_names: fetch.headers.iter().map(|(name, _)| name.clone()).collect(),
            body_bytes: fetch.body.len() as u64,
        };
        // At least once. The count is sends, not extra ones, so a plan of one
        // and no plan at all are the same request and the same receipts.
        let sends = plan.count.max(1);
        let (mut samples, outcome) = if plan.together && sends > 1 {
            self.send_together(&fetch, sends)?
        } else {
            let mut samples = Vec::with_capacity(sends as usize);
            let mut last = None;
            let mut pace = Pace::new(plan.rate);
            for _ in 0..sends {
                pace.hold();
                let (sample, outcome) = self.send_once(&fetch);
                samples.push(sample);
                last = Some(outcome);
            }
            (samples, last.expect("at least one send"))
        };
        // Oldest first, so a reader of the samples reads them in the order they
        // were sent. Threads finish out of order, and a race's whole story is
        // which one landed first.
        samples.sort_by_key(|sample| sample.seq);
        Ok(crate::broker::Edited {
            seq: outcome.seq,
            applied,
            sent,
            samples,
            outcome,
        })
    }
}

/// Return a URL's origin-form request target.
fn request_target(url: &Url) -> String {
    let path = url.path();
    let path = if path.is_empty() { "/" } else { path };
    match url.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    }
}

/// Return `scheme://host[:port]` for display and `Host` construction.
fn scheme_authority(url: &Url) -> String {
    format!("{}://{}", url.scheme(), authority_host(url))
}

/// Return `host[:port]`, omitting the default port.
fn authority_host(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => String::new(),
    }
}

/// Build either a complete raw request or a managed request with a raw target.
fn build_raw_request(
    editable: &crate::edits::Editable,
    plan: &crate::broker::Sends,
) -> Result<crate::broker::RawRequest, String> {
    if let Some(bytes) = &plan.raw_request {
        return parse_raw_request(editable.url.clone(), bytes.clone());
    }
    // `--raw-headers` preserves the existing target.
    let own;
    let target = match plan.raw_target.as_deref() {
        Some(target) => target,
        None if plan.raw_headers => {
            own = request_target(&editable.url);
            &own
        }
        None => return Err("a raw send needs a request-target or a whole request".into()),
    };
    if target.is_empty() || target.as_bytes()[0] != b'/' && !target.contains("://") {
        return Err(format!(
            "{target:?} is not a request-target: it should begin with `/` (an origin-form target \
             like `/cgi-bin/.%2e/etc/passwd`) or be an absolute URL"
        ));
    }
    Ok(build_managed_raw(editable, target))
}

/// Build a request with a verbatim target and computed framing headers.
fn build_managed_raw(
    editable: &crate::edits::Editable,
    target: &str,
) -> crate::broker::RawRequest {
    let method = editable.method.trim().to_ascii_uppercase();
    let mut headers: Vec<(String, String)> = Vec::new();

    // Use the authority unless the caller supplied `Host`.
    let host = editable
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| authority_host(&editable.url));
    headers.push(("Host".to_string(), host));

    // Keep caller headers, but recompute framing headers below.
    for (name, value) in &editable.headers {
        let lower = name.to_ascii_lowercase();
        if matches!(lower.as_str(), "host" | "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        headers.push((name.clone(), value.clone()));
    }
    if !editable.body.is_empty() {
        headers.push(("Content-Length".to_string(), editable.body.len().to_string()));
    }
    // Closing the connection also delimits responses without a length.
    headers.push(("Connection".to_string(), "close".to_string()));

    let mut head = format!("{method} {target} HTTP/1.1\r\n");
    for (name, value) in &headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    let mut wire = head.into_bytes();
    let body_at = wire.len();
    wire.extend_from_slice(&editable.body);

    crate::broker::RawRequest {
        url: editable.url.clone(),
        method,
        target: target.to_string(),
        wire,
        body_at,
        headers,
        broke: Vec::new(),
    }
}

/// Parse audit fields from a complete request without changing its bytes.
fn parse_raw_request(url: Url, bytes: Vec<u8>) -> Result<crate::broker::RawRequest, String> {
    let head_end = find_crlf_crlf(&bytes).map(|at| at + 4).unwrap_or(bytes.len());
    let head = &bytes[..head_end];
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() || target.is_empty() {
        return Err(format!(
            "the raw request's first line is {first:?}, not a `METHOD target HTTP/1.1` line"
        ));
    }
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    let broke: Vec<String> = headers
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "content-length" | "transfer-encoding" | "connection"
            )
        })
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    Ok(crate::broker::RawRequest {
        url,
        method,
        target,
        wire: bytes,
        body_at: head_end,
        headers,
        broke,
    })
}

fn find_crlf_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// How every subresource turned out, for the page's `load` and `error` events.
///
/// Blitz decides each outcome and throws it away, so nothing in the page could
/// see the 404 sitting in the request log. Keyed by URL because that is all the
/// two sides share: Blitz's `Request` does not say which element asked.
pub type ResourceLog = Arc<std::sync::Mutex<ResourceOutcomes>>;

/// The table behind a [`ResourceLog`].
#[derive(Debug, Default)]
pub struct ResourceOutcomes {
    /// `0` for a request that got no answer: refused, or a failed connection.
    /// Both are `error` to a page; which it was is in the receipt.
    by_url: std::collections::HashMap<String, u16>,
}

impl ResourceOutcomes {
    /// Under both URLs: a redirected image is `src` in the markup and the final
    /// URL in the outcome, and the page asks about the first.
    pub fn record(&mut self, asked: &Url, outcome: &FetchOutcome) {
        let status = match (outcome.error.as_ref(), outcome.status) {
            (Some(_), _) | (None, None) => 0,
            (None, Some(status)) => status,
        };
        self.by_url.insert(asked.to_string(), status);
        self.by_url.insert(outcome.final_url.to_string(), status);
    }

    /// The status this URL came back with, or `None` if it was never asked for.
    pub fn status(&self, url: &str) -> Option<u16> {
        self.by_url.get(url).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.by_url.is_empty()
    }
}

/// Adapts the broker to Blitz's [`NetProvider`].
pub struct BrokerNet {
    broker: Arc<dyn crate::broker::Broker>,
    /// The document whose subresources these are.
    ///
    /// Load-bearing, not bookkeeping. Every image, stylesheet and font on the
    /// page arrives through here, and without an origin to attribute them to
    /// the policy read each one as the agent naming a URL, so
    /// `<img src="http://127.0.0.1:3000/…">` on a page from the open web reached
    /// the box's dev server, which is precisely what [`Policy::check_from`]
    /// exists to refuse. `None` only for a document with no origin of its own.
    document: Option<Url>,
    /// Where each subresource's fate is written down for the page to hear.
    resources: ResourceLog,
}

impl BrokerNet {
    pub fn new(broker: Arc<dyn crate::broker::Broker>, document: Option<Url>) -> Self {
        Self::with_log(broker, document, ResourceLog::default())
    }

    pub fn with_log(
        broker: Arc<dyn crate::broker::Broker>,
        document: Option<Url>,
        resources: ResourceLog,
    ) -> Self {
        Self {
            broker,
            document,
            resources,
        }
    }
}

impl NetProvider for BrokerNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let outcome =
            self.broker
                .fetch_from(&request.url, Initiator::Subresource, self.document.as_ref());

        if let Ok(mut log) = self.resources.lock() {
            log.record(&request.url, &outcome);
        }

        // The single exit. A denied or failed request completes with an empty
        // body: Blitz counts the resource as resolved and paints, having
        // loaded nothing. Returning early here, however tempting for a
        // request we refused, leaves the document pending forever and blank.
        handler.bytes(outcome.final_url.to_string(), Bytes::from(outcome.body));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::Broker;
    use crate::receipt::{MemorySink, Phase};
    use std::sync::atomic::AtomicBool;

    fn broker_with(policy: Policy, sink: Arc<dyn Sink>) -> Arc<LocalBroker> {
        LocalBroker::new(policy, sink, None).expect("broker builds")
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url")
    }

    /// A sink that refuses everything, to prove the fail-closed path.
    struct RefusingSink;
    impl Sink for RefusingSink {
        fn append(&self, _record: &RequestRecord) -> Result<(), H5iError> {
            Err(H5iError::Internal("disk is on fire".to_string()))
        }
    }

    /// Records whether Blitz's handler was completed.
    struct SpyHandler {
        called: Arc<AtomicBool>,
        body_len: Arc<AtomicU64>,
    }
    impl NetHandler for SpyHandler {
        fn bytes(self: Box<Self>, _resolved_url: String, bytes: Bytes) {
            self.body_len.store(bytes.len() as u64, Ordering::SeqCst);
            self.called.store(true, Ordering::SeqCst);
        }
    }

    /// The table the page's `load` and `error` events are read from (#610).
    #[test]
    fn a_subresource_outcome_is_recorded_under_both_of_its_urls() {
        let mut outcomes = ResourceOutcomes::default();
        assert!(outcomes.is_empty());

        let asked = url("https://cdn.test/a.png");
        let mut ok = FetchOutcome::failed(url("https://cdn.test/b.png"), String::new());
        ok.error = None;
        ok.status = Some(200);
        outcomes.record(&asked, &ok);
        assert_eq!(outcomes.status(asked.as_str()), Some(200));
        assert_eq!(outcomes.status("https://cdn.test/b.png"), Some(200));

        // `None`, not `0`: only "fetched and got nothing" is an `error`.
        assert_eq!(outcomes.status("https://cdn.test/never.png"), None);

        // A refusal and a failed connection are both `0`.
        let refused = url("https://blocked.test/x.png");
        outcomes.record(
            &refused,
            &FetchOutcome::refused(refused.clone(), "denied by policy".to_string()),
        );
        assert_eq!(outcomes.status(refused.as_str()), Some(0));
    }

    /// Recorded whether it arrived or not, so a denial is an `error` the page
    /// hears rather than a silence it cannot tell from success.
    #[test]
    fn the_blitz_adapter_records_what_each_subresource_came_back_as() {
        let sink = Arc::new(MemorySink::new());
        let broker = broker_with(Policy::new(), sink);
        let log = ResourceLog::default();
        let net = BrokerNet::with_log(broker, Some(url("https://denied.test/")), log.clone());

        net.fetch(
            0,
            Request::get(url("https://tracker.test/pixel.gif")),
            Box::new(SpyHandler {
                called: Arc::new(AtomicBool::new(false)),
                body_len: Arc::new(AtomicU64::new(0)),
            }),
        );

        assert_eq!(
            log.lock().expect("log").status("https://tracker.test/pixel.gif"),
            Some(0),
            "a refused subresource is one the page must be able to hear about"
        );
    }

    #[test]
    fn a_denied_request_never_reaches_the_wire_and_is_recorded_as_a_pair() {
        let sink = Arc::new(MemorySink::new());
        // Empty policy: nothing remote is allowed, so this cannot escape even
        // if the test host has a network.
        let broker = broker_with(Policy::new(), sink.clone());

        let outcome = broker.fetch(&url("https://tracker.test/pixel.gif"), Initiator::Subresource);

        assert!(!outcome.is_ok());
        assert!(outcome.body.is_empty());
        assert!(outcome.error.unwrap().contains("denied by policy"));

        let records = sink.records();
        assert_eq!(records.len(), 2, "a denial is still a request/response pair");
        assert!(records.iter().all(|r| !r.allowed));
        assert_eq!(records[0].phase, Phase::Request);
        assert_eq!(records[1].phase, Phase::Response);
        assert!(sink.fetched_urls().is_empty());
        assert_eq!(sink.denied_urls(), vec!["https://tracker.test/pixel.gif"]);
    }

    #[test]
    fn no_receipt_means_no_fetch() {
        // The fail-closed claim, stated as a test: a sink that cannot record
        // the decision must stop the request, not be ignored.
        let broker = broker_with(Policy::new().allow("example.com"), Arc::new(RefusingSink));

        let outcome = broker.fetch(&url("https://example.com/"), Initiator::Navigation);

        assert!(!outcome.is_ok());
        let error = outcome.error.unwrap();
        assert!(
            error.contains("receipt could not be written"),
            "the refusal must name its cause, got: {error}"
        );
    }

    #[test]
    fn blitz_handler_is_always_completed_even_when_the_request_is_denied() {
        // If this regresses, `paint_scene` silently stops painting: the
        // document keeps a pending critical resource forever and every
        // screenshot comes back blank. See the module docs.
        let sink = Arc::new(MemorySink::new());
        let broker = broker_with(Policy::new(), sink);
        let net = BrokerNet::new(broker, Some(url("https://denied.test/")));

        let called = Arc::new(AtomicBool::new(false));
        let body_len = Arc::new(AtomicU64::new(u64::MAX));
        let handler = Box::new(SpyHandler {
            called: called.clone(),
            body_len: body_len.clone(),
        });

        net.fetch(
            0,
            Request::get(url("https://denied.test/style.css")),
            handler,
        );

        assert!(
            called.load(Ordering::SeqCst),
            "a denied request must still complete its handler"
        );
        assert_eq!(
            body_len.load(Ordering::SeqCst),
            0,
            "completing a denial must hand back no bytes"
        );
    }

    #[test]
    fn loopback_is_reachable_without_an_allowlist_entry() {
        // Not a network test: it asserts the policy decision the broker makes
        // before dialling, using a port nothing is listening on.
        let sink = Arc::new(MemorySink::new());
        let broker = broker_with(Policy::new(), sink.clone());

        let outcome = broker.fetch(&url("http://127.0.0.1:9/"), Initiator::Navigation);

        // The connection fails (nothing is listening), but it was *attempted*,
        // which is the difference from a policy denial.
        assert!(!outcome.is_ok());
        assert_eq!(sink.denied_urls().len(), 0);
        assert_eq!(sink.fetched_urls(), vec!["http://127.0.0.1:9/"]);
    }

    /// A subresource is the *page* reaching for a URL, and it was being policed as
    /// though the agent had named one.
    ///
    /// `check_from` refuses a loopback request from a document that is not itself
    /// local, the rule that stops a page on the open web reading the box's dev
    /// server. Every non-script path into the broker passed `document: None`, which
    /// the same function documents as trusted, so `<img src="http://127.0.0.1:3000/…">`
    /// on a page from the web went straight through the guard.
    #[test]
    fn a_page_from_the_web_cannot_reach_loopback_through_a_subresource() {
        let sink = Arc::new(MemorySink::new());
        let broker = broker_with(Policy::new().allow("docs.test"), sink.clone());
        let dev_server = url("http://127.0.0.1:9/src/main.rs");

        let outcome = broker.fetch_from(
            &dev_server,
            Initiator::Subresource,
            Some(&url("https://docs.test/page")),
        );
        assert!(!outcome.is_ok());
        assert_eq!(sink.denied_urls(), vec![dev_server.as_str()]);
        assert!(sink.fetched_urls().is_empty(), "nothing may reach the wire");

        // ...and the dev server's own page still talks to itself, which is the
        // whole reason loopback is reachable at all.
        let sink = Arc::new(MemorySink::new());
        let broker = broker_with(Policy::new(), sink.clone());
        let _ = broker.fetch_from(
            &dev_server,
            Initiator::Subresource,
            Some(&url("http://127.0.0.1:9/")),
        );
        assert_eq!(sink.denied_urls().len(), 0);
        assert_eq!(sink.fetched_urls(), vec![dev_server.as_str()]);
    }

    /// The Blitz adapter is where every image, stylesheet and font arrives, so
    /// it is the widest of those paths and carries the document explicitly.
    #[test]
    fn the_blitz_adapter_attributes_subresources_to_their_document() {
        let sink = Arc::new(MemorySink::new());
        let broker = broker_with(Policy::new().allow("docs.test"), sink.clone());
        let net = BrokerNet::new(broker, Some(url("https://docs.test/page")));

        let handler = Box::new(SpyHandler {
            called: Arc::new(AtomicBool::new(false)),
            body_len: Arc::new(AtomicU64::new(u64::MAX)),
        });
        net.fetch(0, Request::get(url("http://127.0.0.1:9/secret")), handler);

        assert_eq!(sink.denied_urls(), vec!["http://127.0.0.1:9/secret"]);
        assert!(sink.fetched_urls().is_empty());
    }

    #[test]
    fn a_bad_proxy_url_is_refused_at_construction_not_at_first_fetch() {
        let sink = Arc::new(MemorySink::new());
        let result = LocalBroker::new(Policy::new(), sink, Some("not a url"));
        assert!(result.is_err(), "a malformed proxy must fail loudly, early");
    }
}

#[cfg(test)]
mod caller_header_tests {
    use super::*;
    use crate::broker::{Broker, Fetch};
    use crate::receipt::MemorySink;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A server that writes down every request head it is handed.
    ///
    /// The only way to test a header layer honestly: asserting on what the
    /// engine *meant* to send proves nothing about what went out, and this
    /// engine's whole claim is about the difference.
    pub(super) fn head_recorder(
        hops: usize,
        redirect_to: Option<String>,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        std::thread::spawn(move || {
            for hop in 0..hops {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let _ = reader.read_line(&mut head);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                if let Ok(mut seen) = recorded.lock() {
                    seen.push(head.to_ascii_lowercase());
                }
                let mut stream = stream;
                let _ = match (hop, &redirect_to) {
                    (0, Some(target)) => write!(
                        stream,
                        "HTTP/1.1 302 Found\r\nLocation: {target}\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    _ => write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                    ),
                };
                let _ = stream.flush();
            }
        });
        (port, seen)
    }

    fn broker() -> (Arc<MemorySink>, Arc<LocalBroker>) {
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(Policy::new(), sink.clone(), None).expect("broker");
        (sink, broker)
    }

    /// How many times a header name appears in a recorded head.
    pub(super) fn count(head: &str, name: &str) -> usize {
        head.lines()
            .filter(|line| line.starts_with(&format!("{name}:")))
            .count()
    }

    #[test]
    fn a_caller_header_goes_out_and_the_engines_default_stands_aside() {
        let (port, seen) = head_recorder(1, None);
        let (_sink, broker) = broker();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let outcome = broker.send(&Fetch::get(&url, Initiator::Navigation).with_headers(vec![
            ("X-Forwarded-For".to_string(), "127.0.0.1".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ]));
        assert!(outcome.is_ok(), "{:?}", outcome.error);

        let seen = seen.lock().unwrap();
        let head = &seen[0];
        assert!(head.contains("x-forwarded-for: 127.0.0.1"), "{head}");
        // The engine's own `Accept` would otherwise ride along beside it, and a
        // request with two is neither what the caller asked for nor what the
        // engine sends by default.
        assert_eq!(count(head, "accept"), 1, "{head}");
        assert!(head.contains("accept: application/json"), "{head}");
    }

    /// The framing headers are the client's, and saying so is the point.
    #[test]
    fn a_header_the_client_owns_is_refused_and_named_in_the_receipt() {
        let (port, seen) = head_recorder(1, None);
        let (sink, broker) = broker();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let outcome = broker.send(
            &Fetch::get(&url, Initiator::Navigation).with_headers(vec![
                ("Content-Length".to_string(), "999".to_string()),
                ("Connection".to_string(), "upgrade".to_string()),
            ]),
        );
        assert!(outcome.is_ok(), "{:?}", outcome.error);

        let head = seen.lock().unwrap()[0].clone();
        assert!(!head.contains("content-length: 999"), "{head}");
        assert!(!head.contains("connection: upgrade"), "{head}");

        let record = sink
            .records()
            .into_iter()
            .find(|r| r.phase == crate::receipt::Phase::Request && r.allowed)
            .expect("the decision record");
        assert_eq!(
            record.headers_overridden,
            vec!["content-length".to_string(), "connection".to_string()],
            "the refusal is reported, not performed silently"
        );
    }

    /// A caller naming `Cookie` is testing that value, not adding to the jar's.
    #[test]
    fn a_caller_cookie_stands_alone() {
        let (port, seen) = head_recorder(1, None);
        let (sink, broker) = broker();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        broker.jar().store(&url, ["session=from-the-jar; Path=/"]);

        let outcome = broker.send(
            &Fetch::get(&url, Initiator::Navigation)
                .with_headers(vec![("Cookie".to_string(), "session=forged".to_string())]),
        );
        assert!(outcome.is_ok(), "{:?}", outcome.error);

        let head = seen.lock().unwrap()[0].clone();
        assert_eq!(count(&head, "cookie"), 1, "{head}");
        assert!(head.contains("cookie: session=forged"), "{head}");
        assert!(!head.contains("from-the-jar"), "{head}");

        let response = sink
            .records()
            .into_iter()
            .find(|r| r.phase == crate::receipt::Phase::Response)
            .expect("the outcome record");
        assert_eq!(
            response.cookies_sent,
            Some(0),
            "the jar sent nothing, and the count says so rather than counting the caller's"
        );
    }

    /// A chain is one request to the caller, so its headers ride along it.
    #[test]
    fn caller_headers_follow_a_same_origin_redirect() {
        let (port, seen) = head_recorder(2, Some("/next".to_string()));
        let (_sink, broker) = broker();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/start")).unwrap();
        let outcome = broker.send(&Fetch::get(&url, Initiator::Navigation).with_headers(vec![(
            "Authorization".to_string(),
            "Bearer t0ken".to_string(),
        )]));
        assert!(outcome.is_ok(), "{:?}", outcome.error);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "both hops were made");
        assert!(seen[1].contains("authorization: bearer t0ken"), "{}", seen[1]);
    }

    /// ...and stop at the origin boundary, which is where a chain stops being
    /// the request the caller authorised.
    #[test]
    fn a_credential_does_not_follow_a_redirect_to_another_origin() {
        let (elsewhere, over_there) = head_recorder(1, None);
        let (port, seen) = head_recorder(
            1,
            Some(format!("http://127.0.0.1:{elsewhere}/collect")),
        );
        let (_sink, broker) = broker();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/start")).unwrap();
        let outcome = broker.send(&Fetch::get(&url, Initiator::Navigation).with_headers(vec![
            ("Authorization".to_string(), "Bearer t0ken".to_string()),
            ("X-Trace".to_string(), "keep-me".to_string()),
        ]));
        assert!(outcome.is_ok(), "{:?}", outcome.error);

        assert!(
            seen.lock().unwrap()[0].contains("authorization: bearer t0ken"),
            "the origin the caller named still gets it"
        );
        let landed = over_there.lock().unwrap()[0].clone();
        assert!(
            !landed.contains("t0ken"),
            "a redirect must not harvest the credential: {landed}"
        );
        // Only the credential stops. A header that is not one is still the
        // caller's instruction about this chain.
        assert!(landed.contains("x-trace: keep-me"), "{landed}");
    }
}

#[cfg(test)]
mod capture_wire_tests {
    use super::*;
    use crate::broker::Broker;
    use crate::capture::{Body, Capture, Skip};
    use crate::receipt::MemorySink;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A server that redirects to its own second path, setting a cookie on the
    /// hop, which is the shape a login has.
    fn redirect_then_answer() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for hop in 0..2 {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                let _ = if hop == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 302 Found\r\nLocation: /home\r\n\
                         Set-Cookie: session=s3cr3t; Path=/\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    let body = "{\"user\":\"alice\"}";
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.flush();
            }
        });
        port
    }

    /// The whole point of the store, end to end: a redirect chain leaves one
    /// message per hop, the hop's headers are kept even though its body is never
    /// read, and the final body is on disk and readable.
    #[test]
    fn a_redirect_chain_is_stored_hop_by_hop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::with_limits(
            Policy::new(),
            sink.clone(),
            None,
            crate::budget::Limits::default(),
            Some(capture.clone()),
        )
        .expect("broker");

        let port = redirect_then_answer();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        let outcome = broker.fetch(&url, Initiator::Navigation);
        assert!(outcome.is_ok(), "{:?}", outcome.error);

        // Hop 0: the redirect. Its `Location` and `Set-Cookie` are the content
        // of the message, and its body was never read.
        let hop = capture.read_response(0).expect("the hop is stored");
        assert_eq!(hop.status, Some(302));
        assert!(
            hop.headers.iter().any(|(n, v)| n == "set-cookie" && v.contains("s3cr3t")),
            "the store holds the credential the receipt refuses to: {:?}",
            hop.headers
        );
        assert_eq!(
            hop.body,
            Body::Skipped {
                reason: Skip::NotRead,
                bytes: None
            }
        );

        // Hop 1: the answer, with its body on disk.
        let answer = capture.read_response(1).expect("the answer is stored");
        assert_eq!(answer.status, Some(200));
        let Body::Stored { sha256, .. } = &answer.body else {
            panic!("the body was stored: {:?}", answer.body);
        };
        assert_eq!(
            capture.read_body(sha256).expect("the body reads back"),
            b"{\"user\":\"alice\"}"
        );

        // And the second request carried the cookie the first hop set, which is
        // the header set as built rather than as asked for.
        let second = capture.read_request(1).expect("the second request is stored");
        assert_eq!(second.url, format!("http://127.0.0.1:{port}/home"));
        assert!(
            second.headers.iter().any(|(n, v)| n == "cookie" && v.contains("s3cr3t")),
            "{:?}",
            second.headers
        );

        // The receipt is unchanged by any of this: counts, never values.
        let receipted = serde_json::to_string(&sink.records()).expect("records serialise");
        assert!(!receipted.contains("s3cr3t"), "a credential reached the receipt log");
        assert_eq!(capture.errors(), 0);
    }

    /// A store that dropped something has to be able to say so, or an agent
    /// reads the messages that survived and concludes the rest never happened.
    #[test]
    fn a_session_reports_the_health_of_its_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits::default(),
            Some(capture.clone()),
        )
        .expect("broker");

        let port = redirect_then_answer();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());

        let health = broker.capture().expect("a session with a store reports one");
        assert_eq!(health.messages, 4, "two hops, both phases");
        assert_eq!(health.errors, 0);
        assert!(health.bytes > 0);
    }

    /// The workbench loop, end to end: a request the session made, sent again
    /// with one parameter bent, through the same policy and into a new receipt.
    #[test]
    fn a_stored_request_is_sent_again_with_an_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::with_limits(
            Policy::new(),
            sink.clone(),
            None,
            crate::budget::Limits::default(),
            Some(capture.clone()),
        )
        .expect("broker");

        // Two answers: the original, and the replay.
        let (port, seen) = super::caller_header_tests::head_recorder(2, None);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/api/users?user_id=123")).unwrap();
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());

        let edited = crate::broker::Broker::send_edited(
            broker.as_ref(),
            0,
            &[crate::edits::parse_set("query.user_id=456").expect("parses")],
            false,
            crate::broker::Sends::once(),
        )
        .expect("the stored request is there");

        assert_eq!(edited.applied[0].was.as_deref(), Some("123"));
        assert_eq!(edited.sent.url, format!("http://127.0.0.1:{port}/api/users?user_id=456"));
        assert_eq!(edited.outcome.status, Some(200));
        // A replay is a request like any other: its own receipt, its own
        // sequence, and its own stored message.
        assert_eq!(edited.seq, Some(1));
        assert!(capture.read_request(1).is_ok(), "the replay is itself replayable");

        let seen = seen.lock().unwrap();
        assert!(seen[1].starts_with("get /api/users?user_id=456"), "{}", seen[1]);
    }

    /// A body's content type must go out once, whoever named it.
    ///
    /// A replay carries the stored headers *and* tells the broker the content
    /// type, which are two routes to the same header. Sending both produced a
    /// request with `Content-Type` twice; Flask answers 400 to that, and the
    /// receipt looked entirely normal.
    #[test]
    fn a_replayed_body_names_its_content_type_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits::default(),
            Some(capture),
        )
        .expect("broker");

        let (port, seen) = super::caller_header_tests::head_recorder(2, None);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());

        crate::broker::Broker::send_edited(
            broker.as_ref(),
            0,
            &[
                crate::edits::parse_set("method=POST").expect("parses"),
                crate::edits::parse_set("form.username=test").expect("parses"),
            ],
            true,
            crate::broker::Sends::once(),
        )
        .expect("replayed");

        let seen = seen.lock().unwrap();
        assert_eq!(
            super::caller_header_tests::count(&seen[1], "content-type"),
            1,
            "the composed request sent it twice:\n{}",
            seen[1]
        );
    }

    /// A replay hands back the stored header set, which already holds the
    /// engine's own defaults. Every one of them has to stand aside, or the
    /// request that goes out is not the request that was recorded.
    #[test]
    fn a_replay_does_not_duplicate_the_engines_own_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits::default(),
            Some(capture),
        )
        .expect("broker");

        let (port, seen) = super::caller_header_tests::head_recorder(2, None);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/api?id=1")).unwrap();
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());
        crate::broker::Broker::send_edited(
            broker.as_ref(),
            0,
            &[crate::edits::parse_set("query.id=2").expect("parses")],
            false,
            crate::broker::Sends::once(),
        )
        .expect("replayed");

        let seen = seen.lock().unwrap();
        let replay = &seen[1];
        for header in ["accept-encoding", "accept", "accept-language"] {
            assert_eq!(
                super::caller_header_tests::count(replay, header),
                1,
                "`{header}` went out more than once on the replay:\n{replay}"
            );
        }
    }

    /// `reqwest` merges client defaults at *execute* time, so reading the
    /// built request recorded everything but the `User-Agent`. That also broke
    /// the `message --raw` → `--raw-request` round trip, which went out bare.
    #[test]
    fn the_store_records_the_header_set_that_went_on_the_wire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits::default(),
            Some(capture.clone()),
        )
        .expect("broker");

        let (port, seen) = super::caller_header_tests::head_recorder(1, None);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/api")).unwrap();
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());

        let on_the_wire = seen.lock().unwrap()[0].to_ascii_lowercase();
        let stored = capture.read_request(0).expect("a stored request");
        for (name, _) in &stored.headers {
            assert!(
                on_the_wire.contains(&format!("\n{}:", name.to_ascii_lowercase()))
                    || name.eq_ignore_ascii_case("host"),
                "the store holds `{name}`, which never went out:\n{on_the_wire}"
            );
        }
        // And the one that used to be missing is there.
        assert!(
            stored
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("user-agent")),
            "the store must hold the `User-Agent` the wire carried: {:?}",
            stored.headers
        );
        assert!(on_the_wire.contains("user-agent:"), "{on_the_wire}");
    }

    /// An edit that would change nothing is a mistake, and saying so is the
    /// difference between a five-minute test and an hour reading identical
    /// responses.
    #[test]
    fn an_edit_that_names_nothing_is_refused_before_the_wire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::with_limits(
            Policy::new(),
            sink.clone(),
            None,
            crate::budget::Limits::default(),
            Some(capture),
        )
        .expect("broker");

        let (port, _seen) = super::caller_header_tests::head_recorder(1, None);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/api?user_id=1")).unwrap();
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());
        let before = sink.records().len();

        let error = crate::broker::Broker::send_edited(
            broker.as_ref(),
            0,
            &[crate::edits::parse_set("query.userid=2").expect("parses")],
            false,
            crate::broker::Sends::once(),
        )
        .expect_err("refused");
        assert_eq!(error.code, "bad-edit", "the request was there; the edit was not");
        assert!(error.message.contains("user_id"), "{}", error.message);
        assert_eq!(sink.records().len(), before, "nothing reached the wire");
    }

    /// A request another session recorded, sent under this session's identity.
    ///
    /// The point of the verb is what the *receiving* session contributes: its
    /// jar, its policy, its receipts. The composed message brings only the
    /// method, URL, body and the headers the caller chose to carry.
    #[test]
    fn a_composed_request_is_sent_under_this_sessions_own_cookies() {
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(Policy::new(), sink.clone(), None).expect("broker");

        let (port, seen) = super::caller_header_tests::head_recorder(1, None);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/doc?id=1")).unwrap();
        // This session is logged in as somebody.
        broker.jar().store(&url, ["session=this-sessions-own; Path=/"]);

        let edited = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url: url.clone(),
                // What `--as` hands over: no credential of the other session's.
                headers: vec![("X-Trace".to_string(), "carried".to_string())],
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends::once(),
        )
        .expect("sent");
        assert_eq!(edited.outcome.status, Some(200));

        let head = seen.lock().unwrap()[0].clone();
        assert!(head.contains("x-trace: carried"), "{head}");
        assert!(
            head.contains("cookie: session=this-sessions-own"),
            "the receiving session's jar supplies the credential: {head}"
        );
        // And it is in this session's receipts, because this session made it.
        assert!(
            sink.records().iter().any(|r| r.url.contains("/doc?id=1")),
            "a composed request is recorded like any other"
        );
    }

    /// A browser follows a `Location`; a test wants the 302 itself.
    #[test]
    fn a_replay_can_stop_at_the_redirect_and_report_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Arc::new(Capture::open(&dir.path().join("messages")).expect("store"));
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits::default(),
            Some(capture),
        )
        .expect("broker");

        let port = redirect_then_answer();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        // The session itself follows, as a browser does: two hops.
        assert!(broker.fetch(&url, Initiator::Navigation).is_ok());

        let port = redirect_then_answer();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        let stopped = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: true,
                ..Default::default()
            },
        )
        .expect("sent");

        assert_eq!(stopped.outcome.status, Some(302), "the hop itself, not the page");
        assert!(
            stopped.outcome.error.is_none(),
            "stopping where you asked to stop is not a failure: {:?}",
            stopped.outcome.error
        );
        assert!(
            stopped
                .outcome
                .headers
                .iter()
                .any(|(name, value)| name == "location" && value == "/home"),
            "the `Location` is the whole content of a redirect: {:?}",
            stopped.outcome.headers
        );
    }

    /// Echo `count` request lines.
    fn echo_request_lines(count: usize) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..count {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0
                        || header.trim().is_empty()
                    {
                        break;
                    }
                }
                let body = line.trim_end().to_string();
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            }
        });
        port
    }

    /// Echo a complete request head.
    fn echo_whole_request() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{head}",
                head.len()
            );
            let _ = stream.flush();
        });
        port
    }

    /// Start a server that echoes each request line.
    fn echo_request_line() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                    break;
                }
            }
            let body = line.trim_end().to_string();
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        });
        port
    }

    /// A raw target reaches the wire without URL normalization.
    #[test]
    fn a_raw_target_reaches_the_wire_unresolved() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let port = echo_request_line();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let sent = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: false,
                raw_target: Some("/cgi-bin/.%2e/.%2e/etc/passwd".to_string()),
                raw_request: None,
                raw_headers: false,
                each: None,
                rate: None,
            },
        )
        .expect("sent");
        let echoed = String::from_utf8_lossy(&sent.outcome.body);
        assert_eq!(
            echoed, "GET /cgi-bin/.%2e/.%2e/etc/passwd HTTP/1.1",
            "the server saw the verbatim target, not one a parser straightened"
        );
        assert_eq!(sent.outcome.status, Some(200));
    }

    /// `--raw-headers` preserves header-name casing.
    #[test]
    fn raw_headers_writes_the_names_in_the_case_they_were_given() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let port = echo_whole_request();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/orders?page=2")).unwrap();
        let sent = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "POST".to_string(),
                url,
                headers: vec![("X-Odd-CASE".to_string(), "yes".to_string())],
                body: b"a=b".to_vec(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: false,
                raw_target: None,
                raw_request: None,
                raw_headers: true,
                each: None,
                rate: None,
            },
        )
        .expect("sent");
        let echoed = String::from_utf8_lossy(&sent.outcome.body).to_string();
        assert!(
            echoed.contains("POST /orders?page=2 HTTP/1.1"),
            "the request keeps its own target, query included: {echoed}"
        );
        assert!(
            echoed.contains("Content-Length: 3"),
            "the framing header is written in the case a proxy looks for: {echoed}"
        );
        assert!(
            echoed.contains("X-Odd-CASE: yes"),
            "and so is a name the caller chose: {echoed}"
        );
    }

    /// A walk returns one labeled sample per value.
    #[test]
    fn set_each_sends_once_per_value_and_names_them() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let port = echo_request_lines(3);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/thing?id=0")).unwrap();
        let sent = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: false,
                raw_target: None,
                raw_request: None,
                raw_headers: false,
                rate: None,
                // Commas remain part of a value.
                each: Some(crate::broker::Each::over(
                    "query.id",
                    vec!["1".into(), "2".into(), "a,b".into()],
                )),
            },
        )
        .expect("sent");
        let values: Vec<Option<String>> =
            sent.samples.iter().map(|s| s.value.clone()).collect();
        assert_eq!(
            values,
            vec![Some("1".into()), Some("2".into()), Some("a,b".into())],
            "every send is a sample that names its value"
        );
        assert_eq!(sent.samples.len(), 3, "one send per value, and no more");
        for (sample, value) in sent.samples.iter().zip(["id=1", "id=2", "id=a%2Cb"]) {
            assert!(
                sample.body_preview.contains(value),
                "each sample carries the response that its value produced: {sample:?}"
            );
            assert!(!sample.body_truncated);
        }
    }

    /// The general form: one send sets more than one target.
    #[test]
    fn a_walk_step_sets_every_target_it_names_and_is_labeled_as_one() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let port = echo_request_lines(2);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/thing?user=x&role=y")).unwrap();
        let sent = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: false,
                raw_target: None,
                raw_request: None,
                raw_headers: false,
                rate: None,
                each: Some(crate::broker::Each {
                    steps: vec![
                        crate::broker::Step {
                            label: "user=alice role=user".to_string(),
                            set: vec![
                                "query.user=alice".to_string(),
                                "query.role=user".to_string(),
                            ],
                        },
                        crate::broker::Step {
                            label: "user=bob role=admin".to_string(),
                            set: vec![
                                "query.user=bob".to_string(),
                                "query.role=admin".to_string(),
                            ],
                        },
                    ],
                }),
            },
        )
        .expect("sent");
        assert_eq!(sent.samples.len(), 2, "one send per step");
        assert_eq!(
            sent.samples[1].value.as_deref(),
            Some("user=bob role=admin"),
            "the sample is named by the whole combination, not by one value"
        );
        assert!(
            sent.samples[1].body_preview.contains("user=bob")
                && sent.samples[1].body_preview.contains("role=admin"),
            "both of the step's edits rode on the same request: {:?}",
            sent.samples[1]
        );
    }

    /// A burst meets at a barrier; a rate holds each send back.
    #[test]
    fn a_rate_and_a_race_are_refused_together() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let error = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 4,
                together: true,
                no_follow: false,
                raw_target: None,
                raw_request: None,
                raw_headers: false,
                each: None,
                rate: Some(2.0),
            },
        )
        .expect_err("refused");
        assert!(
            format!("{error:?}").contains("Pick one"),
            "the refusal says why: {error:?}"
        );
    }

    /// A number small enough to overflow the interval used to panic the
    /// sender, which reaches a caller as a request that never answers.
    #[test]
    fn a_rate_too_small_to_be_a_rate_is_refused_and_never_panics() {
        assert_eq!(
            Pace::new(Some(1e-300)).interval,
            Some(SLOWEST_INTERVAL),
            "the arithmetic is bounded even when the check above is bypassed"
        );
        assert_eq!(Pace::new(Some(f64::MIN_POSITIVE)).interval, Some(SLOWEST_INTERVAL));
        assert_eq!(Pace::new(Some(f64::NAN)).interval, None);
        assert_eq!(Pace::new(Some(0.0)).interval, None);
        assert_eq!(Pace::new(None).interval, None);

        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let error = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: false,
                raw_target: None,
                raw_request: None,
                raw_headers: false,
                each: None,
                rate: Some(1e-300),
            },
        )
        .expect_err("refused");
        assert!(
            format!("{error:?}").contains("one send an hour"),
            "the refusal says why: {error:?}"
        );
    }

    /// A rate is a ceiling on what the target sees.
    #[test]
    fn a_rate_holds_the_sends_apart() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let port = echo_request_lines(3);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/thing")).unwrap();
        let started = std::time::Instant::now();
        let sent = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 3,
                together: false,
                no_follow: false,
                raw_target: None,
                raw_request: None,
                raw_headers: false,
                each: None,
                // Fast enough to keep the suite quick, slow enough that three
                // sends cannot land inside one interval by accident.
                rate: Some(25.0),
            },
        )
        .expect("sent");
        assert_eq!(sent.samples.len(), 3);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(80),
            "three sends at 25/s take two intervals: {:?}",
            started.elapsed()
        );
    }

    /// Raw requests cannot be walked.
    #[test]
    fn set_each_and_a_raw_send_are_refused_together() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let error = crate::broker::Broker::send_given(
            broker.as_ref(),
            crate::broker::Given {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
            },
            &[],
            false,
            crate::broker::Sends {
                count: 1,
                together: false,
                no_follow: false,
                raw_target: Some("/x".to_string()),
                raw_request: None,
                raw_headers: false,
                rate: None,
                each: Some(crate::broker::Each::over("query.id", vec!["1".into()])),
            },
        )
        .expect_err("refused");
        assert!(
            format!("{error:?}").contains("already decided"),
            "the refusal says why: {error:?}"
        );
    }

    /// A raw request preserves its bytes and records its framing headers.
    #[test]
    fn a_raw_request_is_written_verbatim_and_its_framing_is_noted() {
        let editable = crate::edits::Editable {
            method: "GET".to_string(),
            url: Url::parse("http://app.test/").unwrap(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
                     Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            .to_vec();
        let plan = crate::broker::Sends {
            raw_request: Some(wire.clone()),
            raw_headers: false,
            each: None,
            ..Default::default()
        };
        let raw = build_raw_request(&editable, &plan).expect("built");
        assert_eq!(raw.method, "POST");
        assert_eq!(raw.target, "/");
        assert_eq!(raw.wire, wire, "not one byte is changed");
        assert!(raw.broke.contains(&"content-length".to_string()));
        assert!(raw.broke.contains(&"transfer-encoding".to_string()));
    }

    /// A managed raw request computes framing but preserves its target.
    #[test]
    fn a_managed_raw_request_frames_itself() {
        let editable = crate::edits::Editable {
            method: "post".to_string(),
            url: Url::parse("http://127.0.0.1:8080/ignored").unwrap(),
            headers: vec![("Accept".to_string(), "*/*".to_string())],
            body: b"hello".to_vec(),
        };
        let raw = build_managed_raw(&editable, "/cgi-bin/.%2e/bin/sh");
        let text = String::from_utf8_lossy(&raw.wire);
        assert!(text.starts_with("POST /cgi-bin/.%2e/bin/sh HTTP/1.1\r\n"));
        assert!(text.contains("Host: 127.0.0.1:8080\r\n"), "{text}");
        assert!(text.contains("Content-Length: 5\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\nhello"), "{text}");
        assert!(raw.broke.is_empty(), "the managed shape breaks nothing");
    }

    /// A session that never captured has nothing to replay, and the message
    /// says what to do about it.
    #[test]
    fn a_session_without_a_store_says_why_it_cannot_replay() {
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
            .expect("broker");
        let error = crate::broker::Broker::send_edited(
            broker.as_ref(),
            0,
            &[],
            false,
            crate::broker::Sends::once(),
        )
        .expect_err("refused");
        assert_eq!(error.code, "no-capture");
        assert!(error.message.contains("--capture"), "{}", error.message);
    }

    /// A session without a store is the default, and it writes nothing.
    #[test]
    fn a_session_with_no_store_keeps_no_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::with_limits(
            Policy::new(),
            sink.clone(),
            None,
            crate::budget::Limits::default(),
            None,
        )
        .expect("broker");

        let port = redirect_then_answer();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        let _ = broker.fetch(&url, Initiator::Navigation);

        assert!(!sink.records().is_empty(), "the receipt is never optional");
        assert!(
            broker.capture().is_none(),
            "no store is a different answer from a healthy one"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("readable").count(),
            0,
            "nothing is written where nothing was asked for"
        );
    }
}

#[cfg(test)]
mod cookie_wire_tests {
    use super::*;
    use crate::broker::Broker;
    use crate::receipt::MemorySink;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A server that answers anything, forever, so a runaway page has
    /// somewhere to run away to.
    fn always_answers(hits: usize) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..hits {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0
                        || header.trim().is_empty()
                    {
                        break;
                    }
                }
                let body = "ok";
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            }
        });
        port
    }

    /// A server that redirects once, to wherever it is told.
    fn redirects_to(target: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0
                        || header.trim().is_empty()
                    {
                        break;
                    }
                }
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });
        port
    }

    /// Where a server redirected a page is not the page's to learn.
    ///
    /// The chain begins at a URL the page named and ends somewhere the
    /// allowlist refuses; naming that target hands the page a URL it had no way
    /// to reach, and a redirect off an authenticated endpoint routinely carries
    /// an identity in it (`/me` -> `/users/alice`). The agent still gets the
    /// full sentence, because "the site moved" is the most actionable thing
    /// this engine can say to whoever is driving it.
    #[test]
    fn a_page_is_not_told_where_a_refused_redirect_pointed_and_the_agent_is() {
        const TARGET: &str = "https://tracker.example/users/alice?token=s3cr3t";
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(
            crate::policy::Policy::new(),
            sink.clone(),
            None,
        )
        .expect("broker");

        // The page asked: a `fetch` from a document, so a CORS context exists.
        let port = redirects_to(TARGET);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/me")).unwrap();
        let document = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let outcome = broker.send_script(
            &url,
            "GET",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::SameOrigin,
        );
        let error = outcome.error.expect("the hop is refused");
        assert!(!error.contains("tracker.example"), "{error}");
        assert!(!error.contains("s3cr3t"), "{error}");
        assert!(error.contains("redirected"), "{error}");

        // The agent asking the same thing is told where it went.
        let port = redirects_to(TARGET);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/me")).unwrap();
        let outcome = broker.send_from(&url, Initiator::Navigation, "GET", &[], None, None);
        let error = outcome.error.expect("the hop is refused");
        assert!(error.contains("tracker.example"), "{error}");

        // And the receipt has it either way: the audit lane is not the page's
        // to be protected from.
        assert!(
            sink.records().iter().any(|r| r.url.contains("tracker.example")),
            "the refused hop is not in the log"
        );
    }

    /// Every request on the wire counts, including the ones that are not the page's
    /// own `fetch`.
    ///
    /// A preflight sat *before* the claim in `send_with_cors`, so a page issuing
    /// non-simple cross-origin requests whose preflights the server refuses made
    /// unlimited round trips while the allowance recorded none of them: the real
    /// request never happened, so the request that was counted never happened
    /// either. A socket handshake and an event stream were outside it too.
    #[test]
    fn a_preflight_a_socket_and_a_stream_all_spend_the_pages_allowance() {
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits {
                max_requests: 2,
                ..Default::default()
            },
            None,
        )
        .expect("broker");

        // Nothing is listening; the point is which of these the *budget*
        // refuses, and it refuses before anything is dialled.
        let ws = Url::parse("ws://127.0.0.1:9/hmr").unwrap();
        assert!(
            broker.authorise_socket(&ws, None).is_ok(),
            "the first is inside the allowance"
        );
        let sse = Url::parse("http://127.0.0.1:9/stream").unwrap();
        // The second spends the last of it; whether the wire answers is not
        // what is under test.
        let _ = broker.begin_event_stream(&sse, None);

        let over = broker
            .authorise_socket(&ws, None)
            .expect_err("the third is over the allowance");
        assert!(over.contains("budget-exceeded"), "{over}");
    }

    /// The budget bounds what an untrusted page can spend, and every limit in
    /// it was checked on the way *into a request*, which a socket makes
    /// exactly one of. A page could hold one open and pull gigabytes through it
    /// while `budget` reported a page that had spent almost nothing: the queue
    /// is bounded so the memory was safe, and the bandwidth, the time and the
    /// honesty of the number were not.
    #[test]
    fn a_long_lived_connection_is_charged_for_what_it_carries() {
        let broker = LocalBroker::with_limits(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
            crate::budget::Limits {
                max_wire_bytes: 4_096,
                ..Default::default()
            },
            None,
        )
        .expect("broker");

        let url = Url::parse("ws://127.0.0.1:9/hmr").unwrap();
        // Inside the allowance: the frame is receipted and the page carries on.
        broker
            .record_socket_frame(&url, crate::wsclient::Direction::Receive, 1_024)
            .expect("a frame inside the allowance");
        assert_eq!(broker.spending().spent().wire_bytes, 1_024);

        // Past it: the frame is refused, which is what closes the socket.
        broker
            .record_socket_frame(&url, crate::wsclient::Direction::Receive, 8_192)
            .expect_err("a frame past the allowance");

        // And the frames are in the log either way. A receipt records what
        // crossed the wire, and these did.
        assert_eq!(
            broker
                .records()
                .iter()
                .filter(|r| r.method == "WS-RECV" && r.phase == crate::receipt::Phase::Response)
                .count(),
            2
        );
    }

    /// The gap the budget fills. Every limit before it was *per request* (a
    /// size cap, a redirect count, a timeout) and none of them bounds a page
    /// that makes many. Recording a runaway is not the same as stopping one.
    #[test]
    fn a_page_that_keeps_asking_is_eventually_refused() {
        let port = always_answers(20);
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::with_limits(
            Policy::new(),
            sink.clone(),
            None,
            crate::budget::Limits {
                max_requests: 3,
                ..Default::default()
            },
            None,
        )
        .expect("broker");
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();

        for at in 1..=3 {
            let outcome = broker.fetch_from(&url, Initiator::Subresource, None);
            assert!(outcome.error.is_none(), "request {at}: {outcome:?}");
        }
        let refused = broker.fetch_from(&url, Initiator::Subresource, None);
        let why = refused.error.expect("the fourth is over budget");
        assert!(why.contains("budget-exceeded"), "{why}");

        // And it is *recorded* as a denial, because "the page ran out of
        // allowance" is exactly what a reader of the log needs to see.
        let denied = sink
            .records()
            .into_iter()
            .filter(|r| !r.allowed)
            .filter(|r| {
                r.denied_reason
                    .as_deref()
                    .is_some_and(|why| why.contains("budget-exceeded"))
            })
            .count();
        assert!(denied >= 1, "the refusal must be in the log");
    }

    /// A fresh page is a fresh decision by the agent, so it gets a fresh
    /// allowance. The budget bounds untrusted page code, not the principal.
    #[test]
    fn navigating_gives_the_next_page_its_own_allowance() {
        let port = always_answers(20);
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::with_limits(
            Policy::new(),
            sink,
            None,
            crate::budget::Limits {
                max_requests: 2,
                ..Default::default()
            },
            None,
        )
        .expect("broker");
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();

        for _ in 0..2 {
            assert!(broker.fetch_from(&url, Initiator::Subresource, None).error.is_none());
        }
        assert!(broker.fetch_from(&url, Initiator::Subresource, None).error.is_some());

        broker.reset_budget();
        assert!(
            broker.fetch_from(&url, Initiator::Subresource, None).error.is_none(),
            "a navigation restores the allowance"
        );
    }

    /// A server that compresses when asked, and reports what it was asked for.
    fn gzip_server(body: &'static [u8], hits: usize) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        use flate2::write::GzEncoder;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let record = seen.clone();
        std::thread::spawn(move || {
            for _ in 0..hits {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut accept = String::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0
                        || header.trim().is_empty()
                    {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("accept-encoding:") {
                        accept = rest.trim().to_string();
                    }
                }
                record.lock().unwrap().push(accept.clone());

                let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body).unwrap();
                let payload = encoder.finish().unwrap();
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Encoding: gzip\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(&payload);
                let _ = stream.flush();
            }
        });
        (port, seen)
    }

    /// The capability that was absent, and the measurement that goes with it.
    ///
    /// Both sizes are *measured* rather than read off a header, which is why
    /// this engine decodes its own bodies: `reqwest` will do it transparently
    /// and strips `Content-Encoding` and `Content-Length` on the way, so the
    /// number that says what the request actually cost is gone before anything
    /// can record it.
    #[test]
    fn a_compressed_response_is_decoded_and_both_sizes_are_recorded() {
        const BODY: &[u8] = b"<html><body>compressible filler compressible filler                               compressible filler compressible filler</body></html>";
        let (port, seen) = gzip_server(BODY, 1);
        let sink = Arc::new(MemorySink::new());
        let broker =
            LocalBroker::new(Policy::new(), sink.clone(), None).expect("broker");
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();

        let outcome = broker.fetch_from(&url, Initiator::Navigation, None);
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(outcome.body, BODY, "the body must arrive decoded");

        // The engine asked for what it can decode, and nothing else.
        assert_eq!(seen.lock().unwrap()[0], "gzip, br, deflate");

        let response = sink
            .records()
            .into_iter()
            .find(|r| r.phase == crate::receipt::Phase::Response)
            .expect("a response record");
        assert_eq!(response.bytes, Some(BODY.len() as u64), "what the page got");
        let wire = response.wire_bytes.expect("what the wire carried");
        assert!(
            wire < BODY.len() as u64,
            "the compressed size should be smaller: {wire} vs {}",
            BODY.len()
        );
        assert_eq!(response.content_encoding.as_deref(), Some("gzip"));

        // And the line a person reads carries both, because "184 KB" and
        // "43 KB on the wire" answer different questions.
        let line = response.render();
        assert!(line.contains("on the wire"), "{line}");
        assert!(line.contains("gzip"), "{line}");
    }

    /// The reason the decoding is capped as well as the reading. A few
    /// kilobytes of zeroes is gigabytes of zeroes, and a browser that decoded
    /// without a limit would let any allowed origin exhaust the box's memory
    /// with one response.
    #[test]
    fn a_response_that_decompresses_past_the_cap_is_refused() {
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(
            Policy::new().set_max_response_bytes(64 * 1024),
            sink,
            None,
        )
        .expect("broker");

        // 4 MiB of zeroes compresses to a few kilobytes: small enough to pass
        // the wire cap, far past the decoded one.
        let bomb = vec![0u8; 4 * 1024 * 1024];
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&bomb).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < 64 * 1024, "the bomb must pass the wire cap");

        let refused = broker.decode_capped(&compressed, "gzip");
        let why = refused.expect_err("a decompression bomb must be refused");
        assert!(
            why.to_string().contains("decompresses past"),
            "the refusal should name what happened: {why}"
        );
    }

    /// An encoding this engine cannot decode is an error, not a body passed
    /// through: handing compressed bytes to the HTML parser would render a page
    /// of binary, which is a wrong answer that looks like a broken site.
    #[test]
    fn an_encoding_this_engine_did_not_ask_for_is_an_error() {
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(Policy::new(), sink, None).expect("broker");
        let why = broker
            .decode_capped(b"whatever", "exotic-zip")
            .expect_err("an unknown encoding is an error");
        assert!(why.to_string().contains("exotic-zip"), "{why}");
    }

    #[test]
    fn every_encoding_this_engine_advertises_round_trips() {
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(Policy::new(), sink, None).expect("broker");
        let body = b"round trip me".repeat(50);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&body).unwrap();
        assert_eq!(broker.decode_capped(&gz.finish().unwrap(), "gzip").unwrap(), body);

        let mut zl = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zl.write_all(&body).unwrap();
        assert_eq!(
            broker.decode_capped(&zl.finish().unwrap(), "deflate").unwrap(),
            body
        );

        // Raw deflate too: the spec says zlib and enough servers send bare that
        // a browser has to try both.
        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(&body).unwrap();
        assert_eq!(
            broker.decode_capped(&raw.finish().unwrap(), "deflate").unwrap(),
            body
        );

        let mut br = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut br, 4096, 5, 22);
            writer.write_all(&body).unwrap();
        }
        assert_eq!(broker.decode_capped(&br, "br").unwrap(), body);

        // A stacked encoding is refused by name. Reading the last one made the
        // engine brotli-decode gzip output and blame brotli for it.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &body).unwrap();
        let err = broker
            .decode_capped(&gz.finish().unwrap(), "gzip, br")
            .expect_err("stacked encodings are refused");
        assert!(format!("{err}").contains("stacks encodings"), "{err}");
        // `identity` in the list is a no-op, not a second layer.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &body).unwrap();
        assert_eq!(
            broker.decode_capped(&gz.finish().unwrap(), "identity, gzip").unwrap(),
            body
        );
    }

    /// Serves a page that says who asked and echoes the cookie it saw, with
    /// whatever CORS headers the test wants.
    fn cors_server(
        allow: Option<&'static str>,
        allow_credentials: bool,
        hits: usize,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let record = seen.clone();
        std::thread::spawn(move || {
            for _ in 0..hits {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let method = line.split_whitespace().next().unwrap_or("").to_string();
                let mut origin = String::new();
                let mut cookie = String::new();
                let mut rest_headers: Vec<String> = Vec::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0
                        || header.trim().is_empty()
                    {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("origin:") {
                        origin = rest.trim().to_string();
                    }
                    if let Some(rest) = lower.strip_prefix("cookie:") {
                        cookie = rest.trim().to_string();
                    }
                    // Everything else, so a test can ask what actually
                    // travelled rather than only what the two named fields did.
                    rest_headers.push(lower.trim().to_string());
                }
                record
                    .lock()
                    .unwrap()
                    .push(format!(
                        "{method} origin={origin} cookie={cookie} {}",
                        rest_headers.join(" ")
                    ));

                let body = "SECRET-BODY";
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     X-Private: nope\r\nConnection: close\r\n",
                    body.len()
                );
                if let Some(allow) = allow {
                    head.push_str(&format!("Access-Control-Allow-Origin: {allow}\r\n"));
                    head.push_str("Access-Control-Allow-Methods: DELETE\r\n");
                    head.push_str("Access-Control-Allow-Headers: x-token\r\n");
                }
                if allow_credentials {
                    head.push_str("Access-Control-Allow-Credentials: true\r\n");
                }
                // So a test can also ask what the *response* was allowed to
                // leave behind, not only what the request carried.
                head.push_str("Set-Cookie: planted=1; Path=/\r\n");
                let mut stream = stream;
                let _ = write!(stream, "{head}\r\n{body}");
                let _ = stream.flush();
            }
        });
        (port, seen)
    }

    fn cors_broker() -> (Arc<LocalBroker>, Arc<MemorySink>) {
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(Policy::new(), sink.clone(), None).expect("broker");
        (broker, sink)
    }

    /// The hole. Loopback is reachable by default (it is the dev server), so
    /// two pages on it are two *origins*, different ports, that the allowlist
    /// both permits. Before the same-origin policy, a script on one could read
    /// the other's body.
    #[test]
    fn a_cross_origin_read_is_refused_unless_the_server_allows_it() {
        let (port, seen) = cors_server(None, false, 1);
        let (broker, _sink) = cors_broker();
        let document = Url::parse("http://127.0.0.1:1/page").unwrap();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/secret")).unwrap();

        let outcome = broker.send_script(
            &target,
            "GET",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );

        assert!(
            outcome.error.is_some(),
            "a cross-origin body must not be handed over: {outcome:?}"
        );
        assert!(
            outcome.error.as_deref().unwrap().contains("same-origin policy"),
            "{outcome:?}"
        );
        assert!(
            outcome.body.is_empty(),
            "the body must not even be read: {outcome:?}"
        );
        // The request *was* made and announced itself, which is what lets a
        // server answer the question at all.
        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("origin=http://127.0.0.1:1"), "{seen:?}");
    }

    /// And the other half: a server that names us back gets read.
    #[test]
    fn a_cross_origin_read_the_server_allows_goes_through() {
        let (port, _seen) = cors_server(Some("*"), false, 1);
        let (broker, _sink) = cors_broker();
        let document = Url::parse("http://127.0.0.1:1/page").unwrap();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/open")).unwrap();

        let outcome = broker.send_script(
            &target,
            "GET",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(String::from_utf8_lossy(&outcome.body), "SECRET-BODY");
        // Headers are filtered to the safelist: the server exposed nothing.
        assert!(
            !outcome.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("x-private")),
            "an unexposed header leaked: {:?}",
            outcome.headers
        );
        assert!(
            outcome.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")),
            "the safelist should still be visible: {:?}",
            outcome.headers
        );
    }

    /// The consequence of roadmap-history.md §B16's cookie work, and the reason this
    /// module was written before any further capability: with `Domain` cookies
    /// and no same-origin policy, a cross-origin read is an *authenticated*
    /// one. The default credentials mode is what stops it.
    #[test]
    fn a_cross_origin_request_does_not_carry_the_session_by_default() {
        let (port, seen) = cors_server(Some("*"), false, 1);
        let (broker, _sink) = cors_broker();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        // A session cookie for the target origin, as a login would have left.
        broker.jar().store(&target, ["sid=s3cr3t; Path=/"]);

        let document = Url::parse("http://127.0.0.1:1/page").unwrap();
        let outcome = broker.send_script(
            &target,
            "GET",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_none(), "{outcome:?}");

        let seen = seen.lock().unwrap();
        assert!(
            seen[0].contains("cookie="),
            "the server should have been asked: {seen:?}"
        );
        assert!(
            !seen[0].contains("s3cr3t"),
            "a cross-origin fetch must not carry the session by default: {seen:?}"
        );
    }

    /// The headers a page sets decided whether a preflight was needed and what
    /// it asked permission for, and were then not sent: the server answered a
    /// question about a request that never happened.
    #[test]
    fn a_page_set_header_reaches_the_request_it_was_preflighted_for() {
        let (port, seen) = cors_server(Some("*"), false, 1);
        let (broker, _sink) = cors_broker();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let document = target.join("/page").unwrap();
        let outcome = broker.send_script(
            &target,
            "GET",
            &[],
            None,
            &document,
            &[
                ("X-Token".to_string(), "carried".to_string()),
                // Forbidden: a page choosing `Host` is a page choosing what
                // the boundary was decided about.
                ("Host".to_string(), "evil.test".to_string()),
                ("Cookie".to_string(), "sid=forged".to_string()),
            ],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_none(), "{outcome:?}");
        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("x-token: carried"), "{seen:?}");
        assert!(!seen[0].contains("evil.test"), "a page may not choose the Host: {seen:?}");
        assert!(!seen[0].contains("sid=forged"), "nor forge a Cookie: {seen:?}");
    }

    /// The other direction of the same flag. A response nobody was allowed to
    /// read must not be allowed to write either: storing its `Set-Cookie`
    /// unconditionally let an attacker page plant a session on another
    /// allowlisted origin with a request whose answer it never sees.
    #[test]
    fn a_cross_origin_response_does_not_leave_a_cookie_behind_by_default() {
        let (port, _seen) = cors_server(Some("*"), false, 1);
        let (broker, _sink) = cors_broker();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let document = Url::parse("http://127.0.0.1:1/page").unwrap();
        let outcome = broker.send_script(
            &target,
            "GET",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert!(
            broker.jar().header_for(&target).is_none(),
            "the response planted a cookie the request was not credentialed for"
        );
    }

    /// A same-origin request is unaffected by any of it, which is what keeps
    /// ordinary pages working.
    #[test]
    fn a_same_origin_request_still_carries_its_session_and_reads_everything() {
        let (port, seen) = cors_server(None, false, 1);
        let (broker, _sink) = cors_broker();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/api")).unwrap();
        broker.jar().store(&target, ["sid=s3cr3t; Path=/"]);

        let document = Url::parse(&format!("http://127.0.0.1:{port}/page")).unwrap();
        let outcome = broker.send_script(
            &target,
            "GET",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(String::from_utf8_lossy(&outcome.body), "SECRET-BODY");
        assert!(
            outcome.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("x-private")),
            "same-origin sees every header: {:?}",
            outcome.headers
        );

        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("s3cr3t"), "{seen:?}");
        // No `Origin` header at all: that is how a server tells a same-origin
        // request from a cross-origin one, and sending one would ask a
        // question that has no business being asked.
        assert!(
            !seen[0].contains("origin=http"),
            "same-origin must send no Origin header: {seen:?}"
        );
    }

    /// A non-simple request asks first, and the preflight is a real request:
    /// policy-checked and receipted, so it appears in the log rather than
    /// arriving from nowhere.
    #[test]
    fn a_non_simple_request_preflights_and_the_preflight_is_receipted() {
        // Two hits: the OPTIONS, then the DELETE.
        let (port, seen) = cors_server(Some("*"), false, 2);
        let (broker, sink) = cors_broker();
        let document = Url::parse("http://127.0.0.1:1/page").unwrap();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/item")).unwrap();

        let outcome = broker.send_script(
            &target,
            "DELETE",
            &[],
            None,
            &document,
            &[("x-token".to_string(), "abc".to_string())],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_none(), "{outcome:?}");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one preflight, one request: {seen:?}");
        assert!(seen[0].starts_with("OPTIONS"), "{seen:?}");
        assert!(seen[1].starts_with("DELETE"), "{seen:?}");

        // And both are in the log. A preflight that did not appear would be a
        // request this engine made and did not record.
        let options = sink
            .records()
            .into_iter()
            .filter(|r| r.method == "OPTIONS")
            .count();
        assert!(options >= 1, "the preflight must be receipted");
    }

    /// A server that refuses at preflight time refuses before the real
    /// request is made, which is the round trip a preflight buys.
    #[test]
    fn a_refused_preflight_stops_the_request_before_it_is_made() {
        let (port, seen) = cors_server(None, false, 1);
        let (broker, _sink) = cors_broker();
        let document = Url::parse("http://127.0.0.1:1/page").unwrap();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/item")).unwrap();

        let outcome = broker.send_script(
            &target,
            "DELETE",
            &[],
            None,
            &document,
            &[],
            crate::cors::Mode::Cors,
            crate::cors::Credentials::default(),
        );
        assert!(outcome.error.is_some(), "{outcome:?}");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "only the preflight was sent: {seen:?}");
        assert!(seen[0].starts_with("OPTIONS"), "{seen:?}");
    }

    /// A request the *agent* made is unrestricted, which is what keeps
    /// `navigate` and the read verbs working exactly as they did.
    #[test]
    fn an_agent_request_is_not_subject_to_the_same_origin_policy() {
        let (port, _seen) = cors_server(None, false, 1);
        let (broker, _sink) = cors_broker();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/anything")).unwrap();

        let outcome = broker.fetch_from(&target, Initiator::Navigation, None);
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(String::from_utf8_lossy(&outcome.body), "SECRET-BODY");
    }

    /// A server that sets a cookie, then reports what came back.
    fn login_server() -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut sent_cookie = String::new();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                    if let Some(rest) = header.to_ascii_lowercase().strip_prefix("cookie:") {
                        sent_cookie = rest.trim().to_string();
                    }
                }
                let mut stream = stream;
                if path == "/login" {
                    let body = "<html><body>ok</body></html>";
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSet-Cookie: sid=s3cr3t-value; Path=/\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).unwrap();
                } else {
                    let body = format!("<html><body>saw:{sent_cookie}</body></html>");
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).unwrap();
                }
                let _ = stream.flush();
            }
        });
        (port, handle)
    }

    /// Redirects a POST once, then reports what the follow-up looked like.
    fn redirecting_server(status: u16) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut second = String::new();
            for hop in 0..2 {
                let Ok((stream, _)) = listener.accept() else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let method = line.split_whitespace().next().unwrap_or("").to_string();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if length > 0 {
                    use std::io::Read;
                    let _ = reader.read_exact(&mut body);
                }
                let mut stream = stream;
                if hop == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 {status} Moved
Location: /after
Content-Length: 0
Connection: close

"
                    ).unwrap();
                } else {
                    second = format!("{method} body={}", String::from_utf8_lossy(&body));
                    let page = "<html><body>done</body></html>";
                    write!(
                        stream,
                        "HTTP/1.1 200 OK
Content-Length: {}
Connection: close

{page}",
                        page.len()
                    ).unwrap();
                }
                let _ = stream.flush();
            }
            second
        });
        (port, handle)
    }

    #[test]
    fn a_redirected_post_does_not_replay_its_body_to_the_next_host() {
        // The browser rule, and it is a security rule: 301/302/303 turn the
        // follow-up into a bodyless GET, so a password typed into one form is
        // not re-sent to wherever that server points next.
        for status in [301u16, 302, 303] {
            let (port, server) = redirecting_server(status);
            let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None).unwrap();
            let target = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();

            let outcome = broker.send_from(
                &target,
                Initiator::Navigation,
                "POST",
                b"password=hunter2",
                Some("application/x-www-form-urlencoded"),
                None,
            );
            assert!(outcome.is_ok(), "{status}: {:?}", outcome.error);

            let followed = server.join().unwrap();
            assert_eq!(
                followed, "GET body=",
                "{status} must downgrade to a bodyless GET, got {followed:?}"
            );
        }
    }

    #[test]
    fn a_307_keeps_the_method_because_the_server_asked_for_that_explicitly() {
        let (port, server) = redirecting_server(307);
        let broker = LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None).unwrap();
        let target = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();

        broker.send_from(
            &target,
            Initiator::Navigation,
            "POST",
            b"password=hunter2",
            Some("application/x-www-form-urlencoded"),
            None,
        );

        let followed = server.join().unwrap();
        assert_eq!(followed, "POST body=password=hunter2", "got {followed:?}");
    }

    #[test]
    fn a_session_survives_between_requests_and_never_reaches_the_log() {
        let (port, server) = login_server();
        let sink = Arc::new(MemorySink::new());
        let broker = LocalBroker::new(Policy::new(), sink.clone(), None).unwrap();

        // Loopback is reachable without an allowlist entry, which is what lets
        // this test exercise the wire without inventing a policy.
        let login = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        let outcome = broker.fetch(&login, Initiator::Navigation);
        assert!(outcome.is_ok(), "login failed: {:?}", outcome.error);
        assert_eq!(broker.jar().len(), 1, "the session cookie was kept");

        let page = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
        let outcome = broker.fetch(&page, Initiator::Navigation);
        let body = String::from_utf8_lossy(&outcome.body).to_string();
        assert!(
            body.contains("saw:sid=s3cr3t-value"),
            "the second request carried the session: {body}"
        );

        // The receipt says how many, and nothing more. A credential in a
        // request log is a credential in every export that log ends up in.
        let log = serde_json::to_string(&sink.records()).unwrap();
        assert!(!log.contains("s3cr3t-value"), "a value reached the log:\n{log}");
        assert!(log.contains("cookies_sent"), "the count is recorded:\n{log}");

        let sent: Vec<usize> = sink
            .records()
            .into_iter()
            .filter_map(|r| r.cookies_sent)
            .collect();
        assert_eq!(sent, vec![0, 1], "none on login, one on the page after it");

        let _ = server.join();
    }
}
