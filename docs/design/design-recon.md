# Design: reconnaissance and the endpoint ledger, sections N1 to N21

Status: phase 1 mostly built, 2026-09-07. This design adds the half of an
engagement that comes before a payload: finding what an application exposes,
recording where each candidate came from, and handing a confirmed request to
the workbench. Sections marked "built" say what shipped, and where it differs
from the paragraph above. `N` is the section prefix because `B`, `V`, `P`, `R`,
`D` and `W` are taken by the browser, the viewers, policy, the runner,
detection and websec, and live code cites these numbers.

## In one screen

> The workbench answers "what happens if I send this again, changed". Recon
> answers "what is there to send at all", and says how it knows.

- **Discovery and confirmation are different states, and the ledger keeps them
  apart.** A URL scraped from a JavaScript bundle was not visited. A URL that
  answered 200 was visited. A URL confirmed to differ from the target's
  not-found baseline is a third thing. Collapsing them is the failure mode of
  every crawler that hands an agent four thousand rows.
- **Recon collects, websec verifies.** Crawling, extraction, path discovery and
  triage live here. Payload injection, authorisation comparison and
  out-of-band callbacks stay in [`design-websec.md`](design-websec.md).
- **No new HTTP client.** Every request recon sends goes through
  `Broker::fetch` like everything else, so the session's policy decides, the
  budget is spent, and the receipt is written before the bytes move. A recon
  tool with a socket of its own would void the claim the product leads with.
- **We write it, we do not shell out to it.** The parsers, the crawler, the
  wordlist expansion and the clustering are ours. Borrowing a binary borrows
  its CVEs, at h5i's privilege, at the exact point the target's bytes arrive,
  and with an HTTP client that answers to nobody here. N19 is the argument.
- **Nothing is flagged.** Recon reports endpoints, responses and differences.
  Calling one of them a vulnerability is the agent's claim, written in a
  finding it signs.
- **It amends websec's refusals.** `design-websec.md` currently refuses "a
  crawl-and-flag mode" and "no wordlists". Recon crawls and takes a wordlist,
  and still flags nothing. N20 says exactly what changes.

Part of the h5i design set. The roadmap is
[`../../ROADMAP.md`](../../ROADMAP.md); the engine is
[`design-browser.md`](design-browser.md); the workbench this feeds is
[`design-websec.md`](design-websec.md).

---

## N1. What is being claimed

> For a session run with capture on, every endpoint recon reports carries the
> source that disclosed it, the identity that observed it, and the message id
> of the request that confirmed it, or an explicit state saying no request was
> ever sent. A row with no request id is a candidate, and the ledger will not
> print it as anything else.

Recon does not claim completeness. A crawl bounded by depth, budget and rate
misses things by construction, and an application that generates URLs at
runtime cannot be enumerated by reading its bundles. What recon claims is that
what it did report is attributable and reproducible.

## N2. Why the split, and why the join is a message id

Burp puts Site map, Crawler, Discover content and Intruder in one window
because one person drives all four. An agent is not one person: it takes a
turn, gets a result, and decides. That makes the boundary between the two
halves worth drawing hard.

- Recon's output is an inventory. It grows monotonically, it is read by
  `--since` cursor, and it is worth keeping after the run.
- Websec's output is an experiment. It is a request, an edit, a response and a
  difference, and it is worth keeping only with the reasoning around it.

They join at one place: the ledger row names a `req_<n>` from the message
store, and `h5i websec replay req_<n>` picks it up with the same jar, identity
and policy that produced it. That is the whole integration. Recon does not
learn to send payloads and websec does not learn to crawl.

## N3. What already exists

More than half of phase 1 is exposure of shipped machinery, which is the same
position websec started from.

