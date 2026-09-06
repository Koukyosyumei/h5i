# Design: the browser

> A pure-Rust browser that lives inside the agent's own sandbox, renders on
> demand, and can prove what it did.

The engine is `crates/h5i-browser`. This file covers both halves of its
design: what an agent sees when it drives the browser, and why the engine is
shaped the way it is. Sections B1 to B5 are cited by live code.

## In one screen

- The engine *is* the HTTP client, so the receipt is the network rather than an
  observation of it. No receipt, no request.
- Blitz owns the DOM, Stylo the CSS, vello_cpu the raster, Boa the script.
- A page is read through refs and a fenced snapshot, so page text reaches a
  model labelled as data.
- Credentials are usable and unreadable: the agent names one, the engine
  resolves it on the way to the field, and the reply echoes the placeholder.
- Web Platform Tests, core tier: 75.7%. A production React build is not cleared.
- No iframes, no vendored engine crates, no JIT.

## Contents

Driving the browser: [why this exists](#why-this-exists),
[measurements](#measurements), [usage](#usage) and the fifteen sections under
it, [composition](#composition), [status](#status).

Scope and architecture: [the session surface](#the-session-surface), then B1
where it stands, B2 architecture, B3 security, B4 what it is not, B5 the rule
that produced it.

Part of the h5i design set. The roadmap, and what is next, is
[`ROADMAP.md`](../../ROADMAP.md). The build log B1 to B22 and the superseded
positioning are in [`roadmap-history.md`](../roadmap-history.md).

---

## Why this exists

A browser engine for AI agents that read thousands of pages at a time, or that
open untrusted pages carrying prompt injections.

h5i's egress proxy sees `CONNECT docs.example.com:443` and nothing more. CDP's
Fetch domain can pause and record a request, but fails open: attach races, fresh
targets and workers, buffer limits and disconnects all leave gaps. Here the
engine *is* the HTTP client, so:

- No receipt, no request. The record is written before any bytes move, and a
  sink that refuses to record refuses to fetch.
- Every redirect hop is policy-checked, so an allowed origin cannot bounce to a
  denied one.
- No JIT. Too much machinery to keep free of exploitable bugs.

## Measurements

Same machine (aarch64, WSL2), median of 7 interleaved runs after a discarded
warm-up, on a local `file://` page. Memory is peak summed RSS across the process
tree at 5 ms intervals; `/usr/bin/time -v` reports only the largest single
process and badly undercounts a multi-process browser.

| | small (1 KB, no script) | docs (22 KB, no script) | app (script-built, 400 elements) |
| --- | --- | --- | --- |
| h5i-browser | **46 ms / 51.7 MB** | **59 ms / 65.6 MB** | 248 ms / **87.4 MB** |
| chromium `headless_shell` | 172 ms / 456.8 MB | 176 ms / 461.5 MB | **176 ms** / 464.4 MB |
| chromium (full) | 672 ms / 1150.7 MB | 758 ms / 1153.8 MB | 824 ms / 1150.6 MB |

Both engines produce identical output on the script page. Caveats:

1. Cold start is included and dominates Chromium's time. Not a steady-state
   throughput comparison.
2. `--script` costs nothing on a page with no script (46 ms against 45 ms), and
   44 ms -> 248 ms on the app page.
3. Rendering is software. Complex CSS narrows the gap.
4. Trust memory most: it follows from the architecture (one process, no
   renderer, no GPU process).
5. These replace a table claiming 5x faster and 15x lighter, from before this
   engine had JavaScript (updated 2026/8/31). Memory roughly doubled since (31
   MB -> 52-66 MB): Boa and a 281 KiB prelude. Date a measurement.

### The step after the first one

The table above is a cold read: launch, load, snapshot once, exit. It is the
right shape for "how heavy is this browser" and the wrong shape for what an
agent does, which is open a page once and then read it, act on it, and read it
again. Cold start dominates that table on both sides and is paid once.

`scripts/bench_agent_loop.py` measures the rest of the session, with the page
already open and the engine resident on both sides. Same machine, median of 10
steps across 3 repetitions after a discarded warm-up. Chromium is driven by
Playwright, which is a different build and a different driver from the
`headless_shell` row above, so read the two tables against themselves rather
than against each other.

| per step | h5i small | chromium small | h5i large | chromium large |
| --- | --- | --- | --- | --- |
| snapshot | 17.6 ms | **10.1 ms** | **18.3 ms** | 33.7 ms |
| snapshot --delta | 17.8 ms | not available | **19.3 ms** | not available |
| click | **19.7 ms** | 42.0 ms | **27.5 ms** | 40.4 ms |

Small is a six-line page; large fills the 500-line snapshot budget. What the
numbers say, including the half that does not flatter this engine:

1. **A verb costs about 7 ms before it reads anything.** h5i spends a process
   launch per command by design, and Playwright holds its connection open. On
   a small page that fixed cost is most of the difference, and Chromium reads
   faster. On a large page it stops mattering.
2. **Reading a large page is where the architecture shows.** 18.3 ms against
   33.7 ms, and the large page now costs about what the small one does, which
   is the more interesting half: a step's cost has stopped tracking the size of
   the page. CDP has no delta either, so an agent driving it re-reads the whole
   accessibility tree every step and pays for the page whether or not it
   moved.
3. **Acting is consistently cheaper here**, roughly half, because a click
   dispatches an event rather than running actionability checks over a
   protocol.
4. **The read path is not the bottleneck at this level, and neither is the
   part that looked like it.** Engine work per read is 9.0 ms small and
   10.5 ms large, against 0.18 ms for the walk and render the Read IR made
   faster (`docs/design/design-h5i-ir.md`). The large one was 22.3 ms before the two
   findings below, and the gap between the two pages has almost closed.

The first guess at where the rest goes was the durable CSS selector the
snapshot verb computes for every ref, each verified with a full-document
query. `benches/read.rs` now times that pass on its own, and the guess was
right about the mechanism and wrong about the size.

It is real and it is per-ref: 4.5 ms for 72 refs, against 0.13 ms to read the
whole page, and exactly 0.000 ms on the fixture that has no refs at all, which
is the control that rules out a per-page explanation. It also said which refs
cost what. A form control carries `name=`, resolves on its first try, and costs
0.016 ms; a link carried nothing, so it walked its ancestors composing a
candidate at each step, and cost 0.063 ms. Giving a link the one attribute it
does have took that page from 4.5 ms to 2.7 ms, and improved the handle while
it was there: `a[href="/pricing"]` says what the link is, where
`section:nth-of-type(37) p a` says where it sat this morning.

But 4.5 ms is a fifth of the 22.3 ms, not the whole of it, and the agent-loop
table above does not move when it is halved. So the arithmetic that pointed
here was pointing at something real and too small to be the answer.

The rest took a profile, after three wrong guesses had been spent on
arithmetic. `H5I_BROWSER_TIMING=1` makes a reply carry a breakdown of where
its own time went, and on a page with three hundred refs it said this, of a
31 ms round trip on the control socket:

| phase | |
| --- | --- |
| reading the page | 0.2 ms |
| every selector on it | 10.2 ms |
| action log, replay step, redaction | 0.8 ms |
| **writing the reply to the socket** | **19.9 ms** |

The reply was written with `writeln!(socket, "{value}")`, which looks like one
write and is not. A `serde_json::Value` renders through `Display`, emitting
each brace, key, comma and string fragment as its own call, and the socket
underneath is unbuffered, so each of those is a syscall. Thirty-five kilobytes
of reply is thousands of them. Rendering to a `String` first and writing once
took the session's own work from 28.8 ms to 15.8 ms, and the whole
`h5i browser snapshot` from 44.3 ms to 30.5 ms.

Two things are worth taking from that. The first is that the cost was not in
reading the page at all, which is where three guesses in a row had put it. The
second is why it stayed hidden: it scales with the size of the *reply*, and the
fixture that separated lines from refs showed output size costing nothing,
because a page with five hundred lines and no refs produces a reply with no
`refs` array in it. Two variables that usually move together, and the one that
mattered was the one the bisect had ruled out.

`H5I_BROWSER_TIMING=1` reports both halves now, the client's on stderr and the
session's in the reply, because the same question keeps being answered wrongly
by reading the code. The next candidate it retired was `scrub`, the client's
pass that walks every string in a reply and allocates one per value. That is
exactly the shape of the thing that had just been found on the other side, and
it measures at 0.3 ms.

What is left of a 30.5 ms `h5i browser snapshot` on a page with three hundred
refs: about 10 ms of selectors, about 5 ms of process launch before `main`
runs, and the rest spread across the socket exchange and the session lookup.
The selectors are near the floor of their design, and the launch is the price
of one process per verb, so the next real move on either is architectural
rather than a hot spot to find.

"Near the floor of its design" was the next thing measured and found wrong.
Every candidate is still verified against the real matcher, so a ref still
costs a query, but it had been the *wrong* query. `resolves_to` asks whether a
selector's first match is a particular element, and it was answered by
collecting every match and reading the first one. Stylo will stop at the first
if asked, and every selector this module produces on a real page resolves to
exactly one element, so the full walk was finding one match and then
confirming there were no others nobody had asked about.

Measured before the change rather than after, on the selectors the module
actually emits: 2.4x to 3.0x. Splitting the cache into a first-match map and an
all-match map, and leaving the ancestor walk on the full query because it
genuinely needs the count, delivered more than that, because the intermediate
candidates got faster too.

| selector pass | at the start | with `href` | with a first-match query |
| --- | --- | --- | --- |
| 72 refs (sections) | 4.50 ms | 2.70 ms | **0.82 ms** |
| 151 refs (forms) | 2.39 ms | 2.40 ms | **1.09 ms** |
| 300 refs (links) | | 9.81 ms | **4.09 ms** |

End to end on the 300-ref page, the session's own work went from 28.8 ms to
8.2 ms across the two changes in this section, and `h5i browser snapshot` from
44.3 ms to 22.8 ms. Most of what is left is now the client: about 5 ms of
process launch before `main` runs, and the socket exchange around it.

Going below *this* floor does mean not recomputing selectors for a document
that has not changed, which needs the retained tree and revision counter of
`docs/design/design-h5i-ir.md` phase 2.

## What it is not

The short version is below; B4 further down carries the full list of surfaces
that will never be built and what is simplified rather than absent.

- Not a Chromium replacement. Docs-grade pages are the compatibility bar; send
  React/Vite apps, video, WebGL and authenticated sessions down the Chromium
  path.
- JavaScript runs, and it is the slow half, so a script-driven page is the one
  case `headless_shell` can be *faster* on. Ask `capabilities`: the §B6 refusals
  are real, with no workers, no second browsing context, no media pipeline.
- Containment claims belong to the box. Bare on a host there is no egress proxy
  and no receipt store.

## Usage

This is the engine's own CLI, and it is what the rest of this document speaks.
The engine stopped being a separate binary at the pivot, so `h5i __engine` is
how it is invoked now: `h5i` execs itself and hands everything after the
subcommand through unchanged.

People and agents do not type that. The user-facing surface is `h5i browser`,
which spells most of these verbs without the `session` word (`h5i browser open`
makes the resident session, then `h5i browser snapshot`, `click`, `type` act on
it) and adds the placement, session and audit verbs the engine knows nothing
about. Four stay here with no `h5i browser` spelling: `serve`, which `open`
starts on your behalf, plus `replay`, `capabilities` and `doctor`.
`h5i browser --help` is that surface's reference.

```
h5i __engine open  <url|path>... [--allow ORIGIN]... [--screenshot PATH]
                                 [--receipts PATH] [--text] [--json]
h5i __engine serve <url|path> [--addr 127.0.0.1:0] [--stream-file PATH]
                              [--control-file PATH]
h5i __engine session status | snapshot | navigate <url> | scroll <px>
                    | type <@ref|--selector CSS> <text>
                    | submit <@ref|--selector CSS>
                    | click <@ref|--selector CSS>
                    | wait-for --selector <css> | wait-for-script <expr>
                    | requests [--since <seq>] | markdown | extract <schema>
                    | structured | script [--save PATH] | env
h5i __engine session snapshot|markdown|extract|structured [--url URL]
h5i __engine replay <script.json>        # a recording, run without a model
h5i __engine open|serve ... [--script]   # limited JavaScript preview
h5i __engine capabilities     # what this engine can do, as JSON
h5i __engine doctor           # fonts, proxy, allowlist, client
```

### Refs, and the reading they came from

A `@ref` names *a position in the snapshot that minted it*, not a durable
handle, and is honoured only against the reading it was served in. Without that
check, a page that moved between snapshot and click resolved `@e5` to a
different element and replied `ok`.

```
$ h5i __engine session click @e2
{"ok":false,"code":"stale-ref","retryable":true,
 "error":"`@e2` came from a snapshot this page has moved on from: it now names
          a button \"Add\". … Take a fresh `snapshot` and use its refs."}
```

It is an equality check on one ref, not a proof the document is unchanged: a
page that mutates something the walk does not record still passes. Typing and
scrolling renumber nothing, so a login loop needs no re-read between steps.

### The durable handle

`snapshot` reports a `refs` array beside the outline, whose selector survives
the reading `@e3` does not. That is what a recording replays into.

```json
{"id": "e3", "role": "button", "name": "Sign in", "selector": "#go"}
```

Built the way Lightpanda's is: the element's own segment, ancestors prepended
only when they shrink the match count, then a strict `a > b > c` chain as a
fallback. Ids are checked rather than trusted: duplicate ids are legal and
`#dup` names the first one. Every candidate is verified with the action verbs'
matcher; where nothing verifies, the field is `null`. Only `snapshot` computes
selectors: a tree walk per ref would cost every click.

Not built: `:has()` disambiguation before `:nth-of-type`. It needs `:has()` in
the borrowed selector parser, which is unverified here.

### When a verb refuses

Every failure carries a `code`, prose naming the recovery, and `retryable`:
whether it is the caller's to fix.

| code | means |
| --- | --- |
| `unknown-verb` | not a verb this session has; the message lists the ones it does |
| `bad-request` | a missing or malformed argument |
| `no-snapshot` | a `@ref` was named before any snapshot was served |
| `no-such-ref` | the ref is not on this page at all |
| `stale-ref` | the ref is on the page and means something else now |
| `wrong-role` | the ref is the wrong kind of thing for this verb |
| `refused` | the policy said no |
| `login-mode` | LOGIN mode is on and this verb reads the page |
| `timeout` / `no-match` / `internal` | as named |

Every per-verb property is an exhaustive match on one table (`src/verbs.rs`),
including which verbs LOGIN mode admits.

### The resident session

`open` renders its own page and exits; `serve` holds a page open for an agent to
drive:

```
$ h5i __engine serve http://localhost:3000 &
$ h5i __engine session snapshot
$ h5i __engine session click @e1
```

Those verbs act on the page the viewers are watching. Several viewers and
control clients can attach at once. The control port is advertised beside the
stream port (`<name>.control` next to `<name>.stream`), so inside a box these
verbs need no flags.

`Page` is not `Send`: Blitz's `BaseDocument` holds an
`Arc<dyn HtmlParserProvider>` and a `Box<dyn FontMetricsProvider>`, so there is
no `Arc<Mutex<Session>>` to be had. One owning thread, everything else by
channel.

### What the agent did, recorded

`serve` writes its own action log (`$H5I_BROWSER_ACTIONS`, set for you inside a
box), which feeds the console's *agent actions* pane. The rows are marked
box-claimed rather than host-observed.

Each verb is recorded *before* it runs and again after. That guards against
accident (a bad path, a full disk), not against a box that lies. It costs 7µs
per verb against the 42ms a single frame encode takes.

### Logging in

```
$ h5i __engine session snapshot
- textbox "username" [ref=e1]
- textbox "password" [ref=e2]
- button "Sign in" [ref=e3]
$ h5i __engine session type @e1 alice
$ h5i __engine session type @e2 hunter2
$ h5i __engine session submit @e3
url: http://localhost:8123/members
```

Blitz owns form submission and dispatches to a navigation provider; this engine
hands it one that *captures* the request instead of performing it, so a
submission is policy-checked and receipted. File inputs are dropped rather than
read.

`type` replaces the field rather than appending, so a retry after a failed
submit does not produce `alicealice`. The snapshot reads values back from the
editor rather than the `value` attribute, or `type` then `snapshot` would look
like it had silently failed.

### Waiting, and the third answer

```
$ h5i __engine session wait-for --selector '#results'
$ h5i __engine session wait-for --text 'Signed in'
$ h5i __engine session wait-for-script 'document.querySelectorAll("li").length > 3'
```

The settle runs on a *virtual* clock, so a page's own `setTimeout(1000)` has
already fired by the time any verb is served. `wait_for` answers:

| `end` | means |
| --- | --- |
| `met` | it is there |
| `quiescent` | it is not, and the page has nothing left to run, so waiting cannot change this |
| `periodic` | it is not, and the only work left re-arms itself, so the page is running but not arriving |
| `budget` | it is not, and the page was still working towards something, so it may yet appear |

`periodic` exists because `requestAnimationFrame` is a `setTimeout` here: an
animation loop re-armed a one-shot timer every frame and answered `budget`
forever. Lightpanda's fix, adapted: the timers still *fire*, they stop
*counting*. Not copied is folding this into `quiescent`, which would claim
nothing can change when a repeating timer can change the DOM.

`wait_for_script` needs `--script` and says so as a routing answer rather than a
failed condition. A condition that throws counts as *not yet*.

### Reading a page cheaply

```
$ h5i __engine session markdown
$ h5i __engine session extract '{"rows": [{"selector": "tr.item", "limit": 5,
    "fields": {"name": ".title", "url": {"selector": "a", "attr": "href"}}}]}'
```

`markdown` is the page as a reader reads it, with no `@ref` handles. Three
details with tests behind them: tables carry the `|---|---|` separator that
makes them GFM, ordered lists carry their real numbers, and nested lists carry
their indent.

`extract` answers a schema. Keys are output names, values selector specs: `"h1"`
for the first match's text, `["a"]` for every match,
`{"selector":"a","attr":"href"}` for an attribute (`href` and `src` come back
absolute), `[{"selector":"a","attr":"href"}]` for that attribute of every match,
`[{"selector":"li","fields":{…}}]` for one object per match with sub-selectors
scoped to it. The array form of a spec differs from the scalar form in arity and
in nothing else: an attribute read over every match is a flat list of values, not
of one-key objects. An empty array is a result, a schema where nothing matched is
an error. Both verbs are fenced.

### The request log, from inside the session

```
$ h5i __engine session requests
   200 GET https://docs.rs/blitz/ (12043 bytes, 84ms)
DENIED GET https://telemetry.example.com/collect
$ h5i __engine session requests --since 41   # only what is new
```

If a request is not in the list, it did not happen. `denied` counts over the
whole session rather than the `--since` window, because "nothing was refused" is
a claim about the session.

### Cookies, and the narrowings that make them safe

- `Domain` honoured, over a compiled-in public suffix list. All four rules must
  pass: the domain must not be a public suffix (`Domain=co.uk` is refused), the
  setter must be within it on a label boundary (`attackerexample.com` may not
  claim `example.com`), an IP host may not widen at all, and `__Host-` forbids
  the attribute outright. The list is compiled in rather than fetched, and goes
  stale safely: it only grows.
- In memory, never on disk. The jar dies with the process.
- Never readable by the agent. No verb returns a value; `session status` reports
  a *count*, and the request log records how many cookies crossed rather than
  which.
- `Secure` enforced, `__Secure-`/`__Host-` prefixes enforced at store time, and
  a redirected POST downgraded to a bodyless GET on 301/302/303 so a password is
  not replayed to wherever a server points next.

### Credentials the agent can use and cannot read

```
$ H5I_SECRET_ACME_PASS=hunter2 h5i __engine serve https://acme.test/ &
$ h5i __engine session env
H5I_SECRET_ACME_PASS          # the name. never the value
$ h5i __engine session type @e2 '$H5I_SECRET_ACME_PASS'
{"ok":true,"ref":"@e2","used":["H5I_SECRET_ACME_PASS"]}
```

The model names a credential, the engine resolves it on the way into the field,
and the reply echoes the *placeholder*. No verb returns a credential's value.

Only the `H5I_SECRET_` namespace is reachable: the rest of `H5I_*` is engine
configuration, and a prefix allowlist fails closed where a denylist would not.
Substitution happens for `type` and nothing else, as a predicate on the verb
table.

`input[type=password]` reports a fixed-width mask rather than its value, so a
credential a *human* typed during LOGIN mode is not readable by the agent once
the mode ends. Whether the field is filled stays visible.

LOGIN mode (5.10) is half built. `session login` refuses every control verb that
reads the page, so a credential typed during it is not in a snapshot the agent
asked for. It does *not* withhold frames: the person typing has to see the page,
and the viewer socket is inside the box, where there is no privilege boundary.

Two verbs pass through, `status` and `login` itself. `requests` is refused
during a login because it names URLs a login flow visited, and `status` reports
an origin rather than a URL: an OAuth callback carries its `code` in the query,
a magic link and a password reset carry their token in the path.

### JavaScript, as a limited preview

Off by default. `--script` turns it on, and `capabilities --script` reports what
that configuration can do: h5i routes on the invocation, not the binary.

```
$ h5i __engine serve http://localhost:3000 --script
$ h5i __engine session click @e1
{"ok":true,"ref":"@e1","requests":["http://localhost:3000/api/item"],
 "settled":"settled after 0ms"}
```

`requests` is the causal link, and the log shows all three legs:

```
200 navigation  /index.html
200 subresource /app.js          <- the script file, fetched before it ran
200 subresource /api/item        <- what the click caused
```

The Rust DOM is the single source of truth and every JS object naming a node
wraps a `NodeId`. The object model lives in a JavaScript prelude rather than in
Rust, because listeners, timer callbacks and promise resolvers are GC-managed.
The Rust surface underneath is about twenty primitives taking ids and strings.

Settling is reported: "run until settled" drains promise jobs and timers on a
*virtual* clock, so two runs settle identically, and a page that never settles
is cut off at a budget and says so. Missing APIs are named, never stubbed
silently.

```
note: still busy after 2000ms (1 timers pending) — this page had not finished
note: this page used Web APIs this engine does not have
      (Element.getBoundingClientRect x3, IntersectionObserver x1). What depends
      on them did not run; the chromium engine has them.
```

ES modules work, and `import "lodash"` does not become a request to a CDN: a
bare specifier is refused by name. Module fetches go through the same broker,
carry the document origin, and appear in the request log.

They are also `cors` requests, which a classic `<script src>` beside them is
not: fetched the classic way, a cross-origin module is *evaluated in the page's
realm* without the server ever being asked. Both `type="module" src` and dynamic
`import()` ask, with the same-origin credentials a module script without
`crossorigin` gets.

### Live connections, and the caveat that travels with them

`WebSocket` and `EventSource` are real objects over real connections, not names
that answer feature detection: the rule here is *absent, not stubbed*.

Every frame is receipted, not just the handshake. Frames are ordinary
request/response pairs with `WS-SEND`, `WS-RECV` or `SSE-RECV` as the method, so
the console, `h5i box watch` and the export bundle show socket traffic
unchanged.

`wss://` works: a socket that owns its transport carries `rustls` directly,
already in the tree through `reqwest`'s own TLS. One transport type serves both
schemes. The TLS half shares its connection between reader and writer under a
lock (a TLS connection is one piece of state and cannot be `try_clone`d the way
a `TcpStream` can) with a short read timeout so the reader drops the lock often
enough for a send to get in.

`EventSource` is a `cors` request, not the agent's own: sending it without an
`Origin` and with session cookies attached let two allowed origins read each
other's streams. An answer that is not `text/event-stream` is refused too, or
the line parser reads *any* body and every line beginning `data:` in someone
else's document is a message the page receives.

CORS does not apply to a WebSocket; `Origin` is all a server has to tell a
page's socket from a program's. The handshake carries the document's origin
(`null` for a document that has none), and a socket the *agent* named carries
none. The address is checked too: the pinning resolver cannot reach a client
that calls `TcpStream::connect` itself, so the socket asks for the addresses the
policy already approved.

One refusal stands: a remote socket is refused whenever an egress proxy is
configured, `wss://` included. A raw socket would not go through the proxy, and
that proxy is how a box's allowlist stays in the path. Loopback is exempt
because the proxy already excludes it.

One caveat: a page holding a live connection is not deterministic. Messages
arrive on wall-clock time, so two reads can differ without the agent having
acted. `snapshot` and `status` report `open_sockets`. Delivery happens when a
verb runs rather than the instant a frame lands. Reconnection is deliberately
not built.

What is not there: `IntersectionObserver` and `ResizeObserver` report themselves
missing; `fetch` is synchronous underneath, so two requests run in order rather
than at once, and `AbortController` cannot cancel one in flight; no iframes,
workers, WebGL or WebAssembly. Those are also what will stop React first: a
production build is not yet verified (roadmap-history.md §12.4 sets that
bar) and what runs today is a hand-written application of the shape above.

Boa is a fork at 0.22.0, pinned by revision: `boa_engine` and `boa_gc` are
patched to `h5i-dev/boa` in the workspace `Cargo.toml`, one commit ahead with
`Script::bind_to_realm`, which compiles the prelude once and runs it in many
realms. The old 0.19 pin and the `icu_normalizer` clash with parley that forced
it are gone, which is what `scripts/check_boa_release.sh` was written to catch.
Patch both crates together: this crate depends on `boa_gc` directly, and a
second copy would make the cancellation token two incompatible types with the
same name.

### The snapshot is fenced

Page content is wrapped in `--- BEGIN/END UNTRUSTED PAGE CONTENT ---` and
labelled as data. `sanitize_display` protects a viewer's chrome, not this
moment.

The fence rests on a tested property: no page-derived value may span a line, so
a page that writes the closing marker into its own text gets it back as quoted
content on a `- ` line. A marker written inline becomes
`[fence marker removed]`, the only content this engine removes.

### The live view

`serve` opens a WebSocket speaking the format h5i's viewers already use, so
`h5i box view` and `h5i box view --term` attach unchanged: base64 JPEG frames in
a JSON envelope, a `status` message carrying the viewport, and `config`/`ack`
pacing. `--stream-file` writes the bound port where the viewers look
(`<env>/tmp/agent-browser/*.stream`).

Frames are driven by change: one is produced when a scroll actually moved or a
navigation landed, and at rest the process is idle. A click the policy refuses
returns a `page_error` and keeps the current page rather than going blank.

The allowlist is fail-closed: with no `--allow`, nothing remote is reachable.
Loopback is allowed by default because it is the dev server, and `--no-loopback`
takes that away. `$H5I_EGRESS_PROXY` is picked up automatically.

### Fonts

Fonts are discovered at runtime rather than linked at build time: Blitz's
`system-fonts` would add a build-time dependency on libfontconfig and break a
hermetic build. A host with no fonts renders pages but draws no text, and
`doctor` says so. `--font-file` and `--font-dir` override the search.

## Composition

Assembled, not written from scratch:

| Concern | Component |
| --- | --- |
| HTML parsing, DOM | `blitz-html`, `blitz-dom` |
| CSS, style resolution | Stylo (via `blitz-dom`) |
| Layout | Taffy (via `blitz-dom`) |
| Paint, rasterisation | `blitz-paint`, `vello_cpu` (CPU: a box has no GPU) |
| Text, fonts | `parley`, `fontique` |
| Policy, receipts, HTTP | this crate |

## Status

Tiers 1 and 2 of roadmap-history.md M10: static render, snapshot,
screenshot, receipts, a live view h5i's viewers attach to, the resident session
and its verbs (§12.1), and JavaScript behind `--script`. Tier 3, policy-gated
script, is deliberately unbuilt; roadmap-history.md §12 is the plan and
§12.5 is what it costs. Not yet done: the frame half of LOGIN mode, and file
uploads, which are dropped rather than read.

Pin a box to this engine with
`h5i box create --profile browser --engine h5i-light`, or
`[profile.browser] engine = "h5i-light"`. Such a box gets `H5I_BROWSER_ALLOW`
(its own `net.egress`) and `H5I_BROWSER_RECEIPTS` (a path inside the box), and
none of agent-browser's variables.

Driven against a real box on 2026-08-08: `h5i box view`'s forward and the
console's frame relay both attach and render, and a control-channel navigation
reaches every attached viewer.

### What a reading of Lightpanda changed, 2026-08-26

roadmap-history.md §B16 is the write-up. What landed here: the fourth wait
outcome above; a snapshot that no longer lets a wrapper swallow the block
beneath it; `--url` on the read verbs; `Domain` cookies; an address-level
rebinding check; record and replay over durable selectors; a real Canvas 2D;
`wss://`; a `structured` verb; and a counter for verb names callers asked for
and this engine does not have.

The comparison was most useful for the three costs it found in *our* load path,
which are §B16.10's queue and are not built yet.

---

## The session surface

### The id is not the interface

`h5i browser open` makes a session and points the *default* at it; every verb
that follows lands there. The opaque id (`br_7k2xqa`) is in the record, in
`--json` and in the receipts, because a durable reference has to survive a
rename. It is not what anyone types. Names are for running several at once
(`--session auth`), and a name is comfortable precisely because it is *not* an
identity, so it can be reused once its session has ended.

Two rules fell out of building it, both about not moving under an agent:

- No "if only one is live, use it". It reads as helpful and silently redirects
  the next verb the moment a second session exists.
- The default outlives the session it names, so the next bare verb can say *"the
  session you were on was closed"* rather than *"no session is open"*. Only a
  pointer to a record that is gone is dropped.

### What is agent-facing, and what is not

| concept | agent-facing? |
| --- | --- |
| session | yes, but usually implicitly: `open` sets the default and verbs follow it. |
| session name | yes, for running several at once (`--session auth`). |
| session id | no. Durable reference, in `--json` and receipts; never typed. |
| tab | yes, when there is more than one page in a session. |
| box | yes, but as a *placement*, never as part of a session's definition. |
| connection, worker, CDP session | no. Internal, and deliberately unnamed. |

The rule the table encodes: a thing that is a session's own implementation does
not get a name in the CLI, and a thing that stands beside a session does.

### Built, 2026-08-27

- `browser_session`: the host-owned registry. Ids never reused, five states,
  endings written down, `EXIT_SESSION_GONE`, host-named artifacts, and the
  scrubber every relayed answer goes through.
- `h5i browser` as the front door: `start`, `list`, `status`, `close`, the
  fourteen session verbs, and the control lock moved onto the session.
- `--in <box>`: the engine runs as a *service*, since the writer lock would
  otherwise shut every later verb out of its own box, and verbs arrive over a
  Unix socket, since every `box run` gets a fresh netns and a port cannot be
  reached from the next run. Preflighted, so a box that cannot hold a session
  says why before anything starts.
- `env::service_start_with_def` and the engine's `--control-socket`.

### Open, and honest about it

- Supervised and container cannot hold a resident process (h5i-sandbox's
  `spawn_background`, "Idea 3.5"), and they are also the two tiers that enforce
  an egress allowlist on Linux, so the only tier that both holds a session and
  earns `host-observed` is `microvm`. Closing this is the highest-value
  remaining work: it makes the central claim reachable on an ordinary Linux box.
- One session per box. A second would need per-session service names and stream
  files.

---

## B1. Where it stands, 2026-08-28

Built and driven end to end: render, snapshot, screenshot and receipts, with
Blitz owning the DOM, Stylo the CSS and vello_cpu the raster. A resident session
several viewers and a control channel share. Cookies over a public suffix list,
persisted only when h5i asks by name. A fenced snapshot, so page text reaches an
agent labelled as data. An action log, and a replay that re-executes it.
JavaScript through Boa, with events, timers and microtasks on a virtual clock
and `fetch` through the broker.

Web Platform Tests, core tier, full fresh sweep 2026-08-28:

    core tier      75.7% (88,199 / 116,471) with the vendored :has() stylo,
                   ~74.5% after its removal. 80% is the next target.
    html/semantics 9,623 · html/dom 62,092 · css/selectors 3,090
    css-conditional 1,601 · custom-elements 2,414 · dom 3,278 · domparsing 384

The `html/dom` figure is bimodal, 58,313 on a loaded machine against 62,183 on
an idle one, because one idlharness file times out. Run the gate on an idle box
before reading a regression into it.

Not cleared: a production React build, which §12.4 of the history document sets
as the bar. What runs is a hand-written application of the right shape.

Where the next ~5,000 subtests live, measured and ranked: the idlharness file
itself (2,628 failing, mostly capability interfaces this engine refuses to
fake), html/semantics' script/img/media/dialog clusters (~6,400), dom's
XML-document family (~600), cssom serialization and scroll geometry, and the
fetch/api JS surface (~400 reachable without wptserve).

---

## B2. Architecture, and the constraints that chose it

Four decisions the compiler or the dependency graph made rather than preference.

*One thread owns the page.* `Page` is not `Send`: Blitz's `BaseDocument` holds
an `Arc<dyn HtmlParserProvider>` and a `Box<dyn FontMetricsProvider>`, neither
thread-safe, so there is no `Arc<Mutex<Session>>` to be had. The page has a
single owning loop and everything else reaches it by channel.

*The Rust DOM is the single source of truth.* Every JS object naming a node is a
wrapper over a `NodeId`. A second tree would let the snapshot, the paint, the
events and the script state drift apart.

*The object model lives in a JavaScript prelude.* Listeners, timer callbacks and
promise resolvers are GC-managed, and holding them Rust-side means tracing them
through Boa's collector. Putting them where Boa already owns their lifetime
leaves a Rust surface of about twenty primitives taking ids and strings.
Compiled once per thread rather than per realm.

*Boa is a fork, pinned by revision.* `boa_engine` and `boa_gc` come from
`h5i-dev/boa` at 0.22.0 (`Cargo.toml`), carrying one commit that adds
`bind_to_realm` so a compiled prelude can be reused across realms. The older
0.19 pin, and the ICU clash with parley that forced it, are gone;
`scripts/check_boa_release.sh` asks crates.io on every CI run whether a
published boa would do, and fails the build the day one would. Patch both crates
together, and note that a `Gc` in a `thread_local` must be `ManuallyDrop` or the
thread aborts at exit.

---

## B3. Security: what script bought and what it cost

Loopback is reachable from a loopback document. `Policy::check` took only a URL,
and loopback is allowed unconditionally because the box's dev server is the
point. Before script an untrusted page could *cause* a loopback request but not
read the response; with `--script` it could `fetch` the dev server, read the
body, and POST it anywhere in `net.egress`: a read primitive against the code
the agent is working on, past a proxy that never sees loopback.
`Policy::check_from(url, document)` closed it. This was a *logic* bug, and Rust
prevents none of them. "Fewer memory bugs" is honest; "safer browser" is earned
by the origin model, not the language.

Site isolation is the one thing the box does not replace. Chromium's process
model contains a compromised renderer against filesystem, network privilege,
crashes and cross-origin theft; the box covers the first three at a stronger
boundary and says nothing about two origins sharing one address space. The
answer is `Jar::retain_origin`: the jar is cleared on cross-origin navigation,
so one session holds one origin's cookies and a page is never in the same
address space as another origin's session. Leaving an origin drops its login,
and the snapshot says so rather than letting the agent discover it by being
logged out. `document.cookie` additionally withholds `HttpOnly`.

The gate is still honoured: `capabilities.javascript` reports the running
configuration, script is opt-in, and with it off `<script>` elements are inert.
The same-origin policy proper lives in `cors.rs`, added once the `Domain`
attribute turned an unauthenticated cross-origin read into an authenticated one.

---

## B4. What this browser deliberately is not

A disposable sandbox removes most of a browser's surface as a *requirement*, not
as a compromise. None of the following is planned, and each should be refused in
review rather than re-argued.

Never: tabs, bookmarks, history UI, downloads manager, password saving,
autofill, extensions, sync, printing, DRM/EME, WebRTC, WebTransport, WebGPU,
WebXR, Bluetooth/USB/Serial/HID/MIDI, camera, microphone, geolocation, sensors,
desktop notifications, push, background sync, Service Workers, Cache Storage,
File System Access, popups, multiple windows, picture-in-picture, fullscreen,
XSLT, FTP.

Simplified rather than absent, and always in memory:

* cookies: session lifetime, persisted only when h5i passes `--cookie-jar`
* `localStorage`/`sessionStorage`: small maps, never a file
* history: the current page and a short navigation list
* clipboard: a sandbox-local buffer, never the host's
* dialogs: `alert` to the console, `confirm` from policy, `prompt` refused
* downloads: handed up to h5i as a response, never written as a file

Not cut, because cutting them makes this a static HTML renderer rather than a
browser: DOM mutation and query, CSS cascade with flex/grid/position/overflow,
click/input/change/submit/focus/keyboard, promises and microtasks and timers,
`fetch` with redirects and TLS, ES modules, forms, images, web fonts,
navigation, the rendered result, and console plus exception capture.

No iframes. Not "same-origin only": none. Each iframe is a second document, a
second script realm and a navigation boundary. It is a second browser.

No vendored engine crates. A 5.6MB in-tree copy of stylo bought `:has()` in
stylesheets and was reversed by owner decision on 2026-08-28: no WPT arithmetic
pays for a fork carried across every stylo bump. The query half of `:has()` is
evaluated in the prelude instead (`withHasMarkers`), so `querySelector`,
`querySelectorAll`, `matches` and `closest` keep it. Stylesheet rules using
`:has()` stay lost until Blitz depends on stylo >= 0.20.

---

## B5. The rule that produced all of it

Nothing is built until a page asks for it, and an instrument that cannot name
what is missing is fixed before anything it failed to name.

The claim is deliberately not speed: this class of engine is slower than
Chromium in wall time, and anyone can beat a benchmark table by shipping less
browser. What no one else can copy back is proving what the engine did, because
that depends on the engine *being* the HTTP client rather than being watched by
one.

Sections B1 to B22 of [`roadmap-history.md`](../roadmap-history.md) carry the build
log: the corpus runs, the WPT campaigns, the reference engines read, and the
reversals.
