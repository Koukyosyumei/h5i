# h5i

> **The version control layer for the age of AI-generated code.**

<p align="center">
  <a href="https://github.com/Koukyosyumei/h5i" target="_blank">
      <img src="./assets/logo.svg" alt="h5i Logo" height="126">
  </a>
</p>

`h5i` (pronounced *high-five*) is a Git sidecar that extends version control beyond text history. Where Git answers *what changed*, h5i answers *who changed it, why, whether it was safe, and how to undo it semantically*.

Built for teams where AI agents write production code alongside humans.

---

## 1. The Problem

Modern AI coding agents — Claude, Copilot, Cursor — generate tens of thousands of lines of code per day. Git was designed for humans. It has no concept of:

- **Provenance** — was this written by a human or an AI, and with what prompt?
- **Intent alignment** — did the agent actually do what was asked?
- **Semantic rollback** — "undo the AI change that broke authentication" not "revert commit `a3f9c2b`"
- **Integrity** — did the agent quietly touch CI/CD files, leak credentials, or remove safety checks?
- **Test traceability** — what was the test pass rate at every commit, regardless of the tool used?
- **Causal history** — which commit introduced the bug that this commit fixes?
- **Concurrent agents** — how do two AI agents edit the same file without corrupting each other's work?

h5i is the missing infrastructure layer.

---

## 2. Features

**AI Provenance Tracking**
Captures the prompt, model name, agent ID, and token usage alongside every commit. The prompt can also be captured automatically from Claude Code via a hook — zero friction.

**Universal Test Metrics**
Attach structured test results from any tool (pytest, cargo test, Jest, Go test, …) to every commit via a neutral JSON adapter format. h5i stores passed/failed/skipped counts, duration, coverage, and a human-readable summary alongside the commit note.

**Causal Commit Chains**
Declare which earlier commits causally triggered the current one (`--caused-by`). h5i stores the causal graph, surfaces it in `h5i log`, and warns during rollback if downstream commits depend on the one being reverted.

**Rule-Based Integrity Engine**
Twelve deterministic rules run before every `--audit` commit. No AI in the audit path. Rules detect credential leaks, dangerous execution patterns, CI/CD tampering, scope creep, and more. The web dashboard lets you audit any historical commit interactively.

**Intent-Based Rollback**
Describe what you want to undo in plain English. h5i semantically matches your description against stored prompts and commit messages, then reverts the right commit — no commit hash needed.

**CRDT Collaborative Sessions**
File-level Yjs documents allow multiple AI agents to edit concurrently with strong eventual consistency. Conflicts resolve mathematically, not by coin flip.

**Semantic Blame**
Line-level and AST-level blame that surfaces the original AI prompt and test status alongside authorship — not just a commit hash.

**Web Dashboard**
`h5i serve` launches a local dashboard with rich commit history, test health sparklines, per-commit GitHub links, inline integrity auditing, agent leaderboards, and a full rules reference.

<img src="./assets/screenshot_h5i_server.png" alt="h5i server">

---

## 3. Installation

Requires Rust 1.70+:

```bash
cargo install --git https://github.com/Koukyosyumei/h5i h5i-core
```

Alternatively, you can install it from a local clone:

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

## 4. Usage

### 4.1. Committing with AI Provenance

```bash
# Explicit flags
h5i commit -m "implement rate limiting" \
  --prompt "add per-IP rate limiting to the auth endpoint" \
  --model claude-sonnet-4-6 \
  --agent claude-code

# Or set environment variables — flags are then optional
export H5I_MODEL=claude-sonnet-4-6
export H5I_AGENT_ID=claude-code
h5i commit -m "implement rate limiting"
```

Resolution order: CLI flag → env var (`H5I_PROMPT`, `H5I_MODEL`, `H5I_AGENT_ID`) → pending context file (written by the Claude Code hook).

With the hook installed (see §6), `--prompt` is captured automatically from your Claude Code conversation — zero friction.

### 4.2. Attaching Test Results

h5i supports three ways to record test metrics alongside a commit. All three store the same structured data (passed/failed/skipped counts, duration, tool name, coverage) in the commit note.

**Option A — pass a pre-computed results file** (recommended for CI):

```bash
python script/h5i-pytest-adapter.py > /tmp/results.json
h5i commit -m "add login tests" --test-results /tmp/results.json

# Or via environment variable (useful in CI pipelines)
export H5I_TEST_RESULTS=/tmp/results.json
h5i commit -m "add login tests"
```

**Option B — let h5i run the test command inline**:

```bash
h5i commit -m "add login tests" \
  --test-cmd "python script/h5i-pytest-adapter.py"
```

**Option C — scan staged files for inline markers** (language-agnostic fallback):

```bash
h5i commit -m "add login tests" --tests
# Looks for // h5_i_test_start … // h5_i_test_end blocks in staged files
```