| piece | where | what it gives |
|---|---|---|
| receipt fold into origins and endpoints | `src/cli/websec.rs` `sitemap`, W17 | methods, statuses, parameter names, hit counts, a navigated mark, refused URLs listed apart |
| message store | `crates/h5i-browser/src/capture.rs`, W5 | exact request and response bytes behind a stable `req_<n>`, the evidence a ledger row points at |
| one fetch entry point | `broker.rs` | the single place a discovery request can be policed, budgeted and recorded |
| scope and limits | `policy.rs`, `budget.rs` | origin and wildcard allowlists, `max_requests`, wire and decoded byte ceilings, network time |
| identities and jars | `identity.rs`, `cookies.rs`, named sessions | the same URL observed anonymously and logged in, without a cookie copy-paste |
| response reading | `extract.rs`, `snapshot.rs`, `markdown.rs`, `structured.rs`, `find` | selectors, text, structured metadata, the primitives a link extractor should reuse rather than reinvent |
| script execution | `crates/h5i-browser/src/script/`, boa | a real parse of a bundle is available, so JavaScript extraction need not be regex only |
| websocket URLs | `wsclient.rs` | endpoints a page opened that no HTML link names |
| a parallel executor with a barrier and a rate flag | W15 `--repeat`, `--race`, `--rate` | the scheduler path discovery needs, already composed with `budget.rs` |
| structural diff | W12 `browser diff` | the comparison triage should call rather than inventing a similarity metric |
| plugin mechanism | `src/cli/plugin.rs`, `crates/h5i-websec` | a separate executable with no privilege of its own, discovered by name |

W17 already anticipated this file. It shipped the observed half of a site map
and said in as many words that the disclosed but unvisited half "belongs in a
separately named command". This is that command.

## N4. What is missing, precisely

1. **No ledger.** `sitemap` recomputes a fold from the request log on every
   call and keeps nothing. There is nowhere to put a candidate, a discovery
   source, a confirmation state, or an observation under a second identity.
2. **No extraction of endpoints from content.** Forms, `link` and `script`
   sources, redirect targets, `fetch` and `XMLHttpRequest` targets in bundles,
   and URLs in JSON responses are all read by the engine and thrown away.
3. **No crawl.** There is `navigate`, `click` and `submit`, and no loop that
   walks a frontier with depth, cycle detection and a login-loss check.
4. **No path discovery.** No wordlist input, no extension or backup name
   generation, no recursion into a directory that answered.
5. **No triage.** A target that answers 200 for everything produces one row per
   probe, and an agent reading four thousand of them is worse off than before.
   Nothing calibrates a not-found baseline and nothing clusters near-identical
   responses.
6. **No job control past the page budget.** `budget.rs` defaults to 500
   requests, which is right for a page and an order of magnitude short for a
   discovery run, and there is no per-host rate, no resume and no progress.

## N5. The endpoint ledger

The core data structure, and the section the rest of the design hangs from.

Built 2026-09-07 in `crates/h5i-recon/src/ledger.rs`, as described, with one
addition the first live run forced: receipts are numbered from **zero**, so the
"how far have I folded" cursor is an `Option<u64>` rather than a count. A zero
meaning "nothing yet" silently dropped every session's first request, which is
its navigation. There is a sixth source, `calibration`, for the paths N11 asks
for on purpose because they should not exist: they are in the receipts either
way, and naming them keeps a reader from wondering why the inventory holds a
path nobody would have.

- One append-only JSONL file per session at `<session>/recon/ledger.jsonl`,
  mode 0600, with a derived index beside it. Append-only because resume,
  provenance and "what is new since the last turn" are all reads of the same
  log, and because a discovery run that crashes has still earned what it found.
- **Never in an export.** Candidate URLs are the target's data and a ledger row
  can carry a session token in a query string. It follows the message store's
  rule from W5, not the receipt's: not shipped in an export, a share or a bug
  report unless the caller names it, and dropped with the capture on close.
- **Untrusted input.** Every path, parameter name and title in it came from the
  target, exactly as snapshots do.

A row, in the shape the JSON contract carries:

