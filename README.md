# h5i

> **Version control for the age of AI-generated code.**

<p align="center">
  <a href="https://github.com/Koukyosyumei/h5i" target="_blank">
      <img src="./assets/logo.svg" alt="h5i Logo" height="126">
  </a>
</p>

`h5i` (pronounced *high-five*) is a Git sidecar that answers the questions Git can't: *Who prompted this change? What did the AI skip or defer? What was it thinking, and can we safely resume where it left off?*

```bash
cargo install --git https://github.com/Koukyosyumei/h5i h5i-core
cd your-project && h5i init
```

---

## Three things h5i does

### 1. `h5i commit` — record why the code was written

Every commit stores the exact prompt, model, and agent alongside the diff. With Claude Code hooks installed, this happens automatically — no flags to set.

```bash
h5i commit -m "add rate limiting"
```

```
● a3f9c2b  add rate limiting
  2026-03-27 14:02  Alice <alice@example.com>
  model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
  prompt: "add per-IP rate limiting to the auth endpoint"
  tests: ✔ 42 passed, 0 failed, 1.23s [pytest]
```

When a design choice isn't obvious, record the reasoning inline:

```bash
h5i commit -m "switch session store to Redis" --decisions decisions.json
```

```
Decisions:
  ◆ src/session.rs:44  Redis over in-process HashMap
    alternatives: in-process HashMap, Memcached
    40 MB overhead is acceptable; survives process restarts; required for horizontal scaling
```

The `--audit` flag runs twelve deterministic rules — credential leaks, CI/CD tampering, scope creep — before the commit lands.

---

### 2. `h5i notes` — understand what Claude actually did

After a Claude Code session, `h5i notes analyze` parses the conversation log and stores structured metadata linked to the commit.

```bash
h5i notes analyze        # index the latest session
h5i notes footprint      # which files did Claude read vs. edit?
h5i notes uncertainty    # where was Claude unsure?
h5i notes omissions      # what did Claude defer, stub, or promise but not deliver?
h5i notes coverage       # which files were edited without being read first?
h5i notes review         # ranked list of commits that most need human review
```

**Footprint** reveals the implicit dependencies Git's diff never captures:

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

**Uncertainty** surfaces every moment Claude hedged, with confidence score and the exact quote:

```
── Uncertainty Heatmap ─────────────────────────────────────────────────
  7 signals  ·  3 files

  src/auth.rs    ████████████░░░░  ●●●  4 signals  avg 28%
  src/main.rs    ██████░░░░░░░░░░  ●●   2 signals  avg 40%
  src/server.rs  ██░░░░░░░░░░░░░░  ●    1 signal   avg 52%

  ██ t:32   not sure    src/auth.rs  [25%]
       "…token validation might break if the token contains special chars…"

  ▓▓ t:220  let me check  src/main.rs  [45%]
       "…The LSP shows the match still isn't seeing the new arm. Let me check…"
```

**Omissions** surface what Claude left incomplete — extracted from its own thinking:

```
── Omission Report ─────────────────────────────────────────────
  5 signals  ·  2 deferrals  ·  2 placeholders  ·  1 unfulfilled promise

  ⏭ DEFERRAL    src/auth.rs · "for now"
       "…I'll hardcode the token TTL for now — a proper config value can be added later…"

  ⬜ PLACEHOLDER  src/auth.rs · "stub"
       "…this refresh handler is a stub; the actual token rotation logic isn't wired up yet…"

  💬 UNFULFILLED  src/auth.rs · "i'll also update"
     → promised file: src/auth/tests.rs  (never edited)
```

**Coverage** flags blind edits — files Claude modified without first reading:

```
  File                        Edits   Coverage   Blind edits
  src/auth.rs                     4       75%             1
  src/session.rs                  2        0%             2   ← review these
  src/main.rs                     1      100%             0
```

---

### 3. `h5i context` — give Claude a memory that survives session resets

Long-running tasks lose context when a session ends. The `h5i context` workspace is a version-controlled notepad that Claude reads at the start of each new session to restore its state.

```bash
# Claude runs this once at project start
h5i context init --goal "Build an OAuth2 login system"

# During the session — Claude logs its reasoning
h5i context trace --kind OBSERVE "Redis p99 latency is 2 ms"
h5i context trace --kind THINK   "40 MB overhead is acceptable"
h5i context trace --kind ACT     "Switching session store to Redis"

# After each meaningful milestone
h5i context commit "Implemented token refresh flow" \
  --detail "Handles 401s transparently; refresh token stored in HttpOnly cookie."

# At the start of every new session — Claude restores its state
h5i context show --trace
```

```
── Context ─────────────────────────────────────────────────
  Goal: Build an OAuth2 login system  (branch: main)

  Milestones:
    ✔ [x] Initial setup
    ✔ [x] GitHub provider integration
    ○ [ ] Token refresh flow  ← resume here

  Recent Trace:
    [ACT] Switching session store to Redis in src/session.rs
```

Use `h5i context branch` and `h5i context merge` to explore risky alternatives without losing the main thread — exactly like `git branch`. Run `h5i context prompt` to get a ready-made system prompt that tells Claude how to use these commands.

---

## Setup with Claude Code

Install hooks so the prompt is captured automatically on every `h5i commit` — no flags needed:

```bash
h5i hooks
# Prints the hook script and the settings.json snippet to register it.
# Follow the printed instructions to complete setup.
```

Then begin any session with a full situational briefing:

```bash
h5i resume
```

```
── Session Handoff ─────────────────────────────────────────────────
  Branch: feat/oauth  ·  Last active: 2026-03-27 14:22 UTC
  HEAD: a3f9c2b  implement token refresh flow

  Goal: Build an OAuth2 login system
  Progress: ✔ Initial setup  ✔ GitHub provider  ○ Token refresh  ○ Logout

  ⚠  High-Risk Files  (review before continuing)
    ██████████  src/auth.rs       4 uncertainty signals  churn 80%
    ██████░░░░  src/session.rs    2 signals  churn 60%

  Suggested Opening Prompt
  ─────────────────────────────────────────────────────────────────
  Continue building "Build an OAuth2 login system". Completed: Initial
  setup, GitHub provider. Next: Token refresh flow. Review src/auth.rs
  before editing — 4 uncertainty signals recorded in the last session.
  ─────────────────────────────────────────────────────────────────
```

No API call needed — every field comes from locally stored h5i data.

---

## Web Dashboard

```bash
h5i serve        # opens http://localhost:7150
```

<img src="./assets/screenshot_h5i_server.png" alt="h5i web dashboard — Timeline tab">

The **Timeline** tab shows every commit with its full AI context inline: model, agent, prompt, test badge, and a one-click **Re-audit** button. The **Sessions** tab visualizes footprint, uncertainty heatmap, and churn per commit.

---

## Documentation

See [MANUAL.md](MANUAL.md) for the complete command reference — commit flags, integrity rules, notes subcommands, context workspace, memory management, sharing with your team, and the web dashboard guide.

---

## License

Apache 2.0 — see [LICENSE](LICENSE).
