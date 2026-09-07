# Design: the HTTP workbench, sections W1 to W20

Status: proposed, 2026-09-02. This design turns h5i's HTTP records into an
agent-callable workbench. Sections marked "built" describe later progress.

## In one screen

> Burp Suite is an HTTP workbench for humans. h5i is an HTTP workbench for
> agents.

- The engine is the HTTP client, so it needs no MITM proxy, CA certificate,
  proxy setting, or manual history-to-replay handoff.
- h5i owns the deterministic half: exact capture,
  stable ids, session and cookie state, structured edit, resend, structural
  diff, timing, scope, rate limit, audit. The agent owns the judgement half:
  which request matters, which parameter to bend, what a difference means, what
  to try next.
- Scanning, payload generation, wordlists, and vulnerability verdicts remain
  the agent's responsibility.
- The CLI is Bash-first and designed for a later thin Python wrapper (W10).
- **Not in the default binary.** `h5i websec` arrives with
  `h5i plugin install websec` and is a separate executable that holds no
  privilege of its own. W21 is the packaging design.
- The message store (W5) remains separate because receipts are export-safe but
  workbench data may contain bodies and credentials.

Part of the h5i design set. The roadmap, and what is next, is
[`ROADMAP.md`](../../ROADMAP.md); the engine it builds on is
[`design-browser.md`](design-browser.md).

---

## W1. What is being claimed

> For a session run with capture on, every HTTP message the engine sent or
> received has a stable id, and `h5i websec replay <id>` re-sends that exact
> message, with the edits named on the command line and nothing else changed,
> through the same policy, the same cookie jar and the same identity that
> produced it.

h5i does not find vulnerabilities or promise TLS-level byte identity. Connection
reuse, client header ordering, and current cookies may differ. It reports replay
changes explicitly so callers can explain divergent results.

## W2. Why this belongs in h5i and not in a proxy

Because Burp does not own the browser, it requires interception, proxy setup,
and often a CA certificate. It also cannot directly know why a request occurred.

h5i already funnels requests through one `Fetch` in
`crates/h5i-browser/src/broker.rs`, while
`crates/h5i-browser/src/receipt.rs` already records the decision before the wire
and the outcome after it. Three things follow that a proxy cannot have:

1. **Provenance.** The receipt carries an `Initiator` (`navigation`,
   `subresource`, `frame`, `redirect`, `replay`), and the action log beside it
   carries the verb the agent asked for. `replay` is the workbench's own: a
   reviewer separating what the application did from what the tester did is
   asking that field, and a resend recorded as a navigation said the browser
   had gone somewhere it never went. A proxy sees a GET; h5i knows it was the third
   redirect hop of a click on "Export".
2. **No interception gap.** There is nothing to bypass. A request that is not in
   the log did not happen, which is the guarantee the whole product already
   leads with.
3. **Existing state.** Replay reuses the session's jar, identity, policy, and
   budget.

## W3. What already exists

The workbench is mostly a matter of exposing machinery that is shipped.

| piece | where | what it gives |
|---|---|---|
| named sessions, cookie jars, `--restore` | `src/cli/browser.rs` | multiple logged-in identities on one machine (feature 9) |
| request log | `crates/h5i-browser/src/receipt.rs` | seq, time, method, url, initiator, allowed, status, decoded and wire bytes, duration, cookie counts (part of features 1, 2) |
| action log and `h5i browser audit` | `receipt.rs`, `src/cli/browser.rs` | one ordered timeline of verbs and fetches, lane-labelled (feature 3) |
| single fetch entry point | `broker.rs` | one place to add capture and one place to police a replay |
| policy, budget, CORS, identity | `policy.rs`, `budget.rs`, `cors.rs`, `identity.rs` | scope and rate limiting already exist and already refuse (W16) |
| `extract`, `snapshot`, `markdown`, `find` | `extract.rs`, `snapshot.rs`, `markdown.rs` | response reading primitives to reuse rather than reinvent (feature 14) |
| `session script --save` and `browser replay` | `replay.rs` | the *page-level* replay layer, which W11 keeps distinct from the HTTP-level one |

## W4. What is missing, precisely

1. ~~**Bodies and headers are not stored.**~~ Built 2026-09-02; see W5. The
   engine now records both directions of every hop when a session is opened
   with `--capture`, including the hops a redirect chain passes through and the
   header set as the client built it rather than as the caller asked for it.
2. **No stable message id.** `seq` is per session and per process. Feature 2
   needs an id that survives a session restart and names a request in a
   filename, a diff and a finding.
3. ~~**No way to send an arbitrary request.**~~ Built 2026-09-02. `Fetch`
   carries the caller's headers (W16's rules enforced: the three framing
   headers are refused and named in the receipt, credentials stop at an origin
   boundary), `crates/h5i-browser/src/edits.rs` is the edit language, and the
   `resend` session verb applies edits to a stored request and sends it through
   the same broker, policy, budget and receipt path as everything else. The
   edits are applied *in the broker process*, so the renderer never holds the
   credential a stored request carries.