| field | meaning |
|---|---|
| `id` | `ep_<hash>` over the key below, stable across runs |
| `origin`, `path`, `method` | the endpoint |
| `params` | parameter names seen, with where each lived: query, form, json, header, cookie |
| `identity` | the session identity that observed it, or `anonymous` |
| `state` | `candidate`, `observed`, `confirmed`, `refused`, `gone` |
| `source` | how it was learned, see below |
| `evidence` | the `req_<n>` list that observed it, most recent last |
| `cluster` | the triage cluster this response fell into, when triage ran |
| `first_seen`, `last_seen` | RFC3339 with microseconds, matching `receipt.rs` |
| `notes` | agent-written, never tool-written |

The key is `(origin, path, method, identity)`. Identity is in the key, not a
column beside it, because the question "does this endpoint answer differently
when logged in" is the one an inventory is for, and a schema that stores one
row per URL cannot hold the answer.

The five states are the whole discipline:

- `candidate`: disclosed by something, never sent. A bundle string, a form
  action never submitted, a wordlist entry not yet tried, an imported URL.
- `observed`: a request was sent and a response came back. Carries a `req_<n>`.
- `confirmed`: observed, and distinguished from the target's not-found baseline
  by N11's calibration. This is the only state that means "this exists".
- `refused`: policy declined it. Off-scope candidates stay in the ledger as
  refused rather than being dropped, because "recon wanted to reach this and
  was not allowed" is a fact a reviewer wants and a scope argument the operator
  may want to revisit.
- `gone`: previously confirmed, now matching the baseline or answering 404.

Sources, spelled as strings so a row is self-explaining:
`page:<req_n>` (HTML of a fetched page), `script:<req_n>` (a bundle),
`json:<req_n>` (a response body), `header:<req_n>` (a redirect or link header),
`known-file:<req_n>` (robots, sitemap), `wordlist:<name>`,
`openapi:<req_n|path>`, `import:<tool>`, `manual`.

## N6. Ids, and how a ledger row joins the workbench

Websec ids stay exactly as W6 defines them: `req_<n>` and `res_<n>`, scoped to
a session, fully qualified as `<session>/req_42` across sessions. Recon adds
one id, `ep_<hash>`, and no other numbering.

The handoff is one line:

```bash
h5i recon endpoints --state confirmed --json |
  jq -r '.endpoints[] | select(.params | index("id")) | .evidence[-1]' |
  xargs -I{} h5i websec replay {} --set query.id=1 --json
```

A ledger row is not a request. It is a name for a place, plus the evidence of
the last time h5i was there. Everything that sends bytes at it does so by
handing that evidence to websec.

## N7. The command surface

One noun, `recon`, matching W7's shape: every verb takes `--session`, defaults
to the default session, takes `--json`, and names its inputs rather than acting
on an implicit last result.

```
h5i recon endpoints  [--state S] [--origin O] [--identity I] [--since CURSOR]
h5i recon show       <ep_hash>
h5i recon extract    [--from req_42 | --since SEQ] [--kind html,js,json,headers]
h5i recon known      [--origin O]
h5i recon crawl      [--seed URL]... [--depth N] [--max-requests N] [--rate R]
h5i recon paths      --wordlist PATH [--extensions php,bak] [--recurse N]
h5i recon triage     [--origin O] [--calibrate]
h5i recon jobs       list | status <job> | stop <job> | resume <job>
h5i recon import     --format katana|gau|subfinder|httpx|openapi PATH
h5i recon export     [--format jsonl] [--state S]
```

`extract` is deliberately separate from `crawl`. Extraction reads what the
session already fetched and sends nothing, so it is the verb an agent can run
freely, and the one that keeps working when the budget is spent.

Built 2026-09-07: `endpoints`, `show`, `extract`, `known`, `crawl` and
`triage`, in `crates/h5i-recon/src/main.rs`. `paths`, `jobs`, `import` and
`export` are not built. Every verb that sends anything does it by running
`h5i browser resend --raw-target <path>` in a subprocess, which is the same
verb a person types: the fetch is the engine's, the policy decides it, the
budget pays for it, and the receipt is written first. The plugin has no other
route to the network.

