# h5i Manual

Complete reference for all h5i commands, configuration, and internals.

---

## Table of Contents

1. [Installation](#1-installation)
2. [Committing with AI Provenance](#2-committing-with-ai-provenance)
3. [Attaching Test Results](#3-attaching-test-results)
4. [Causal Commit Chains](#4-causal-commit-chains)
5. [Integrity Engine](#5-integrity-engine)
6. [Enriched Log and Blame](#6-enriched-log-and-blame)
7. [Intent-Based Rollback](#7-intent-based-rollback)
8. [CRDT Collaborative Sessions](#8-crdt-collaborative-sessions)
9. [Memory Management](#9-memory-management)
10. [Session Log Analysis (h5i notes)](#10-session-log-analysis-h5i-notes)
11. [Context Workspace (h5i context)](#11-context-workspace-h5i-context)
12. [Web Dashboard](#12-web-dashboard)
13. [Claude Code Hooks](#13-claude-code-hooks)
14. [Sharing h5i Data with Your Team](#14-sharing-h5i-data-with-your-team)
15. [Storage Layout](#15-storage-layout)
16. [Session Handoff (h5i resume)](#16-session-handoff-h5i-resume)

---

## 1. Installation

Requires Rust 1.70+:

```bash
cargo install --git https://github.com/Koukyosyumei/h5i h5i-core
```

From a local clone:

```bash
git clone https://github.com/koukyosyumei/h5i
cd h5i
cargo install --path .
```

Initialize h5i in any Git repository:

```bash
cd your-project
h5i init
# → h5i sidecar initialized at .git/.h5i
```

---

## 2. Committing with AI Provenance

```bash
h5i commit -m "implement rate limiting" \
  --prompt "add per-IP rate limiting to the auth endpoint" \
  --model claude-sonnet-4-6 \
  --agent claude-code
```

**Flag resolution order:** CLI flag → environment variable → pending context file (written by the Claude Code hook).

| Flag | Env var | Description |
|------|---------|-------------|
| `--prompt` | `H5I_PROMPT` | The user prompt that triggered this commit |
| `--model` | `H5I_MODEL` | Model name (e.g. `claude-sonnet-4-6`) |
| `--agent` | `H5I_AGENT_ID` | Agent identifier (e.g. `claude-code`) |
| `--caused-by` | — | OID of a commit that causally triggered this one (repeatable) |
| `--test-results` | `H5I_TEST_RESULTS` | Path to a JSON test results file |
| `--test-cmd` | — | Shell command whose stdout produces test results JSON |
| `--tests` | — | Scan staged files for inline `h5_i_test_start`/`h5_i_test_end` markers |
| `--ast` | — | Capture AST snapshot for semantic blame |
| `--audit` | — | Run integrity rules before committing |
| `--force` | — | Commit despite integrity warnings (violations still block) |

With `h5i hooks` installed, `--prompt` is captured automatically from your Claude Code conversation. See §13.

---

## 3. Attaching Test Results

h5i supports three ways to record test metrics alongside a commit.

**Option A — pre-computed results file** (recommended for CI):

```bash
python script/h5i-pytest-adapter.py > /tmp/results.json
h5i commit -m "add login tests" --test-results /tmp/results.json

# Via environment variable
export H5I_TEST_RESULTS=/tmp/results.json
h5i commit -m "add login tests"
```

**Option B — inline test command**:

```bash
h5i commit -m "add login tests" \
  --test-cmd "python script/h5i-pytest-adapter.py"
```

**Option C — inline markers** (language-agnostic fallback):

```bash
h5i commit -m "add login tests" --tests
# Scans staged files for // h5_i_test_start … // h5_i_test_end blocks
```

### Bundled adapters

`script/h5i-pytest-adapter.py` — runs pytest, uses `pytest-json-report` when available, falls back to output parsing:

```bash
pip install pytest pytest-json-report
python script/h5i-pytest-adapter.py
```

`script/h5i-cargo-test-adapter.sh` — runs `cargo test`, accumulates counts across lib/integration/doc-test sections:

```bash
bash script/h5i-cargo-test-adapter.sh
```

### Writing your own adapter

Produce a JSON file matching this schema and pass it via `--test-results`:

```json
{
  "tool":          "jest",
  "passed":        42,
  "failed":        1,
  "skipped":       3,
  "total":         46,
  "duration_secs": 4.7,
  "coverage":      0.87,
  "exit_code":     1,
  "summary":       "42 passed, 1 failed, 3 skipped in 4.70s"
}
```

All fields are optional. `exit_code` takes precedence over counts when determining pass/fail; `total` is computed from counts if omitted.

---

## 4. Causal Commit Chains

Declare which earlier commits causally triggered the current one:

```bash
h5i commit -m "fix off-by-one in validate_token" \
  --caused-by a3f9c2b \
  --prompt "fix the bug introduced by the rate limiter"

# Multiple causes
h5i commit -m "unify auth flow" \
  --caused-by a3f9c2b \
  --caused-by d4e5f6a
```

Abbreviated OIDs are resolved at commit time; invalid OIDs are rejected.

The causal link surfaces in `h5i log`:

```
commit b2f3a1c...
Author:    Alice <alice@example.com>
Agent:     claude-code (claude-sonnet-4-6)
Caused by: a3f9c2b "implement rate limiting"
Prompt:    "fix the off-by-one in validate_token"
Tests:     ✔ 42 passed, 0 failed, 1.23s [pytest]
```

**Rollback cascade warning** — when rolling back a commit, h5i scans recent history for any commits that declared it as a cause:

```
⚠ Warning: 2 later commits causally depend on this one:
  → b2f3a1c "fix bug introduced by rate limiter"
  → c3d4e5f "add test coverage for rate limiter"
Continue anyway? [y/N]
```

Use `--yes` to skip the prompt in CI.

---

## 5. Integrity Engine

The `--audit` flag (and the dashboard's per-commit audit button) runs twelve deterministic rules against the diff. Rules are pure string/stat checks — no AI, no network.

| Rule | Severity | Trigger |
|------|----------|---------|
| `CREDENTIAL_LEAK` | **Violation** | Added line contains credential keyword + assignment + quoted value, or PEM header |
| `CODE_EXECUTION` | **Violation** | Added non-comment line contains `eval()`, `exec()`, `os.system()`, `subprocess.*`, etc. |
| `CI_CD_MODIFIED` | **Violation** | `.github/workflows/`, `Jenkinsfile`, etc. modified without CI/CD intent in prompt |
| `SENSITIVE_FILE_MODIFIED` | Warning | `.env`, `.pem`, `.key`, `id_rsa`, `credentials` in diff |
| `LOCKFILE_MODIFIED` | Warning | `Cargo.lock`, `package-lock.json`, `go.sum` changed without dependency intent |
| `UNDECLARED_DELETION` | Warning | >60% of changes are deletions with no deletion/refactor intent stated |
| `SCOPE_EXPANSION` | Warning | Prompt names a specific file but other source files were also modified |
| `LARGE_DIFF` | Warning | >500 total lines changed |
| `REFACTOR_ANOMALY` | Warning | "refactor" intent but insertions are 3× or more the deletions |
| `PERMISSION_CHANGE` | Warning | `chmod 777`, `sudo`, `setuid`, `chown root` in added lines |
| `BINARY_FILE_CHANGED` | Info | Binary file appears in diff |
| `CONFIG_FILE_MODIFIED` | Info | `.yaml`, `.toml`, `.json`, `.ini` etc. modified |

**Why rule-based?** AI-generated code should be audited by deterministic rules that humans can read and reason about. A fuzzy ML classifier would itself be a trust problem.

To add a rule: add a `pub const` to `rule_id` in `src/rules.rs`, write one pure `fn check_*(ctx: &DiffContext) -> Vec<RuleFinding>` function, and register it in `run_all_rules`. No other changes needed.

---

## 6. Enriched Log and Blame

```bash
h5i log --limit 5
h5i blame src/auth.rs
h5i blame src/auth.rs --mode ast   # AST-level semantic blame
```

`h5i blame` shows two status columns before each line:

- Test status: `✅` (passing), `✖` (failing), blank (no data)
- AI indicator: `✨` (AI-authored line)

---

## 7. Intent-Based Rollback

```bash
h5i rollback "the OAuth login changes"
h5i rollback "rate limiting" --dry-run   # preview without reverting
h5i rollback "the broken migration" --yes  # skip confirmation in CI
```

h5i matches the description against stored prompts and commit messages using Claude for semantic search, or keyword matching if `ANTHROPIC_API_KEY` is not set.

---

## 8. CRDT Collaborative Sessions

```bash
h5i session --file src/auth.rs
# → Watching for changes... (Press Ctrl+C to stop)

h5i resolve <ours-oid> <theirs-oid> src/auth.rs
```

Each agent gets its own session; changes merge via Yjs CRDT automatically. `h5i resolve` reconstructs the conflict-free state from Git Notes — no interactive merge editor required.

---

## 9. Memory Management

h5i versions Claude Code's persistent memory files alongside your code, so every commit can carry an exact record of what the AI "knew" when it made that change.

Claude Code stores per-project memory in `~/.claude/projects/<repo-path>/memory/`. These files are local-only and unversioned by default. h5i fixes that.

```bash
# Snapshot current memory state
h5i memory snapshot
h5i memory snapshot --commit a3f9c2b   # tie to a specific commit

# View history
h5i memory log

# Diff memory across commits
h5i memory diff                    # last snapshot → live memory
h5i memory diff a3f9c2b b2f3a1c    # between two snapshots
h5i memory diff a3f9c2b            # snapshot → live

# Restore
h5i memory restore a3f9c2b    # interactive confirmation
h5i memory restore a3f9c2b -y # skip prompt

# Share with team
h5i memory push
h5i memory pull
```

Example diff output:

```
memory diff a3f9c2b..b2f3a1c
────────────────────────────────────────────────────────────
  added     project_auth.md
    +  The auth middleware rewrite is driven by legal compliance
    +  requirements around session token storage.
  modified  feedback_tests.md
    -Why: prior incident where mocks masked a broken migration.
    +Why: prior incident where mocks masked a broken migration.
    +How to apply: always use a real DB in integration tests.
────────────────────────────────────────────────────────────
  1 added, 0 removed, 1 modified
```

Memory snapshots are backed by real git objects stored under `refs/h5i/memory`. See §14 for the team sharing workflow.

---

## 10. Session Log Analysis (h5i notes)

Claude Code stores a detailed JSONL log of every conversation in `~/.claude/projects/<repo>/`. h5i parses these logs and extracts structured metadata stored in `.git/.h5i/session_log/<commit-oid>/analysis.json`.

### Subcommands

| Command | Description |
|---------|-------------|
| `h5i notes analyze [--session <path>] [--commit <oid>]` | Parse a session log and link it to a commit |
| `h5i notes show [--commit <oid>]` | Show stored analysis |
| `h5i notes footprint [<oid>]` | Exploration footprint: files read vs edited |
| `h5i notes uncertainty [--commit <oid>] [--file <path>]` | Uncertainty heatmap |
| `h5i notes omissions [--commit <oid>] [--file <path>]` | Omission report: deferrals, placeholders, unfulfilled promises |
| `h5i notes churn [--limit N]` | Per-file edit churn scores |
| `h5i notes graph [--limit N] [--mode <mode>]` | Intent graph |
| `h5i notes review [--limit N] [--min-score F] [--json]` | Review summary |

### Exploration footprint

Every file the AI *read* before making a change is recorded, revealing the implicit dependencies that Git's diff never captures. *Implicit dependencies* (read but not edited) are the most actionable output: these are files the agent had to understand to make the change.

### Uncertainty heatmap

When Claude expresses uncertainty — phrases like "not sure", "let me check", "this might break" — h5i records the surrounding context and the file being edited at that moment. Confidence scores: **red** (<35%) = very uncertain, **yellow** (35–55%) = moderate, **green** (>55%) = incidental mention.

### Omission report

`h5i notes omissions` detects three categories of incomplete work that Claude left behind, extracted from its thinking blocks:

| Kind | Badge | What it catches |
|------|-------|-----------------|
| **Deferral** | `⏭` red | Claude acknowledged something needed doing but explicitly chose to skip or defer it (`"for now"`, `"out of scope"`, `"separate PR"`, `"leave this for later"`, …) |
| **Placeholder** | `⬜` yellow | Claude inserted an admittedly incomplete implementation (`"stub"`, `"placeholder"`, `"hardcoded for now"`, `"simplified version"`, `"workaround"`, …) |
| **Unfulfilled promise** | `💬` cyan | Claude stated in thinking `"I'll also update X"` or `"I should also add Y"` but the corresponding file never appeared in the edit sequence |

```bash
h5i notes omissions                          # all omissions for HEAD
h5i notes omissions --commit a3f8c12         # for a past commit
h5i notes omissions --file src/auth.rs       # filter to one file
```

Example output:

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

  ⏭ DEFERRAL      src/auth.rs · t:29 · "out of scope"
       "…proper error propagation is out of scope for this change, skipping for now…"

  ⬜ PLACEHOLDER   src/auth.rs · t:52 · "stub"
       "…this refresh handler is a stub; the actual token rotation logic isn't wired up yet…"

  ⬜ PLACEHOLDER   src/session.rs · t:55 · "hardcoded for now"
       "…session timeout is hardcoded for now at 3600s, should come from config…"

  💬 UNFULFILLED   src/auth.rs · t:61 · "i'll also update"
     → promised file: src/auth/tests.rs
       "…i'll also update src/auth/tests.rs to cover the new TTL behaviour…"

  ■ red=deferral  ■ yellow=placeholder  ■ cyan=unfulfilled promise
```

**Unfulfilled promise cross-check:** when a promise phrase names a file path (e.g. `"i'll also update src/auth/tests.rs"`), h5i checks whether that file actually appears in the session's edit sequence. If it does not, the omission is flagged. When no file path can be extracted the promise phrase is flagged unconditionally for manual review.

**Recommended use:** run `h5i notes omissions` immediately after `h5i notes uncertainty`. Uncertainty tells you where Claude was unsure; omissions tell you what Claude left incomplete. Together they form a targeted review checklist — without reading the full diff.

### File churn

Churn measures how many edits a file received relative to how many times it was read — a proxy for rework and fragility beyond line-count metrics. High churn means trial-and-error rather than confident, planned changes.

### Replay hash

Each analysis stores a SHA-256 hash of the raw JSONL content. Given the same model, the same JSONL, and the same starting state, the commit should be reproducible. Teams can store session files in a separate artifact store and reference them by commit OID.

---

## 11. Context Workspace (h5i context)

Long-running AI agent sessions suffer from context-window overflow. The `h5i context` workspace (based on arXiv:2508.00031) maintains a version-controlled memory workspace at `.h5i-ctx/` with commands that mirror Git's own model.

### Workspace layout

```
.h5i-ctx/
├── main.md               ← global roadmap: project goal, milestones, progress notes
└── branches/
    └── <branch>/
        ├── commit.md     ← milestone summaries (append-only, rolling)
        ├── trace.md      ← OTA (Observation–Thought–Action) execution trace
        └── metadata.yaml ← file structure, dependencies, env config
```

### Subcommands

| Command | Description |
|---------|-------------|
| `h5i context init --goal <text>` | Create workspace with initial goal |
| `h5i context commit <summary> --detail <text>` | Save a milestone checkpoint |
| `h5i context branch <name> --purpose <text>` | Create and switch to a new branch |
| `h5i context checkout <name>` | Switch to an existing branch |
| `h5i context merge <branch>` | Merge a branch's log and summaries into current |
| `h5i context show [--branch B] [--commit H] [--trace] [--metadata S] [--window N] [--trace-offset N]` | Retrieve working context |
| `h5i context trace --kind <KIND> <content>` | Append an OTA trace entry |
| `h5i context status` | Quick overview: branch, commit count, trace lines |
| `h5i context prompt` | Print a Claude system prompt that explains how to use these commands |

### `h5i context show` flags

| Flag | Description |
|------|-------------|
| `--branch <name>` | Show context for a specific branch without switching |
| `--commit <hash>` | Pull a specific commit entry by hash prefix |
| `--trace` | Include recent OTA trace lines |
| `--metadata <segment>` | Pull a named section from `metadata.yaml` |
| `--window <N>` | Number of recent commits to include (default: 3) |
| `--trace-offset <N>` | Scroll back N lines from the end of the trace (sliding window) |

### Trace entry kinds

Valid `--kind` values: `OBSERVE`, `THINK`, `ACT`, `NOTE` (case-insensitive).

### Suggested agent workflow

```
# Session start
h5i context show --trace       # restore working state

# During session
h5i context trace --kind OBSERVE "..."
h5i context trace --kind THINK   "..."
h5i context trace --kind ACT     "..."

# Before exploring a risky alternative
h5i context branch experiment/alt-approach --purpose "..."

# After a meaningful milestone
h5i context commit "Short summary" --detail "Full explanation"

# Session end (after merging any branches)
h5i context status
```

---

## 12. Web Dashboard

```bash
h5i serve            # http://localhost:7150
h5i serve --port 8080
```

**Timeline** — full commit history with colored test-status borders, AI prompt detail panels, and a per-card `🛡 Audit` button that runs all twelve integrity rules inline.

**Summary** — aggregate stats, agent leaderboard, and filter pills (`🤖 AI only`, `🧪 With tests`, `✖ Failing`).

**Integrity** — manually audit any commit message + prompt against the rule engine without committing.

**Intent Graph** — directed graph of commit causal chains.

**Memory** — browse and diff Claude memory snapshots linked to each commit.

**Sessions** — per-commit session log data: exploration footprint, causal chain, uncertainty heatmap, and churn bars.

<img src="./assets/screenshot_h5i_server.png" alt="h5i server">

---

## 13. Claude Code Hooks

h5i can capture your prompt automatically every time you submit a message to Claude Code:

```bash
h5i hooks
```

This prints:
1. A shell script to save at `~/.claude/hooks/h5i-capture-prompt.sh`
2. The exact `~/.claude/settings.json` snippet to register the hook

After setup, the prompt flows: conversation → `.git/.h5i/pending_context.json` → consumed and cleared by the next `h5i commit`. No flags, no copy-paste.

**Environment variable fallback** (works without hooks, or with any AI agent):

```bash
export H5I_PROMPT="implement rate limiting on the auth endpoint"
export H5I_MODEL="claude-sonnet-4-6"
export H5I_AGENT_ID="claude-code"
h5i commit -m "add rate limiting"
```

---

## 14. Sharing h5i Data with Your Team

h5i stores its data in two git refs that are **not** included in a normal `git push`:

| Ref | Contains |
|-----|----------|
| `refs/h5i/notes` | AI provenance, test metrics, causal links, integrity reports |
| `refs/h5i/memory` | Claude memory snapshots |

### Push and pull

```bash
h5i push                   # push both refs to origin
h5i push --remote upstream

# Pull manually
git fetch origin refs/h5i/notes:refs/h5i/notes
git fetch origin refs/h5i/memory:refs/h5i/memory

# Or use the memory subcommand
h5i memory pull
```

### Automating with CI/CD

Add to your CI push step:

```yaml
# GitHub Actions
- name: Push h5i metadata
  run: |
    git push origin refs/h5i/notes
    git push origin refs/h5i/memory
```

Add fetch refspecs to `.git/config` so `git pull` picks them up automatically:

```ini
[remote "origin"]
    url = git@github.com:you/repo.git
    fetch = +refs/heads/*:refs/remotes/origin/*
    fetch = +refs/h5i/notes:refs/h5i/notes
    fetch = +refs/h5i/memory:refs/h5i/memory
```

### Full team workflow

```bash
# — Alice —
h5i commit -m "add rate limiting" --prompt "..." --agent claude-code
h5i memory snapshot
h5i push
git push origin main

# — Bob —
git pull                            # fetches code + notes + memory (with refspecs configured)
h5i log                             # sees Alice's AI provenance
h5i memory pull
h5i memory restore <alice-commit>   # apply Alice's Claude memory to Bob's Claude session
```

---

## 15. Storage Layout

h5i stores all metadata as a Git sidecar — nothing lives outside your repository.

```
.git/
└── .h5i/
    ├── ast/                        # SHA-256-keyed S-expression AST snapshots
    ├── crdt/                       # Yjs CRDT document state
    ├── delta/                      # Append-only CRDT update logs (per file)
    ├── memory/                     # Claude memory snapshots (one dir per commit OID)
    │   └── <commit-oid>/
    │       ├── MEMORY.md
    │       ├── feedback_tests.md
    │       └── _meta.json          # snapshot timestamp + file count
    ├── session_log/                # AI session log analyses (one dir per commit OID)
    │   └── <commit-oid>/
    │       └── analysis.json
    └── pending_context.json        # Transient: consumed at next commit

.h5i-ctx/                           # Context workspace (h5i context)
├── main.md
└── branches/
    └── <branch>/
        ├── commit.md
        ├── trace.md
        └── metadata.yaml
```

**`refs/h5i/notes`** stores extended commit metadata — AI provenance, test metrics, causal links, integrity reports — as JSON blobs attached to each commit. Inspect any entry with:

```bash
git notes --ref refs/h5i/notes show <commit-oid>
```

**`refs/h5i/memory`** stores Claude memory snapshots as a linear commit history of git tree objects. Each memory commit carries the linked code-commit OID in its message.

> Neither `refs/h5i/notes` nor `refs/h5i/memory` is pushed or fetched by a plain `git push` / `git pull`. You must share them explicitly — see §14.

### Module overview

| Module | Description |
|--------|-------------|
| `repository.rs` | Core hub — `H5iRepository` wraps `git2::Repository`, orchestrates all five dimensions |
| `session.rs` | `LocalSession` — per-file Yrs CRDT documents for collaborative editing |
| `delta_store.rs` | Append-only binary log for CRDT updates, keyed by `sha256(file_path)` |
| `metadata.rs` | Data types: `H5iCommitRecord`, `AiMetadata`, `TestMetrics`, `IntegrityReport` |
| `ast.rs` | `SemanticAst` (S-expressions), `AstDiff`, similarity scoring, SHA-256 structure hashing |
| `blame.rs` | Line and AST blame modes with AI metadata and test results |
| `ctx.rs` | Context workspace implementing arXiv:2508.00031 |
| `memory.rs` | Claude memory snapshot/diff/log/restore/push/pull |
| `session_log.rs` | Claude Code JSONL log parsing → footprint, causal chain, uncertainty, omissions, churn |
| `resume.rs` | Session handoff briefing assembled from all locally stored h5i data |
| `watcher.rs` | File system watcher (notify crate) → syncs to CRDT session |
| `rules.rs` | Twelve deterministic integrity rules |
| `server.rs` | Embedded web dashboard (HTML/JS served from Rust) |
| `error.rs` | Error categories mirroring the five dimensions |

---

## 16. Session Handoff (h5i resume)

`h5i resume` generates a structured briefing from locally stored h5i data so that an AI agent — or a human — can pick up exactly where the last session left off. No AI API call is required; every field is assembled from your repository.

```bash
h5i resume              # briefing for the current branch
h5i resume feat/oauth   # briefing for a specific branch
```

### What the briefing shows

| Section | Source |
|---------|--------|
| **Branch / HEAD commit** | `git2` + h5i commit record (agent, model) |
| **Goal & milestone progress** | Context workspace (`.h5i-ctx/main.md`) |
| **Last session statistics** | Session log analysis (`.git/.h5i/session_log/<oid>/analysis.json`) |
| **High-risk files** | Weighted risk score across uncertainty signals + churn |
| **Causal exposure** | Causal chain stored in Git Notes |
| **Memory changes** | Diff between memory snapshots (`h5i memory diff`) |
| **Suggested opening prompt** | Template-generated from goal + pending milestones + risky files |

### Risk score formula

Each file is ranked by a composite score:

```
risk = 0.4 × (1 − avg_confidence) + 0.3 × churn_score + 0.3 × (signal_count / max_signal_count)
```

- `avg_confidence` — mean confidence of uncertainty annotations for that file (lower = riskier)
- `churn_score` — edit / (edit + read) ratio from the last session
- `signal_count / max_signal_count` — relative frequency of uncertainty signals

Files are sorted by `risk_score` descending and the top 5 are shown.

### Example output

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

  Recent Context Commits
    ◈ Implemented token refresh flow

  Suggested Opening Prompt
  ────────────────────────────────────────────────────────────────────
  Continue building "Build an OAuth2 login system". Completed so far:
  Initial setup, GitHub provider integration. Next milestone: Token
  refresh flow. Review src/auth.rs before editing — 4 uncertainty
  signals were recorded there in the last session.
  ────────────────────────────────────────────────────────────────────
```

### Prerequisites

The briefing is richer when other h5i features have been used:

| Feature needed | How to enable |
|----------------|---------------|
| Goal + milestones | `h5i context init --goal "..."` |
| Last session stats + risky files | `h5i notes analyze` after each session |
| Memory changes | `h5i memory snapshot` after each session |
| Agent / model in header | `h5i commit --agent ... --model ...` (or hook) |

If none of these are set up, `h5i resume` still shows branch, HEAD commit, and the suggested prompt — prompting you to initialize the missing features.

### Recommended workflow

```bash
# End of every session
h5i notes analyze                          # store session analysis
h5i memory snapshot -m "end of session"   # checkpoint memory

# Start of every new session
h5i resume                                 # get the full briefing
```

---

## Demo Repository

`examples/dnn-from-scratch` (also at [github.com/Koukyosyumei/dnn-from-scratch](https://github.com/Koukyosyumei/dnn-from-scratch)) is a fully-connected neural network built entirely with Claude Code and version-controlled with h5i.

```bash
# Show h5i log, blame, and run the XOR demo
bash examples/dnn-from-scratch/demo.sh --inspect

# Replay the full build from scratch
bash examples/dnn-from-scratch/demo.sh
```
