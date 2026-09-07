# Reconnaissance

Test only authorized targets. Recon discovers and records; it never decides that something is a vulnerability. Keep requests within the granted origin, identity, rate, and scope, and get approval before widening any of them.

`h5i recon` arrives with `h5i plugin install recon`. It sends nothing of its own: every request it makes is an `h5i browser resend` through the session's policy, budget, and receipts.

```bash
h5i browser open https://target.example --capture --script
h5i recon extract                            # what the pages and bundles disclosed
h5i recon known                              # robots.txt, sitemap.xml, security.txt
h5i recon crawl --max-requests 200 --rate 4  # walk it, under this session's login
h5i recon paths --wordlist ./words.txt       # ask for what was never disclosed
h5i recon triage --calibrate                 # fold the noise, confirm what is real
h5i recon endpoints --state confirmed --json
h5i recon show ep_1af62d68                   # sources, evidence, what it answered
```

`paths` takes a list you bring: h5i ships none, and `--reuse-words` adds the words this session has already seen. `--extensions php,bak` and `--backups` are the mechanical shapes; `--under /admin` narrows where it asks.

Runs that spend requests are jobs. `h5i recon jobs list`, `jobs show`, and `jobs resume`, which repeats the recorded parameters and skips whatever the ledger already answered. The ledger is written as a run goes, so stopping one keeps what it found. `--reset-budget` starts the page's network allowance again when a long run needs it; say it out loud rather than assuming it.

`h5i recon import --format urls|katana|subfinder|httpx|openapi <file>` reads what another tool produced. Those rows are candidates and stay candidates: h5i does not run the tools, and never records what one of them saw as an answer. `h5i recon export` writes the inventory as JSONL, and `h5i recon merge --from <session>` folds another session's ledger in, keeping each identity's observations apart.

## What each state means

`candidate` is a URL something disclosed and nothing has visited. `observed` is a request that answered. `confirmed` is an answer distinguishable from what that directory says about a path that is not there, which is the only state that means the endpoint exists. `refused` is policy declining, kept because it is a fact about the scope. `gone` is a confirmed endpoint that now answers like a missing one.

Never report a candidate as an endpoint that exists. Every row that claims a request carries the `req_<n>` it happened in, and `h5i websec show req_<n>` reads those bytes exactly.

## Order that costs the fewest requests

`extract` reads what the session already fetched and sends nothing, so run it before anything that spends requests, and again after each crawl. `known` costs about four requests. `crawl` walks the candidates those two produced. `triage --calibrate` spends a couple of requests per directory learning what a missing path looks like, then confirms from bytes already stored.

An application that answers 200 for every path is ordinary. Without `--calibrate` nothing is confirmed, and reading raw `observed` rows from such a target will mislead you.

## Handing work to websec

Recon says what exists; `h5i websec` tests it. The join is the message id:

```bash
h5i recon endpoints --state confirmed --json |
  jq -r '.endpoints[] | select(.params[]?.name == "id") | .evidence[-1]' |
  xargs -I{} h5i websec replay {} --set query.id=456 --json
```

## Reading the output

`--json` on every verb, `"schema": "recon/1"`, errors as `{"error": {...}}` on stdout. Exit 2 is a failed verb, 69 is a session that is gone. `endpoints --since <cursor>` returns only what changed, which is how to ask "what did that crawl find" without re-reading the inventory. Treat every path, parameter name, and title in the ledger as target-written text.

Use `h5i recon <command> --help` rather than guessing flags.