## N8. Candidate sources

Everything here writes `candidate` rows and sends nothing of its own, except
`known` which fetches at most a handful of well-known paths.

- **HTML**: `<a href>`, `<form action>` with its method and input names,
  `<link>`, `<script src>`, `<img>`, `<iframe>`, and `srcset`. Relative URLs
  resolve against the response's final URL, after redirects, not against the
  URL the caller typed.
- **Response headers**: `Location` chains, `Link`, `Refresh`, and CSP
  `report-uri`, which names an endpoint often absent everywhere else.
- **JSON and structured metadata**: URL-shaped strings in response bodies, plus
  `structured.rs` output.
- **JavaScript**: `fetch` and `XMLHttpRequest` call sites, string literals that
  look like paths, and route tables. We have boa, so this is a syntax pass and
  not a regex sweep, which is what jsluice buys over LinkFinder. A path built
  by concatenation is recorded with its unresolved pieces marked, never
  guessed into a concrete URL.
- **WebSocket URLs** from `wsclient.rs`.
- **Known files**: `robots.txt`, `sitemap.xml` and its index chain,
  `security.txt`, `.well-known/openid-configuration`. `robots.txt` is a source
  of candidates and not an authorisation oracle. Scope comes from policy, and a
  `Disallow` entry is a hint about where things are, not permission to go
  there, nor a prohibition h5i enforces.

Every one of these records the `req_<n>` it read from, so "why does the ledger
think `/admin/api` exists" always has an answer that is a stored message.

Built 2026-09-07: `extract.rs` (markup, headers, JSON strings), `js.rs` (the
token scan) and `known.rs` (robots, sitemap and its index chain). The
JavaScript reader is a scanner rather than a boa pass, and it earns the
distinction the design asked for: it knows a comment is not code, that a `//`
inside a string is not a comment, and that a template literal with `${` is a
prefix. A concatenated path is reported as a *partial* and never written to the
ledger, because half a URL is not a place a request can go.

## N9. Authenticated crawl

The crawl is a frontier walk over ledger candidates, and it is the verb that
proves the split is worth having: it drives the same session an operator logged
into by hand, so there is no cookie to copy.

- **Seeds** are the session's current page, plus any `--seed` given, plus
  candidates already in the ledger.
- **Bounds** are depth, per-host request count, total budget and rate, all
  named on the command line and all reported in `jobs status`.
- **Cycle detection** is on the normalised URL, with a per-path-template cap so
  a calendar that generates a distinct URL per day does not consume the run.
- **Login loss** is the failure this has to handle. The check is a probe
  request to a page known to differ between identities, run every N requests,
  and the crawl stops with a named state rather than continuing to inventory
  the login page under an identity that no longer holds. Re-login is not
  automated in phase 1: the crawl stops, says so, and the operator or agent
  re-runs the login sequence with `websec sequence` or the browser verbs, which
  is where CSRF token refresh already lives.
- **Rendering** is opt-in. Without `--script`, a crawl fetches and parses. With
  it, pages execute and a form is submitted through `submit` rather than by
  constructing a POST. The second is slower by an order of magnitude and finds
  what the first cannot.

Two identities crawling the same target produce two sets of rows, not one set
with a flag. That is N5's key doing its job.

Built 2026-09-07 in `crawl.rs` (the frontier, the shape cap, the fingerprint)
and the `crawl` verb. Two deviations. The walk is a `GET` walk: a disclosed
`POST` endpoint stays a candidate, because submitting a form is `h5i browser
submit` and needs the page, not a path. And the login check re-probes the first
page the walk visited rather than a page named for the purpose, which needs no
configuration and costs one request per interval.

## N10. Path and word discovery

Wordlist-driven discovery of paths the application never disclosed.

- **h5i ships no wordlist.** `--wordlist` takes a path. Bundling one is a
  licensing question and a posture question, and the answer to both is that the
  operator brings it, exactly as W15 says payloads arrive from outside.