#### Bundled adapters

`script/h5i-pytest-adapter.py` — runs pytest, uses `pytest-json-report` when available, falls back to output parsing:

```bash
pip install pytest pytest-json-report   # one-time
python script/h5i-pytest-adapter.py     # prints JSON to stdout
```

`script/h5i-cargo-test-adapter.sh` — runs `cargo test`, accumulates counts across lib / integration / doc-test sections:

```bash
bash script/h5i-cargo-test-adapter.sh  # prints JSON to stdout
```

#### Writing your own adapter

Any tool can produce compatible output. Write a JSON file matching this schema and pass it via `--test-results`:

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

### 4.3. Causal Commit Chains

Declare which earlier commits causally triggered the current one — for example, "this commit fixes a bug introduced by commit X":

```bash
h5i commit -m "fix off-by-one in validate_token" \
  --caused-by a3f9c2b \
  --prompt "fix the bug introduced by the rate limiter"

# Multiple causes are allowed
h5i commit -m "unify auth flow" \
  --caused-by a3f9c2b \
  --caused-by d4e5f6a
```

Abbreviated OIDs are resolved to full OIDs at commit time; invalid OIDs are rejected with an error.

The causal link is surfaced in `h5i log`:

```
commit b2f3a1c...
Author:    Alice <alice@example.com>
Agent:     claude-code (claude-sonnet-4-6)
Caused by: a3f9c2b "implement rate limiting"
Prompt:    "fix the off-by-one in validate_token"
Tests:     ✔ 42 passed, 0 failed, 1.23s [pytest]
```

**Rollback cascade warning** — when rolling back a commit, h5i scans recent history for any commits that declared it as a cause, and warns before proceeding:

```
⚠ Warning: 2 later commits causally depend on this one:
  → b2f3a1c "fix bug introduced by rate limiter"
  → c3d4e5f "add test coverage for rate limiter"
Continue anyway? [y/N]
```

Use `--yes` to skip the prompt in CI.

### 4.4. Auditing Before Commit

```bash
h5i commit -m "refactor auth module" --audit
```

```
⚠ INTEGRITY WARNING (score: 0.70)
  ⚠ [UNDECLARED_DELETION]  247 lines deleted (72% of total changes) with no deletion intent stated.
  ℹ [CONFIG_FILE_MODIFIED]  Configuration file 'config/auth.yaml' modified.
```

Use `--force` to commit despite warnings. Violations block the commit by default.

Flags can be combined freely:

```bash
h5i commit -m "add rate limiter" \
  --prompt "add per-IP rate limiting" \
  --model claude-sonnet-4-6 \
  --agent claude-code \
  --test-results /tmp/results.json \
  --caused-by d4e5f6a \
  --audit
```

### 4.5. Enriched Commit Log

```bash
h5i log --limit 5
```

```
commit b2f3a1c...
Author:    Alice <alice@example.com>
Agent:     claude-code (claude-sonnet-4-6) 󱐋
Caused by: a3f9c2b "implement rate limiting"
Prompt:    "fix the off-by-one in validate_token"
Tests:     ✔ 42 passed, 0 failed, 1.23s [pytest]

    fix off-by-one in validate_token

────────────────────────────────────────────────────────────
```

### 4.6. Semantic Blame

```bash
h5i blame src/auth.rs
h5i blame src/auth.rs --mode ast   # AST-level semantic blame
```

```
STAT COMMIT   AUTHOR/AGENT    | CONTENT
✅ ✨ a3f9c2b  claude-code     | fn validate_token(tok: &str) -> bool {
✅    a3f9c2b  claude-code     |     tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit())
      9eff001  alice           | }
```

### 4.7. Intent-Based Rollback

```bash
h5i rollback "the OAuth login changes"
```

```
🔍 Searching for intent: "the OAuth login changes" across last 50 commits
   Using Claude for semantic search (claude-haiku-4-5-20251001)

Matched commit:
  commit 7d2f1a9e3b...
  Message:   add OAuth login with GitHub provider
  Agent:     claude-code (claude-sonnet-4-6)
  Prompt:    "implement OAuth login flow with GitHub as the identity provider"
  Date:      2026-03-10 14:22 UTC

Revert this commit? [y/N]
```

```bash
h5i rollback "rate limiting" --dry-run   # preview without reverting
h5i rollback "the broken migration" --yes  # skip confirmation in CI
```

Falls back to keyword search if `ANTHROPIC_API_KEY` is not set.

### 4.8. CRDT Collaborative Sessions

```bash
h5i session --file src/auth.rs
# → Watching for changes... (Press Ctrl+C to stop)

h5i resolve <ours-oid> <theirs-oid> src/auth.rs
```

