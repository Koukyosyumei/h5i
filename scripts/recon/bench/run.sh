#!/usr/bin/env bash
# Score discovery against four targets that know what they have.
#
# The suite proves the verbs work; this says whether a change made discovery
# better. Four shapes, one fixed budget each, scored on endpoints confirmed,
# endpoints missed, false confirmations, and requests per confirmed endpoint
# (docs/design/design-recon.md N15).
#
#   ./scripts/recon/bench/run.sh [path-to-h5i] [path-to-h5i-recon]
set -uo pipefail

H5I="${1:-target/release/h5i}"
RECON="${2:-target/release/h5i-recon}"
[ -x "$H5I" ] || { echo "no h5i at $H5I"; exit 2; }
[ -x "$RECON" ] || { echo "no h5i-recon at $RECON"; exit 2; }
export H5I_BIN="$(cd "$(dirname "$H5I")" && pwd)/$(basename "$H5I")"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
SHAPES="plain soft bundle quiet"
PIDS=""

# The wordlist a run is allowed: small, and the same for every shape, so a
# score is about discovery rather than about the list.
printf 'admin\nbackup\nconfig\nlogin\none\ntwo\napi\n' > "$WORK/words.txt"

cleanup() {
    for pid in $PIDS; do kill "$pid" 2>/dev/null; done
    for shape in $SHAPES; do "$H5I" browser close --session "bench-$shape" >/dev/null 2>&1; done
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "| target | real | confirmed | missed | false | requests | per confirmed |"
echo "|---|---:|---:|---:|---:|---:|---:|"

for shape in $SHAPES; do
    PORT=$((21000 + RANDOM % 9000))
    python3 "$HERE/targets.py" "$shape" "$PORT" &
    PIDS="$PIDS $!"
    sleep 1
    ORIGIN="http://127.0.0.1:$PORT"
    SESSION="bench-$shape"

    "$H5I" browser close --session "$SESSION" >/dev/null 2>&1
    "$H5I" browser open "$ORIGIN/" --session "$SESSION" --capture --script --json >/dev/null 2>&1

    # The pipeline an agent would run, in the order that spends the fewest
    # requests: read what is already there, ask for the published files, walk,
    # then guess, then sort.
    "$RECON" extract  --session "$SESSION" --json >/dev/null 2>&1
    "$RECON" known    --session "$SESSION" --json >/dev/null 2>&1
    CRAWL=$("$RECON" crawl --session "$SESSION" --max-requests 60 --rate 0 --json 2>/dev/null)
    PATHS=$("$RECON" paths --session "$SESSION" --wordlist "$WORK/words.txt" \
              --extensions php,sql,bak --backups --max-requests 120 --rate 0 --json 2>/dev/null)
    TRIAGE=$("$RECON" triage --session "$SESSION" --calibrate --json 2>/dev/null)

    REQUESTS=$(python3 -c "
import json,sys
total = 0
for blob in sys.argv[1:]:
    try: total += json.loads(blob).get('requests', 0)
    except Exception: pass
print(total)
" "$CRAWL" "$PATHS" "${TRIAGE:-{\}}")

    curl -s "$ORIGIN/__truth" > "$WORK/truth.json"
    "$RECON" endpoints --session "$SESSION" --json > "$WORK/endpoints.json" 2>/dev/null
    SCORE=$(python3 "$HERE/score.py" "$ORIGIN" "$WORK/truth.json" "$WORK/endpoints.json" "$REQUESTS")
    echo "$SCORE" > "$WORK/$shape.json"
    python3 -c "
import json,sys
d = json.load(open('$WORK/$shape.json'))
print('| %s | %d | %d | %d | %d | %d | %s |' % (
    '$shape', d['real'], d['confirmed'], len(d['missed']),
    len(d['false_confirmations']), d['requests'],
    d['requests_per_confirmed'] if d['requests_per_confirmed'] is not None else '-'))
"
done

echo
for shape in $SHAPES; do
    echo "── $shape"
    python3 -c "
import json
d = json.load(open('$WORK/$shape.json'))
if d['missed']: print('  missed:', ', '.join(d['missed']))
if d['false_confirmations']: print('  falsely confirmed:', ', '.join(d['false_confirmations']))
if not d['missed'] and not d['false_confirmations']: print('  clean')
"
done