- **Generation is mechanical and named**: extension appending, backup name
  forms (`.bak`, `~`, `.old`, `.swp`), case variants, and reuse of words the
  crawl already saw, which is the single highest-yield source and costs
  nothing.
- **Recursion** enters a directory that confirmed, to a stated depth.
- **It sends through the same barrier-and-rate executor as W15.** This is the
  shared engine: Intruder-style position substitution for websec, path
  substitution for recon, one scheduler, one budget, one receipt path. Building
  it twice is the outcome this section exists to prevent.

Discovery output is never `confirmed` until N11 has run. A 200 from a target
that returns 200 for everything is an `observed` row, and calling it a finding
is the mistake the next section is entirely about.

## N11. Triage: calibration and clustering

The most important section in the file, and the one that decides whether an
agent can use any of the rest.

**Calibrate.** Before probing a directory, send a small number of random paths
that cannot exist, under each identity in play. Record the baseline: status,
wire length, decoded length, word count, content type, redirect target shape,
and a DOM skeleton hash from `snapshot.rs`. A response matching the baseline on
enough of those is not-found-like whatever its status code says. Recalibrate
per directory, because a soft 404 at the root and a real 404 under `/api` is
the common case.

**Cluster.** Group observed responses by (status, content type, DOM skeleton,
length bucket, redirect target). Report one representative per cluster, the
count, and the members that differ from their representative by more than a
threshold, computed with W12's structural diff rather than a new metric.

**Aggregation never destroys.** A cluster row points at `req_<n>` ids. Every
message stays in the store, `h5i websec show` reads any of them, and the
agent-facing summary is a view over the evidence, not a replacement for it.

What an agent gets back from a five thousand request run is on the order of
twenty rows: the clusters, their sizes, and the outliers. That is the difference
between a tool an agent can drive and a tool that fills its context window.

Built 2026-09-07 in `triage.rs`. Calibration is per directory and persisted
beside the ledger's cursor, so the cheap verb stays cheap. Two rules the code
adds: a baseline whose probes disagree with each other is *unstable* and
confirms nothing, because a directory that answers unpredictably would confirm
at random; and only something already confirmed can become `gone`, since a path
that never existed has not stopped existing. Verified against a server that
answers `200` with its own template for every path: the three invented paths
were folded into one cluster and marked as answering like a path that is not
there, while the two real pages confirmed.

## N12. Job control

- **Its own budget, visibly raised.** `budget.rs` defaults to 500 requests
  because that is a page. A discovery job takes `--max-requests` and spends
  from the session's `Limits`, and raising them is recorded in `status` and the
  digest the way `cross_site_credentials` is. A discovery run that quietly
  raised its own ceiling would be the first thing in h5i that did.
- **Rate is per host**, not global, and is a first-class flag for the same
  reason W15 gives: a CTF target and an authorised engagement both have someone
  who will notice.
- **Resume is a read of the ledger.** Rows already `confirmed`, `refused` or
  `gone` are skipped, `candidate` rows are the remaining frontier, and the job
  file at `<session>/recon/jobs/<id>.json` holds the parameters so a resume
  runs the same job rather than a similar one.
- **Progress goes to stderr, results to stdout**, per W9. A job that is
  streaming JSONL to a pipe must not interleave a progress bar into it.
- **Stopping is exact.** `jobs stop` finishes in-flight requests and writes the
  ledger, so a stopped job and a completed job differ in coverage and not in
  integrity.

Built 2026-09-07, with one deviation and one addition. The deviation: there is
no `jobs stop`, because a run is a foreground process and the way to stop it is
to stop it. What the design was really asking for is that stopping costs
nothing, so observations are written in batches of 25 as the run goes rather
than at the end; a killed run keeps what it found, which is what the ledger
being append-only was for. The addition: `--reset-budget` is a flag on `paths`
and `crawl` rather than something either does on its own. A page's allowance
bounds page code; a discovery run is the opposite case, and raising it is the
operator saying so out loud.

