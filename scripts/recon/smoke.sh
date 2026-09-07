#!/usr/bin/env bash
# The recon verbs, driven end to end against scripts/websec/server.py.
#
# Both bugs this suite was written for were invisible to the unit tests and
# obvious on the first real run: receipts are numbered from zero, so every
# session's first request was dropped from the ledger, and a probe built from
# the newest stored request inherited a POST and asked for robots.txt with a
# body. Unit tests cover the readers; this covers the seams between the plugin,
# the CLI, the engine and the store.
#
#   ./scripts/recon/smoke.sh [path-to-h5i] [path-to-h5i-recon]
#
# Exits non-zero on the first failed expectation, so it is usable as a gate.
set -uo pipefail

H5I="${1:-target/release/h5i}"
RECON="${2:-target/release/h5i-recon}"
[ -x "$H5I" ] || { echo "no h5i at $H5I — cargo build --release"; exit 2; }
[ -x "$RECON" ] || { echo "no h5i-recon at $RECON — cargo build --release -p h5i-recon"; exit 2; }
export H5I_BIN="$(cd "$(dirname "$H5I")" && pwd)/$(basename "$H5I")"
HERE="$(cd "$(dirname "$0")" && pwd)"
PORT=$((20000 + RANDOM % 10000))
# A name of its own per run: a leftover session from a previous run would be
# reused by `browser open --session`, and the suite would test that one.
SESSION="recon-smoke-$$"
WORDS="${TMPDIR:-/tmp}/recon-words.$$"
IMPORTS="${TMPDIR:-/tmp}/recon-import.$$"
FAILED=0

python3 "$HERE/../websec/server.py" "$PORT" &
SERVER=$!
cleanup() {
    kill "$SERVER" 2>/dev/null
    "$H5I" browser close --session "$SESSION" >/dev/null 2>&1
    "$H5I" browser close --session "${SESSION}-b" >/dev/null 2>&1
    rm -f "$WORDS" "$IMPORTS"
}
trap cleanup EXIT
sleep 1

ok()  { printf '  \033[32m✔\033[0m %s\n' "$1"; }
bad() { printf '  \033[31m✘\033[0m %s\n' "$1"; FAILED=1; }
is()  { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (got '$2', wanted '$3')"; fi; }
has() { case "$2" in *"$3"*) ok "$1";; *) bad "$1 (got '$2')";; esac; }
py()  { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)" 2>/dev/null; }

echo "── the ledger starts from the session's own log ─────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/site/" --session "$SESSION" --capture --script --json >/dev/null 2>&1
LEDGER=$("$RECON" endpoints --session "$SESSION" --json)
is "the navigation itself is in the ledger (receipts start at seq 0)" \
   "$(echo "$LEDGER" | py "len([e for e in d['endpoints'] if e['path'] == '/site/'])")" "1"
is "and it is observed, not a candidate" \
   "$(echo "$LEDGER" | py "[e['state'] for e in d['endpoints'] if e['path'] == '/site/'][0]")" "observed"

echo "── extract reads what the session already fetched ───────────────────"
OUT=$("$RECON" extract --session "$SESSION" --json)
has "a form's action is disclosed" "$("$RECON" endpoints --session "$SESSION" --json)" "/site/login"
is "a path the bundle builds is reported, not stored" \
   "$(echo "$OUT" | py "len([p for p in d['partial'] if p['prefix'] == '/site/api/item/'])")" "1"
is "and no endpoint was invented for it" \
   "$("$RECON" endpoints --session "$SESSION" --json | py "len([e for e in d['endpoints'] if 'item' in e['path']])")" "0"

echo "── known files are asked for, through the engine ────────────────────"
OUT=$("$RECON" known --session "$SESSION" --json)
is "robots.txt answered" "$(echo "$OUT" | py "[a['status'] for a in d['asked'] if a['path'] == '/robots.txt'][0]")" "200"
has "and what it disallows is a candidate" "$("$RECON" endpoints --session "$SESSION" --json)" "/site/admin/"
has "the sitemap's location too" "$("$RECON" endpoints --session "$SESSION" --json)" "/site/help"

echo "── the crawl walks under the session, and stays bounded ─────────────"
OUT=$("$RECON" crawl --session "$SESSION" --max-requests 12 --rate 20 --json)
is "it spent no more than it was allowed" \
   "$(echo "$OUT" | py "d['requests'] <= 12")" "True"