4. **No timing detail, parallelism, site map, sequence engine, DOM taint or
   OAST.** Features 11, 12, 15 to 20. Match and extract (feature 14) were built
   2026-09-03: `h5i websec match` takes regex, substring, JSON path, header,
   status and length conditions, ANDs them, hands back what each captured, and
   keeps three answers apart in its exit code (matched, did not match, could not
   look). Comparison (feature 13) and the raw and
   typed views (feature 5) were built 2026-09-03 in `src/cli/websec.rs`, reached
   by `h5i websec show` and `h5i websec diff`. Both read the store from
   disk on h5i's side rather than through a session verb: a verb's reply travels
   out through the renderer, and asking the untrusted parser to relay a stored
   `Authorization` header would undo on request exactly what the broker split
   is for.

## W5. The message store, and the credential problem

Receipts store cookie counts so they remain safe to share. A workbench needs
exact bytes and headers, including credentials, so the artifacts stay separate.

- **Receipts** stay exactly as they are. Append-only JSONL, safe to paste,
  shipped in exports, fail-closed (no writable log, no fetch). Nothing in this
  design adds a value to them.
- **The message store** is new, opt-in, and separate. `h5i browser open
  --capture` (off by default) writes each request and response into
  `<session-dir>/messages/`, mode 0600, as a header sidecar plus a
  content-addressed body blob so that fifty identical 2 MB responses cost one.
  It is never included in an export, a share or a bug report unless the caller
  names it, and `h5i browser close` can drop it with `--capture-drop`.

The format enforces an 8 MiB per-message cap, a 512 MiB session cap, and a MIME
skip list for fonts and media. Truncated or skipped bodies have explicit states.
The session cap counts the header sidecars as well as the bodies: a message
file is not small, since the raw sender accepts a response head as large as the
whole response cap, and counting only bodies left the directory unbounded in
the one case the cap exists for.

Built, 2026-09-02: `crates/h5i-browser/src/capture.rs`, reached by
`h5i browser open --capture`. Messages land in `<session>/messages/` as one JSON
file per phase per receipt sequence number, with bodies content-addressed under
`bodies/<sha256>` so a loop that gets the same answer a hundred times stores it
once. Two deviations from the paragraph above, both deliberate. The store
*refuses* rather than evicts when it fills, recording `store-full` against the
body, because eviction wants the pin that `websec pin` has not built yet and
dropping the oldest evidence silently is worse than refusing the newest loudly.
And writing is best-effort where the receipt is fail-closed: a fetch is never
refused because the store could not be written, since the receipt still records
that the request happened. Failures are counted and reported.

Benchmarking added a lossy preview beside non-UTF-8 bodies' authoritative length
and digest, allowing `match` and `diff` to inspect mixed content. `h5i browser
message --body-to PATH` writes the exact bytes.

All store content is untrusted target input, like snapshots.

## W6. Ids

`req_<n>` and `res_<n>`, where `<n>` is the receipt `seq`, scoped to a session
and stable for its life. Fully qualified as `<session>/req_42` when a command
spans sessions, which cross-session replay and diff both do.

The qualified form is written but not yet parsed: `websec show` and
`websec show` takes a bare id plus `--session`, and recon's `merge` records
cross-session evidence as `<session>/req_n` and says so. Accepting the
qualified form wherever an id is taken is a small piece of work and is not
done.

A request and its response share a number because they are two phases of one
receipt row, which is already how `RequestRecord` models them. `res_42` is the
response to `req_42` and no other numbering is introduced.

## W7. The command surface

One noun, `websec`, under the existing `h5i` binary, with the engine-side work
reached over the control channel a resident session already listens on. Every
verb takes `--session`, defaults to the default session, and takes `--json`.

```
h5i websec requests   [--filter EXPR] [--since SEQ] [--limit N]
h5i websec show       <req_42> [--raw | --json] [--part request|response|both]
h5i websec replay     <req_42> [--set K=V]... [--edits-file PATH|-] [--as SESSION]
h5i websec diff       <res_42> <res_43> [--body-mode auto|json|dom|text|none]
h5i websec match      <res_43> [--regex RE] [--jsonpath P] [--css SEL] [--header H]
h5i websec sitemap    [--origin ORIGIN]
h5i websec sequence   run <file.json> [--var K=V]...
h5i websec finding    create --from req_42,res_43 [--note TEXT]
h5i websec rpc        --stdio
```

`replay` always creates a new receipt and a new pair of ids, so a replay is
itself replayable and a chain of five attempts is five rows in the audit
timeline. There is no hidden buffer that the next command implicitly acts on:
every verb names its inputs, because an agent's turns are not a session in the
shell sense and "the last response" is exactly the state that breaks when two
agents share a box.

## W8. The edit language

Structured edits, not string surgery, because the point of owning the client is
that nobody has to hand-maintain `Content-Length`.

```
--set path=/api/v2/users/456
--set query.user_id=456
--set header.X-Forwarded-For=127.0.0.1
--set cookie.session=<value>
--set json.$.user.role=admin
--set form.username=admin
--set body.raw=@payload.bin
--set multipart.avatar.filename=../../etc/passwd
--set multipart.avatar.content_type=image/png
--unset header.Origin
```

Rules that have to be settled once:

- **Order is applied, then recomputed.** Edits apply in the order given, then
  `Content-Length` and multipart boundaries are recomputed. `Host` follows the
  URL unless explicitly set, because overriding it is a real test.
- **Nothing is silently corrected.** A JSON edit against a body that is not JSON
  is an error naming the actual content type, not a coerced string. An edit
  whose path does not exist is an error unless `--set-create` is given, since a
  typo that silently no-ops is a wrong answer that looks right.
- **An edit changes what it names and nothing else.** A `query.`, `form.` or
  `cookie.` edit rewrites one piece of the string it lives in and leaves the
  other pieces byte for byte, rather than decoding the whole thing and encoding
  it again: `?debug` is not `?debug=`, `x[]=1` is not `x%5B%5D=1`, and a `Cookie`
  header carrying a bare token still carries it afterwards. A `json.` edit is
  the one that cannot manage that, because a structured JSON edit reserialises
  the document; field order survives (`serde_json` is built with
  `preserve_order` for exactly this) but whitespace does not, so a body whose
  exact bytes are signed wants `body.raw` rather than `json.`.
- **Raw is available and honest.** `--edits-file` accepts a `raw` key holding a
  whole request, in which case h5i sends the bytes given and reports which of
  its own invariants it had to break to do so.
- **Binary is a path or base64, never a shell argument.** Built 2026-09-03 as
  a separate flag rather than an `@` prefix: `--set-file TARGET=PATH`. The
  prefix form makes a value that *begins* with `@` ambiguous, and a payload
  that begins with `@` is not unusual. The file's bytes go through unaltered —
  a magic-number check wants a real `ff d8`, which is not text in any encoding
  and so cannot survive argv or the JSON control message. `--set-file` edits
  apply after every `--set`, in the order given.
- **Multipart is parsed, edited and rebuilt, never patched as text.** Built
  2026-09-03 (`crates/h5i-browser/src/multipart.rs`). `multipart.<field>`,
  `.filename` and `.content_type` are three separate targets because a server
  checks them separately: the filter reads the declared type and the store uses
  the filename. With `--set-create` an upload is built from nothing, which is
  the usual case rather than the exception, since this engine never posts a file
  itself and so records no upload to start from. The boundary is always the
  engine's own: re-using the incoming one lets a caller who puts that string in
  a part's data split the message and send something other than what was asked
  for.
- **`--edits-file -` reads stdin.** This exists from day one, because shell
  quoting is where a Bash-driven agent loop actually breaks:

```bash
jq -n --arg p "$PAYLOAD" '{set: {"query.username": $p}}' |
  h5i websec replay req_42 --edits-file - --json
```

The reply echoes the edits as applied, so the log of an exploit is the log of
what was sent rather than of what was requested.

## W9. The JSON contract

This section is what makes W10 cheap later, so it is a phase A requirement, not
a polish item.

- `--json` on every verb, including errors. An error is
  `{"error": {"code": "...", "message": "...", "hint": "..."}}` on stdout with a
  nonzero exit, never a bare string on stderr.
- Exit codes are stable and few, and they follow what the tools around them
  already use rather than a scheme invented here. `match` is a grep, so it uses
  grep's: 0 matched, 1 did not match, 2 could not look (a pattern that does not
  compile, a body that was never stored). A verb that cannot reach its session
  exits 69, which is what `browser_session::EXIT_SESSION_GONE` has always been.
  The rule that matters is that "no" and "could not ask" are never the same
  code.
- stdout carries machine-readable results only. Progress, timing chatter and
  warnings go to stderr.
- Every payload carries `"schema": "websec/1"`. Fields are added, never
  repurposed; a removal is a new major.
- Time is RFC3339 with microseconds, matching `receipt.rs`.

## W10. Bash now, RPC later, Python last

Finish the CLI and JSON, validate them with Bash, then add a client only where
loops prove costly.

Bash covers more than expected:

```bash
for id in $(seq 100 200); do
  h5i websec replay req_42 --set "query.user_id=$id" --json |
    jq -c '{id: .request.edits, status: .response.status, len: .response.bytes}'
done
```

It stops covering it at adaptive payloads, blind extraction character by
character, timing statistics, race windows, multipart and crypto, and anything
past a few hundred replays where process startup dominates.

Measured, 2026-09-03, on XBEN-037-24: recovering a 64-character flag through a
one-bit oracle is 256 probes, and each probe is two `h5i` processes (send, then
ask about the answer). That run takes 23 seconds, of which almost all is process
startup; the requests themselves are 45 ms each. It works, and it is the
clearest argument yet for the RPC below. Two other things that run cost: a
payload that made the target's own `ping` wait three seconds turned the same
extraction into ten minutes, and the page's network budget ran out partway
through, which is why `resend --reset-budget` exists. The answer to the
last one is not a Python library, it is a long-lived process:

```
h5i websec rpc --stdio
{"id":1,"method":"replay","request":"req_42","set":{"query.id":123}}
{"id":2,"method":"replay","request":"req_42","set":{"query.id":124}}
```

