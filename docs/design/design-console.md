# Design: the console, sections C1 to C8

Status: sessions and attention built, 2026-09-07. `h5i ui` is the read-only
screen over everything h5i is doing on this machine: boxes, and now the browser
sessions the workbench and recon act on. `C` is the section prefix; `B`, `V`,
`P`, `R`, `D`, `W` and `N` are taken.

## In one screen

> A person running one agent reads its terminal. A person running twelve needs
> to be told which one wants them.

- **Attention is a state with evidence, never a score.** Five states, borrowed
  from herdr because the problem is herdr's, and each one carries the clause
  that produced it.
- **The console shows the account, not the evidence.** Receipts, ledger states,
  job records. Never a stored body, header or cookie: those are owner-only on
  disk, and reading them stays a command someone types.
- **It never writes.** Every route is a GET. Even "I have looked at this" is
  remembered in the browser, not sent back, because a passive view that writes
  is not a passive view.
- **It is bounded.** A registry grows without limit; a poll does not.

## C1. What it shows about a session

The row is what a session's own files say, and nothing inferred:

| shown | from |
|---|---|
| requests, refusals | `requests.jsonl`, both phases folded into one fetch |
| origins reached | the same log, parsed |
| capture on, and how many messages | the presence and contents of `messages/`, counted |
| ledger counts by state | `recon/ledger.jsonl`, folded through `h5i-wire` |
| runs that spent requests | `recon/jobs/*.json` |
| who holds the wheel | `control.json` |

Selecting one opens three tabs: the request log (hetty's shape: method, path,
status, initiator, timing, refusals in red), the endpoint inventory (state,
method, path, identity, evidence), and the job records.

## C2. What it refuses to show

The capture store holds bodies, cookies and `Authorization` in full. It is the
one artifact h5i keeps that is *not* safe to paste, which is why it is 0600, why
it is never in an export unless named, and why the console does not render it.

Every request row prints the command that reads it instead:
`h5i websec show req_42 --session <name>`. Clicking copies it. The same for an
endpoint's evidence and for resuming a job. The console teaches the next command
rather than performing it, which keeps a browser page from becoming a second
way to reach a credential.

## C3. Attention: five states

herdr's vocabulary, because the problem it solves is the one h5i now has: many
sessions, one person, and no useful way to know which wants them.

| state | shown as | fires when |
|---|---|---|
| `blocked` | waiting on you | a human holds the control lock, or a run stopped for a reason only a person can answer |
| `done` | finished, unread | the session ended, or its last run finished, and this client has not looked |
| `working` | working | live, and something happened in the last minute |
| `idle` | idle | live and quiet |
| `unknown` | unclassified | the record says live and the engine's control file is gone |

Two rules keep it honest. **Every state carries its evidence**, in the `why`
field, and the UI shows it: a badge that cannot say why it is amber is a score,
and this console does not score. **`blocked` is strict**: a run that spent the
allowance the operator gave it is a bounded run ending as asked, not a question,
so it is `done`; a login that went away mid-crawl is a question, so it is
`blocked`. herdr's reason applies exactly: a state that fires on a guess teaches
the reader to ignore it.

## C4. Seen is the client's, not the server's

`done` is the only state that depends on who is looking, so the server reports
what is true and each client remembers what it has read, in `localStorage`.
Opening a session clears its `done`; opening it in one browser does not clear it
in another. Nothing is written back.

This is not only tidiness. The console's whole posture is that it cannot change
what it watches, and a "seen" flag on disk would be the first write. It is also
the rule `h5i box watch` already follows for message read-state.

## C5. Boxes keep their own words

A box's pressure is not one of the five states: an egress refusal is the
boundary working, not somebody waiting. The attention bar counts sessions in the
five states and boxes in their own two (`refused egress`, `with failures`), in
one row, each labelled. Folding them into one vocabulary would be tidier and
would be a lie.

## C6. Two scopes, said out loud

Boxes belong to the repository the console was started in. Sessions belong to
the machine: `h5i browser open` needs no repository, and the registry lives in
the user's state directory. The tab says which, because a reader who assumes one
scope for both will misread an empty column.

## C7. What a poll costs

A registry of a thousand sessions is ordinary after a week of benchmarks, and
reading every one of their logs every eight seconds would make the console the
most expensive process on the machine. Two bounds:

- **The newest 120 are read**, live first. The rest are counted, and the column
  says so: "the newest 120 of 1727 recorded sessions".
- **A row is cached against a stamp** of the files it was folded from: the
  record, the request log, the ledger, the jobs directory, the message store and
  the control lock, each by size and mtime. An ended session is folded once and
  never again; a live one refolds exactly when something it is made of moved.

Measured on a registry of 1,730 sessions, 120 of them read: 90 ms for the first
poll and 18 ms for every one after it. Without the cache every poll is the first
one.

## C8. What is deliberately not built

- **No actions.** The console cannot open, drive, crawl or replay. Every route
  is a GET, and the buttons copy commands rather than run them.
- **No message rendering**, per C2.
- **No cross-machine fleet.** It shows this machine and this repository. A
  runner's boxes appear through the runner's own records, not by the console
  reaching out.
- **No notifications.** Attention is a state on a screen someone is looking at.
  A console that pushed would need a channel, a policy and a reason to be
  trusted with one.
