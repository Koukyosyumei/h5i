# Vibe Coding With Claude Is Fun — Until It Silently Ships a Risk Into Your Git History

*One command to make Claude confess every line it wasn't sure about.*

---

You've been doing AI-assisted code review wrong.

Not the review itself — the *targeting*. You stare at a diff that touched 23 files across 1,400 lines, and you start from line 1. You read the obvious stuff carefully. You skim the boring stuff. You hope the risky stuff isn't buried somewhere in the middle.

Here's what nobody tells you: **the AI already knows which parts are risky.** It told itself, in private, while writing the code. You just never had a way to read that conversation — until now.

---

## The Secret Monologue Inside Every AI Edit

When Claude Code works on your codebase, it doesn't just output edits. It *thinks*. Before touching a file, it reasons through the problem in an internal monologue — a "thinking block" — that never appears in the chat window.

Inside those thinking blocks, you'll find sentences like:

> *"I'm not sure this is the right migration path for the foreign key constraint…"*

> *"This might break the async flush if the buffer is partially written — need to verify."*

> *"Tricky — the lock order here assumes single-threaded initialization, which may not hold."*

The AI is hedging. It's expressing doubt. It's waving a tiny red flag that says *a human should look here* — and then burying that flag in a log file you've never opened.

**`h5i notes uncertainty` unearths every one of those flags.**

---

## What It Looks Like in Practice

Say Claude just refactored your database layer. You run two commands:

```bash
h5i notes analyze    # parses the session log, links to HEAD
h5i notes uncertainty
```

You get this:

```
── Uncertainty Heatmap ─────────────────────────────────────────────
  9 signals  ·  session a3f8c12d  ·  4 files

  Risk Map
  ──────────────────────────────────────────────────────────────────────────
  src/db/migrations.rs        ████████████░░░░  ●●●  3 signals  avg  28%
  src/db/connection_pool.rs   ██████░░░░░░░░░░  ●●   2 signals  avg  38%
  src/repository.rs           ████░░░░░░░░░░░░  ●    1 signal   avg  45%
  tests/integration_test.rs   ░░░░░░░░░░░░░░░░  ●    1 signal   avg  62%

  Timeline
  t:12 ──────────────────────────────────────────────────────────── t:87
  ··············█···············▓·········█··█·····················░·····
               ↑t:18           ↑t:34     ↑t:52↑t:55

  Signals
  ──────────────────────────────────────────────────────────────────────────
  ⚠ HIGH  src/db/migrations.rs · t:18 · "might break" · 20% confidence
       "…might break the rollback if the constraint was partially applied
        on a previous failed run…"

  ⚠ HIGH  src/db/migrations.rs · t:52 · "not sure" · 25% confidence
       "…not sure whether the index creation here should be inside or
        outside the transaction boundary…"

  ▲ MOD   src/db/connection_pool.rs · t:34 · "assuming" · 50% confidence
       "…assuming the pool max is set before the first acquire call,
        but I should verify this…"
```

In under three seconds, you know exactly where to focus your review.

- `migrations.rs` is where the AI was genuinely nervous — two high-risk signals, average confidence 28%
- The timeline shows signals clustered around turns 18, 52, and 55 — a burst of uncertainty mid-session, not at the end
- The individual snippets give you the *reason* for the doubt, quoted verbatim from the AI's thinking

You didn't have to read a single line of diff to know where to start.

---

## Why This Changes Code Review

The standard review process is symmetric: every line costs the same amount of reviewer attention. But AI-generated code isn't symmetric. Some of it the model wrote with total confidence — patterns it has seen ten thousand times. Some of it the model wrote while silently hedging, reaching into unfamiliar territory, making assumptions it flagged to itself as unverified.

Treating those two categories identically is a waste of your most limited resource: focused human attention.

`h5i notes uncertainty` makes the review process *asymmetric in the right direction*. High-confidence AI code gets a lighter pass. Low-confidence AI code — the stuff the model itself flagged — gets your full attention.

The result: you catch more real bugs in less time.

---

## The Signal Table

h5i watches for a calibrated vocabulary of self-doubt phrases inside thinking blocks, each mapped to a confidence score:

| Phrase | Confidence |
|---|---|
| `"might be wrong"`, `"could be wrong"` | 20% |
| `"not sure"`, `"i'm unsure"`, `"not confident"` | 25% |
| `"unclear"`, `"not certain"` | 30% |
| `"might break"`, `"could break"`, `"risky"` | 30–35% |
| `"need to check"`, `"should verify"`, `"double-check"` | 40% |
| `"assuming"`, `"i'll assume"`, `"maybe"` | 40–50% |
| `"perhaps"`, `"let me verify"` | 45% |

The lower the confidence score, the brighter the red in the heatmap. The higher the density of signals on a given file, the more the risk bar fills.

This isn't sentiment analysis. It's the AI's own internal calibration — surfaced, structured, and pointed at the files that deserve your eyes.

---

## Filter by File

Already know which module worries you? You can filter signals to a specific path:

```bash
h5i notes uncertainty --file src/db/migrations.rs
```

Useful when you're reviewing a PR and want to jump straight to the model's doubts about the file you're responsible for — without wading through signals from unrelated parts of the diff.

---

## The Workflow

Here's how it fits into a natural AI-assisted development cycle:

```bash
# 1. Claude finishes a task and you're ready to review
h5i notes analyze              # parse the session, link to HEAD

# 2. Get the 10,000-foot view of what the AI touched
h5i notes show                 # footprint: consulted vs. edited files

# 3. Find the risky spots
h5i notes uncertainty          # heatmap + timeline + verbatim snippets

# 4. Now open your diff viewer — but start with migrations.rs, turn 18
```

The whole thing takes ten seconds. What you get back is a prioritized review agenda, assembled from the AI's own private doubts.

---

## Under the Hood

h5i reads Claude Code's session logs — `.jsonl` files that record every message, tool call, and thinking block in a session. It auto-detects the latest session for your repository, so you rarely need to pass a path manually.

After analysis, the results are persisted in `.git/.h5i/` and linked to the commit OID, so you can revisit the uncertainty map for any past commit:

```bash
h5i notes uncertainty --commit a3f8c12
```

Team members who pull your h5i data (`h5i pull`) can run the same query on sessions they weren't present for.

---

## Getting Started

```bash
# Install h5i (see github.com/koukyosyumei/h5i for build instructions)
cargo install --path .

# Initialize in your repo
h5i init

# After your next Claude Code session:
h5i notes analyze
h5i notes uncertainty
```

That's it. The heatmap is waiting.

---

*h5i is open source. The uncertainty heatmap is one of several session-analysis features — alongside causal chain reconstruction, file churn tracking, and AI-assisted review point identification. If the idea resonates, star the repo and try it on your next AI-assisted PR.*

*Because if the AI was nervous about it, you probably should be too.*