Built 2026-09-07 as `h5i browser rpc --stdio`, engine-side, speaking `ping` and
`resend`: one JSON object per line in, one per line out, ids matched, every
request through the same `ask_session` a typed verb uses, so the control lock,
the policy and the receipts see no difference. Measured on 200 path probes
against a local target: 6.1s through the loop against 10.1s spawning `h5i` per
request. Less than the startup cost alone predicts, because each call still
resolves the session and opens the control channel; holding that connection
open across a run is the next saving and is not built.

One JSONL request per line, one reply per line, ids for correlation, and the
same schema as `--json`. A Python client is then roughly 150 lines wrapping that
pipe, with `requests()`, `inspect()`, `replay()`, `compare()` and nothing else.
No `scan_sqli()`, no `find_idor()`, no `exploit_ssrf()`: the moment security
logic enters the client, h5i owns a payload database and the product changes
shape. Keeping the RPC language-neutral also means Node, Rust or Bash get the
same surface for free.

## W11. Two replay layers, and why both

h5i will have `h5i browser replay` (a recorded *page* script: click, type,
submit) and `h5i websec replay` (a recorded *HTTP message*). They are not
redundant and neither can be built from the other.

The page layer reproduces a human path through an application, including
JavaScript that computes a nonce. The HTTP layer reproduces one message with a
field bent, which is what a test needs and what the page layer cannot express.
The workflow uses both: drive the browser to reach state, then work at the HTTP
layer from the requests that state produced.

`h5i websec sequence` is the join. A sequence is an ordered list of steps, each
either a page verb or an HTTP replay, with `extract` bindings between them
(feature 11, feature 12):

```json
{
  "steps": [
    {"replay": "req_login", "extract": {"csrf": "jsonpath:$.csrf"}},
    {"replay": "req_update",
     "set": {"header.X-CSRF-Token": "${csrf}", "json.$.role": "admin"}}
  ]
}
```

Bindings are named, typed by their extractor prefix (`jsonpath:`, `regex:`,
`css:`, `header:`, `cookie:`), and a binding that fails stops the sequence by
default, because a step acting on a token the previous step failed to produce is
acting somewhere the sequence never described. That is the same rule
`browser replay` already applies to its steps.

### Cross-session replay, and what `--as` means

Built 2026-09-03. `h5i browser resend <seq> --as <session>` reads the message
out of one session's store and sends it from another, and the meaning of that
had to be settled before it could be built.

"Send Alice's request as Bob" means *Bob's session makes it*: Bob's cookies,
Bob's identity, Bob's policy, Bob's receipts. So the source session's `Cookie`,
`Authorization` and `Proxy-Authorization` are dropped on the way across, and the
receiving session's jar supplies its own. Carrying Alice's cookie along would
send a request that is neither Alice's (it went through Bob's session) nor
Bob's (it carried Alice's credential), and the answer would settle nothing.
`--keep-credentials` sends them anyway, for the test where that *is* the
question, and a caller who wants one exact header can read it with `message` and
set it with `--set`, which is a deliberate act rather than a default.

The dropped names are reported, because a silent strip is a different request
than the caller thinks they sent. In the reply as `credentials_dropped` and
not only on the terminal: `--json` is how an agent reads this verb, and a
note printed in the human view is a note the one reader who needs it cannot
see.

### Stopping at a redirect

Built 2026-09-03. `resend --no-follow` sets this request's hop limit to zero:
the engine reports the 302 with its headers instead of following it. That is
where an authentication flow names you, where an open redirect proves it accepts
anything, and where the `Set-Cookie` that logs you in rides.

Two rules. Stopping where the caller asked to stop is not an error, so the
outcome carries a status and no error; only running out of hops somebody wanted
is a failure. And the per-request limit can lower the session's policy limit,
never raise it: a request able to raise its own ceiling would be a request
setting its own policy.

## W12. Structural diff

`h5i websec diff res_42 res_43` is Comparer for programs. The reply has three
layers so an agent can branch cheaply and read expensively only when it must:

1. **Verdict fields**: `same`, `status_changed`, `length_delta`,
   `time_delta_ms`, `similarity` (0.0 to 1.0).
2. **Header diff**: added, removed, changed, with a stable order and cookie
   values redacted by default (`--reveal-cookies` to override, since the whole
   point of some tests is the cookie).
3. **Body diff**, typed by content type: JSON gets a structural diff keyed by
   JSON pointer; HTML is diffed as a DOM shape through the machinery
   `snapshot.rs` and the read IR already have, so a re-ordered ad slot does not
   read as a difference; anything else is line-based. `--body-mode` forces it.

The `similarity` number matters more than it looks: boolean-based blind
injection is a loop over "is this response the true page or the false page", and
a single float an agent can threshold is the difference between a five-line loop
and a model reading two HTML pages per candidate character.

## W13. Match and extract on responses

Intruder's grep-match and grep-extract, without Intruder. `h5i websec match`
takes any number of conditions (`--regex`, `--jsonpath`, `--css`, `--header`,
`--status`, `--length-gt`, `--time-gt`), returns each one's boolean and its
captures, and sets exit code 5 when the combined expression is false. That makes
the common CTF loop a shell conditional with no `jq` in it, while the same call
with `--json` gives a script the captures.

This is an executor for someone else's conditions. h5i ships no conditions of
its own.

## W14. Timing

