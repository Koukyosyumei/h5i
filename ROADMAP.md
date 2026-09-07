# ROADMAP

Status: in progress, 2026-08-27. This file is the scope authority: what h5i is,
what is built, and what is deliberately not. It is meant to be read in one
sitting. The design behind each part lives in `docs/`, one file per part, and
the two superseded positionings are kept in
[`docs/roadmap-history.md`](docs/roadmap-history.md) because both describe
machinery that is still shipped and tested.

> Give an AI agent a browser it can drive and you can audit. Every request is
> policy-checked and written down before the bytes move, and the fetch is
> refused when the record cannot be written.

Playwright and Puppeteer drive a browser and cannot tell you what it reached,
because neither *is* the HTTP client. That is the piece with no equivalent
elsewhere, and it is what the product leads with. Nothing was thrown away to get
here: the engine, the broker, the egress proxy, the receipt lanes, the control
lock and the box tiers were all built for the environment story and all are
essential to this one.

## Where it stands

| part | status | design |
| --- | --- | --- |
| browser engine | shipped. WPT core tier 75.7%; a production React build is not cleared | [`docs/design/design-browser.md`](docs/design/design-browser.md) |
| in-terminal viewer | shipped, V1 to V8. Keyboard-driven, in the terminal or a browser | [`docs/design/design-interminal-browser.md`](docs/design/design-interminal-browser.md) |
| policy resolution | P1 shipped; P2 shipped, opt-in; P3 and P4 designed, not built | [`docs/design/design-policy.md`](docs/design/design-policy.md) |
| remote runner | R13.1 built. R13.2 to R13.4 are not | [`docs/design/design-runner.md`](docs/design/design-runner.md) |
| runtime detection | built 2026-08-19, off by default at three layers | [`docs/design/design-detect.md`](docs/design/design-detect.md) |
| HTTP workbench | phases A and B built, benchmarked; installed with `h5i plugin install websec` | [`docs/design/design-websec.md`](docs/design/design-websec.md) |
| reconnaissance | phase 1 mostly built 2026-09-07: ledger, extract, known, crawl, triage. `h5i plugin install recon`; N10 and N12 are not built | [`docs/design/design-recon.md`](docs/design/design-recon.md) |

## The three decisions the pivot rests on

1. The sandbox is opt-in. `h5i browser open` runs on the host like any other
   headless browser and says so on the placement line. Requiring a box up front
   would fail hello-world on CI, under AppArmor, on macOS and in a container,
   for nothing the record does not already give. Containment is `--in <box>`.
2. The box stays a separate, orthogonal, agent-facing surface, not the browser's
   implementation detail. `h5i box run -- h5i browser open` is ordinary
   composition and `--in` is sugar over the same placement.
3. The lane is earned. A boxed session is `host-observed` only when something
   outside the engine decides what may leave; a box that lets the browser reach
   the whole network keeps `engine-claimed`. See
   `browser_session::Session::lane_for`.

## What is next, ranked

1. A tier that both holds a session and earns `host-observed`. Supervised
   and container cannot hold a resident process, and they are also the two tiers
   that enforce an egress allowlist on Linux, so today only `microvm` does both.
   This is what makes the central claim reachable on an ordinary Linux box.
2. WPT core tier to 80%. The next ~5,000 subtests are measured and ranked in
   [`docs/design/design-browser.md`](docs/design/design-browser.md) B1.
3. R13.2 to R13.4: remote create, exec and export against a paired runner.
   The design is settled; see [`docs/design/design-runner.md`](docs/design/design-runner.md).
4. More than one session per box. Needs per-session service names and stream
   files.

## What is deliberately not built

Each of these is a decision, not a gap, and should be refused in review rather
than re-argued.

- The browser will never grow tabs, extensions, Service Workers, WebRTC, iframes
  or two dozen other surfaces. The full list, and what is simplified rather than
  absent, is B4 in
  [`docs/design/design-browser.md`](docs/design/design-browser.md).
- No vendored engine crates, by owner decision on 2026-08-28 (B4).
- The runtime detector never denies anything: no `bpf_send_signal`, no LSM
  programs, no daemon, no privilege escalation of its own (D12 in
  [`docs/design/design-detect.md`](docs/design/design-detect.md)).
- The runner MVP refuses profiles that need the secrets broker or the auth
  proxy, and any request past a runner's advertised capabilities (R12 in
  [`docs/design/design-runner.md`](docs/design/design-runner.md)).

## How to read the design set

Each design file opens with a one-screen summary, so you can tell from the top
of it whether you need the rest.

| prefix | file | what it covers |
| --- | --- | --- |
| B1 to B5 | `docs/design/design-browser.md` | the engine, the session surface, and what it is not |
| V1 to V8 | `docs/design/design-interminal-browser.md` | the viewers: the keymap, hints, latency, the lock |
| P1 to P4 | `docs/design/design-policy.md` | resolution, the authority validator, mount realization |
| R1 to R13 | `docs/design/design-runner.md` | placement, transport, the frame protocol, export |
| D1 to D14 | `docs/design/design-detect.md` | the kernel-observed lane |
| W1 to W21 | `docs/design/design-websec.md` | capture, replay, diff, and the plugin the workbench ships as |
| N1 to N21 | `docs/design/design-recon.md` | the endpoint ledger, discovery, triage, and where recon stops |

Live code cites these section numbers. The prefixes do not collide with
[`docs/roadmap-history.md`](docs/roadmap-history.md), which holds the superseded
environment positioning (sections 1 to 12) and the engine's build log (B1 to
B22); a `roadmap-history.md` citation always names that file.