`jobs list`, `jobs show` and `jobs resume` are built. A resume reads the
recorded parameters and skips whatever the ledger has already answered, so it
continues a run rather than repeating one.

## N13. Scope, policy and authorisation

Recon inherits W16 unchanged, and adds one rule of its own.

The crawl and the path prober reach the network through `Broker::fetch`, so an
off-scope candidate is refused with a reason and recorded as refused rather
than sent. Recon cannot widen a session's allowlist. Widening scope means a new
session with a new policy, which is a visible act by the operator.

The addition: **discovery is the verb most likely to wander**, because its
whole job is to follow links to places nobody named. A crawl that follows an
off-site link is refused by policy today, and recon should say so loudly in
`jobs status` rather than leaving a silent gap in the map. A run whose refused
count is large is usually a scope that was set too narrowly, and the operator
is the one who decides.

## N14. What recon returns to an agent

Three shapes, and no others.

1. **A cursor diff.** `endpoints --since <cursor>` returns what is new, with a
   new cursor. An agent's turn asks "what did that job find", not "give me the
   inventory".
2. **A cluster summary.** N11's representatives and outliers.
3. **A row.** `show <ep_hash>` gives one endpoint with its sources, its
   evidence ids and its per-identity observations.

`"schema": "recon/1"`, fields added and never repurposed, errors as
`{"error": {...}}` on stdout with a nonzero exit, exit code 69 when the session
is gone. All of that is W9, and recon does not get a dialect of its own.

## N15. Phase 1, and what it has to prove

Features 1 to 8 of the proposal, mapped to sections.

| # | feature | section | notes |
|---|---|---|---|
| 1 | endpoint ledger | N5 | extends the shipped `sitemap` fold into stored state |
| 2 | URL extraction from HTML and traffic | N8 | reuses `extract.rs`, `snapshot.rs` |
| 3 | endpoint extraction from JavaScript | N8 | syntax pass on boa, unresolved parts marked |
| 4 | authenticated crawl | N9 | drives the session, stops on login loss |
| 5 | known files | N8 | cheapest of the eight |
| 6 | path and word discovery | N10 | wordlist supplied, never bundled |
| 7 | triage and aggregation | N11 | the one that decides whether the rest is usable |
| 8 | job control | N12 | on W15's executor and `budget.rs` |

**The acceptance test is one continuous run.** Log in with the browser, crawl
under that identity, discover paths, triage the result down to clusters, pick a
confirmed endpoint, and hand its `req_<n>` to `h5i websec replay`, with every
request in the session's receipts and no second HTTP client anywhere in the
path. Anything that cannot be done that way in phase 1 is not in phase 1.

**And a measured one.** websec's benchmark discipline applies here: targets
scored on endpoints confirmed, requests spent per confirmed endpoint, and false
confirmations. A feature that does not move those numbers is not finished,
whatever its tests say.

Built 2026-09-07: `scripts/recon/bench/run.sh` and
[`docs/benchmarks/recon.md`](../benchmarks/recon.md). Four targets that declare
what they have, including one that answers 200 for every path and one where
everything real is behind the wordlist. The first run scored 17 of 20 and found
two defects in a day-old implementation: triage read a shared template as a
missing page, and `paths` asked for `/admin` but never `/admin/`. Both are
fixed, and the run is 20 of 20 with no false confirmations. A corpus of real
applications is still not built.

The smoke suite is: `scripts/recon/smoke.sh` drives every verb against
`scripts/websec/server.py`, whose `/site/` tree now includes a directory that
answers 200 with its own "not found" page. It earned its place immediately.
Both bugs it exists for were invisible to the unit tests and obvious on the
first real run: receipts are numbered from **zero**, so every session's first
request, its navigation, was dropped from the ledger; and a probe built from
the newest stored request inherited a `POST` and asked for `robots.txt` with a
body, which the target answered `501` four times over.

## N16. Phase 2, more discovery

Features 9 to 12, all of which benefit from being inside the engine because
they need the session's identity, jar and scope.