`duration_ms` on the receipt is a page-load number. Blind injection needs a
better one, so replay reports the clock per send, plus `--repeat N` returning
median and median absolute deviation rather than a mean that one scheduling
hiccup ruins.

The honest caveat belongs in the reply, not in a footnote: a session inside a
box pays a proxy hop and a namespace, so absolute latency is not the host's.
Comparisons within one session are sound, comparisons across placements are not,
and the reply says so.

Built 2026-09-03, with one honest narrowing. Two numbers are reported, not four:
`ttfb_ms` (to the response's headers, which is the server's *decision*) and
`total_ms` (to the body in hand). `connect_ms` and `tls_ms` would need a custom
connector under `reqwest`, and inventing them from a wall clock around the call
would be a guess wearing a measurement's name. The split that was actually
needed is the one that shipped: a server that decides slowly and streams a large
body answers late with a fast total, so collapsing the two would hide a
time-based injection under the size of the page.

`--repeat` runs inside the broker rather than in a shell loop, because the thing
being measured is milliseconds and starting a process costs tens of them: a loop
outside the engine measures the loop. Each send is a receipt of its own, so a
repeated replay is as auditable as a single one. The count is clamped rather
than trusted, since a caller asking for a million sends is asking the session to
spend its whole budget on one verb.

## W15. Parallel replay

Built 2026-09-03 as `--repeat N --race`: N threads that meet at a barrier and
then send. The barrier is the point. Without it the first thread is already
waiting on the network before the last has been spawned, and a check-then-act
window measured in milliseconds closes in between.

Named a burst rather than a single-packet attack, because that is what it is:
the requests leave within the cost of waking a thread, not with their last bytes
held back and released together. Ordinary check-then-act windows do not need
that, and claiming the stronger thing would be claiming a capability the code
does not have. `scripts/websec/smoke.sh` redeems a one-use coupon twelve times
out of twenty against a server that sleeps between its check and its act.

A panicked sender fails the whole call rather than being skipped: a burst that
quietly sent fewer requests than it was asked for would make a race that did not
reproduce look like a race that does not exist.

Two things have to be right:

- It composes with `budget.rs`. A parallel replay spends from the same
  `Limits`, so a runaway loop hits `max_requests` exactly as a runaway page
  does. Rate limiting is a first-class flag (`--rate R`) because a CTF target
  and an authorised engagement both have someone who will notice.
- It is not a fuzzer. The payloads come from the caller, one per line on stdin
  or as a list in an edits file. h5i sends them efficiently, records each, and
  returns a table. It never generates one.

## W16. Scope, policy and authorisation

Replay does not get a new path to the network. It goes through `Broker::fetch`
like everything else, which means the session's policy decides, the receipt is
written first, and an off-scope replay is refused with a reason rather than
sent. A replay cannot widen its session's allowlist; changing scope means a new
session with a new policy, which is a visible act.

Adding a header map to `Fetch` (W4) is the one place this could go wrong. The
rule: headers are carried, recorded and sent, and the policy checks the request
by URL as it does now, with the header set included in the recorded decision.
Hop-by-hop headers the client owns (`Content-Length`, `Transfer-Encoding`,
`Connection`) are recomputed, and an attempt to set them is reported as
overridden rather than accepted silently, since request smuggling is a real test
and pretending to support it is worse than declining.

The audit posture is the product's, not an afterthought: every payload an agent
sent is in the timeline with the verb that sent it, so a session is reproducible
by someone who was not there. That is what makes this usable in an engagement
rather than only in a CTF.

## W17. Site map

Fold the request log into a tree by origin, then path, carrying methods seen,
status codes seen, parameter names observed, content types, and whether each
node was reached by navigation or by script (feature 18). Add endpoint
candidates the pages themselves disclose: form actions, `fetch` targets found in
loaded JavaScript, `link` and `script` sources, WebSocket URLs from
`wsclient.rs`, and Structured metadata from `structured.rs`.

Candidates are labelled as candidates. A URL scraped from a bundle was not
visited, and the map must not blur the two, because "what did this session
reach" is the question the receipts exist to answer.

Built 2026-09-03, with deliberately narrower scope:
`h5i websec sitemap` folds the *receipts* into origins and endpoints, with
methods, statuses, parameter names, hit counts, a mark for what was navigated to
rather than pulled in, and the refused URLs listed apart. The disclosed-but-
unvisited half is not built. The verb reports only observed endpoints; bundle
scraping belongs in a separately named command.

## W18. DOM instrumentation

The DOM Invader analogue, and the first feature that is a plugin rather than
core. With `--script` on, instrument the sources (`location`, `name`,
`document.referrer`, `postMessage` data, storage) and the sinks (`innerHTML`,
`eval`, `document.write`, `setAttribute` on URL attributes, `Function`), and
report reachability from source to sink with the path taken. Add a
`postMessage` log and a prototype pollution probe.

It sits last but one because it is engine-invasive, it only pays off on
client-side problems, and everything before it pays off on every problem.

## W19. OAST

Out-of-band detection needs a callback receiver, which means a service, which
means an operating cost and a privacy story. The design here stops short of
running one:

- A neutral interface: `h5i websec oast token` mints a correlation id and
  returns a hostname and URL to embed; `h5i websec oast poll <token>` returns
  the interactions seen, with type, source address and time.
