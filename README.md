# h5i

> **The version control layer for the age of AI-generated code.**

<p align="center">
  <a href="https://github.com/Koukyosyumei/h5i" target="_blank">
      <img src="./assets/logo.svg" alt="h5i Logo" height="126">
  </a>
</p>

`h5i` (pronounced *high-five*) is a Git sidecar that extends version control for teams where AI agents write production code alongside humans. Where Git answers *what changed*, h5i answers *who changed it, why, whether it was safe, and how to undo it*.

```bash
cargo install --git https://github.com/Koukyosyumei/h5i h5i-core
cd your-project && h5i init
```

---

## Feature Overview

| Feature | Command | What it does |
|---------|---------|--------------|
| **AI-tagged commits** | `h5i commit --prompt "…" --model claude-sonnet-4-6 --audit` | Stores the exact prompt, model, agent ID, and test results alongside every commit |
| **AI-aware blame** | `h5i blame <file>` | Shows per-line authorship with agent name, model, and test status |
| **Enriched log** | `h5i log` | Displays the full AI provenance inline — prompt, model, agent, test badge |
| **Intent rollback** | `h5i rollback "the OAuth changes"` | Reverts a commit by matching your description against stored prompts — no hash needed |
| **Integrity audit** | `h5i commit --audit` | Runs 12 deterministic rules before committing: credential leaks, CI/CD tampering, scope creep, `eval()` patterns, and more |
| **Session footprint** | `h5i notes analyze && h5i notes footprint` | Maps which files the AI read vs. edited in a Claude Code session |
| **Uncertainty heatmap** | `h5i notes uncertainty` | Surfaces every moment the AI hedged, with the exact quote and confidence score |
| **Omission report** | `h5i notes omissions` | Detects deferrals, placeholders, and unfulfilled promises left behind by the AI |
| **File churn** | `h5i notes churn` | Quantifies the edit-to-read ratio per file so high-churn code gets extra review |
| **Intent graph** | `h5i notes graph` | Visualises the causal chain across commits — which AI commit triggered which |
| **Context workspace** | `h5i context init / trace / commit` | Gives the AI a version-controlled notepad (goal, OTA trace, milestones) that survives session resets |
| **Session handoff** | `h5i resume` | Generates a ready-to-paste briefing — goal, progress, risky files, suggested opening prompt — from local data, no API call |
| **Memory tracking** | `h5i memory snapshot / diff` | Snapshots Claude's memory files at every commit and diffs them across versions |
| **Team sharing** | `h5i push / pull` | Syncs all AI metadata (`refs/h5i/notes`, `refs/h5i/memory`) to/from the remote in one command |
| **Web dashboard** | `h5i serve` | Opens a browser UI with Timeline, Summary, Integrity, Intent Graph, Memory, and Sessions tabs |

---

## Example Use Cases

### 1. Find out who wrote this — and with what prompt

```bash
h5i blame src/auth.rs
```

```
STAT COMMIT   AUTHOR/AGENT    | CONTENT
✅ ✨ a3f9c2b  claude-code     | fn validate_token(tok: &str) -> bool {
✅    a3f9c2b  claude-code     |     tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit())
      9eff001  alice           | }
```

```bash
h5i log --limit 3
```

```
commit a3f9c2b...
Author:  Alice <alice@example.com>
Agent:   claude-code (claude-sonnet-4-6) ✨
Prompt:  "add per-IP rate limiting to the auth endpoint"
Tests:   ✔ 42 passed, 0 failed, 1.23s [pytest]

    implement rate limiting
```

Every commit carries the exact prompt, model, agent ID, and test results from when the code was written. Use `h5i serve` to browse it all in a web dashboard.

---

### 2. Undo an AI change — by describing it, not by hash

```bash
h5i rollback "the OAuth login changes"
```

```
Matched commit:
  a3f9c2b  add OAuth login with GitHub provider
  Agent:   claude-code  ·  Prompt: "implement OAuth login flow with GitHub"
  Date:    2026-03-10 14:22 UTC

Revert this commit? [y/N]
```

h5i matches your description against stored prompts and commit messages. No commit hash needed. Use `--dry-run` to preview, `--yes` to skip confirmation in CI.

---

### 3. Audit what the integrity engine caught

```bash
h5i commit -m "refactor auth module" --audit
```

```
⚠ INTEGRITY WARNING (score: 0.70)
  ⚠ [UNDECLARED_DELETION]  247 lines deleted (72% of total changes) with no deletion intent stated.
  ℹ [CONFIG_FILE_MODIFIED]  Configuration file 'config/auth.yaml' modified.
```