| # | feature | notes |
|---|---|---|
| 9 | hidden parameter discovery | candidate names in batches, response differencing against a calibrated baseline, then re-verification of each hit alone to drop noise. Arjun and Param Miner are the references |
| 10 | API definition import | OpenAPI first: paths, methods and input shapes become ledger rows with `source: openapi`, and the ledger can then answer what the definition promises and the crawl never saw, which is the more interesting direction |
| 11 | service and technology probe | status, title, redirect chain, TLS details, headers, technology guesses. Every guess carries the evidence string that produced it, because an unsourced fingerprint is a rumour |
| 12 | virtual host discovery | `Host` candidates against a pinned address, compared to the default response. Connect address, TLS SNI and `Host` are three separate fields and the surface must not conflate them |

## N17. Phase 3, the perimeter, by import first

Features 13 to 15 are about hosts h5i has not talked to yet, and none of them
is an HTTP conversation.

The stance: `h5i recon import` reads a file the operator produced with gau,
subfinder, dnsx, httpx or naabu, writes ledger rows with `source: import:<tool>`
and a timestamp, and confirms nothing. h5i does not run those tools and does not
fetch their output; see N19. Everything imported is a `candidate` until an
h5i request observes it, which is exactly the discipline N5 exists to enforce
and is a better fit than reimplementing five mature tools.

Native passive collection, DNS resolution with wildcard detection, and TCP port
probing are deliberately deferred and may never be built. An archived URL is a
claim about the past, and h5i's whole posture is about what this machine
observed now.

Built 2026-09-07: `recon import --format urls|katana|subfinder|httpx|openapi`.
Two things it refuses to do. It never records what another tool saw: an httpx
row carries its URL and not its status, because a status is an answer and only
an h5i request can produce one here. And OpenAPI is read as JSON only; a YAML
document is one `yq` away and converting it stays the operator's job, for the
reason N19 gives about parsers standing where someone else's bytes arrive.

## N18. What belongs to websec, not here

Listed so the boundary can be defended in review rather than re-argued.

| capability | why it is websec's |
|---|---|
| general HTTP fuzzing at named positions | it is verification of a known request, which is W7's `replay` with W15's executor |
| authorisation comparison across identities | recon supplies the inventory and the identities; the comparison is a payload-free experiment on a known request, and its verdict logic is judgement |
| passive response rules | reads stored messages, produces candidate observations with evidence, and belongs beside `match` |
| out-of-band callbacks | already designed as W19, with no hosted service and pluggable backends |
| template-driven checks | if it ever exists, it starts by importing external results. Maintaining a template corpus is a product, not a feature |

## N19. Own the code that touches the target

Everything in N8, N10 and N11 is written here, in Rust, against crates already
in the workspace. Recon does not run katana, ffuf, feroxbuster, jsluice, arjun
or httpx, and does not call a service that does. Those tools are named through
this file as references for behaviour worth reproducing, not as dependencies.

Three reasons, in the order they bite.

1. **A bug in a borrowed tool is a bug at h5i's privilege.** The code in
   question sits exactly where hostile bytes arrive: a link extractor parses
   whatever the target served, and a JavaScript reader parses a bundle the
   target wrote. That is the least appealing place in the system to run a
   program whose release cadence, parser, and unsafe code are somebody else's.
   A CVE in it is our incident, in a process holding the session's cookie jar.
2. **A borrowed tool brings its own HTTP client.** Its requests miss
   `policy.rs`, spend nothing from `budget.rs`, and appear in no receipt, which
   is the one rule in N21 that is not a preference. The two failures compound:
   a program we did not write both parses hostile input and reaches the network
   outside the boundary the product is about.
3. **A shell-out is an unversioned dependency on whatever is on `PATH`.** It is
   not in `Cargo.lock`, not in a lockfile audit, and not reproducible for the
   reviewer reading a run six months later.