- Backends are pluggable and none ships hosted. Bring your own domain, your own
  webhook endpoint, or a self-hosted receiver; a lab backend that binds a local
  listener covers CTF targets that can reach the host.
- Nothing is sent to an h5i-operated service, because a payload URL in a target
  application is data about someone else's system.

## W20. Order, and what each phase buys

The twenty features, ranked, with the phase that carries them.

| # | feature | Burp analogue | phase |
|---|---|---|---|
| 1 | full request and response capture | Proxy history, Logger | A, built |
| 2 | stable message id | Proxy history | A, partly built |
| 3 | action to request provenance | Logger, site map | A, partly built |
| 4 | history search and filter | Proxy history | A, built |
| 5 | raw and typed views | Message editor, Inspector | A, built |
| 6 | same-session replay | Repeater | A, built |
| 7 | structured request edit | Repeater, Inspector | A, built |
| 9 | multiple sessions and jars | browser sessions | A, built |
| 13 | machine-readable response diff | Comparer | A, built |
| 14 | match and extract primitives | Intruder grep | A, built |
| 8 | multipart and upload editing | Repeater | B, built |
| 10 | replay as another session | Repeater plus session rules | B, built |
| 11 | extract and bind | macros, session rules | B, built |
| 12 | multi-request sequences | Repeater sequences | B, built |
| 15 | precise timing | Repeater, Intruder | B, built |
| 16 | controlled parallel replay | Intruder, race testing | B, built |
| 17 | redirect chain observation and control | Proxy, Repeater | B, built |
| 18 | site map and inventory | Target site map | B, built |
| 19 | DOM instrumentation | DOM Invader | C |
| 20 | OAST callbacks | Collaborator | C |

**Phase A, the workbench.** Features 1 to 7, 9, 13, 14, plus W9's JSON contract
and the plugin packaging of W21. The engine work is the message store, the
header field on `Fetch`, the replay path and the diff. This alone covers IDOR, authentication bypass, basic
injection, reflected XSS, SSRF against a named target, path traversal, header
and cookie manipulation, and a useful slice of business logic. The acceptance
test is a person driving an agent through easy and medium CTF web problems with
Bash and no Python.

**Phase B, the harder half.** Features 8, 10, 11, 12, 15 to 18. This is what
CSRF-protected multi-step flows, blind and time-based injection, race
conditions, upload chains, OAuth and session problems, and SSRF pivots need. The
RPC of W10 lands here, driven by whichever loop first proves too slow, and the
Python client lands after that, extracted from scripts that already exist rather
than designed in advance.

**Phase C, the long tail.** Features 19 and 20. Client-side and blind classes.

**The benchmark.** Each phase's claim is a measured one: a corpus of web CTF
problems, run by an agent with only these verbs, scored on solved and on how
many turns and requests it took. A feature that does not move that number is not
finished, whatever its tests say. The corpus belongs in `docs/benchmarks/`
beside the environment ones, with the same rule that the harness is committed.

The first piece of it exists: `scripts/websec/smoke.sh` drives every verb end to
end against `scripts/websec/server.py`, and it is committed because it keeps
earning its place. Four bugs so far were invisible to the unit suite and obvious
on the first real run: a verb missing from `Verb::ALL` (wired everywhere, refused
at the session), a duplicated `accept-encoding` on every replay, two exit codes
collapsed into one, and a `null` read as a composed request, which broke plain
`resend` between two commits. Unit tests cover the pieces. This covers the seams
between the CLI, the control channel, the engine and the store, which is where
every one of those lived.

## W21. Packaging: a plugin, not a part of the binary

`h5i websec` is not in the default build and is not a cargo feature someone
turns on at compile time. It is installed, after the fact, into an h5i that is
already on the machine:

```
h5i plugin install websec
h5i plugin list
h5i plugin remove websec
```

Nothing like this exists today. `h5i` has cargo features (`web`, `browser`,
`share`, `runner`, `ytdlp`, `identity`) and no runtime install path at all, so
W21 is the section that has to be built before phase A can ship the way this
file describes.

### What is built, 2026-09-03

`h5i plugin install|list|remove`, and `h5i websec` dispatching to an installed
`h5i-websec` (`src/cli/plugin.rs`, `crates/h5i-websec`). The release ships
`h5i-websec` as its own archive per target, so `--from` has a file to point at
without a source tree; `install` still takes a path rather than fetching one,
because a downloaded executable needs a provenance story this does not have. A name h5i knows but
does not have answers with what the capability is and how to install it, rather
than "unknown command". Installs are refused for names not on the known list,
because a plugin directory anything can add a name to is a directory where
`h5i <anything>` becomes an execution. Exit codes pass through both hops
unchanged, so `websec match` still exits 1 for a miss and 2 for "could not
look".

Two honest limits, both since removed (see below). The plugin composed `h5i
browser` verbs in a subprocess rather than reading the message store itself,
because the store's types lived in `h5i-browser` and a plugin depending on that
crate would link Blitz, Stylo and Boa into a second binary. And so the workbench
verbs were still in the default build, which left the posture argument above
undelivered: an install *did* include them.

### What moved, 2026-09-07