Each agent gets its own session; changes merge via CRDT automatically. `h5i resolve` reconstructs the conflict-free state from Git Notes — no interactive merge editor required.

### 4.9. AI Memory Management

h5i versions Claude Code's persistent memory files alongside your code, so every commit can carry an exact record of what the AI "knew" when it made that change.

#### How Claude Code memory works

Claude Code stores per-project memory in:

```
~/.claude/projects/<repo-path>/memory/
  MEMORY.md            ← index
  user_role.md         ← user profile facts
  feedback_tests.md    ← remembered instructions
  project_auth.md      ← project-specific context
  …
```

These files are **local-only and unversioned** by default. h5i fixes that.

#### Snapshotting

```bash
# After a Claude session, snapshot current memory to .git/.h5i/memory/<HEAD-oid>/
h5i memory snapshot

# Or tie it to a specific commit OID
h5i memory snapshot --commit a3f9c2b
```

#### Viewing history

```bash
h5i memory log
```

```
COMMIT      TIMESTAMP               FILES
a3f9c2b…  2026-03-27 09:14 UTC    4 files
b2f3a1c…  2026-03-27 11:42 UTC    5 files
```

#### Diffing memory across commits

```bash
h5i memory diff                    # last snapshot → live memory
h5i memory diff a3f9c2b b2f3a1c    # between two snapshots
h5i memory diff a3f9c2b            # snapshot → live
```

```
memory diff a3f9c2b..b2f3a1c
────────────────────────────────────────────────────────────
  added     project_auth.md
    +  The auth middleware rewrite is driven by legal compliance
    +  requirements around session token storage.
  modified  feedback_tests.md
     Don't mock the database in tests.
    -Why: prior incident where mocks masked a broken migration.
    +Why: prior incident where mocks masked a broken migration.
    +How to apply: always use a real DB in integration tests.
────────────────────────────────────────────────────────────
  1 added, 0 removed, 1 modified
```

#### Restoring

```bash
h5i memory restore a3f9c2b    # interactive confirmation
h5i memory restore a3f9c2b -y # skip prompt
```

#### Sharing memory with teammates

Memory snapshots are backed by real git objects, so they travel with the repository (see §9 for the push/pull workflow):

```bash
h5i memory push           # serialize latest snapshot → refs/h5i/memory → git push origin
h5i memory pull           # git fetch origin → extract snapshot locally
h5i memory restore <oid>  # apply the received snapshot to your Claude session
```

### 4.10. Web Dashboard

```bash
h5i serve            # http://localhost:7150
h5i serve --port 8080
```

The dashboard has three tabs:

**Timeline** — full commit history with:
- Colored left borders: green for passing tests, red for failing
- Rich test badges showing `🧪 ✔42 ✖0 ⊘1` counts
- Per-commit `↗ GitHub` links (auto-detected from the `origin` remote)
- `⛓ caused by N` badge when a causal link is present
- Expandable detail panels showing AI prompt, model, token count, and full test breakdown table
- `🛡 Audit` button on every card — runs the twelve integrity rules against that commit's diff and shows results inline, with a collapsible panel listing every rule and its pass/fail status

**Summary** — aggregate stats cards, agent leaderboard bar chart, and a list of commits with failing tests. Filter pills: `🤖 AI only`, `🧪 With tests`, `✖ Failing`.

**Integrity** — manually audit any commit message + prompt against the rule engine without committing.

The sidebar shows a test-health sparkline across all loaded commits.

---

## 5. Integrity Engine

