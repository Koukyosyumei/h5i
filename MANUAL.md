# h5i Manual

Command reference for all h5i subcommands and flags.

---

## Table of Contents

- [Installation](#installation)
- [h5i init](#h5i-init)
- [h5i hooks](#h5i-hooks)
- [h5i commit](#h5i-commit)
- [h5i log](#h5i-log)
- [h5i blame](#h5i-blame)
- [h5i rollback](#h5i-rollback)
- [h5i notes](#h5i-notes)
  - [h5i notes analyze](#h5i-notes-analyze)
  - [h5i notes show](#h5i-notes-show)
  - [h5i notes footprint](#h5i-notes-footprint)
  - [h5i notes uncertainty](#h5i-notes-uncertainty)
  - [h5i notes omissions](#h5i-notes-omissions)
  - [h5i notes coverage](#h5i-notes-coverage)
  - [h5i notes churn](#h5i-notes-churn)
  - [h5i notes graph](#h5i-notes-graph)
  - [h5i notes review](#h5i-notes-review)
- [h5i context](#h5i-context)
  - [h5i context init](#h5i-context-init)
  - [h5i context show](#h5i-context-show)
  - [h5i context trace](#h5i-context-trace)
  - [h5i context commit](#h5i-context-commit)
  - [h5i context branch](#h5i-context-branch)
  - [h5i context checkout](#h5i-context-checkout)
  - [h5i context merge](#h5i-context-merge)
  - [h5i context status](#h5i-context-status)
  - [h5i context prompt](#h5i-context-prompt)
- [h5i memory](#h5i-memory)
  - [h5i memory snapshot](#h5i-memory-snapshot)
  - [h5i memory log](#h5i-memory-log)
  - [h5i memory diff](#h5i-memory-diff)
  - [h5i memory restore](#h5i-memory-restore)
  - [h5i memory push](#h5i-memory-push)
  - [h5i memory pull](#h5i-memory-pull)
- [h5i resume](#h5i-resume)
- [h5i vibe](#h5i-vibe)
- [h5i serve](#h5i-serve)
- [h5i mcp](#h5i-mcp)
- [h5i push](#h5i-push)
- [h5i pull](#h5i-pull)
- [h5i session](#h5i-session)
- [h5i resolve](#h5i-resolve)
- [Appendix: Storage Layout](#appendix-storage-layout)
- [Appendix: Integrity Rules](#appendix-integrity-rules)
- [Appendix: Test Adapter Schema](#appendix-test-adapter-schema)

---

## Installation

Requires Rust 1.70+.

```bash
# From crates.io (via git)
cargo install --git https://github.com/Koukyosyumei/h5i h5i-core

# From a local clone
git clone https://github.com/Koukyosyumei/h5i
cd h5i && cargo install --path .
```

---

## h5i init

```
h5i init
```

Initialize h5i in the current Git repository. Creates `.git/.h5i/` with subdirectories for AST snapshots, CRDT state, session logs, and memory snapshots.

Must be run once per repository before any other h5i command.

```bash
cd your-project
h5i init
# → h5i sidecar initialized at .git/.h5i
```

---

## h5i hooks

```
h5i hooks
```

Print setup instructions for the Claude Code prompt-capture hook.

Running this command outputs:
1. A shell script to save at `~/.claude/hooks/h5i-capture-prompt.sh`
2. The `~/.claude/settings.json` snippet to register it

After setup, every user message submitted to Claude Code is written to `.git/.h5i/pending_context.json`. The next `h5i commit` consumes and clears this file, recording the prompt automatically — no `--prompt` flag needed.

---

## h5i commit

```
h5i commit -m <message> [options]
```

Create a Git commit and store AI provenance metadata in `refs/h5i/notes`.

Flag resolution order: CLI flag → environment variable → pending context file (written by the Claude Code hook).

**Options**

| Option | Env var | Description |
|--------|---------|-------------|
| `-m, --message <text>` | — | Commit message (required) |
| `--prompt <text>` | `H5I_PROMPT` | The user prompt that triggered this commit. Auto-captured when the hook is installed. |
| `--model <name>` | `H5I_MODEL` | Model name, e.g. `claude-sonnet-4-6` |
| `--agent <id>` | `H5I_AGENT_ID` | Agent identifier, e.g. `claude-code` |
| `--decisions <file>` | — | Path to a JSON file of structured design decisions (see below) |
| `--caused-by <oid>` | — | OID of a commit that causally triggered this one. Repeatable. |
| `--test-results <file>` | `H5I_TEST_RESULTS` | Path to a JSON test results file (see [Appendix: Test Adapter Schema](#appendix-test-adapter-schema)) |
| `--test-cmd <cmd>` | — | Shell command whose stdout produces a test results JSON object |
| `--tests` | — | Scan staged files for inline `h5_i_test_start` / `h5_i_test_end` markers |
| `--ast` | — | Capture an AST snapshot for semantic blame |
| `--audit` | — | Run integrity rules before committing (see [Appendix: Integrity Rules](#appendix-integrity-rules)) |
| `--force` | — | Commit despite integrity warnings. Violations always block regardless of this flag. |

**Example — basic commit with hooks**

```bash
# Prompt is captured automatically from the Claude Code session
h5i commit -m "add rate limiting"
```

```
✔  Committed a3f9c2b  add rate limiting
   model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
```

**Example — commit with test results and audit**

```bash
h5i commit -m "add login tests" \
  --test-cmd "python script/h5i-pytest-adapter.py" \
  --audit
```

**Example — causal chain**

Link a fix to the commit that introduced the bug:

```bash
h5i commit -m "fix off-by-one in validate_token" --caused-by a3f9c2b
```

When rolling back a commit, h5i warns if later commits declared it as a cause:

```
⚠ Warning: 2 later commits causally depend on this one:
  → b2f3a1c "fix bug introduced by rate limiter"
Continue anyway? [y/N]
```

**Example — design decisions**

Record which alternatives were considered and why the chosen approach was preferred:

```bash
cat > decisions.json << 'EOF'
[
  {
    "location": "src/session.rs:44",
    "choice": "Redis over in-process HashMap",
    "alternatives": ["in-process HashMap", "Memcached"],
    "reason": "survives process restarts; required for horizontal scaling"
  }
]
EOF

h5i commit -m "switch session store to Redis" --decisions decisions.json
```

Decisions are stored in `refs/h5i/notes` and shown in `h5i log`:

```
Decisions:
  ◆ src/session.rs:44  Redis over in-process HashMap
    alternatives: in-process HashMap, Memcached
    survives process restarts; required for horizontal scaling
```

Decision schema: array of objects. `location` and `choice` are required; `alternatives` and `reason` are optional but recommended.

```json
{
  "location":     "src/file.rs:42",
  "choice":       "the approach taken",
  "alternatives": ["option A", "option B"],
  "reason":       "why this was chosen"
}
```

---

## h5i log

```
h5i log [options]
```

Show commit history with full AI provenance inline.

**Options**

| Option | Description |
|--------|-------------|
| `--limit <n>` | Number of commits to show (default: all) |
| `--ancestry <file>:<line>` | Trace every commit that touched a specific line, annotated with its prompt |

**Example — recent commits**

```bash
h5i log --limit 3
```

```
● a3f9c2b  add rate limiting
  2026-03-27 14:02  Alice <alice@example.com>
  model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
  prompt: "add per-IP rate limiting to the auth endpoint"
  tests:  ✔ 42 passed, 0 failed, 1.23s [pytest]

● 9e21b04  fix off-by-one in parser
  2026-03-26 11:45  Bob <bob@example.com>
  (no AI metadata)
```

**Example — prompt ancestry for a specific line**

```bash
h5i log --ancestry src/auth.rs:42
```

```
── Prompt ancestry for src/auth.rs:42

  [1 of 3]  a3f9c2b  Alice · 2026-03-27 14:02 UTC
       line:    check_rate_limit(&ip, &config.rate_limit)
       prompt:  "add per-IP rate limiting to the auth endpoint"

  [2 of 3]  9e21b04  Bob · 2026-03-26 11:45 UTC
       line:    check_rate_limit(&ip)
       prompt:  (none recorded)

  [3 of 3]  4c8d2a1  Alice · 2026-03-20 09:10 UTC
       line:    true  // placeholder
       prompt:  "stub out the rate limiter"
```

---

## h5i blame

```
h5i blame <file> [options]
```

Show line-level authorship with AI provenance. Two status columns precede each line:

- Column 1 — test status: `✅` passing, `✖` failing, blank = no data
- Column 2 — AI indicator: `✨` AI-authored line

**Options**

| Option | Description |
|--------|-------------|
| `--mode <line\|ast>` | Blame mode. `line` (default) is traditional line blame; `ast` is semantic blame that tracks code structure through renames and reformatting. |
| `--show-prompt` | Annotate each commit boundary with the human prompt that triggered it |

**Example**

```bash
h5i blame src/auth.rs
```

```
STAT COMMIT   AUTHOR/AGENT    | CONTENT
✅✨  a3f9c2b  claude-code     | fn validate_token(tok: &str) -> bool {
✅✨  a3f9c2b  claude-code     |     tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit())
     9eff001  alice           | }
```

**Example — with prompt annotations**

```bash
h5i blame src/auth.rs --show-prompt
```

```
── commit a3f9c2b ── prompt: "add per-IP rate limiting to the auth endpoint" ──
✅✨  a3f9c2b  claude-code  | pub fn check_rate_limit(ip: IpAddr) -> bool {
── commit 9e21b04 ── (no prompt recorded) ──
     9e21b04  alice        | pub fn authenticate(token: &str) -> Result<User> {
```

---

## h5i rollback

```
h5i rollback <description> [options]
```

Revert a commit by matching a description against stored prompts and commit messages. No commit hash required.

Uses Claude for semantic matching when `ANTHROPIC_API_KEY` is set; falls back to keyword matching otherwise.

**Options**

| Option | Description |
|--------|-------------|
| `--dry-run` | Preview the matched commit without reverting |
| `--yes` | Skip the confirmation prompt (useful in CI) |

**Example**

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

---

## h5i notes

Parse Claude Code session logs and store enriched metadata linked to commits. Session logs are read from `~/.claude/projects/<repo>/`.

All `h5i notes` subcommands accept `--commit <oid>` to target a specific commit (default: HEAD).

---

### h5i notes analyze

```
h5i notes analyze [options]
```

Parse a Claude Code session log and store the analysis in `.git/.h5i/session_log/<commit-oid>/analysis.json`. Run this after each session before using any other `h5i notes` subcommand.

**Options**

| Option | Description |
|--------|-------------|
| `--session <path>` | Path to a specific JSONL session file. Defaults to the most recent log in `~/.claude/projects/<repo>/`. |
| `--commit <oid>` | Link the analysis to a specific commit (default: HEAD) |
| `--since <oid>` | Only analyze messages after the given commit's timestamp |

---

### h5i notes show

```
h5i notes show [--commit <oid>]
```

Print the raw stored analysis for a commit: session ID, message count, tool call count, files consulted and edited.

---

### h5i notes footprint

```
h5i notes footprint [--commit <oid>]
```

Show which files Claude read vs. edited, and which files were read but not edited (*implicit dependencies* — what Claude had to understand to make the change, which Git's diff never captures).

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

---

### h5i notes uncertainty

```
h5i notes uncertainty [options]
```

Show every moment Claude expressed uncertainty, with the exact quote, confidence score, and the file being edited at that moment.

Confidence scoring: **red** (<35%) = very uncertain, **yellow** (35–55%) = moderate, **green** (>55%) = incidental mention.

**Options**

| Option | Description |
|--------|-------------|
| `--commit <oid>` | Target a specific commit (default: HEAD) |
| `--file <path>` | Filter signals to a specific file |

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

  ░░ t:496  perhaps        src/server.rs  [52%]
       "…perhaps we should also handle the case where body is empty…"
```

---

### h5i notes omissions

```
h5i notes omissions [options]
```

Detect incomplete work Claude left behind, extracted from its thinking blocks. Three categories:

| Kind | Badge | Trigger phrases |
|------|-------|-----------------|
| **Deferral** | `⏭` | `"for now"`, `"out of scope"`, `"separate PR"`, `"leave this for later"` |
| **Placeholder** | `⬜` | `"stub"`, `"hardcoded for now"`, `"simplified version"`, `"workaround"` |
| **Unfulfilled promise** | `💬` | `"I'll also update X"` / `"I should also add Y"` where that file was never edited |

**Options**

| Option | Description |
|--------|-------------|
| `--commit <oid>` | Target a specific commit (default: HEAD) |
| `--file <path>` | Filter signals to a specific file |

```
── Omission Report ─────────────────────────────────────────────
  5 signals  ·  2 deferrals  ·  2 placeholders  ·  1 unfulfilled promise

  ⏭ DEFERRAL    src/auth.rs · t:18 · "for now"
       "…I'll hardcode the token TTL for now — a proper config value can be added later…"

  ⬜ PLACEHOLDER  src/session.rs · t:55 · "hardcoded for now"
       "…session timeout is hardcoded for now at 3600s, should come from config…"

  💬 UNFULFILLED  src/auth.rs · t:61 · "i'll also update"
     → promised file: src/auth/tests.rs  (never edited)
```

For unfulfilled promises that name a file path, h5i cross-checks whether that file appeared in the session's edit sequence. If it did not, the omission is flagged.

---

### h5i notes coverage

```
h5i notes coverage [options]
```

Show per-file attention coverage: the fraction of edits that were preceded by at least one Read in the same session. An edit with no prior Read is a **blind edit** — Claude modified the file without direct evidence it understood the current state.

**Options**

| Option | Description |
|--------|-------------|
| `--commit <oid>` | Target a specific commit (default: HEAD) |
| `--max-ratio <f>` | Only show files at or below this coverage ratio (0.0–1.0) |

```
── Attention Coverage — a3f9c2b

  File                    Edits   Coverage   Blind edits
  src/auth.rs                 4       75%             1
  src/session.rs              2        0%             2   ← review these
  src/main.rs                 1      100%             0

  2 file(s) with blind edits.
```

Files are sorted by blind edit count (most risky first). When coverage data is available, `h5i notes review` adds a `BLIND_EDIT` signal weighted at 0.10 per file (max contribution 0.30) to the review score.

---

### h5i notes churn

```
h5i notes churn [--limit <n>]
```

Show per-file churn: the edit-to-read ratio across all analyzed sessions. High churn indicates trial-and-error rather than confident, planned changes.

**Options**

| Option | Description |
|--------|-------------|
| `--limit <n>` | Number of files to show (default: all) |

---

### h5i notes graph

```
h5i notes graph [options]
```

Visualize the causal chain across commits — which AI commit triggered which.

**Options**

| Option | Description |
|--------|-------------|
| `--limit <n>` | Number of commits to include (default: 20) |
| `--mode <mode>` | Output mode (default: terminal graph) |

---

### h5i notes review

```
h5i notes review [options]
```

Print a ranked list of commits that most need human review, scored by a composite of uncertainty signals, churn, diff size, and blind edits.

**Options**

| Option | Description |
|--------|-------------|
| `--limit <n>` | Number of commits to scan (default: 50) |
| `--min-score <f>` | Only show commits at or above this score (0.0–1.0, default: 0.40) |
| `--json` | Output raw JSON instead of formatted text |

```
Suggested Review Points — 2 commits flagged
──────────────────────────────────────────────────────────────
  #1  a3f8c12  score 0.74  ████████░░
     Alice · 2026-03-27 14:02 UTC
     add retry logic to HTTP client
     ⚠ high uncertainty · BLIND_EDIT · 5 edits · 4 files touched

  #2  9e21b04  score 0.45  ████░░░░░░
     Bob · 2026-03-26 11:45 UTC
     refactor parser
     moderate complexity
```

---

## h5i context

A version-controlled reasoning workspace at `.h5i-ctx/` that survives session resets. Command structure mirrors Git. Based on [arXiv:2508.00031](https://arxiv.org/abs/2508.00031).

**Workspace layout**

```
.h5i-ctx/
├── main.md               ← global roadmap: goal, milestones, progress notes
└── branches/
    └── <branch>/
        ├── commit.md     ← milestone summaries (append-only)
        ├── trace.md      ← OTA (Observe–Think–Act) execution trace
        └── metadata.yaml ← file structure, dependencies, env config
```

**Recommended per-session workflow**

```bash
h5i context show --trace                        # session start: restore state
h5i context trace --kind OBSERVE "..."          # during: log observations
h5i context trace --kind THINK   "..."          # during: log reasoning
h5i context trace --kind ACT     "..."          # during: log actions
h5i context commit "Summary" --detail "..."     # after milestone: checkpoint
h5i context status                              # session end: overview
```

---

### h5i context init

```
h5i context init --goal <text>
```

Create the context workspace and set the project goal. Must be run once before other `h5i context` commands.

| Option | Description |
|--------|-------------|
| `--goal <text>` | The overall goal for this task or project (required) |

```bash
h5i context init --goal "Build an OAuth2 login system"
```

---

### h5i context show

```
h5i context show [options]
```

Print working context: goal, milestone progress, recent commits, and optionally the OTA trace.

**Options**

| Option | Description |
|--------|-------------|
| `--branch <name>` | Show context for a branch without switching to it |
| `--commit <hash>` | Pull a specific milestone entry by hash prefix |
| `--trace` | Include recent OTA trace lines |
| `--window <n>` | Number of recent milestone commits to include (default: 3) |
| `--trace-offset <n>` | Scroll back N lines from the end of the trace (sliding window) |
| `--metadata <segment>` | Pull a named section from `metadata.yaml` |

```
── Context ──────────────────────────────────────────────────
  Goal: Build an OAuth2 login system  (branch: main)

  Milestones:
    ✔ [x] Initial setup
    ✔ [x] GitHub provider integration
    ○ [ ] Token refresh flow  ← resume here

  Recent Commits:
    ◈ Implemented GitHub provider integration

  Recent Trace:
    [ACT] Switching session store to Redis in src/session.rs
    [NOTE] TODO: add integration test for the timeout path
```

---

### h5i context trace

```
h5i context trace --kind <KIND> <content>
```

Append a single OTA (Observe–Think–Act) entry to the trace log.

**Options**

| Option | Description |
|--------|-------------|
| `--kind <KIND>` | Entry type: `OBSERVE`, `THINK`, `ACT`, or `NOTE` (case-insensitive, required) |

```bash
h5i context trace --kind OBSERVE "Redis p99 latency is 2 ms under load"
h5i context trace --kind THINK   "40 MB overhead is acceptable given the scale"
h5i context trace --kind ACT     "Switched session store to Redis in src/session.rs"
h5i context trace --kind NOTE    "TODO: add integration test for the timeout path"
```

---

### h5i context commit

```
h5i context commit <summary> [--detail <text>]
```

Save a milestone checkpoint. Appended to `commit.md` on the current branch.

**Options**

| Option | Description |
|--------|-------------|
| `<summary>` | Short summary of the milestone (required, positional) |
| `--detail <text>` | Full explanation to store alongside the summary |

```bash
h5i context commit "Implemented token refresh flow" \
  --detail "Handles 401s transparently; refresh token stored in HttpOnly cookie."
```

---

### h5i context branch

```
h5i context branch <name> [--purpose <text>]
```

Create a new context branch and switch to it. Use this before exploring a risky alternative so the main thread is preserved.

**Options**

| Option | Description |
|--------|-------------|
| `<name>` | Branch name (required, positional) |
| `--purpose <text>` | One-line description of what this branch is exploring |

```bash
h5i context branch experiment/sync-session --purpose "try synchronous session store as fallback"
```

---

### h5i context checkout

```
h5i context checkout <name>
```

Switch to an existing context branch.

```bash
h5i context checkout main
```

---

### h5i context merge

```
h5i context merge <branch>
```

Merge a branch's commit log and trace into the current branch, then delete the merged branch.

```bash
h5i context merge experiment/sync-session
```

---

### h5i context status

```
h5i context status
```

Print a one-line overview: current branch, number of milestone commits, trace line count, and goal.

---

### h5i context prompt

```
h5i context prompt
```

Print a ready-made system prompt that can be prepended to a Claude session to give it full awareness of the `h5i context` commands and the recommended workflow.

---

## h5i memory

Version and share Claude Code's persistent memory files. Claude stores per-project memory in `~/.claude/projects/<repo-path>/memory/`. These files are local-only by default; `h5i memory` snapshots and versions them under `refs/h5i/memory`.

---

### h5i memory snapshot

```
h5i memory snapshot [options]
```

Snapshot the current state of Claude's memory files and store it as a git object linked to a commit.

**Options**

| Option | Description |
|--------|-------------|
| `--commit <oid>` | Link snapshot to a specific commit (default: HEAD) |
| `-m, --message <text>` | Optional annotation message |

```bash
h5i memory snapshot -m "end of session"
```

---

### h5i memory log

```
h5i memory log
```

List all memory snapshots in reverse chronological order, showing the linked commit OID, timestamp, file count, and annotation message.

---

### h5i memory diff

```
h5i memory diff [<from-oid> [<to-oid>]]
```

Show what changed between two memory snapshots.

| Form | Compares |
|------|----------|
| `h5i memory diff` | Last snapshot → live memory |
| `h5i memory diff <oid>` | Snapshot at `<oid>` → live memory |
| `h5i memory diff <oid-a> <oid-b>` | Snapshot at `<oid-a>` → snapshot at `<oid-b>` |

```
memory diff a3f9c2b..b2f3a1c
────────────────────────────────────────────────────────────
  added     project_auth.md
    +  The auth middleware rewrite is driven by legal compliance
    +  requirements around session token storage.
  modified  feedback_tests.md
    +How to apply: always use a real DB in integration tests.
────────────────────────────────────────────────────────────
  1 added, 0 removed, 1 modified
```

---

### h5i memory restore

```
h5i memory restore <oid> [options]
```

Restore Claude's memory files to the state captured in a snapshot. Prompts for confirmation by default.

**Options**

| Option | Description |
|--------|-------------|
| `<oid>` | Commit OID whose linked snapshot to restore (required, positional) |
| `-y, --yes` | Skip the confirmation prompt |

---

### h5i memory push

```
h5i memory push [--remote <name>]
```

Push `refs/h5i/memory` to the remote (default: `origin`).

---

### h5i memory pull

```
h5i memory pull [--remote <name>]
```

Fetch `refs/h5i/memory` from the remote (default: `origin`).

---

## h5i resume

```
h5i resume [<branch>]
```

Generate a session handoff briefing assembled entirely from local h5i data — no API call required. Prints branch state, goal, milestone progress, last session statistics, high-risk files, memory changes since the last snapshot, and a suggested opening prompt for Claude.

**Options**

| Option | Description |
|--------|-------------|
| `<branch>` | Branch to generate a briefing for (default: current branch) |

The briefing grows richer as more h5i features are active:

| Section | Requires |
|---------|----------|
| Goal + milestone progress | `h5i context init` |
| Last session stats + risky files | `h5i notes analyze` run after each session |
| Memory changes | `h5i memory snapshot` run after each session |
| Agent + model in header | Claude Code hook, or `H5I_MODEL` / `H5I_AGENT_ID` env vars |

If none of these are set up, `h5i resume` still shows branch, HEAD commit, and a suggested prompt.

**Risk score formula** used to rank high-risk files:

```
risk = 0.4 × (1 − avg_confidence) + 0.3 × churn_score + 0.3 × (signal_count / max_signal_count)
```

Top 5 files by risk score are shown.

**Recommended end-of-session checklist**

```bash
h5i notes analyze                        # index the session log
h5i memory snapshot -m "end of session"  # checkpoint memory
```

Then at the start of the next session:

```bash
h5i resume                               # get the full briefing
```

---

## h5i vibe

```
h5i vibe [OPTIONS]
```

Show an instant AI footprint audit of the repository: what fraction of recent commits are AI-generated, which directories are fully AI-written, and which files carry the highest risk.

**Options**

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --limit N` | `500` | Number of recent commits to scan |
| `--json` | off | Output raw JSON instead of the styled report |

**Output sections**

| Symbol | Section | Description |
|--------|---------|-------------|
| 🤖 | AI % | Fraction of scanned commits that carry AI provenance metadata |
| 👥 | Contributors | Count of distinct human authors and AI models; names listed below |
| 📁 | Fully AI dirs | Directories where every commit is AI-generated (minimum 2 commits) |
| 🔥 | Hot dirs | Directories with ≥ 80% AI commits (minimum 3 commits) |
| ⚠ | Blind edits | Total blind edits from all analysed sessions, and the number of affected files |
| 💀 | Risky files | Top 5 files with ≥ 70% AI commits **plus** at least one risk signal |

**Risky file signals**

A file is flagged when its AI commit ratio is ≥ 70% and at least one of:

- No passing test metrics found in any touching commit
- One or more blind edits (edits made without a prior Read in the same session)
- One or more uncertainty annotations expressed while editing

Files are ranked by a composite score:

```
score = 0.35 × ai_ratio
      + min(0.25, 0.08 × blind_edit_count)
      + min(0.20, 0.06 × uncertainty_count)
      + 0.35  (if no tests)
```

The blind-edit and uncertainty data come from session analyses stored by [`h5i notes analyze`](#h5i-notes-analyze). Files with no session data show only their AI commit ratio.

**Example output**

```
  Vibe Report  my-startup/backend
  ──────────────────────────────────────────────────────
  🤖  61% of 51 commits touched by AI
  👥  2 humans  ·  2 models
      claude-sonnet-4-6 (32), gpt-4o (10)
      Alice, Bob
  ──────────────────────────────────────────────────────
  📁  src/auth/  ← fully AI-written (8 commits, 0 human)
  🔥  src/api/   87% AI  (13/15 commits)
  ──────────────────────────────────────────────────────
  ⚠   23 blind edits across 7 files
  ──────────────────────────────────────────────────────
  💀  src/payment.rs  94% AI  no tests, 3 blind edits, 2 uncertainty flags
  💀  src/auth/token.rs  100% AI  no tests, 1 blind edit
  ──────────────────────────────────────────────────────
  ℹ scanned 51 commits
```

**JSON output (`--json`)**

```json
{
  "repo_name": "backend",
  "total_commits": 51,
  "ai_commits": 31,
  "ai_pct": 60.8,
  "human_authors": ["Alice", "Bob"],
  "ai_models": [["claude-sonnet-4-6", 32], ["gpt-4o", 10]],
  "total_blind_edits": 23,
  "blind_edit_file_count": 7
}
```

---

## h5i serve

```
h5i serve [options]
```

Start the web dashboard.

**Options**

| Option | Description |
|--------|-------------|
| `--port <n>` | Port to listen on (default: 7150) |

```bash
h5i serve         # → http://localhost:7150
h5i serve --port 8080
```

**Dashboard tabs**

| Tab | Content |
|-----|---------|
| **Timeline** | Full commit history with model, agent, prompt, test badge, and a one-click Re-audit button that runs all integrity rules inline |
| **Summary** | Aggregate stats, agent leaderboard, filter pills (AI only / with tests / failing) |
| **Integrity** | Manually audit any commit message + prompt against all 12 rules without committing |
| **Intent Graph** | Directed graph of causal commit chains |
| **Memory** | Browse and diff Claude memory snapshots linked to each commit |
| **Sessions** | Per-commit session data: exploration footprint, uncertainty heatmap, omissions, churn |

---

## h5i mcp

```
h5i mcp
```

Start the h5i MCP (Model Context Protocol) server on stdio. Any MCP client — including Claude Code — can connect to it to call h5i tools and read h5i resources directly without invoking the CLI.

The server implements the **2024-11-05** MCP specification over a newline-delimited JSON-RPC 2.0 stdio transport.

### Registering with Claude Code

Add the following entry to your `~/.claude/settings.json` (or the project-level `.claude/settings.json`):

```json
{
  "mcpServers": {
    "h5i": {
      "command": "h5i",
      "args": ["mcp"]
    }
  }
}
```

After restarting Claude Code, all h5i tools become available natively inside any session — no shell commands needed.

### Tools

| Tool | Equivalent CLI | Description |
|------|----------------|-------------|
| `h5i_log` | `h5i log` | Recent commits with AI provenance metadata |
| `h5i_blame` | `h5i blame` | Per-line or AST-node authorship with model/prompt annotation |
| `h5i_notes_show` | `h5i notes show` | Full session analysis for a commit |
| `h5i_notes_uncertainty` | `h5i notes uncertainty` | Uncertainty moments expressed during a session |
| `h5i_notes_coverage` | `h5i notes coverage` | Per-file blind-edit coverage |
| `h5i_notes_review` | `h5i notes review` | Commits ranked by review worthiness |
| `h5i_notes_churn` | `h5i notes churn` | Aggregate file churn across all sessions |
| `h5i_context_init` | `h5i context init` | Initialize the reasoning workspace |
| `h5i_context_trace` | `h5i context trace` | Append an OBSERVE/THINK/ACT/NOTE step |
| `h5i_context_commit` | `h5i context commit` | Checkpoint reasoning progress |
| `h5i_context_branch` | `h5i context branch` | Create a reasoning branch |
| `h5i_context_checkout` | `h5i context checkout` | Switch active reasoning branch |
| `h5i_context_merge` | `h5i context merge` | Merge a reasoning branch back into current |
| `h5i_context_show` | `h5i context show` | Full workspace state as JSON |
| `h5i_context_status` | `h5i context status` | Compact workspace summary |

**Tool parameters**

| Tool | Parameter | Type | Required | Default | Description |
|------|-----------|------|----------|---------|-------------|
| `h5i_log` | `limit` | integer | no | 20 | Max commits to return |
| `h5i_blame` | `file` | string | **yes** | — | Relative path to blame |
| `h5i_blame` | `mode` | `"line"` \| `"ast"` | no | `"line"` | Blame granularity |
| `h5i_notes_show` | `commit` | string | no | HEAD | Commit OID or prefix |
| `h5i_notes_uncertainty` | `commit` | string | no | HEAD | Commit OID or prefix |
| `h5i_notes_uncertainty` | `file` | string | no | — | Filter to a specific file path |
| `h5i_notes_coverage` | `commit` | string | no | HEAD | Commit OID or prefix |
| `h5i_notes_coverage` | `max_ratio` | number (0–1) | no | — | Only files at or below this read-before-edit ratio |
| `h5i_notes_review` | `limit` | integer | no | 50 | Commits to scan |
| `h5i_notes_review` | `min_score` | number (0–1) | no | 0.4 | Minimum review score |
| `h5i_context_init` | `goal` | string | no | — | Project goal for the reasoning session |
| `h5i_context_trace` | `kind` | `"OBSERVE"` \| `"THINK"` \| `"ACT"` \| `"NOTE"` | **yes** | — | Trace entry type |
| `h5i_context_trace` | `content` | string | **yes** | — | Content of the trace step |
| `h5i_context_commit` | `summary` | string | **yes** | — | One-line checkpoint summary |
| `h5i_context_commit` | `detail` | string | no | — | Extended description |
| `h5i_context_branch` | `name` | string | **yes** | — | Branch name (slashes allowed, e.g. `experiment/alt`) |
| `h5i_context_branch` | `purpose` | string | no | — | Why this branch is being explored |
| `h5i_context_checkout` | `name` | string | **yes** | — | Branch to switch to |
| `h5i_context_merge` | `branch` | string | **yes** | — | Branch to merge from |
| `h5i_context_show` | `branch` | string | no | current | Branch to inspect |
| `h5i_context_show` | `window` | integer | no | 3 | Recent checkpoints to include |
| `h5i_context_show` | `trace` | boolean | no | false | Include recent OTA trace lines |

### Resources

| URI | MIME type | Content |
|-----|-----------|---------|
| `h5i://context/current` | `application/json` | Live reasoning workspace state (goal, milestones, current branch, recent checkpoints, trace). Use this at session start instead of `h5i context prompt`. |
| `h5i://log/recent` | `application/json` | 10 most recent commits with AI provenance metadata and test metrics. |

Both resources support **live subscriptions** — see [Resource Subscriptions](#resource-subscriptions) below.

### Resource Subscriptions

The server declares `capabilities.resources.subscribe = true` in the `initialize` response. Clients can subscribe to any resource URI to receive push notifications when the content changes, without polling.

**Protocol flow**

```
# 1. Subscribe
→ { "jsonrpc": "2.0", "id": 1, "method": "resources/subscribe",
    "params": { "uri": "h5i://log/recent" } }
← { "jsonrpc": "2.0", "id": 1, "result": {} }

# 2. Server pushes when the resource changes (notification — no id)
← { "jsonrpc": "2.0", "method": "notifications/resources/updated",
    "params": { "uri": "h5i://log/recent" } }

# 3. Client re-reads to get updated content
→ { "jsonrpc": "2.0", "id": 2, "method": "resources/read",
    "params": { "uri": "h5i://log/recent" } }
← { "jsonrpc": "2.0", "id": 2, "result": { "contents": [...] } }

# 4. Unsubscribe when done
→ { "jsonrpc": "2.0", "id": 3, "method": "resources/unsubscribe",
    "params": { "uri": "h5i://log/recent" } }
← { "jsonrpc": "2.0", "id": 3, "result": {} }
```

**What triggers a notification**

| URI | Triggers when |
|-----|---------------|
| `h5i://log/recent` | A new `h5i commit` lands and HEAD advances |
| `h5i://context/current` | The reasoning workspace is updated (`h5i context commit`, `h5i context trace`, branch switch, etc.) |

**Implementation**

When a subscription is registered, h5i spawns a background polling thread (2-second interval) per URI. Each poll serialises the full `resources/read` response and compares it to the last-seen snapshot. If the content changed, the snapshot is updated and a `notifications/resources/updated` notification is pushed to stdout. Subscribing to an already-watched URI is idempotent — the existing thread is reused. Unsubscribing removes the URI from the watch map; the thread exits on its next poll.

**Error responses**

| Condition | JSON-RPC code | Message |
|-----------|---------------|---------|
| Missing `uri` param | `-32602` | `missing param: uri` |
| Unknown/non-subscribable URI | `-32602` | `not a subscribable resource: <uri>` |

### Typical agent workflow using MCP tools

```
# 1. Establish context at the start of the session
→ read resource  h5i://context/current
→ h5i_context_init  { "goal": "add retry logic to HTTP client" }

# 2. Understand what has already been done
→ h5i_log         { "limit": 5 }
→ h5i_blame       { "file": "src/http_client.rs" }

# 3. Record reasoning as you work
→ h5i_context_trace  { "kind": "OBSERVE", "content": "send() has no retry guard" }
→ h5i_context_trace  { "kind": "THINK",   "content": "exponential backoff with jitter is safest" }
→ h5i_context_trace  { "kind": "ACT",     "content": "added retry loop in send() with 5-attempt cap" }

# 4. Checkpoint progress
→ h5i_context_commit { "summary": "implemented retry loop", "detail": "capped at 5, async-safe" }

# 5. Explore an alternative without losing the current thread
→ h5i_context_branch   { "name": "experiment/sync-retry", "purpose": "simpler sync fallback" }
→ h5i_context_checkout { "name": "main" }
→ h5i_context_merge    { "branch": "experiment/sync-retry" }

# 6. After committing, check what needs human review
→ h5i_notes_review   { "limit": 20 }
→ h5i_notes_coverage {}
```

---

## h5i push

```
h5i push [--remote <name>]
```

Push both `refs/h5i/notes` and `refs/h5i/memory` to the remote (default: `origin`). Neither ref is included in a plain `git push`.

To automate in CI:

```yaml
- name: Push h5i metadata
  run: |
    git push origin refs/h5i/notes
    git push origin refs/h5i/memory
```

To make `git pull` fetch h5i refs automatically, add fetch refspecs to `.git/config`:

```ini
[remote "origin"]
    url = git@github.com:you/repo.git
    fetch = +refs/heads/*:refs/remotes/origin/*
    fetch = +refs/h5i/notes:refs/h5i/notes
    fetch = +refs/h5i/memory:refs/h5i/memory
```

---

## h5i pull

```
h5i pull [--remote <name>]
```

Fetch both `refs/h5i/notes` and `refs/h5i/memory` from the remote (default: `origin`).

---

## h5i session

```
h5i session --file <path>
```

Start a CRDT collaborative session for a file. Watches for local changes and syncs them to the Yjs document stored in `.git/.h5i/crdt/` and `.git/.h5i/delta/`. Multiple agents can edit concurrently; changes merge automatically. Press `Ctrl+C` to stop.

**Options**

| Option | Description |
|--------|-------------|
| `--file <path>` | File to watch (required) |

---

## h5i resolve

```
h5i resolve <ours-oid> <theirs-oid> <file>
```

Reconstruct the conflict-free merged state of a file from two CRDT session OIDs stored in Git Notes. No interactive merge editor required.

---

## Appendix: Storage Layout

### Filesystem (`.git/.h5i/`)

```
.git/.h5i/
├── memory/                          # Claude memory snapshots
│   └── <commit-oid>/
│       ├── <uuid>.jsonl             # Claude Code session log files
│       └── _meta.json               # snapshot timestamp + file count
├── session_log/                     # Claude Code session analyses
│   └── <commit-oid>/
│       └── analysis.json
├── delta/                           # CRDT update logs (created on demand)
│   ├── <sha256(file_path)>.bin      # active append-only log
│   ├── <sha256(file_path)>.snapshot # CRDT snapshot
│   └── <commit-oid>/
│       └── <sha256(file_path)>.bin  # committed delta archive
└── pending_context.json             # Transient: written by hook, consumed by next commit
```

Three additional directories (`ast/`, `crdt/`, `metadata/`) are created on `h5i init` but are not actively used for storage — data is stored in Git refs instead.

### Git Refs

| Ref | Type | Contains |
|-----|------|----------|
| `refs/h5i/notes` | Git notes | Commit metadata: AI provenance, test metrics, causal links, integrity reports, design decisions |
| `refs/h5i/memory` | Linear commit history | Claude memory snapshots as git tree objects; each commit carries the linked code-commit OID |
| `refs/h5i/context` | Git tree | Context workspace: `main.md`, `.current_branch`, `branches/<name>/{commit.md,trace.md,metadata.yaml}` |
| `refs/h5i/ast` | Git objects | AST hash snapshots for semantic blame |

The context workspace commands display paths under `.h5i-ctx/` in their output, but the data is stored in `refs/h5i/context`.

Inspect any notes entry directly:

```bash
git notes --ref refs/h5i/notes show <commit-oid>
```

None of the `refs/h5i/*` refs are pushed or fetched by a plain `git push` / `git pull`. Use `h5i push` / `h5i pull` to share them.

---

## Appendix: Integrity Rules

Run with `h5i commit --audit` or via the Re-audit button in `h5i serve`. Pure string and stat checks — no AI, no network.

| Rule | Severity | Trigger |
|------|----------|---------|
| `CREDENTIAL_LEAK` | **Violation** | Credential keyword + assignment + quoted value, or PEM header in added lines |
| `CODE_EXECUTION` | **Violation** | `eval()`, `exec()`, `os.system()`, `subprocess.*` in non-comment added lines |
| `CI_CD_MODIFIED` | **Violation** | `.github/workflows/`, `Jenkinsfile`, etc. modified without CI/CD intent in prompt |
| `SENSITIVE_FILE_MODIFIED` | Warning | `.env`, `.pem`, `.key`, `id_rsa`, `credentials` in diff |
| `LOCKFILE_MODIFIED` | Warning | `Cargo.lock`, `package-lock.json`, `go.sum` changed without dependency intent in prompt |
| `UNDECLARED_DELETION` | Warning | >60% of changes are deletions with no deletion/refactor intent stated |
| `SCOPE_EXPANSION` | Warning | Prompt names a specific file but other source files were also modified |
| `LARGE_DIFF` | Warning | >500 total lines changed |
| `REFACTOR_ANOMALY` | Warning | "refactor" intent but insertions ≥ 3× deletions |
| `PERMISSION_CHANGE` | Warning | `chmod 777`, `sudo`, `setuid`, `chown root` in added lines |
| `BINARY_FILE_CHANGED` | Info | Binary file appears in diff |
| `CONFIG_FILE_MODIFIED` | Info | `.yaml`, `.toml`, `.json`, `.ini` modified |

Violations always block the commit. Warnings block unless `--force` is passed.

To add a rule: add a `pub const` to `rule_id` in `src/rules.rs`, write one pure `fn check_*(ctx: &DiffContext) -> Vec<RuleFinding>`, and register it in `run_all_rules`. No other changes needed.

---

## Appendix: Test Adapter Schema

Pass a JSON file via `--test-results`, or produce it on stdout for `--test-cmd`. All fields are optional.

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

`exit_code` takes precedence over counts when determining pass/fail. `total` is computed from counts if omitted.

Bundled adapters in `script/`:

| Adapter | Usage |
|---------|-------|
| `h5i-pytest-adapter.py` | `python script/h5i-pytest-adapter.py` — uses `pytest-json-report` when available, falls back to output parsing |
| `h5i-cargo-test-adapter.sh` | `bash script/h5i-cargo-test-adapter.sh` — accumulates counts across lib/integration/doc-test sections |