Both limits above are gone. `crates/h5i-wire` holds the store's types and its
readers, so the plugin reads the bytes itself, and the four reading verbs moved
into it: `show`, `diff`, `match` and `sitemap` are no longer in the default
build. `h5i browser message|diff|match|sitemap` do not exist; the capability
arrives with `h5i plugin install websec`, which is what the posture argument
above claimed and did not deliver.

What stays in the binary is what sends: `requests`, `resend`, `sequence` and
`rpc`. They need the engine's control channel, and a plugin reaching the
network by any path other than the engine's is the thing this design refuses.
The binary still reads the store for its own two cases, `resend --as` carrying
a request and `sequence` binding a value out of a response, through the same
`h5i-wire` readers rather than a second copy.

Recon depends on the same split: it sends through `h5i browser resend` and
`h5i browser rpc`, and reads through `h5i-wire` (`docs/design/design-recon.md`
N19).

### What a plugin is

A separate executable, discovered by name the way `git` finds its subcommands.
`h5i plugin install websec --from <path>` copies `h5i-websec` into a per-user
plugin directory, and `h5i websec <verb>` execs it with the arguments forwarded
and the environment that names the session. The release publishes the
executable; fetching it by name is not built. A build without the
plugin still knows the name: `h5i websec` prints what it is and how to install
it, rather than an unknown-subcommand error, so the feature is discoverable
without being present.

Not a dynamically loaded library, and the reason is the product's whole claim.
The h5i process is the one that resolves policy, writes the receipt before the
bytes move and refuses the fetch when the record cannot be written. Code
`dlopen`ed into that process sits *inside* the boundary it is supposed to be
subject to, and every guarantee in `ROADMAP.md` would then rest on the good
behaviour of whatever was installed last.

A plugin as a separate process is subject to the boundary instead. The websec
plugin reaches a session over the same control channel `h5i browser snapshot`
uses, and it has no other route to the network. Its replays are the engine's
fetches, checked by the engine's policy, spent from the engine's budget and
written into the engine's receipts. An audit of a session driven by a plugin
reads exactly like an audit of a session driven by hand, which is the property
worth protecting.

### What still has to live in the binary

A plugin cannot add a field to `Fetch` or write the message store, so phase A's
engine work (W5, and the header map of W4) ships in the engine regardless.
Correctly, it is *capability without behaviour*: capture is off unless a session
is opened with `--capture`, and a build that has never had the plugin installed
behaves exactly as it does today.

The two halves negotiate through the verb that already exists for this.
`h5i browser capabilities` reports what an engine can do, as JSON, so h5i can
route by capability rather than by version number. The plugin reads it, and an
engine too old to capture gets a named refusal naming the build, not a confusing
empty result.

### Why opt in

Dependency weight is the small reason. The real one is posture. A browser for
agents that ships an HTTP attack workbench in every install is a different
product to defend, to a security review and to a user who wanted a page reader.
Making the workbench a deliberate act by the operator keeps the default install
honest about what it is, and costs an authorised user one command.

It is worth being precise about what this does not do: it is not a security
control. Anyone who can install a plugin can install any tool. What it buys is
that the capability is *named* at install time and visible in `h5i plugin list`,
which is the same stance the rest of h5i takes toward capability, and none of
the enforcement in W16 depends on it.

## What is deliberately not built

Each of these is a decision to be defended in review, not a gap to be filled.

- **A scanner.** No crawl-and-flag mode, no severity ratings. Crawling itself
  moved out rather than staying refused: it belongs to recon
  ([`design-recon.md`](design-recon.md) N9), which discovers and never flags.
  The *and-flag* half is what this file still refuses, wherever the crawl lives.
- **Payload generation.** No SQL injection strings, no XSS vectors, no
  encoders beyond the ones an edit needs to be correct. Wordlists are input,
  never cargo: recon's path discovery (N10) takes `--wordlist PATH` and h5i
  ships no corpus, exactly as W15 takes payloads from the caller and generates
  none.
- **Vulnerability verdicts.** h5i reports differences. Calling one a
  vulnerability is the agent's claim, made in a finding it writes and signs.
- **An exploit database or plugin marketplace.**
- **An agent planner.** The loop belongs to whatever is driving h5i.

Parallel replay and match are not exceptions to this. Intruder is not a scanner
either: both are executors for conditions and payloads that arrive from outside.

## What benchmarking changed

**2026-09-03.** Phases A and B were tested against the 104-benchmark XBOW
validation corpus. The first pass solved 98. Raw request targets then enabled
XBEN-026 and XBEN-031, bringing the total to 100. The separate `h5i-benchmark`
repository contains the runner, solutions, and notes about the remaining cases.

The point of the exercise was not the score. It was to find the places where the
workbench could not say what a person needed to say, and each of those turned
into a change:

- **Bytes a command line cannot carry.** A magic-number upload filter wants a
  file that begins `ff d8`, which is not text in any encoding, and `--set`
  reaches the broker through argv and then through JSON. `--set-file
  TARGET=PATH` sends the file's bytes unaltered.