Twelve deterministic rules — no AI in the audit path — check for credential leaks, CI/CD tampering, scope creep, dangerous `eval()` patterns, and more. Use `--force` to commit despite warnings.

---

### 4. Understand what Claude actually did in a session

After a Claude Code session, analyze the conversation log:

```bash
h5i notes analyze      # auto-detect most recent session
h5i notes footprint    # which files did Claude read vs. edit?
```

```
── Exploration Footprint ──────────────────────────────────────
  Session 90130372  ·  503 messages  ·  181 tool calls

  Files Consulted:
    📖 src/main.rs ×13  [Read]
    📖 src/server.rs ×17  [Read,Grep]

  Files Edited:
    ✏ src/main.rs  ×18 edit(s)
    ✏ src/server.rs  ×17 edit(s)

  Implicit Dependencies (read but not edited):
    → src/metadata.rs
    → Cargo.toml
```

```bash
h5i notes uncertainty  # where was Claude unsure?
```

```
── Uncertainty Heatmap ─────────────────────────────────────────────────
  7 signals  ·  session 90130372  ·  3 files

  Risk Map
  ──────────────────────────────────────────────────────────────────────
  src/auth.rs                                   ████████████░░░░  ●●●  4 signals  avg  28%
  src/main.rs                                   ██████░░░░░░░░░░  ●●   2 signals  avg  40%
  src/server.rs                                 ██░░░░░░░░░░░░░░  ●    1 signal   avg  52%

  Timeline  t:0 ─────────────────────────────────────────── t:503
  ·········█····················▓·····················▓···················▓·····
           ↑t:32                ↑t:220               ↑t:496

  Signals
  ──────────────────────────────────────────────────────────────────────
  ██  t:32    not sure            src/auth.rs              [ 25%]
       "…token validation might break if the token contains special chars…"

  ▓▓  t:220   let me check        src/main.rs              [ 45%]
       "…The LSP shows the match still isn't seeing the new arm. Let me check…"

  ░░  t:496   perhaps             src/server.rs            [ 52%]
       "…perhaps we should also handle the case where body is empty…"

  ██ high risk (<35%)   ▓▓ moderate (35–55%)   ░░ low (>55%)
```

```bash
h5i notes omissions    # what did Claude defer, stub out, or promise but not deliver?
```

```
── Omission Report ─────────────────────────────────────────────
  5 signals  ·  session a3f8c12d  ·  2 deferrals  ·  2 placeholders  ·  1 unfulfilled promise

  File Summary
  ────────────────────────────────────────────────────────────────────────────
  file                                          deferral  placeholder  unfulfilled
  ────────────────────────────────────────────────────────────────────────────
  src/auth.rs                                          2            1           1
  src/session.rs                                       -            1           -

  Signals
  ────────────────────────────────────────────────────────────────────────────

  ⏭ DEFERRAL      src/auth.rs · t:18 · "for now"
       "…I'll hardcode the token TTL for now — a proper config value can be added later…"

  ⬜ PLACEHOLDER   src/auth.rs · t:52 · "stub"
       "…this refresh handler is a stub; the actual token rotation logic isn't wired up yet…"

  💬 UNFULFILLED   src/auth.rs · t:61 · "i'll also update"
     → promised file: src/auth/tests.rs
       "…i'll also update src/auth/tests.rs to cover the new TTL behaviour…"
```

Three kinds of omissions are detected:

- **Deferral** (`⏭` red) — Claude noticed something was wrong or missing and chose to skip it
- **Placeholder** (`⬜` yellow) — Claude inserted a stub or admittedly incomplete implementation
- **Unfulfilled promise** (`💬` cyan) — Claude stated it would also edit another file, but never did

Filter to a specific file: `h5i notes omissions --file src/auth.rs`

```bash
h5i notes churn        # which files had the most back-and-forth?
```

The Sessions tab in `h5i serve` visualizes all of this per-commit.

---

### 5. Keep Claude's context alive across sessions

Long-running tasks lose context after a session ends. The `h5i context` workspace gives Claude a version-controlled notepad that survives resets.

```bash
# At project start — Claude runs this once
h5i context init --goal "Build an OAuth2 login system"

# During the session — Claude records its reasoning
h5i context trace --kind OBSERVE "Redis p99 latency is 2 ms"
h5i context trace --kind THINK   "40 MB overhead is acceptable"
h5i context trace --kind ACT     "Switching session store to Redis"

# After each meaningful milestone
h5i context commit "Implemented token refresh flow" \
  --detail "Added automatic refresh using stored refresh token; handles 401s transparently."

# At the start of every new session — Claude restores its state
h5i context show --trace
```

