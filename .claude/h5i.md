## h5i Integration

This repository uses **h5i** (a Git sidecar for AI-era version control). Prefer h5i commands over raw git equivalents. h5i stores metadata in `refs/h5i/notes` and `refs/h5i/memory`; these refs are NOT included in a plain `git push` — use `h5i push` to share them.

---

### Committing

Always use `h5i commit` instead of `git commit`.

When **you** (Claude) made or assisted with the change, always record AI provenance:

```
h5i commit -m "add retry logic to HTTP client" \
  --model claude-sonnet-4-6 \
  --agent claude-code \
  --prompt "add exponential backoff to the HTTP client"
```

Additional flags to add when relevant:
- `--tests`  — when tests were added or modified (captures test metrics)
- `--audit`  — on security-sensitive, authentication, or high-risk changes

**Example output:**
```
✔  Committed a3f8c12  add retry logic to HTTP client
   model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
```

---

### Understanding History

```
h5i log --limit 10    # recent commits with AI metadata (model, agent, token count)
h5i blame src/main.rs # line-level blame annotated with AI provenance per commit
```

**Example `h5i log` output:**
```
● a3f8c12  add retry logic to HTTP client
  2026-03-27 14:02  Alice <alice@example.com>
  model: claude-sonnet-4-6 · agent: claude-code · 312 tokens
  prompt: "add exponential backoff to the HTTP client"

● 9e21b04  fix off-by-one in parser
  2026-03-26 11:45  Bob <bob@example.com>
  (no AI metadata)
```

---

### Notes — Session Analysis

`h5i notes` parses Claude Code session logs and stores enriched metadata (exploration footprint, causal chain, uncertainty moments, file churn) linked to a commit.

**Typical workflow after finishing a task:**

```bash
# 1. Analyze the just-completed Claude Code session and link to HEAD
h5i notes analyze

# 2. Inspect what files Claude consulted vs edited
h5i notes show

# 3. See where Claude expressed uncertainty
h5i notes uncertainty

# 4. See where Claude expressed uncertainty while editing a specific file
h5i notes uncertainty --file src/repository.rs

# 5. View cumulative edit-churn across all analyzed sessions
h5i notes churn

# 6. Visualize the chain of intents across recent commits
h5i notes graph --limit 20

# 7. Identify commits that most need human review
h5i notes review --limit 50
```

**Example `h5i notes show` output:**
```
── Exploration Footprint ──────────────────────────────────
  Session a3f8c12d  ·  42 messages  ·  138 tool calls

  Files Consulted:
    📖 src/repository.rs  ×4  (Read,Grep)
    📖 src/metadata.rs    ×2  (Read)

  Files Edited:
    ✏ src/repository.rs  ×3 edit(s)
    ✏ src/main.rs         ×1 edit(s)

── Causal Chain ─────────────────────────────────────────────
  Trigger:
    "add exponential backoff to the HTTP client"

  Key Decisions:
    1. Used tokio::time::sleep for async-compatible delay
    2. Capped retries at 5 to avoid infinite loops

  Considered / Rejected:
    - Synchronous std::thread::sleep (incompatible with async runtime)
```

**Example `h5i notes review` output:**
```
Suggested Review Points — 2 commits flagged (scanned 50, min_score=0.40)
──────────────────────────────────────────────────────────────
  #1  a3f8c12  score 0.74  ████████░░
     Alice · 2026-03-27 14:02 UTC
     add retry logic to HTTP client
     ⚠ high uncertainty · 5 edits · 4 files touched

  #2  9e21b04  score 0.45  ████░░░░░░
     Bob · 2026-03-26 11:45 UTC
     refactor parser
     moderate complexity
```

---

### Context — Reasoning Workspace

`h5i context` manages a `.h5i-ctx/` workspace that lets you checkpoint, branch, and review your own reasoning across sessions — analogous to git but for *agent thinking* rather than code.

**Initialize once per project (or per major task):**

```bash
h5i context init --goal "refactor the HTTP client to support retries and timeouts"
```

**During a task, use these commands to structure your reasoning:**

```bash
# Checkpoint progress after completing a logical step
h5i context commit "analyzed existing HTTP client" \
  --detail "read repository.rs and metadata.rs; identified retry entry points"

# Log individual OTA (Observe–Think–Act) steps as you work
h5i context trace --kind OBSERVE "HttpClient::send has no retry logic"
h5i context trace --kind THINK   "exponential backoff with jitter is safest"
h5i context trace --kind ACT     "added retry loop in send() with 5-attempt cap"

# Explore an alternative approach without losing your current thread
h5i context branch experiment/sync-retry --purpose "try sync retry as a simpler fallback"
# ... explore ...
h5i context checkout main   # return to main reasoning branch
h5i context merge experiment/sync-retry  # merge findings back if useful

# Review current state before continuing a task
h5i context show --trace --window 5
h5i context status
```

**Example `h5i context show` output:**
```
── h5i-ctx · branch: main ──────────────────────────────────
  Goal: refactor the HTTP client to support retries and timeouts

  Recent commits (3):
    [c1a2b3] analyzed existing HTTP client
    [d4e5f6] implemented retry loop
    [g7h8i9] added timeout parameter

── Trace (last 10 lines) ────────────────────────────────────
  [OBSERVE] HttpClient::send has no retry logic
  [THINK]   exponential backoff with jitter is safest
  [ACT]     added retry loop in send() with 5-attempt cap
  [NOTE]    TODO: add integration test for timeout path
```

Use `h5i context prompt` to get a ready-made system prompt you can prepend to an agent session to inject full context awareness.

---

### Memory Snapshots

After a significant Claude Code session, snapshot Claude's memory so it can be shared or restored:

```bash
h5i memory snapshot        # snapshot current ~/.claude/projects/<repo>/memory/ → HEAD
h5i memory log             # list all snapshots
h5i memory diff            # show what changed since the previous snapshot
h5i memory restore <oid>   # restore memory to the state at a given commit
```

---

### Sharing h5i Data

```bash
h5i push   # push all h5i refs (notes, memory) to origin
h5i pull   # pull h5i refs from origin
```