- **Bytes that came back and could not be read.** A response whose body was not
  valid UTF-8 was reported as a length and a digest, which hides the answer in
  exactly the case the store exists for: a secret echoed after a file
  signature. Bodies now carry a lossy reading too, and `match` and `diff` can
  see it. `show --body-to PATH` writes the exact bytes out, for the responses
  that are files rather than text.
- **Normalized requests.** `resend --raw-target` and `--raw-request` preserve
  request targets and bytes that URL parsing would change.

Two smaller things the corpus caught: `submit` refused a `<form>` selector with
"is a an element", and a replay's own sequence number was reported but not
obviously enough for a caller to read its answer back by name. The rest of what
the run needed was the corpus's own problem and stayed with it.

**2026-09-04.** A second corpus, the 71-benchmark Argus set, run over its first
sixty. Different shape from XBOW: current stacks rather than PHP, a randomised
flag per run, and a third of it not a single request at all but a scenario with
an admin bot, a mail server, a message broker and an object store. Fifty-four
solved. The same exercise, and six more changes:

- **A flag that did nothing.** `show --raw` was ignored unless `--human` was
  also given, which is the shape of silently doing something other than what
  was asked. It outranks the JSON envelope now: a wire message is bytes, and
  bytes inside a JSON string are no longer the message.
- **The plugin could not reach the raw send path.** `--raw-target` and
  `--raw-request` were on `browser resend` and not on `websec replay`, which is
  the surface the design leads with. Both are there now, with
  `--keep-credentials`.
- **A refusal that named a flag which does not exist.** Editing an absent
  parameter answered "Pass `--set-create`". The flag is `--create`.
- **Two raw features that did not compose.** The obvious way to build a file
  for `--raw-request` is to dump a request that was already sent, and the dump
  used bare LF. The request half is CRLF now.
- **A desync with nothing to show for it.** `--raw-request` shipped for request
  smuggling, and a successful smuggle produces *two* responses on one
  connection — the second being the smuggled request's answer. The engine read
  one and closed the socket, so the attack could work and report nothing. What
  follows a raw response is kept beside it now (`trailing` in the stored
  message, `trailing_bytes` in the receipt).
- **A protocol with no verb.** The engine has had a WebSocket client since it
  had a browser and only page JavaScript could reach it, so an application
  whose commands travel over a socket was one this workbench could watch
  connect and never speak to. `websec socket` is `resend` for that protocol.

And one defect characterised but not yet fixed: roughly one send in fifteen
that takes around two seconds costs about three seconds more than the request
did, and on one target `total_ms` came back as 587 for a request the server
cannot answer in under two thousand. A timing oracle is a first-class use of
this workbench and that corrupts one. The reproducer is a server that sleeps
two seconds and a loop of forty replays.

## Raw requests

**Added 2026-09-03 after benchmarking.** Normal requests use parsed `Url`s, which
normalize dot segments and percent encoding. For example,
`--set path=/cgi-bin/.%2e/.%2e/etc/passwd` sends `/etc/passwd`. `apply` rejects
such edits (`crates/h5i-browser/src/edits.rs`).

The raw send path bypasses `reqwest` so it can preserve those bytes. It shares
the socket transport in `crates/h5i-browser/src/rawsock.rs` with the WebSocket
client. `LocalBroker::send_raw` sends raw HTTP/1.1 requests.

`RawRequest` in `broker.rs` supports two forms:

- **Raw target.** `h5i browser resend <seq> --raw-target
  /cgi-bin/.%2e/.%2e/bin/sh` preserves the target but computes `Host` and
  `Content-Length`.
- **Raw request.** `h5i browser resend --raw-request <path>` preserves the full
  request, including framing headers. The receipt records broken framing
  invariants.

Both forms check policy, pin the address, claim the budget, and write a receipt
before dialing. Because raw sockets cannot use the egress proxy, proxied sessions
allow raw requests only to loopback, as the WebSocket client does.

**Now reachable:** `--raw-target` solves XBEN-026 (CVE-2021-42013) and XBEN-031
(CVE-2021-41773) end to end. Worked scripts are in `h5i-benchmark`.

**Still open:** XBEN-066 can now express arbitrary request framing, but its
mitmproxy 6.0.2, HAProxy 2.0.5, and Apache stack still needs a working desync
payload. The front proxy reparses requests and rewrites `Host`; this is now a
payload problem, not a missing h5i capability.

## Open questions

1. **Capture default.** Off is the safe default and also the one that makes a
   fresh session useless for this workflow. A middle option is capture on for
   sessions created by `h5i websec` verbs and off for `h5i browser open`.
2. **Store lifetime across restarts.** Ids are stable per session; a session
   that is closed and reopened from a restored jar is arguably the same
   engagement and arguably not.
3. **Whether `websec` is a noun under `h5i` or a mode of `h5i browser`.** The
   verbs act on a browser session either way. `h5i websec` reads better in a
   script and keeps the browser's own verb list from doubling.
4. **How much of the diff belongs in the read IR.** The DOM-shape body mode
   wants the IR, and the IR was built for a different consumer.
5. **Whether plugins are signed, and how they are pinned.** A plugin speaks the
   `websec/1` schema against an engine that has to be new enough for it, so
   install needs a compatibility check, and a downloaded executable needs a
   provenance story that `install.sh` does not currently have.