The `--audit` flag (and the dashboard's per-commit audit button) runs twelve deterministic rules against the diff. Rules are pure string/stat checks — no AI, no network, no false trust.

| Rule | Severity | Trigger |
|------|----------|---------|
| `CREDENTIAL_LEAK` | **Violation** | Added line contains credential keyword + assignment + quoted value, or PEM header |
| `CODE_EXECUTION` | **Violation** | Added non-comment line contains `eval()`, `exec()`, `os.system()`, `subprocess.*`, etc. |
| `CI_CD_MODIFIED` | **Violation** | `.github/workflows/`, `Jenkinsfile`, etc. modified without CI/CD intent in prompt |
| `SENSITIVE_FILE_MODIFIED` | Warning | `.env`, `.pem`, `.key`, `id_rsa`, `credentials` in diff |
| `LOCKFILE_MODIFIED` | Warning | `Cargo.lock`, `package-lock.json`, `go.sum` changed without dependency intent |
| `UNDECLARED_DELETION` | Warning | >60% of changes are deletions with no deletion/refactor intent stated |
| `SCOPE_EXPANSION` | Warning | Prompt names a specific file but other source files were also modified |
| `LARGE_DIFF` | Warning | >500 total lines changed — difficult for humans to audit |
| `REFACTOR_ANOMALY` | Warning | "refactor" intent but insertions are 3× or more the deletions |
| `PERMISSION_CHANGE` | Warning | `chmod 777`, `sudo`, `setuid`, `chown root` in added lines |
| `BINARY_FILE_CHANGED` | Info | Binary file appears in diff |
| `CONFIG_FILE_MODIFIED` | Info | `.yaml`, `.toml`, `.json`, `.ini` etc. modified |

**Why rule-based?** AI-generated code should be audited by deterministic rules that humans can read and reason about. A fuzzy ML classifier would itself be a trust problem.

To add a rule: add a `pub const` to `rule_id` in `src/rules.rs`, write one pure `fn check_*(ctx: &DiffContext) -> Vec<RuleFinding>` function, and register it in `run_all_rules`. No other changes needed.

---

## 6. Claude Code Integration

h5i can capture your prompt automatically every time you submit a message to Claude Code, with zero manual intervention.

```bash
h5i install-hooks
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

## 7. Demo Repository

`examples/dnn-from-scratch` (also at [github.com/Koukyosyumei/dnn-from-scratch](https://github.com/Koukyosyumei/dnn-from-scratch)) is a self-contained Python project — a fully-connected neural network trained from scratch with NumPy — built entirely with Claude Code and version-controlled with h5i.

It demonstrates the full workflow end-to-end:

```
git init → h5i init → (write code → pytest → h5i commit --test-results …) × N
```

The repo has eight h5i commits with full AI provenance (agent: `claude-code`, model: `claude-sonnet-4-6`) and test metrics attached via `h5i-pytest-adapter.py`.

```bash
# Show h5i log, blame, and run the XOR demo on the already-built repo
bash examples/dnn-from-scratch/demo.sh --inspect

# Replay the full build from scratch in a temp directory
bash examples/dnn-from-scratch/demo.sh
```

---

## 8. How It Works

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
    └── pending_context.json        # Transient: consumed at next commit
```

**Git Notes** (`refs/notes/commits`) store extended commit metadata — AI provenance, test metrics, causal links, integrity reports — as JSON blobs attached to each commit. They are readable with standard Git tooling:

```bash
git notes show <commit-oid>
```

**Memory ref** (`refs/h5i/memory`) stores Claude memory snapshots as a linear commit history of git tree objects. Each memory commit carries the linked code-commit OID in its message, enabling correlation between code state and AI knowledge state.

> **Important:** Neither `refs/notes/commits` nor `refs/h5i/memory` is pushed or fetched by a plain `git push` / `git pull`. You must share them explicitly — see §9.

---

## 9. Sharing h5i Data with Your Team

h5i stores its data in two git refs that are **not** included in a normal `git push`. You need to push them explicitly.

| Ref | Contains | Push command |
|-----|----------|--------------|
| `refs/notes/commits` | AI provenance, test metrics, causal links, integrity reports | `git push origin refs/notes/commits` |
| `refs/h5i/memory` | Claude memory snapshots | `git push origin refs/h5i/memory` |

### One-shot push

The `h5i push` command pushes both refs at once:

```bash
h5i push                 # push to origin
h5i push --remote upstream
```

To pull both refs from a remote:

```bash
git fetch origin refs/notes/commits:refs/notes/commits
git fetch origin refs/h5i/memory:refs/h5i/memory
```

Or use `h5i memory pull` for the memory ref alone (it handles the fetch refspec automatically).

### Automating with CI/CD

Add these lines to your CI push step so teammates always receive up-to-date h5i data when they pull:

```yaml
# GitHub Actions example
- name: Push h5i metadata
  run: |
    git push origin refs/notes/commits
    git push origin refs/h5i/memory
```

For `git fetch`, add corresponding fetch refspecs to `.git/config` so `git pull` picks them up automatically:

```ini
[remote "origin"]
    url = git@github.com:you/repo.git
    fetch = +refs/heads/*:refs/remotes/origin/*
    fetch = +refs/notes/commits:refs/notes/commits
    fetch = +refs/h5i/memory:refs/h5i/memory
```

With these refspecs in place, a plain `git fetch origin` (or `git pull`) will also update your local h5i notes and memory snapshots.

### Full team workflow example

```bash
# — Alice —
h5i commit -m "add rate limiting" --prompt "..." --agent claude-code
h5i memory snapshot
h5i push                            # push code + notes + memory in one step
                                    # (git push origin is separate for code)
git push origin main

# — Bob —
git pull                            # fetches code + notes + memory (with refspecs configured)
h5i log                             # sees Alice's AI provenance immediately
h5i memory pull                     # or manual: git fetch origin refs/h5i/memory:refs/h5i/memory
h5i memory restore <alice-commit>   # apply Alice's Claude memory to Bob's Claude session
```

---

## 11. License

Apache 2.0 — see [LICENSE](LICENSE).
