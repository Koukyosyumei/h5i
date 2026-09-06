# h5i Manual

Command reference for h5i. New here? Read [What h5i is](#what-h5i-is) and [The
loop](#the-loop) first: they give the mental model before the per-command
reference.

`h5i <command> --help` is always authoritative for flags. This manual explains
what the commands are *for*.

---

## What h5i is

> Give an AI agent a browser it can drive and you can audit. Every request is
> policy-checked and written down before the bytes move, and the fetch is
> refused when the record cannot be written.

*h5i* (pronounced *high-five*) is a secure, auditable browser for AI agents. An
agent drives a session by name and reads an outline with `@ref` handles; you
read the log the engine wrote as it went. Because the engine *is* the HTTP
client, that log is a decision record rather than an observation made beside the
network: a request that is not in it did not happen.

Auditable by default. Containable on demand. A session runs on your machine like
any other headless browser, and h5i says so rather than letting the word
"browser" imply a boundary. One flag places the same session inside a sandbox,
which changes nothing an agent types and changes who saw the network.

It is one Rust binary, engine included. No server, no daemon, no SaaS.

### Four parts

Everything h5i does maps to one of these:

1. A browser session. One page state, one cookie jar, one request log, one
   policy, under one name. Every request is checked and recorded before the
   wire; a fetch that cannot be recorded is refused. Page text comes back fenced
   as data, stripped of anything that could repaint a terminal.
2. An audit. The verbs the agent asked for, the decision the engine made about
   every fetch, the moments a human took the controls, and how the session
   ended, in one ordered timeline. Every row says whether it is the engine
   describing itself or something h5i saw from outside, and the two are never
   merged.
3. A box, when you want one. A disposable environment holding the code, the
   agent, the toolchain, the dev server and, on request, the browser session
   itself. Nothing of your machine inside it; egress is an allowlist enforced at
   its boundary.
4. An output gate. At the end you export a patch, a report, an execution receipt
   and every session's timeline, after inspection. The agent has no direct write
   path to the host.

The value is not any one of these. It is that what the agent read, what it
reached, who was driving and what came back all sit in one record that both the
agent and the human can operate on.

### What it is not

- Not a sandbox by default. A session with no `--in` is not contained, and `h5i
  browser status` says so on every line. Containment is a placement, and h5i
  never claims one it does not have.
- Not a complete browser. Of twenty single-page applications measured, eighteen
  read usefully and one not at all. Canvas, WebSockets, Workers and IndexedDB
  are absent, and a page needing an API the engine lacks gets that API *named*
  in the snapshot rather than a blank space.
- Not a content filter. h5i does not classify what a page says. It bounds what a
  persuaded agent can reach.
- Not a provenance system. h5i used to record who wrote what, with git notes,
  blame overlays and a multi-agent orchestra. That is gone. What survives is
  containment and the receipt of what actually ran.
- Not a defence against a targeted kernel exploit. See [Limits](#limits).

---

## The loop

Reading and acting on a page, which needs nothing else:

```bash
h5i browser open https://example.com       # the page grants itself; `--allow` adds more
h5i browser snapshot                       # the outline, with @ref handles
h5i browser click @e3
h5i browser requests                       # what it reached, and what was refused
h5i browser audit                          # the whole session, afterwards
h5i browser close
```

Building something and then browsing it, which is what a box is for:

```bash
h5i box .                                  # a box from this repository
h5i box shell mybox                        # work in it (this is where an agent runs)
# inside: edit, build, start the dev server
h5i browser open http://localhost:3000 --in mybox
h5i box export mybox --out ./review        # patch + report + receipt + timelines
git apply --3way ./review/patch.diff       # apply it where you want
```

The full loop the browser makes possible:

```
agent edits code -> starts dev server -> opens a session against it
  -> reads the outline -> clicks and fills -> reads the request log and the
  console -> fixes the code -> human takes the controls, hands them back
  -> export patch, report, receipt, session timeline
```

---

## Install

```bash
curl -fsSL https://h5i.dev/install.sh | sh     # prebuilt binary
cargo install --path .                         # from source
```

`h5i.dev/install.sh` and `raw.githubusercontent.com/h5i-dev/h5i/main/install.sh`
are the same file, and CI fails if they ever stop being. Use the second one if
you would rather the install path not depend on the domain.

Then, so your agent knows how to use it:

```bash
h5i skill install           # writes the skill into ~/.claude/skills/h5i (or ~/.codex)
npx skills add h5i-dev/h5i  # same bytes, if you do not have the binary yet
```

---

## Command groups

| Group | What it is for |
|---|---|
| [`h5i browser`](#h5i-browser) | Browser sessions: open one, drive it, close it. Auditable by default, containable with `--in`. |
| [`h5i box`](#h5i-box) | Create, run, inspect and export boxes. The sandbox a session, an agent and a dev server can be placed in. |
| [`h5i box share`](#h5i-box-share) | Open one box's dev server to one other person. The only inbound path. |
| [`h5i ui`](#h5i-ui) | The box console: the whole fleet, as one read-only screen. |
| [`h5i runner`](#h5i-runner) | Pair a second Linux machine and run boxes there over SSH. |
| [`h5i skill`](#h5i-skill) | Write or print the agent skill this binary carries. |
| [`h5i join`](#h5i-box-share) | Open a box someone else is sharing, from their ticket. |
| `h5i completion` | Shell completions for bash, zsh, fish and friends. |

`h5i dev *` and `h5i env *` both remain as hidden aliases for `h5i box *`
through one release. The noun the product uses everywhere else is *box*, so the
command is too.

---

## h5i browser

A *session* is the whole agent-facing surface. `h5i browser open` makes one,
every verb that follows acts on it, and `h5i browser close` ends it. Nothing
else is a concept an agent has to learn: not the process that renders the page,
not the port it listens on, not whether it is running inside a box, and not, in
the ordinary case, the session itself.

```bash
h5i browser open https://example.com
h5i browser snapshot            # the page as a model should read it
h5i browser click @e3
h5i browser screenshot          # a PNG of the page, into the session's artifacts
h5i browser reload              # re-fetch where the session actually is
h5i browser requests            # what it asked for, and what was refused
h5i browser close
```

`screenshot` writes into the session's own artifacts directory under a name *h5i
chooses*; the engine picks only the bytes. `--out` names a file instead. Like
every other verb that reads the page, it is refused while `login` is on: a
password is pixels before it is anything else.

### The id is internal

Every session has an opaque id (`br_7k2xqa`), and it is in the record, in
`--json` and in the receipts, because a durable reference has to be something no
rename can break. It is not what you type. A CLI that demands an opaque string
on every verb is copying a remote-browser HTTP API, where the id exists because
the client and the browser share nothing else. Here they share a filesystem.

So a verb resolves its session in three steps, most explicit first:

1. `--session <name>` (`-s`), a name someone chose, or an id pasted from
   `--json`
2. `$H5I_BROWSER_SESSION`
3. the default: the session `open` last made

Running several at once is what names are for:

```bash
h5i browser open https://example.com/login --session auth   --new
h5i browser open https://example.com/      --session public --new
h5i browser snapshot --session auth
h5i browser list                       # the default is the row marked `*`
```

A name is comfortable to type precisely because it is not an identity: it can be
reused once the session it named has ended. The id cannot, which is why the id
is what gets written down, and why `--restore` takes one.

There is deliberately *no* "if only one session is live, use it" rule. It reads
as helpful and is the same hazard as a moving default: an agent that opened one
session, had it end, and opened another under a different name would find its
next verb quietly landing somewhere it never asked for.

### `open` navigates a session that is already there

Opening a URL in a browser that is already up means *go there*. So `open`
navigates the session it finds, and `--new` is how you say you meant a second
one. The flags that only make sense at creation (`--allow`, `--in`, `--script`,
`--no-loopback`, `--permissive-cors`, `--expires-in`, `--restore`, `--capture`) are *refused*
rather than ignored when a session is reused: a session's policy is fixed when its engine starts, so
accepting a grant and doing nothing with it would be a grant the caller believes
it made.

### What is true by default

Started with no flags, a session runs on this machine in your ordinary process
space, like any other headless browser. There is no sandbox, and h5i does not
claim one.

What it does that another headless browser does not is record. The engine is the
HTTP client, so every request is checked against the session's policy and
written down *before* the bytes move, and the fetch is refused when the record
cannot be written. A request that is not in `h5i browser requests` did not
happen. That is a property of the engine, not of a container, so it holds
whether or not there is a box.

The honest name for that is auditability, and the CLI says so on every status
line:

```
requests : engine-claimed (fail-closed, and the engine's own account of what it fetched)
```

#### The page grants itself

A session reaches the URL it was opened on, and nothing else remote. Naming a
URL and then naming its origin again is ceremony that teaches nothing, so `open`
grants the page it was given exactly as `read` grants its targets. `--allow` is
for the origins beyond it: an API the page calls, a CDN it pulls from. Loopback
is reachable by default because it is the dev server, and `--no-loopback` takes
that back.

The grant is the page and not "and whatever this page pulls in". An off-origin
subresource is still refused, and still says so in the request log, which is the
part a wider default would have given away.

#### Cross-site credentials, and the one flag that changes them

A page here may not send this session's credentials to another origin on a
request whose answer nobody can read. In fetch terms that is `mode: "no-cors"`
with `credentials: "include"`, and h5i refuses it: an opaque response cannot be
checked, so nothing could ever show the server agreed.

That is the right default for containing an agent and the wrong one for testing
a target, because the shape being refused is the classic POST-based CSRF. With
the refusal in force h5i cannot act as the *victim*, so a negative result means
"h5i declined", not "the target is safe".

`--permissive-cors` makes one session behave like a browser here:

```bash
h5i browser open https://attacker.example --script --permissive-cors
```

It is scoped to that session, part of its policy digest, and named on the `open`
banner and in `h5i browser status`, so nobody is in it by accident and no
finding gathered under it can be mistaken for one gathered without it. It widens
exactly that: a cross-origin `cors` read still has to be permitted by the server,
and `mode: "same-origin"` still refuses to cross.

One thing it does not do is put a credential where there was not one. The cookie
jar holds the session for the origin currently loaded and drops the rest on
navigation, so a cross-host attack page has nothing of the target's to send.
Two ports on one host are two origins and one jar, which is the shape a local
CSRF lab has.

### Browser identities

A session uses one identity for HTTP headers, JavaScript, screen geometry, locale,
and time zone. The identity is fixed when the session opens and recorded in its
audit data.

```bash
h5i browser identity list
h5i browser identity check firefox-143-linux --script
h5i browser open https://example.com --script --identity privacy
```

| mode | behavior |
|---|---|
| `native` | Truthfully identifies h5i. This is the default. |
| `privacy` | Uses stable h5i values so installations reveal fewer local differences. |
| `compatible` | Coherently presents another supported browser identity. |

Use `identity show <name>` to print an identity as TOML, or pass a TOML file to
`--identity`. Contradictory identities and identities requiring unsupported
features are refused rather than partially applied. Currently,
`firefox-143-linux` is supported; Chrome identities require client hints and
WebGL capabilities this engine does not provide.

Identity consistency is not anonymity. TLS and HTTP/2 fingerprints, installed
fonts, network location, and input timing remain outside this feature.
### `read`: one page, no session

```bash
h5i browser read https://example.com
h5i browser read url1 url2 url3 --json
```

```
confined : process (files and environment; the origin allowlist is the engine's)
```

For the shape a crawl has: fetch, read, move on. No cookies carried between
verbs, no `@ref` to click, nothing resident afterwards, and `h5i browser list`
shows nothing when it is done.

The targets grant themselves, and only themselves. The engine is fail-closed, so
something has to name the origins, and a URL you typed is one you asked for:
making you name it and then name its origin again is ceremony that teaches
nothing. A page that pulls a script from a third-party CDN, or redirects to
another host, is refused and says so in the log, which is the part a wider
default would have given away.

`--allow ORIGIN`, repeatable, is for when that refusal is the problem rather
than the point: a page written in a library served from a CDN, read without the
grant, is the page the library never ran on. It is the same flag `open` takes
and it grants the same thing, one named origin at a time. Inside a box it can
only narrow: the box's own egress list is enforced at a boundary outside the
engine, and a flag cannot widen it.

Several targets share one browser (one connection pool, one cookie jar and one
font set across the batch) and a page that fails does not stop the ones after
it. `--json` returns the page, its request log, and what was holding the engine
together, which is what a crawl wants and what no other headless browser can
hand over completely.

#### `--in <box>`: an allowlist a tier enforces

An allowlist that is not simply "what I asked for" belongs in a file, not in
arguments. Write it in `.h5i/env.toml`:

```toml
[profile.docs]
isolation = "supervised"

[profile.docs.net]
mode   = "host"
egress = ["docs.rs", "*.rust-lang.org"]
```

```bash
h5i browser read https://docs.rs/serde --in docs --json
```

```
confined : box docs, policy 6bca3b30c268
```

The read runs inside that box, through the same `box run` you would type
yourself: the tier resolves the pinned policy, enforces egress at a network
namespace boundary outside the engine, and writes a receipt. The digest on the
line is the policy that was actually enforced, which is the thing an allowlist
assembled from command-line arguments could never hand back.

A read can have this and a session cannot, and the reason is the difference
between the two rather than a preference. A session is resident by design
(`snapshot` then `click @e3` needs the page to still be there), and the
supervised tier cannot hold a resident process yet: its seccomp-notify gate is
served by a thread inside the `h5i` process that started the run, so when that
command exits the gate has no server and every filtered syscall blocks. A read
runs to completion inside that command, which is the shape that tier already
has.

Aim a read at `localhost` and use no box: under a tier with its own network
namespace the loopback is the sandbox's, not the one your dev server is on.

### The default sandbox

A local session runs in a process-tier sandbox that confines files, environment,
syscalls, and resources. It does not enforce the browser origin allowlist at the
network boundary; the engine enforces and records that policy itself.

```
placed : on this machine, in a process-tier sandbox
         (files and environment; not its network)
```

The browser broker owns policy, credentials, cookies, budgets, and receipts. A
separate renderer parses and executes page content. The renderer receives only
the responses the broker has authorized, and a renderer crash ends the session.

### `--in <box>`: the same session, inside a box

```bash
h5i browser open http://localhost:3000 --in mybox
```

This places the resident browser in the named box and records the box policy
digest. On Linux, resident sessions currently require a tier that can keep the
engine alive; use `browser read --in` when you need a one-shot read behind the
supervised tier's network allowlist. A microVM can provide both residence and a
network boundary.

Inside a network namespace, `localhost` means the box, not the host. Ensure the
engine binary is installed inside the box or configure
`H5I_BROWSER_ENGINE_IN_BOX`.
### Opening a session from inside a box

An agent already in a box does not need `--in`, and cannot use it: `--in` means
"put this session in a box I am outside of", which is what lets it promise an
enforced takeover and a lane the engine did not claim for itself. From inside,
neither is true, so it is refused with the reason rather than silently doing
something weaker.

Open it without the flag. It runs beside the agent, in the same box, and the
record says exactly that:

```
placed   : this machine, which is box env/human/web (its policy is not readable from in here)
requests : engine-claimed (fail-closed, and the engine's own account of what it fetched)
```

Two things are deliberate in those lines. The box is *named*, because a session
there is not uncontained and saying "no containment beyond the engine" would
understate what is true: the same class of error as overstating it, in the
direction that happens to be safe. And nothing is claimed about what the box
enforces, because the policy is host-side and sealed: from in there, h5i cannot
read its own boundary.

The control channel inside a box is a Unix socket rather than a loopback port.
Not a preference either: a box's netns may have no usable loopback at all
(`net.mode = deny` leaves nothing to dial), and every `h5i box run` gets a fresh
one, so a port bound in one is unreachable from the next.

### Reading and acting, beyond `snapshot` and `click`

```bash
h5i browser structured                          # what the page says about itself
h5i browser transcript                          # what its media says, from `<track>`
h5i browser markdown --url https://example.com  # go there and read, in one trip
h5i browser find  --role button --name 'Sign in'
h5i browser click --role button --name 'Sign in'
h5i browser set-checked @e4 true
h5i browser select @e5 'Express shipping'
h5i browser press  @e1 Enter
h5i browser script --save flow.json
```

`structured` is the cheapest read: JSON-LD, OpenGraph, `<meta>`, `<link rel>`, a
few hundred bytes where a snapshot is a few hundred lines. A page with no
metadata answers `empty`, which is a fact about the page rather than a failed
read. Every read verb takes `--url`, which goes there first and then reads: one
round trip where `navigate` and then the read would be two, and the reply still
names the URL it ended up on so a redirect is not silent.

`transcript` reads the hole the other verbs leave. A `snapshot` names a
`<video>` and `markdown` skips it, so a page whose substance is a forty-minute
talk reads as a title and a play button. Most players ship a `<track>`, and a
caption file is prose with timestamps: the shape a model reads well, and the one
audio is not.

```console
$ h5i browser transcript --url https://example.com/talk
url: https://example.com/talk
media: 1 element(s), 1 with timed text, 412 cue(s) read
--- BEGIN UNTRUSTED PAGE CONTENT ---
…
--- END UNTRUSTED PAGE CONTENT ---
```

### When the page acts on its own

With `--script`, a page can move the ground under a verb, and both ways it can
are reported rather than left to be inferred.

**A form the page submits itself.** `form.submit()` from a handler, or a form
that submits on load, produces a real request. It is not sent from inside the
page, because this engine drives navigation through its own verbs so that an
agent and a receipt both see it. It goes out at the verb boundary instead,
through the broker and into the request log like any other. The reply carries
`page_submitted` with where it went, and the session has landed on the answer by
then, so take a new `snapshot`: every `@ref` you hold describes the page that is
gone.

**`load` and `error` on subresources.** An `<img>`, `<script>`, `<link>` or
`<iframe>` that did or did not arrive fires at the element that asked for it, so
`<img src=x onerror=…>` and `<svg onload=…>` behave the way they do in a
browser. An element whose only interactivity is a handler attribute, such as
`<div onclick=…>`, reads as role `clickable` and takes a `@ref`, which is how
you fire one.

## Video transcripts

`snapshot` and `markdown` describe a media element but do not decode its
audio. Use `transcript` to read caption tracks declared by the page:

```bash
h5i browser transcript --url https://example.com/talk
h5i browser transcript --url https://example.com/talk --lang en
```

Caption files are fetched through the browser broker, so normal origin policy
and receipts apply. h5i reads at most one language track plus a chapters track.
Media without captions is reported explicitly.

Some sites expose captions only through player APIs. The optional yt-dlp helper
handles those sites:

```bash
h5i browser transcript --via yt-dlp   --url https://www.youtube.com/watch?v=…
```

This helper is never an implicit fallback. It opens its own connections, so its
traffic cannot appear in the engine request log; instead, the exact helper
command and its host-observed result appear in the audit. It runs in the
session's placement, receives no browser credentials, ignores user yt-dlp
configuration, and has a two-minute default budget. Set
`H5I_HELPER_BUDGET_SECS` to override that budget.

Use an exact language tag such as `--lang en`, or an intentional pattern such
as `--lang 'ja.*'`. Automatic captions are labeled as such. The `ytdlp`
build feature, enabled by default, controls whether this helper path exists.
### `audit`: the whole session, in one timeline

`requests` is the network layer on its own, and it is the verb to reach for in a
loop. `h5i browser audit` is the one to read afterwards: what the agent asked
for, what the engine decided about every fetch, who was driving, and how the
session ended, merged and ordered.

```console
$ h5i browser audit
  sources  : actions read · requests read · control read
  note     engine rows are ordered by the engine's own clock, which h5i cannot verify

  host    session opened  (http://localhost:3000/ — on this machine, no containment…)
  engine  #0 GET http://localhost:3000/
  engine  #0 200  153 bytes
  engine  verb   snapshot
  host    control -> human  (taken by a human)
  host    control -> agent  (handed back; the agent must re-snapshot)
  engine  verb   snapshot
  engine  #1 DENIED GET https://tracker.example/px  (origin is not in the allowlist)
  engine  verb ! click @e1 — denied by policy
  host    session closed  (closed by the user)
```

Three things this does that neither log does alone:

- The two lanes stay apart. The action and request rows are the engine's own
  account of itself; the handovers and the lifecycle are h5i's, written from
  outside. Every row says which. Merging them into one confident-looking column
  is the exact confusion the lane split exists to prevent.
- It orders across sources. "Was a human at the controls when that form was
  submitted" is a question about two logs at once, and a current-holder field
  cannot answer it. The engine stamps its own rows and h5i stamps its own; the
  engine's clock is the engine's claim, and the output says so.
- It says what it could not read. `sources` reports each log as `read`, `empty`
  or *`unavailable`*. An empty timeline over a log h5i cannot see looks exactly
  like a session that did nothing, and those are different findings.

Rows carry `caused_by` where the source recorded the link, so a fetch can be
traced to the verb the page was under when it went out. Nothing here infers a
link from timing: a request that merely happened near a verb is not a request
that verb caused.

`--json` gives the whole thing, including the session record. It is the same
structure `h5i box export` writes for each session placed in a box.

### Sessions end, and endings are recorded

A session directory outlives the session. Closing one writes the ending into its
record instead of deleting it, which is what makes "how did this end" answerable
afterwards, and what makes an id impossible to reuse.

| state | what happened |
| --- | --- |
| `live` | started, and the engine answered the last time anyone looked |
| `closed` | ended by `h5i browser close`; the record is complete |
| `died` | the engine stopped without being asked; the record has a gap and says so |
| `expired` | outlived `--expires-in` |
| `evicted` | the box holding it was removed |

A verb sent to a session that is not live is refused with exit code 69
(`EX_UNAVAILABLE`), never silently restarted:

```console
$ h5i browser snapshot
browser session `br_7k2xqa` was closed: closed by the user. It will not be
restarted automatically. Start a new one with `h5i browser open <url>`, or
carry this one's storage forward with `h5i browser open <url> --restore br_7k2xqa`.
$ echo $?
69
```

The distinct code is the point. An agent whose retry cannot tell "the session is
gone" from "the click did not work" is an agent that silently starts a second
browser and loses both the page it was reasoning about and the record of how it
lost it.

`--restore` is an inheritance, not a resurrection: it produces a *new id*, and
writes `restored_from` into the new record.

What it carries is the *cookie jar*, and only that. A session mirrors its jar
into its own directory while it runs: owner-only, written whenever the jar
changes rather than at exit, because a session is stopped with a signal and a
shutdown hook would never run. So a login a human performed once at the live
view survives into the next session:

```bash
h5i browser open https://example.com/login --session auth
h5i browser login --session auth        # the human types the password
h5i browser login --session auth --off
h5i browser close --session auth

h5i browser open https://example.com/app --restore br_7k2xqa   # still signed in
```

No verb returns a cookie value, and this adds none: the file is handed to the
next engine, never to a model. A session that left no jar (one that ran in a box
whose `/tmp` this machine cannot read, or one from before this existed) is
refused by name rather than silently seeding nothing.

### Everything a session returns is untrusted

The page composed the title, the link text, the error message and the URL; the
engine only carried them. So every answer h5i relays is scrubbed before it
reaches a terminal or a model: escape sequences never survive, other control
characters are removed, and strings, arrays and nesting are capped with the
truncation stated in the value rather than performed quietly.

Escape sequences matter most. `ESC` in a relayed string is a page rewriting the
terminal it is printed into: moving the cursor over the line above, hiding what
it just did, repainting a prompt. Nothing a browser has to say needs `ESC`.

Files a session produces are named by the host, never by the session, and land
under the session's own `artifacts/` directory.

### h5i browser view

Watch the page and take the controls, without a box.

```bash
h5i browser view                      # draw it in this terminal
h5i browser view --web                # serve it to your browser instead
h5i browser view --session auth       # when more than one is open
```

`h5i box view` reaches the same viewer for a session in a box. The difference
that shows on screen is what the status line can claim: a boxed session's egress
is enforced outside the engine, while a host session's rests on the engine's own
word, so it reads `engine-claimed` rather than naming a box. Watching changes
neither; `--in` is what makes the claim checkable.

The keys are the ones under [h5i box view](#h5i-box-view).

### The control lock

Two clients can drive one page: the agent, and a human at the live view.

- The agent holds control by default. A session exists to let an agent work; it
  should not have to ask.
- A human takes control, never asks for it: `h5i browser take <session>`, or by
  reaching for the controls at either live view, which takes it for you. The
  agent's mutating verbs are refused with a typed message rather than fighting
  for the pointer; read-only verbs keep working, because watching never
  collides.
- Handing control back invalidates what the agent knew. The page moved, so every
  `@ref` from its last snapshot may point somewhere else. It must re-snapshot
  before acting, and acting first is refused rather than mis-clicked.

`take` says which kind of pause it just created, because the two are genuinely
different:

- In a box: enforced. Every verb is carried in from the host, and none of them
  is now.
- On this machine: advisory. It pauses `h5i browser` and nothing else. An agent
  that drives the engine binary directly is not stopped by it.

### Where sessions live

`$H5I_BROWSER_HOME`, else the box's own `/tmp` when h5i is running inside one,
else `$XDG_STATE_HOME/h5i/browser`, else `~/.local/state/h5i/browser`.

Deliberately *not* under a git repository: every other noun in h5i stores its
state under the enclosing repo because every other noun is about a repo, and a
browser is not. `h5i browser open` in an empty directory is the ordinary case.

The box case is not a preference. Inside a box `$HOME` is the host's path over a
sealed overlay and `~/.local/state` is not writable, so a session there would
fail to start; the box's `/tmp` is private to it and lives exactly as long as
its sessions can.

The default session is per registry, so two agents sharing a `$HOME` share it.
Give each its own with `$H5I_BROWSER_HOME`, or give each session a `--session
<name>`.

| variable | what it names |
| --- | --- |
| `H5I_BROWSER_HOME` | the session registry's directory |
| `H5I_BROWSER_SESSION` | which session a verb acts on, when `--session` is not given |
| `H5I_BROWSER_ENGINE` | the engine binary on this machine |
| `H5I_BROWSER_ENGINE_IN_BOX` | the engine command inside a box, when the box's `PATH` is not where it is |

The last two are separate on purpose. Mixing them points one side at a path the
other cannot see.

### Choosing the engine

`--engine` selects the browser engine and records that choice in the policy
digest.

| engine | use |
|---|---|
| `chromium` | Default; broadest web compatibility. |
| `lightpanda` | Third-party lightweight headless engine. |
| `h5i` | Built-in, auditable engine optimized for agent reading and actions. |

The h5i engine is smaller than Chromium but slower on script-heavy pages. It
supports JavaScript, redirects, cookies, policy-checked subrequests, page
outlines, screenshots, forms, and common agent actions. It is intentionally not
a complete browser: unsupported APIs are named in snapshots and console errors
rather than silently approximated.

Use `--script` only when the page needs JavaScript. For maximum compatibility,
choose Chromium. Chromium runs with an isolated profile and the box's policy,
but its internal requests are not the built-in engine's broker receipts; inspect
the box-level network evidence instead.
## h5i box

### Making a box

```bash
h5i box .                       # snapshot this repository at HEAD
h5i box --pr 1234               # a pull request (number, #number, or URL)
h5i box https://github.com/o/r  # clone an external repository
h5i box --new                   # an empty box; the agent builds from nothing
```

`h5i box [SOURCE]` is shorthand for [`h5i box create`](#h5i-box-create), and
takes the same flags. A pull request is `--pr`, not a positional: a bare number
is ambiguous with everything else a source could be, and `h5i box create`
already spelled it as a flag.

Where the code comes from decides the shape of the box:

- *This repository* → a real git worktree on its own branch, sharing the object
  store, so `h5i box apply` can land it back locally.
- A URL, a PR, or `--new` → a *detached* box. It gets a repository of its own
  inside its directory, this repository is neither read nor written after
  creation, and the inherited `origin` remote is dropped so the box cannot reach
  a network handle nobody granted it. `apply` and `rebase` refuse and point at
  `export`. This is the shape external code should always arrive in.

### h5i box create

```
h5i box create <NAME> [--from <rev>] [--pr <n>] [--clone <url>] [--new]
                      [--profile <p>] [--isolation <tier>] [--image <img>]
                      [--engine <chromium|lightpanda|h5i>]
```

The base revision is frozen at creation and pinned immutably. The policy is
resolved, digested and stored *before* any state is created on disk, so an
unsatisfiable request fails closed rather than leaving half a box behind.

| Flag | Meaning |
|---|---|
| `--from <rev>` | Base revision (default `HEAD`). |
| `--pr <n\|url>` | Fetch `refs/pull/<n>/head` and pin it as the base. Needs only `git`. |
| `--clone <url>` | Copy an external repository in. Detached. |
| `--new` | Empty box (a fresh repository with one empty commit). Detached. |
| `--profile <p>` | See [Profiles](#profiles). |
| `--isolation <tier>` | See [Isolation tiers](#isolation-tiers). |
| `--image <img>` | Base image for `isolation=container` and `isolation=microvm`. Pre-pulled; runs never pull. |
| `--engine <e>` | Browser engine for the `browser` profile: `chromium` (default), `lightpanda`, or `h5i`. Pinned in the digest; never falls back. See [Choosing the engine](#choosing-the-engine). |

A profile can also refuse individual browser actions, enforced by h5i on the
daemon's control socket rather than advised:

```toml
[profile.browser.browser]
deny = ["evaluate", "state"]   # a bare family name covers state_save/state_load
```

`evaluate` is arbitrary code in the page; `state_*` and `credentials_*` reach
the browser's stored secrets. A denied verb never reaches the browser, and the
refusal lands in the receipt's `browser-proxy` lane. This is enforcement against
an agent using the documented path, not containment against one that goes
looking: the daemon runs inside the box, and a box has no internal privilege
boundary.

### Working in a box

```bash
h5i box ls                            # every box on this clone
h5i box status <name>                 # policy actually enforced, evidence, base drift
h5i box run <name> -- cargo test      # one command; the exit code passes through
h5i box shell <name>                  # interactive confined session
h5i box diff <name>                   # what changed against the pinned base
h5i box log <name>                    # the box's event log
```

`h5i box shell` is the agent-in-box: stdio is inherited, so every command the
session spawns is contained by the box rather than by the agent choosing to wrap
each call.

### h5i box detect

Runtime detection: what an eBPF collector in the kernel saw inside a box.
Read-only, and available on every build, because the verbs are how you find out
why the collector is *not* working, so gating them behind it would hide the
answer from the hosts that need it.

```bash
h5i box detect probe                  # can this machine watch a box, and if not, why
h5i box detect rules                  # the whole signature catalogue
h5i box detect rules --filter secret  # one family, or one rule id
h5i box detect show <name>            # what fired in this box, worst first
h5i box detect show <name> --min alert
```

Turn it on per profile with `[profile.<name>.detect] enabled = true`; see
[Runtime detection](#runtime-detection) for the section and what it costs.

### Services and ports

```bash
h5i box service start <name> <service>   # a declared long-lived process
h5i box service status <name>
h5i box service logs <name> <service>
h5i box ports <name>                     # the per-box dynamic port map
```

Services are declared in `.h5i/env.toml`:

```toml
[service.web]
command = "npm run dev"
port = 3000
```

Supported at the `workspace` and `process` tiers in v1. At `supervised` and
`container` the network namespace belongs to a single session, so run the dev
server inside the same `h5i box shell` as everything else.

### h5i box export

The output gate. A box has no write access to anything outside itself; this is
the only way out, and it is deliberately a human step.

```bash
h5i box export <name> --out ./review
```

Produces:

| File | What it is |
|---|---|
| `patch.diff` | The tree diff against the pinned base, path-validated: no symlink escapes, no nested `.git`, no agent-introduced gitlinks. |
| `report.md` | What ran, what the browser saw, what the kernel saw, who was at the controls, and the agent's own proposal. |
| `receipt.json` | Every observed execution, with the policy digest that was enforced. |
| `receipts/<id>.raw` | The full account of each ingress session: who connected, over what path, for how long, how much moved, what was refused. Present when the box was shared. |

It refuses rather than overwrites an existing non-empty directory (`--force` to
replace). Secret redaction and size caps apply to all of it.

Read `report.md` before applying. It surfaces, in this order:

- denied egress attempts: the box tried to reach hosts the policy refused
- *what ran*: every command, its lane, its exit code
- what the browser saw: console errors, uncaught exceptions and failed requests,
  observed by h5i rather than reported by the agent
- what the kernel saw: signatures that fired against the syscalls a box actually
  made, when runtime detection was on for the run
- viewer sessions: including whether a human took the controls
- the agent's proposal

Then apply it where you want:

```bash
git apply --3way ./review/patch.diff
```

`h5i box apply <name>` still lands a proposed box onto its parent branch in this
repository, for the local case where that is what you want. It refuses for a
detached box.

### h5i box cache

Cold dependency install is the difference between a 20-second box and a
four-minute one, so warm caches are in scope.

```bash
h5i box cache ls              # caches for this project, and whether they are stale
h5i box cache mounts          # exactly what a box would get
h5i box cache refresh <eco>   # populate one, in a dedicated box with no agent in it
h5i box cache rm <eco>
```

Rules that make this safe rather than merely fast:

- One cache per project and ecosystem, keyed by a digest of that ecosystem's
  lockfiles. A cache whose key no longer matches is listed as stale and never
  handed to a box: packages resolved for a different dependency set are a
  silent, hard-to-explain wrong answer.
- Mounted *read-only* into an agent box. That costs nothing in correctness:
  every package manager falls back to fetching what it cannot find.
- Written *only* by `h5i box cache refresh`, which runs the install step alone,
  with egress narrowed to the registry hosts and no agent inside. `refresh`
  needs a project-declared profile whose egress is the registry hosts and
  nothing else, and it refuses with that profile written out ready to paste
  rather than creating a box whose fetch could not have worked.

No mutable surface is ever shared between an agent box and anything else.

### h5i box view

```bash
h5i box view mybox
h5i box view mybox --web
```

The viewer combines the rendered page with action, network, console, and policy
events. Each row keeps its observation source and evidence grade separate; h5i
does not infer causal links that are absent from the event stream.

| Key | Does |
| --- | --- |
| `j` `k` | Scroll a line |
| `d` `u` | Scroll half a page |
| `space` `b` | Scroll a page |
| `gg` `G` | Top, bottom |
| `f` | Label everything on screen, then follow the one you type |
| `F` | Label the fields, then type into the one you choose |
| `yf` | Label everything, then copy that link |
| `gi` | Type into the first field on the page |
| `yy` | Copy this page's URL |
| `H` `L` | Back, forward |
| `r` | Reload |
| `i` | Hand the keyboard to the page, where an engine can use it |
| `Esc` | Return it |
| `D` | The console pane: what the page logged and what it threw |
| `?` | The key list |
| `q` | Leave |

A viewer attaches read-only. It does not weaken the box policy, publish the
browser port, or become part of the agent's session.
### The engine, underneath

The engine is part of the `h5i` binary. `h5i browser` runs it as a separate
process and speaks a protocol to it, the way it always has; what changed is that
it execs itself to get there instead of a second file.

It used to ship as `h5i-browser-light` beside `h5i`, and two files bought three
problems. The default install left `h5i browser open` with nothing to render a
page. Two halves of one protocol could drift apart with no handshake between
them. And a box could *read* the engine without being allowed to `exec` it,
because Landlock makes `~/.cargo/bin` readable and not executable, so `command
-v` found it and `exec` refused it.

The engine's own CLI is still reachable, hidden, for the cases that want it
directly:

```bash
h5i __engine --help
h5i __engine open https://docs.rs/ --allow docs.rs   # one-shot render, then exit
h5i __engine doctor                                  # what fonts it found
h5i __engine skill install                           # the engine's own skill
```

Take it deliberately. `h5i browser` is the surface that knows about session
names, placement, the control lock and the audit; `__engine` is what sits under
it, and reaching past the front door means giving all of that up. It is hidden
from `--help` for that reason and documented here for the same one.

What the engine gives you with no box is a browser whose whole network activity
is in a log you can read. What a box adds is that the agent cannot go around it.

### Inspecting what happened

```bash
h5i box probe                       # what this host can enforce at all
h5i box capabilities <name> --json  # what this box actually got
h5i box doctor <name>               # can it still enforce its claim? are its refs intact?
h5i box secrets <name>              # declared grants, dry-run resolution, never values
h5i box inspect <name> --capture <id>
h5i box compare <a> <b>             # boxes side by side
h5i box watch <name>                # policy decisions, one line each, as they happen
h5i box watch <name> --deny-only    # only what was refused
```

`h5i box watch` is the tail of the receipt rather than a viewer: no viewport, no
panes, no control lock, and nothing it prints can take the controls. It is meant
to be piped, grepped, and left running in a second pane while an agent works.

Every row names the lane that observed it and the grade of that evidence, as
words:

```
09:14:02  box  fail-closed  request   allow  GET https://docs.rs/blitz/  #41 subresource
09:14:02  box  fail-closed  response  200    #41 12.0 KB, 84ms
09:14:03  box  fail-closed  request   DENY   GET https://telemetry.example.com/collect  #43
09:14:03  box  fail-closed  policy           telemetry.example.com: not in net.egress   (<- #43)
```

Terse is not licence to drop the qualifier. A row that did not say whether the
box or the host observed it would assert more than h5i knows, so the lane and
the grade are on every line and colour never carries them alone.

`--deny-only` keeps a refusal's *pair*: the request row carries the method and
the URL, the verdict row carries the reason, and dropping either leaves half an
answer. `--json` emits the same event envelope the console reads, one object per
line, so the three readers of that stream agree on the wire shape.

Only h5i's own browser engine writes a live request log, and an image-backed
tier keeps it out of the host's reach. `watch` says so in its header rather than
leaving an empty screen to be interpreted.

### Lifecycle

```bash
h5i box rebase <name>       # re-pin onto the parent branch's current tip
h5i box abort <name>        # stop; manifest and workspace preserved for forensics
h5i box rm <name> [--force] # remove entirely
h5i box gc                  # reclaim applied/aborted workspaces
```

### h5i box allow

```bash
h5i box allow                 # list the current entries
h5i box allow api.example.com
```

A persistent, user-level egress allowlist merged into every container-tier box
whose profile *already* sets `net.egress`. A deny-all profile is never widened.
Stored under `~/.config/h5i/`, outside every box-granted path, and it refuses to
run inside a box.

---

## h5i box share

Share one port from a running box without publishing that port directly.

```bash
h5i box share <name> [--port 3000] [--expire 60m] [--label alex]
h5i box share <name> --direct-only
h5i box share <name> --tunnel
h5i box share status <name>
h5i box share grant <name> --label sam --expire 30m
h5i box share revoke <name> <grant>
h5i box share stop <name>
```

The recipient runs `h5i join -` and supplies the ticket on stdin. Passing the
ticket as an argument also works, but exposes it to shell history and the process
list. Treat a ticket like a password: possession is authorization, forwarding it
admits another person, and h5i stores only its hash.

Peer-to-peer mode is end-to-end encrypted and may use a relay that sees endpoint
addresses, timing, and volume but not content. `--direct-only` refuses relay
fallback. `--tunnel` creates a normal browser link through Cloudflare; Cloudflare
terminates TLS and can read that traffic. Tunnel mode requires `cloudflared`.

A share needs a live box session. On Linux the box must have a usable network
namespace; h5i refuses configurations where it cannot distinguish the box's port
from the host's. On macOS h5i verifies that the listening process belongs to the
box and repeats that ownership check for every connection.

The share credential moves from the first URL into an HttpOnly cookie, and h5i
removes it before forwarding the request to the app. Proxy and visitor identity
headers are also removed. The app still receives ordinary browser headers, its
own cookies, and its query string. WebSocket upgrades are supported.

Each request is authorized independently and normally uses one connection into
the box. A share permits at most 64 concurrent box connections. Malformed,
unauthorized, expired, revoked, overloaded, and unreachable attempts are counted
separately in the receipt.

### Joining safely

The shared page is agent-written code running in your browser. Use a private
window when practical, especially when the joiner must bind `127.0.0.1`, because
browser cookies are scoped by host rather than port.

h5i normally chooses a private address from `127.0.0.0/8`. macOS may require
`--shared-jar`; WSL browser access may require `--bind 127.0.0.1`. Both choices
share browser storage with other services on that host and therefore require
explicit consent. h5i never binds the join proxy outside loopback.

h5i blocks service-worker registration and cross-site requests carrying the
share credential. It does not otherwise sandbox the page: downloads, granted
permissions, browser storage, and page scripts have the same powers as on any
link you open.

### Grants, revocation, and receipts

Use one labeled grant per person. `share revoke` drops that person's live
connections; `share stop` ends every grant and writes the final receipt.
Additional grants are currently available only for tunnel shares. The maximum
share lifetime is 24 hours and the default is one hour.

The receipt records transport, duration, grants, peers, connection and byte
counts, refusals, route failures, incomplete responses, clock anomalies, and
whether shutdown produced partial totals. Tunnel receipts explicitly state that
the connection was not end-to-end encrypted.
## h5i ui

```bash
h5i ui
h5i ui --port 0
h5i ui --open
```

The read-only web console lists boxes, resolved policies, services, diffstats,
receipts, and browser evidence. Selecting a box shows its filesystem, network,
process, resource, and page lanes; selecting a row shows the same receipt text
as `h5i box inspect`.

The console binds loopback and uses a random bearer token in the URL. Keep that
URL private: any local process or page that obtains it can read the console.
Untrusted box and page strings are rendered as text, never HTML. The console can
stop a browser or take its control lock, but it cannot edit the box or widen its
policy.
## h5i skill

`skills/h5i/` is embedded in the binary at build time, so the skill cannot
document flags the installed binary does not have.

```bash
h5i skill install [--target <dir>]   # write it out
h5i skill show [<page>]              # print SKILL.md or one reference page
h5i skill path                       # where an install would write
```

This is also how the *in-box* agent gets the skill: nothing is baked into an
image, and nothing is copied from host to box.

---

## h5i runner

A *runner* is a second Linux machine you own that h5i reaches over SSH: a spare
laptop, a lab box, a VM, a small server. Boxes run there; the repository, the
policy, the credentials and the patch gate stay here.

This is *placement*, a second axis beside the isolation tier a box already
declares. It does not change what a box is allowed to do. What it changes is
which machine an escape would reach.

```bash
h5i runner pair pi5 h5i@pi.local      # pair, pinning the machine's host key
h5i runner probe pi5                  # what can it actually do, right now
h5i runner list                       # what this account has paired
h5i runner unpair pi5                 # forget it here
```

### What pairing does

1. Reads the machine's SSH *host key* and pins it. That key is the runner's
   identity: `runner_id` is its SHA-256, and a box records the id, never the
   name. Renaming a runner, or pointing the name at other hardware, therefore
   cannot move a box onto a machine it was not built for.
2. Generates a keypair used for this runner and nothing else, owner-only, under
   `~/.config/h5i/runners/<name>/`.
3. Installs one line in the runner's `authorized_keys`:

   ```
   restrict,command="/usr/local/bin/h5i runner serve-stdio" ssh-ed25519 AAAA…
   ```

   `restrict` is the whole security argument in one word: with it that key
   cannot open a shell, forward a port, forward your agent, or allocate a
   terminal. It can run that one command and nothing else.
4. Connects over the new key and probes, so that pairing either works end to end
   or leaves nothing behind.

Nothing listens on the runner. There is no daemon, no port, no token and no TLS:
the worker is a process per request, started by sshd and gone when the request
ends.

Pairing trusts the host key it sees the first time, exactly like your first
`ssh` to a new host. To close that window, read the real fingerprint on the
machine itself and pass it:

```bash
# on the runner
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
# here
h5i runner pair pi5 h5i@pi.local --fingerprint SHA256:…
```

`--print-only` prints the `authorized_keys` line instead of installing it, for a
machine where keys are added another way.

### Capabilities are advertised, never assumed

A runner needs Linux, sshd and `h5i`. It does not need a container runtime.
Everything past those three is *advertised* by `h5i runner probe`:

```
$ h5i runner probe pi5
✔ `pi5` — h5i 0.3.4 on linux aarch64, protocol 1

  isolation     process, supervised
  container     no
  memory        7.6 GiB
  workspace     41.2 GiB free
  boxes persist yes
  own egress    yes
  kvm           no
  runner id     3f9a1c04b7e2
```

A box asking for something a runner does not advertise is refused, with the
missing capability named. It is never quietly given something weaker: the same
rule `--isolation` already follows here.

The isolation list is what the runner's kernel demonstrably ran a moment ago,
not which kernel features are present. The two are not the same thing, and only
the first is worth advertising.

Two entries change what you can do next, so they are called out rather than left
as a `no` in a table:

- boxes persist: no. Box state does not survive a reboot (a read-only OS, a
  tmpfs workspace). A reboot is an expired lease: anything not exported is gone.
- own egress: no. The runner has no default route, so a box on it cannot pull
  images or install packages. Egress brokered through this machine is a later
  milestone.

### Putting a box on one

```bash
h5i box create fix-auth --runner pi5
```

The base commit is pinned here, the branch is created here, and the policy is
resolved and digested here. What crosses is the source, as a git bundle, and
what comes back is the digest of the policy the runner actually enforced. The
box is refused if that does not match what was sent.

```
$ h5i box ls
env/human/fix-auth   created   isolation=container  base=fa31b1f97547 captures=0 on=pi5
```

The manifest records the runner's *host-key hash*, not its name. Renaming a
runner, or pointing a name at different hardware, therefore cannot move a box
onto a machine it was not built for: `h5i box rm` checks the identity before it
removes anything there.

`h5i box rm` clears both sides. It removes this side first: `rm` refuses a box
that is still live, and clearing the runner before that check would destroy the
box there while telling you the removal had failed. If the runner is unreachable
when its turn comes, the box is left there and its lease reaps it.

### Working in one

```bash
h5i box run fix-auth -- cargo test      # runs on the runner
h5i box propose fix-auth                # bring the work home
h5i box diff fix-auth                   # review it here
h5i box apply fix-auth                  # land it
h5i box export fix-auth                 # or take the patch and receipts
```

`box run` executes on the runner under the policy pinned at create, and the
receipt comes home with the exit code, the timings and the runner's own egress
summary. It is filed under a lane of its own, *`runner-observed`*: h5i saw it
from outside the box, so the box could not have forged it, but *this* machine
did not watch it either. It is not counted as host-observed and not counted as
box-claimed, because it is neither.

`box propose` is where the work returns, and it is the careful part. The runner
commits what the box has and sends a bundle of just the new work. That bundle is
unpacked into a throwaway repository with its own object database (not a branch,
not a ref namespace, a separate repository) and inspected there: size and count
ceilings, path traversal, nested git repositories, submodule pointers the base
did not have. Only a tree that passes crosses into your repository, and h5i
writes the commit itself. The runner's history and authorship never enter your
history at all.

If something is refused, nothing lands:

```
$ h5i box propose fix-auth
Error: mediated commit refused (fail-closed) — 1 path violation(s):
  - a submodule pointer the base did not have, at vendor/thing
```

After a successful propose, `diff`, `apply` and `export` behave exactly as they
do for a local box. There is nothing special about applying work that came from
a runner, which is the point.

### What is not built yet

- `box shell` on a runner. Interactive means a pty, which means bidirectional
  streaming and resize; that is the next piece of work.
- Streaming output. `box run` returns everything when the command finishes, so a
  long build is silent until it ends. The exit code, timings and evidence are
  all correct; you just do not see the log as it happens.
- Agents on a runner. An agent profile needs model credentials, and h5i will not
  send those to another machine. A credential channel that keeps them here is a
  later milestone, and until then a runner box runs builds, tests and commands
  rather than Claude or Codex.
- `clone:` and `--new` sources. Those build their repository inside the box;
  sending one across belongs with a later milestone.

The design, including what is deliberately deferred and why, is
`docs/design/design-runner.md` sections R1 to R13.

### Unpairing

`h5i runner unpair <name>` removes the record, the key and the pin from this
machine. It does not touch the runner: the `authorized_keys` line stays until
you delete it, and the command says so, with the comment to search for.

---

## Policy

A box's policy is resolved at creation, serialized to `policy.resolved.toml`,
and *digested*. Every receipt records the digest that was actually in force, so
"what was enforced" is never a matter of trust.

### Profiles

Built-ins need no file:

| Profile | What it grants |
|---|---|
| `default` | Fail-closed build/test confinement: system paths read-only, `$WORK` read-write, no network. |
| `agent` | The agent-in-box surface, scoped to `$H5I_AGENT`'s runtime. |
| `agent-claude` / `agent-codex` | Pin one runtime: only that agent's HOME state and API egress. |
| `browser` | The agent profile plus headless Chrome and the `agent-browser` daemon. |

Runtime scoping is not cosmetic: a Claude box must not get Codex's credentials
or egress to OpenAI, because a prompt-injected agent could otherwise read the
*other* runtime's token and use it against an allowlisted host.

A note on the one grant nobody would think to write. The built-in read set
carries the handful of paths `/etc/resolv.conf` is a symlink *to*
(`/mnt/wsl/resolv.conf` on WSL, the systemd-resolved and resolvconf locations
under `/run`), one file each. `/etc` alone is not enough, because Landlock
follows the link to a path the box was never granted, and what that costs does
not look like a denied file: `getaddrinfo` answers "Temporary failure in name
resolution" and a `net.mode = "host"` box reads as a machine with no network.
The entries are the same on every host whether the files exist or not, because a
grant resolved from the local `/etc` would give one profile a different policy
digest on every machine. A custom profile that sets `fs.read` replaces that
list, so a box of your own with `mode = "host"` needs the line for your host,
which `readlink -f /etc/resolv.conf` names.

Custom profiles live in `.h5i/env.toml`:

```toml
[profile.review]
isolation = "supervised"

[profile.review.fs]
read  = ["/usr", "/etc"]
write = ["$WORK"]

[profile.review.net]
mode     = "deny"
egress   = ["api.github.com"]
unix     = false          # AF_UNIX sockets; see below
loopback = [3000]         # macOS only; see below

[profile.review.resources]
mem   = "4G"
procs = 256
wall  = "30m"
```

### Runtime detection

Optional eBPF detection reports what a run actually did from kernel syscall
tracepoints. It observes; it does not block. Landlock, seccomp, namespaces, and
the egress proxy remain the enforcement mechanisms.

```toml
[profile.review.detect]
enabled = true
require = false
buffer_kb = 256
rules = ["*"]
```

`require = true` refuses a run when observation cannot attach. Detection needs
a build with `--features bpf`, Linux 5.8 or newer, and `CAP_BPF` plus
`CAP_PERFMON`. Use `h5i box detect probe`, `detect rules`, and
`detect show <box>` to inspect availability and results.

Coverage is full for workspace, process, and supervised runs; partial for
containers whose workload leaves h5i's process tree; and unavailable inside a
microVM's guest kernel. Every receipt states its coverage.

### Isolation tiers

| Tier | Boundary | Network |
|---|---|---|
| `workspace` | Separate worktree only | none |
| `process` | Landlock, seccomp, namespaces, and resource limits | deny or host |
| `supervised` | Process tier plus private network and socket supervision | L3/L4 allowlist |
| `container` | Rootless Podman | proxy-based L7 allowlist |
| `microvm` | Hardware-isolated guest via microsandbox | guest L3/L4 allowlist |

`auto` chooses the strongest available tier. An explicitly requested tier
fails rather than silently downgrading.

The microVM tier requires `msb` 0.6 or newer, host virtualization, and a
pre-pulled OCI image. It protects the host kernel boundary but currently provides
no per-request egress tally or authenticated-egress grants. `h5i box probe`
reports missing requirements.
### AF_UNIX sockets

`[profile.X.net] unix = true` lets the box create `AF_UNIX` sockets. Off by
default, because `SCM_RIGHTS` passes file descriptors, which is authority
smuggling.

What the grant does *not* open, which is why it can exist at all: abstract
sockets are scoped by the box's private netns; filesystem-bound ones are scoped
by Landlock; and `/tmp`, where `.X11-unix`, `tmux-*` and an ssh-agent live, is a
per-box scratch at the kernel tiers. What is left is a host socket sitting
inside a granted path, so the grant is opt-in per profile and pinned in the
digest.

The `browser` profile sets it, because the `agent-browser` daemon's control
socket is a filesystem-bound `AF_UNIX` listener.

### Credentials

- *Model API*: the key stays on the host. A reverse proxy injects it into
  outbound requests from the box, scoped per runtime, so a Claude box cannot
  reach the OpenAI credential.
- Any other service: the same mechanism, declared as policy:

        [[profile.review.auth]]
        host           = "api.github.com"
        credential_env = "GITHUB_TOKEN"   # read on the host, never in the box
        base_url_var   = "GH_HOST"        # what the client reads
        token_var      = "GH_TOKEN"       # where the box gets its per-run dummy

    `token_var` is required. The proxy gates every request on a per-run token, so
    the box has to be handed it in whatever variable its client already sends as
    a credential. The real credential stays on the host; the box only ever holds
    the dummy.

    The limit is real, so know it before you declare a grant: it binds clients
    you can point at another origin, so a plain `curl https://api.github.com`
    still goes nowhere. A TLS-terminating forward proxy would lift that, at the
    cost of a CA the box trusts, and it is deliberately not built.

    Restricting *what* the box may do with a credential is authorization, and it
    belongs where it is already solved: a fine-grained token scoped to one
    repository and the operations you meant.

- Per-box HOME state is a copy of the host agent's config, seeded once and never
  written back, with credential-shaped entries stripped at any depth
  (`credentials*`, `.netrc`, ssh keys, `*.pem`/`*.key`/`*.p12`), keeping only
  the runtime's own token, which it cannot function without.

### Secrets

Declared per profile, brokered host-side, injected for the life of one run:

```toml
[profile.review]
secrets = ["DEPLOY_KEY"]

[profile.review.secret.DEPLOY_KEY]
source = "env:H5I_SECRET_DEPLOY_KEY"   # the default for a bare name
inject = "env"                          # `file` is workspace-tier only in v1
```

The value never appears in the policy, the digest, or any receipt. `h5i box
secrets <name>` dry-runs the resolution and reports a fingerprint, never a
value. A grant that cannot be resolved fails the run closed rather than starting
a box that will fail confusingly later.

Two limits on what a source may be, because a profile lives in the repository:

- `source = "command:…"` runs host-side code outside the sandbox, as you. It
  needs the profile's `allow_command_extractors = true` *and*
  `H5I_ALLOW_COMMAND_EXTRACTORS=1` in your own environment. The profile flag is
  what pins the decision in the policy digest; it cannot also be the authority
  for it, because whoever wrote the repository wrote it.
- `source = "file:…"` is a host-side read handed to the box, so it may not point
  inside the profile's `fs.deny` list. A policy that says `~/.ssh` is out of the
  box's reach cannot read `~/.ssh/id_ed25519` on its behalf.

An `[[auth]]` grant is the one place h5i attaches a credential you hold to a
request it originates. The destination must be a bare hostname, and every run
prints which variable is being attached and where it goes.

`ttl` is advisory and is shown as `ttl=<value>(advisory)`: h5i resolves a grant
once and never expires it.

---

## Receipts

One append-only JSONL log per box, plus the raw payload of each record. A record
is generated from observation, never from the agent's account of itself.

Two properties the design depends on:

- Append only, and sealed. The box's write window under its own directory is
  exactly `<box>/spool`. The receipt log and the stored payloads are siblings of
  that spool, outside every grant. The box stages a record; the host ingests it.
  There is no path from inside to a record the host has already written.
- Redacted at the boundary. Secrets are scrubbed from the command and from the
  payload *before* either is written, and the scrub is recorded by rule id,
  never by value.

Every record carries the *lane* that observed it, so the two kinds of evidence
never blur:

| Lane | Who observed it |
|---|---|
| `host-env-run` | h5i, host-side. Exits and resource usage come from the supervisor's `wait4`; egress from the allowlist proxy's own log. |
| `viewer` | h5i's own viewer forward. The box supplies none of it. |
| `tee-shim` | The box's shell shim. Box-claimed. |
| `inbox-capture` | Staged by the box. Box-claimed. |

A record can also carry a `runtime` block, which is a *second observer of the
same command* rather than a lane of its own. See below.

### What the browser saw

A run that drove the browser also carries what the page said back: console
errors, uncaught exceptions, and requests that failed. h5i collects these
itself, right after the command, in the same box under the same policy, so the
timing is not the agent's to choose. Only what is new since the last drain is
recorded.

A browser command with no browser to ask is recorded as `unavailable`, not as a
clean page. "Nothing was looked at" is a different claim from "nothing was
wrong", and a reviewer has to be able to tell them apart.

### What the kernel saw

A run under a profile with `[detect] enabled = true` carries a `runtime` block:
which scope selected the events, how completely it covered the tier, how many
events were seen and how many were lost, and every signature that fired with a
few examples of what tripped it.

The block is written even when the collector could not attach, carrying the
reason. That is deliberate and it is the point of the whole lane: a missing
block and a quiet box would otherwise look identical. Read it this way:

- No `runtime` block at all: the profile did not ask to be watched.
- A block with `unavailable`: it asked, and the probe could not attach. The
  reason is in the block; `h5i box detect probe` explains it in full.
- A block with `coverage: none` or `partial`: some or all of the run happened
  where this scope cannot reach it, with the reason attached.
- A block with detections and `events_lost` above zero: the list is a lower
  bound, because events were dropped before anything examined them.
- A block with no detections, `coverage: full` and `events_lost: 0`: nothing the
  catalogue models happened. That is not the same as nothing happening: `h5i box
  detect rules` is a finite list, and behaviour no rule models produces no line.

Every path and command line in the block is a string the box passed to a
syscall, captured on the way *in*. It is not the kernel's resolution of that
string, and the probe sees the attempt rather than the outcome. A `connect` the
network namespace refused looks exactly like one that succeeded.

### What we do not claim

An agent can stage *extra* records, or stop writing. Both are visible: a gap
between host-observed exits and box-reported commands is itself a finding. We do
not claim more than that.

The kernel lane narrows what "stop writing" buys, and does not close it: the
collector cannot be defeated by a box declining to cooperate, but it only runs
where the profile asked and the host could attach, and it only reports what a
signature models.

---

## Limits

h5i states these limits explicitly because its claims are security-sensitive.

- Containment cannot stop an agent from sending source code to an allowed model
  API. Use a trusted model endpoint or deny model egress when that matters.
- Workspace, process, supervised, and container tiers share the host kernel.
  They are not defenses against a targeted kernel exploit. The microVM tier adds
  a hypervisor boundary.
- Container egress is proxy-based and therefore applies only to software that
  honors the proxy. Supervised and microVM tiers enforce network rules lower in
  the stack.
- Interactive workspace, process, and supervised shells share a terminal with
  the operator. Depending on the OS and kernel, a process may inject input,
  continue reading an inherited terminal, or leave terminal settings changed.
  `h5i box probe` reports input-injection exposure. Container and microVM
  terminals are separate.
- Chrome's own Linux sandbox is disabled inside a box because h5i's seccomp
  policy blocks the namespace operations it needs. The box boundary remains,
  but one browser defense layer is absent.
- On macOS, a box shares the host's loopback. Declared service ports are
  reachable locally, and manually started services must list allowed loopback
  ports. Linux boxes use a private network namespace.
- macOS Seatbelt has no seccomp equivalent, cgroup memory ceiling, or per-box
  process-count ceiling. Status and probe output mark those gaps. Use a container
  or microVM when hard resource ceilings are required.
- Chromium placement on macOS relies on the tier's egress boundary rather than
  agent-browser's in-process domain list. Chrome may restart when its proxy route
  changes, losing its temporary browser profile.
- Browser viewing covers the rendered tab, not browser chrome, native dialogs,
  or the desktop.
- Chrome consumes substantial memory and CPU. Browser support is opt-in per box.
- Chromium driving depends on the pinned external `agent-browser` tool.
## Files

| Path | What it is |
|---|---|
| `.h5i/env.toml` | Checked-in policy: profiles, services, container image. |
| `.git/.h5i/env/<agent>/<slug>/` | One box: its manifest, resolved policy, receipts, workspace. |
| `.git/.h5i/cache/<eco>/<key>/` | Warm dependency caches. |
| `.git/.h5i/env/<agent>/<slug>/spool/` | The box's one writable window: staged posts and capture records. |
| `~/.config/h5i/` | Host-side egress allowlist. Outside every box-granted path. |
| `~/.config/h5i/runners/<name>/` | One paired runner: its record, its dedicated key, its pinned host key. Owner-only, and outside every box-granted path for the same reason the allowlist is. |

---

## Environment variables

All optional; h5i ships with working defaults.

### Set by you

| Variable | Purpose |
|---|---|
| `H5I_AGENT` | Which runtime a box is scoped to (`claude`, `codex`). Decides the env's branch namespace and the `agent` profile's credentials and egress. The namespace takes 1–64 ASCII letters, digits, hyphens, or underscores after trimming; unset is `human` silently, anything else warns on stderr and namespaces the box under `human`. |
| `H5I_DEFAULT_ISOLATION` | Pin this clone's default tier when `--isolation` is not given. `--isolation auto` re-probes past it. |
| `H5I_SECRET_<NAME>` | Default source for a secret grant `<NAME>`. Injected for one run, redacted from evidence, audited by fingerprint. |
| `H5I_SKILL_DIR` | Where `h5i skill install` writes. |
| `H5I_CREDENTIAL_PROXY` | Turn the credential proxy off (`0`) for a box that must reach the model API directly. |
| `H5I_LOG` | `tracing_subscriber` filter for h5i's own diagnostics, e.g. `h5i_core=debug`. Goes to stderr. `RUST_LOG` is honoured as a fallback. |
| `H5I_NO_PROBE_CACHE` | Re-probe host capabilities instead of reusing the cached answer. |

### Set by h5i, inside a box

Read these to detect that you are in one; do not set them yourself.

| Variable | Meaning |
|---|---|
| `H5I_ENV_ID` | The box's id. Its presence is how the skill decides you are inside. |
| `H5I_ENV_POLICY_DIGEST` | The digest of the policy actually enforced. |
| `H5I_ENV_CAPTURE_SPOOL` | The box's only write window: staged receipt records. |
| `H5I_ENV_BASE_TREE`, `H5I_ENV_AUDIT_CAPTURE` | Box plumbing. |

### Tests

| Variable | Purpose |
|---|---|
| `H5I_TEST_CONTAINER` | Opt in to the real-container integration tests (pulls an image, makes a live call). |
| `H5I_TEST_NET` | Opt in to the supervised egress allowlist end-to-end test (needs outbound network). |
| `H5I_RUNNER_STATE_DIR` | Where a runner worker keeps box state. For driving a worker against a scratch directory; a real runner uses its default. |
| `H5I_BPF_LIVE` | Opt in to the live eBPF attach suite. It loads programs into the running kernel, so it needs `CAP_BPF` and does not run by accident; without it the suite skips and prints why. |

### Builds

| Variable | Purpose |
|---|---|
| `H5I_BPF_REQUIRE` | Fail the build if the eBPF probe cannot be compiled, instead of shipping a binary whose detector reports `unavailable` forever. Set it in CI and for releases. |
| `CLANG` | Which `clang` compiles the eBPF probe. Otherwise `clang`, then `clang-20` down to `clang-14`, each tested against the BPF target before it is trusted. |
| `H5I_SKIP_WEB_BUILD` | Skip the console bundle and leave a stub, for a Rust-only build with no Node on the machine. |

---

## See also

- `h5i <command> --help`: the authoritative flag reference
- `man h5i`: the terse CLI reference
- [`skills/h5i/`](skills/h5i/): the agent-facing skill (`h5i skill show`)
- [`ROADMAP.md`](ROADMAP.md): what is built and what is not
- [`docs/design/`](docs/design/): the design behind each part
  (`design-browser.md`, `design-policy.md`, `design-runner.md`,
  `design-detect.md`)
- [`SECURITY.md`](SECURITY.md): reporting a vulnerability