```
── Context ─────────────────────────────────────────────────
  Project: Build an OAuth2 login system  (branch: main)

  Milestones:
    ✔ [x] Initial setup
    ○ [ ] Token refresh flow

  Recent Commits:
    ◈ Added automatic access-token refresh

  Recent Trace:
    [14:22:01] ACT: Switching session store to Redis in src/session.rs
```

Use `h5i context branch` and `h5i context merge` to explore risky alternatives without losing the main thread — just like `git branch`.

To get a ready-made system prompt that tells Claude how to use these commands:

```bash
h5i context prompt
```

---

### 6. Start the next session with full situational awareness

Before beginning a new AI session, run:

```bash
h5i resume              # briefing for the current branch
h5i resume feat/oauth   # briefing for a specific branch
```

```
── Session Handoff ─────────────────────────────────────────────────
  Branch: feat/oauth  ·  Last active: 2026-03-27 14:22 UTC
  Agent: claude-code  ·  Model: claude-sonnet-4-6
  HEAD: a3f9c2b  implement token refresh flow

  Goal
    Build an OAuth2 login system

  Progress
    ✔ Initial setup
    ✔ GitHub provider integration
    ○ Token refresh flow  ← resume here
    ○ Logout + session cleanup

  Last Session
    90130372  ·  503 messages  ·  181 tool calls  ·  4 files edited

  ⚠  High-Risk Files  (review before continuing)
    ██████████  src/auth.rs                         4 signals  churn 80%  "not sure"
    ██████░░░░  src/session.rs                      2 signals  churn 60%  "let me check"

  ⚠ 3 later commits causally depend on HEAD — review before pushing.

  Memory Changes Since Last Snapshot
    + 2 files added
    ~ 1 file modified
    ℹ Run h5i memory diff to see the full diff.

  Suggested Opening Prompt
  ────────────────────────────────────────────────────────────────────
  Continue building "Build an OAuth2 login system". Completed so far:
  Initial setup, GitHub provider integration. Next milestone: Token
  refresh flow. Review src/auth.rs before editing — 4 uncertainty
  signals were recorded there in the last session.
  ────────────────────────────────────────────────────────────────────
```

No AI API call is needed — every field comes from locally stored h5i data: Git notes, the context workspace, session log analysis, and memory snapshots.

---

### 7. Share AI history with your team

h5i metadata lives in Git refs that aren't pushed by default. Push everything at once:

```bash
h5i push           # sends refs/h5i/notes + refs/h5i/memory to origin
git push origin main
```

Teammates fetch and immediately see full AI provenance in `h5i log`:

```bash
git pull           # works automatically if fetch refspecs are configured (see MANUAL.md)
h5i log            # Alice's prompts, models, test results — all visible
```

---

### 8. Browse everything in the web dashboard

```bash
h5i serve        # opens http://localhost:7150
```

<img src="./assets/screenshot_h5i_server.png" alt="h5i web dashboard — Timeline tab">

The **Timeline** tab shows every commit with its full AI context inline. In the screenshot above:

- **Top bar** — 8 commits, all AI-assisted, 100% test pass rate at a glance
- **Commit card** — model (`claude-sonnet-4-6`), agent (`claude-code`), test badge (✔ 36, pytest, 1.32s), and the exact prompt in orange italics
- **Expanded detail** — full test results table (passed / failed / skipped / duration / tool) and a one-click **Re-audit** button that runs all 12 integrity rules against that commit's diff
- **Integrity score** — `Valid · 100%` when all rules pass; shows findings inline if any trigger
- **Left sidebar** — live repo stats (total vs AI-authored commits, AI ratio) and a test-health sparkline across all loaded commits
- **Filter pills** — `🤖 AI only`, `🧪 With tests`, `✖ Failing` to slice the history instantly

The **Summary**, **Integrity**, **Intent Graph**, **Memory**, and **Sessions** tabs give deeper views into the same data. See [MANUAL.md](MANUAL.md#12-web-dashboard) for the full tab guide.

---

## Full Documentation

See [MANUAL.md](MANUAL.md) for:

- All `h5i commit` flags (prompts, test adapters, causal chains)
- Integrity engine rules reference
- Web dashboard guide
- Memory snapshot workflow
- Context workspace command reference
- Session handoff briefing (`h5i resume`)
- CI/CD push/fetch configuration
- Storage layout and architecture

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
