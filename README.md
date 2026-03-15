# h5i

> **The version control layer for the age of AI-generated code.**

<p align="center">
  <a href="https://github.com/Koukyosyumei/h5i" target="_blank">
      <img src="./assets/logo.svg" alt="h5i Logo" height="126">
  </a>
</p

`h5i` (pronounced *high-five*) is a Git sidecar that extends version control beyond text history. Where Git answers *what changed*, h5i answers *who changed it, why, whether it was safe, and how to undo it semantically*.

Built for teams where AI agents write production code alongside humans.

---

## 1. The Problem

Modern AI coding agents — Claude, Copilot, Cursor — generate tens of thousands of lines of code per day. Git was designed for humans. It has no concept of:

- **Provenance** — was this written by a human or an AI, and with what prompt?
- **Intent alignment** — did the agent actually do what was asked?
- **Semantic rollback** — "undo the AI change that broke authentication" not "revert commit `a3f9c2b`"
- **Integrity** — did the agent quietly touch CI/CD files, leak credentials, or remove safety checks?
- **Concurrent agents** — how do two AI agents edit the same file without corrupting each other's work?

h5i is the missing infrastructure layer.

---

## 2. Features

**AI Provenance Tracking**
Captures the prompt, model name, agent ID, and token usage alongside every commit. The prompt can also be captured automatically from major AI coding tooks like Claude Code via a hook.

**Rule-Based Integrity Engine**
Human-auditable rules that run before every `--audit` commit. No AI involvement in the audit path. Rules detect credential leaks, dangerous execution patterns, CI/CD tampering, scope creep, and more.

**Intent-Based Rollback**
Describe what you want to undo in plain English. h5i can use AI agent like Claude to semantically match your description against stored prompts and commit messages, then reverts the right commit — no commit hash needed.

**CRDT Collaborative Sessions**
File-level Yjs documents allow multiple AI agents to edit concurrently with strong eventual consistency. Conflicts resolve mathematically, not by coin flip.

**Semantic Blame**
Line-level and AST-level blame that surfaces the original AI prompt and test status alongside authorship — not just a commit hash.

---

## 3. Installation

Requires Rust 1.70+ and an existing Git repository.

```bash
git clone https://github.com/koukyosyumei/h5i
cd h5i
cargo build --release
cp target/release/h5i /usr/local/bin/
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

With the Claude Code hook installed (see below), `--prompt` is captured automatically from your conversation — zero friction.

### 4.2. Auditing Before Commit

```bash
h5i commit -m "refactor auth module" --audit
```

```
⚠ INTEGRITY WARNING (score: 0.70)
  ⚠ [UNDECLARED_DELETION]  247 lines deleted (72% of total changes) with no deletion intent stated.
  ℹ [CONFIG_FILE_MODIFIED]  Configuration file 'config/auth.yaml' modified.
```

Use `--force` to commit despite warnings. Violations block the commit by default.

### 4.3. Enriched Commit Log

```bash
h5i log --limit 5
```

```
commit a3f9c2b14...
Author:   Alice <alice@example.com>
Agent:    claude-code (claude-sonnet-4-6) 󱐋
Prompt:   "implement per-IP rate limiting on the auth endpoint"
Tests:    ✔ 94.2%

    implement rate limiting

────────────────────────────────────────────────────────────
```

### 4.4. Semantic Blame

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

### 4.5. Intent-Based Rollback

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
# Dry run to preview without reverting
h5i rollback "rate limiting" --dry-run

# Skip confirmation in CI
h5i rollback "the broken migration" --yes
```

Falls back to keyword search if `ANTHROPIC_API_KEY` is not set.

### 4.6. CRDT Collaborative Sessions

Start a real-time recording session for a file. Each agent gets its own session; changes are merged via CRDT automatically.

```bash
h5i session --file src/auth.rs
# → Watching for changes... (Press Ctrl+C to stop)
```

### 4.7. CRDT Merge Resolution

```bash
h5i resolve <ours-oid> <theirs-oid> src/auth.rs
```

Resolves conflicts using the mathematical CRDT state stored in Git Notes — no interactive merge editor required.

---

## 5. Integrity Engine

The `--audit` flag runs twelve deterministic rules against the staged diff before committing. Rules are pure string/stat checks — no AI, no network, no false trust.

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

To extend the engine: add a `pub const` to `rule_id`, write one pure `fn check_*(ctx: &DiffContext) -> Vec<RuleFinding>` function, and register it in `run_all_rules`. No other changes needed.

---

## 6. Claude Code Integration

h5i can capture your prompt automatically every time you submit a message to Claude Code, with zero manual intervention.

```bash
h5i install-hooks
```

This prints:
1. A shell script to save at `~/.claude/hooks/h5i-capture-prompt.sh`
2. The exact `~/.claude/settings.json` snippet to register the hook

After setup, the prompt flows from your conversation → `.git/.h5i/pending_context.json` → consumed and cleared by the next `h5i commit`. No flags, no copy-paste.

**Environment variable fallback** (works without hooks, or with any AI agent):

```bash
export H5I_PROMPT="implement rate limiting on the auth endpoint"
export H5I_MODEL="claude-sonnet-4-6"
export H5I_AGENT_ID="claude-code"
h5i commit -m "add rate limiting"
```

Resolution order at commit time: CLI flag → env var → pending context file.

---

## 7. How It Works

h5i stores all metadata as a Git sidecar — nothing lives outside your repository.

```
.git/
└── .h5i/
    ├── ast/                  # SHA-256-keyed S-expression AST snapshots
    ├── metadata/             # Per-commit JSON records (legacy)
    ├── crdt/                 # Yjs CRDT document state
    ├── delta/                # Append-only CRDT update logs (per file)
    └── pending_context.json  # Transient: consumed at next commit
```

Extended commit metadata is stored in Git Notes (`refs/notes/commits`) as JSON, so it travels with the repository on push/fetch and is visible via standard Git tooling.

---

## 8. License

Apache 2.0 — see [LICENSE](LICENSE).