The same instinct, pointed the other way, is already an owner ruling: no
vendored engine crates, ever (ROADMAP B4). Dependencies are versioned crates we
resolve and audit, not copies in the tree and not binaries on the host.

**Reuse inside the process is different and is encouraged.** The JavaScript
pass is boa, which is already linked. Link extraction reads the DOM the engine
already built. Clustering uses W12's diff. Reimplementing those would be the
same mistake in the opposite direction.

**Import stays, and is a file, not an execution.** N17 takes JSON the operator
produced with whatever they like. h5i does not spawn the tool, does not fetch
its output over the network, and reads the file as untrusted input with a size
cap and a streaming parse, exactly as it treats a snapshot. Every imported row
lands as a `candidate`, so nothing an outside tool said is believed until an
h5i request observes it. That is the property that makes the import safe: it
is testimony, and the ledger already has a state for testimony.

**If a helper lane is ever genuinely needed**, it takes the shape the two that
exist already take. `h5i browser transcript --via yt-dlp` is feature-gated,
named at the call site, and recorded as an outside program run deliberately;
the microvm tier shells out to `msb` and says so. Both are opt-in, both are
visible in the record, and neither is in the default path. A recon helper would
have to clear the same bar, and none is planned.

## N20. What this changes in the websec design

`design-websec.md`'s "What is deliberately not built" refuses "a scanner. No
crawl-and-flag mode" and "payload generation. No SQL injection strings, no XSS
vectors, no wordlists". Both lines need amending when phase 1 lands, and the
amendment is narrow:

- Crawling exists, in recon, and it flags nothing. The refusal was of
  crawl-*and-flag*, and the second half is what h5i still refuses.
- Wordlists are accepted as input and never shipped. h5i generates no payloads
  and carries no corpus.

Without that amendment the next reviewer refuses N9 and N10 by the book, and
they would be right to.

## N21. What is deliberately not built

- **Verdicts and severities.** No finding is produced by recon. `confirmed`
  means distinguishable from the not-found baseline, not interesting.
- **Bundled wordlists, payload corpora or fingerprint databases with no
  provenance.** Every guess names its evidence.
- **A hosted service of any kind.** Nothing about a target leaves the machine.
- **Execution of third-party security tools.** No spawning katana, ffuf,
  feroxbuster, subfinder or naabu, and no wrapper that pretends the output is
  h5i's own observation. N19.
- **Crawling outside the session policy.** There is no `--all-origins`.
- **An agent planner.** Recon does not decide what to crawl next beyond its
  frontier rules. The loop belongs to whatever is driving h5i.
- **A second HTTP client**, in the plugin or anywhere else. This is the one that
  is not a preference. It is the product's claim.

## Decided, and what is still open

**Recon is its own plugin.** Owner ruling, 2026-09-07: `h5i recon` arrives the
way `h5i websec` does, with `h5i plugin install recon`, and is not in the
default build. `crates/h5i-recon` is the binary and the library it links;
`src/cli/plugin.rs` knows the name whether or not it is installed, so `h5i
recon` on a plain build says what the capability is and how to get it rather
than "unknown command".

**The types crate exists.** W21 recorded that the store's types lived in
`h5i-browser`, so a plugin reading them would link Blitz, Stylo and Boa. They
are in `crates/h5i-wire` now: the receipt row, the stored message and the name
of the file each phase is written to, re-exported from the engine so no caller
changed. That is what lets the recon plugin read a ledger and a message store
in 2.4 MB. It also unblocks the step W21 called next for the workbench.

Still open:

1. ~~**Where the ledger's scope ends.**~~ Answered 2026-09-07: `recon merge
   --from <session>` folds another session's ledger in, and `recon export`
   writes the inventory out as JSONL. Identity stays in the key, so what each
   session saw stays apart, and carried evidence is qualified with the session
   that made it.
2. **Phase 1 is built.** What is left of N12 is a per-host rate rather than a
   per-run one, which only matters once a run reaches more than one host.
3. **Section prefix.** `N` here, and now cited by code. Moving it costs a
   sweep.