has "and it walked a disclosed page" "$OUT" "/site/one"

echo "── paths asks with a list the operator brings ───────────────────────"
# The list lives until cleanup: a job records the path it was given, so a
# resume needs the file to still be there.
printf 'help\nadmin\n' > "$WORDS"
OUT=$("$RECON" paths --session "$SESSION" --wordlist "$WORDS" \
        --under /site --extensions php --max-requests 10 --rate 0 --json)
is "it asked, and stayed inside its allowance" \
   "$(echo "$OUT" | py "0 < d['requests'] <= 10")" "True"
is "and it used one process for the run" "$(echo "$OUT" | py "d['one_process']")" "True"
is "a word list with no words is refused, not guessed at" \
   "$("$RECON" paths --session "$SESSION" --under /site >/dev/null 2>&1; echo $?)" "2"

echo "── a run is a job, and a job can be run again ───────────────────────"
JOBS=$("$RECON" jobs list --session "$SESSION" --json)
is "the paths run was recorded" "$(echo "$JOBS" | py "len([j for j in d['jobs'] if j['verb'] == 'paths'])")" "1"
is "and it says it finished" "$(echo "$JOBS" | py "[j['ended_at'] is not None for j in d['jobs'] if j['verb'] == 'paths'][0]")" "True"
AGAIN=$("$RECON" jobs resume --session "$SESSION" --json 2>/dev/null)
is "resuming asks only for what is not answered yet" \
   "$(echo "$AGAIN" | py "d['skipped'] > 0")" "True"

echo "── triage tells a soft 404 from a page ──────────────────────────────"
OUT=$("$RECON" triage --session "$SESSION" --calibrate --json)
is "the baseline is stable" "$(echo "$OUT" | py "d['calibrated'][0]['stable']")" "True"
is "real pages confirm" "$(echo "$OUT" | py "d['confirmed'] > 0")" "True"
is "a 200 that means nothing is not confirmed" \
   "$("$RECON" endpoints --session "$SESSION" --state confirmed --json | py "len([e for e in d['endpoints'] if 'admin' in e['path']])")" "0"
is "every confirmed row names the message that proves it" \
   "$("$RECON" endpoints --session "$SESSION" --state confirmed --json | py "all(e['evidence'] for e in d['endpoints'])")" "True"

echo "── another tool's file is testimony, not evidence ───────────────────"
printf '/site/from-a-list\nhttps://elsewhere.test/x\n' > "$IMPORTS"
OUT=$("$RECON" import --session "$SESSION" --format urls "$IMPORTS" --json)
is "both lines were read" "$(echo "$OUT" | py "d['read']")" "2"
is "and every one of them is a candidate" \
   "$("$RECON" endpoints --session "$SESSION" --json | py "[e['state'] for e in d['endpoints'] if e['path'] == '/site/from-a-list'][0]")" "candidate"
is "an unknown format is refused" \
   "$("$RECON" import --session "$SESSION" --format nmap "$IMPORTS" >/dev/null 2>&1; echo $?)" "2"

echo "── the inventory outlives the session ───────────────────────────────"
is "export writes one endpoint per line" \
   "$("$RECON" export --session "$SESSION" | wc -l | tr -d ' ' | awk '{print ($1 > 0) ? "yes" : "no"}')" "yes"
"$H5I" browser open "http://127.0.0.1:$PORT/site/one" --session "${SESSION}-b" --capture --json >/dev/null 2>&1
OUT=$("$RECON" merge --session "${SESSION}-b" --from "$SESSION" --json)
is "merging carries the other session's endpoints" "$(echo "$OUT" | py "d['written'] > 0")" "True"
is "and their evidence keeps the session it belongs to" \
   "$("$RECON" endpoints --session "${SESSION}-b" --json | py "any('/req_' in (e['evidence'][-1] if e['evidence'] else '') for e in d['endpoints'])")" "True"

echo "── the contract ─────────────────────────────────────────────────────"
is "an unknown endpoint exits 2" "$("$RECON" show ep_nope --session "$SESSION" >/dev/null 2>&1; echo $?)" "2"
is "a session that is not there exits 69" "$("$RECON" endpoints --session no-such >/dev/null 2>&1; echo $?)" "69"
has "and an error is JSON on stdout" "$("$RECON" show ep_nope --session "$SESSION" 2>/dev/null)" '"error"'

echo
[ "$FAILED" -eq 0 ] && echo "all good" || echo "failures above"
exit "$FAILED"
