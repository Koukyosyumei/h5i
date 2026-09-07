#!/usr/bin/env bash
# The websec verbs, driven end to end against scripts/websec/server.py.
#
# Every bug this suite was written for was invisible to the unit tests and
# obvious the first time the real binary ran: a verb missing from `Verb::ALL`,
# a duplicated `accept-encoding` on every replay, two exit codes collapsed into
# one, a `null` read as a composed request. Unit tests cover the pieces; this
# covers the seams between the CLI, the control channel, the engine and the
# store.
#
#   ./scripts/websec/smoke.sh [path-to-h5i]
#
# Exits non-zero on the first failed expectation, so it is usable as a gate.
set -uo pipefail

H5I="${1:-target/release/h5i}"
[ -x "$H5I" ] || { echo "no h5i at $H5I — cargo build --release --features browser"; exit 2; }
# Reading a store is the plugin's job now, so the suite needs both binaries:
# `show`, `diff`, `match` and `sitemap` are not in the default build (W21).
WEBSEC="${2:-$(dirname "$H5I")/h5i-websec}"
[ -x "$WEBSEC" ] || { echo "no h5i-websec at $WEBSEC — cargo build --release -p h5i-websec"; exit 2; }
export H5I_BIN="$(cd "$(dirname "$H5I")" && pwd)/$(basename "$H5I")"
HERE="$(cd "$(dirname "$0")" && pwd)"
PORT=$((20000 + RANDOM % 10000))
SESSIONS=(ws-smoke-a ws-smoke-b ws-smoke-csrf ws-smoke-log ws-smoke-time ws-smoke-race ws-smoke-up)
FAILED=0

python3 "$HERE/server.py" "$PORT" &
SERVER=$!
cleanup() {
    kill "$SERVER" 2>/dev/null
    for name in "${SESSIONS[@]}"; do "$H5I" browser close --session "$name" >/dev/null 2>&1; done
    rm -f /tmp/ws-smoke-flow.$$.json
}
trap cleanup EXIT
sleep 1

ok()   { printf '  \033[32m✔\033[0m %s\n' "$1"; }
bad()  { printf '  \033[31m✘\033[0m %s\n' "$1"; FAILED=1; }
is()   { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (got '$2', wanted '$3')"; fi; }
has()  { case "$2" in *"$3"*) ok "$1";; *) bad "$1 (got '$2')";; esac; }
jqp()  { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)" 2>/dev/null; }

echo "── capture and replay ───────────────────────────────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/profile?user_id=1" \
    --session ws-smoke-a --new --capture >/dev/null 2>&1

is "the request log has the navigation" \
   "$("$H5I" browser requests --session ws-smoke-a --json 2>/dev/null | jqp 'd["total"]')" "2"

REPLAY="$("$H5I" browser resend 0 --set 'query.user_id=2' --session ws-smoke-a --json 2>/dev/null)"
is "a replay changes the parameter"  "$(echo "$REPLAY" | jqp 'd["applied"][0]["was"]')" "1"
is "and comes back 200"              "$(echo "$REPLAY" | jqp 'd["response"]["status"]')" "200"
has "with the other user's record"   "$("$WEBSEC" show res_1 --session ws-smoke-a 2>/dev/null)" "bob"

is "a typo is refused, not sent" \
   "$("$H5I" browser resend 0 --set 'query.userid=2' --session ws-smoke-a --json 2>/dev/null | jqp 'd["code"]')" "bad-edit"

RAW="$("$WEBSEC" show req_1 --session ws-smoke-a --raw 2>/dev/null)"
is "a replay sends one accept-encoding" "$(echo "$RAW" | grep -c '^accept-encoding:')" "1"

echo
echo "── diff ─────────────────────────────────────────────────────────────"
DIFF="$("$WEBSEC" diff res_0 res_1 --session ws-smoke-a 2>/dev/null)"
is "the diff names the changed fields" "$(echo "$DIFF" | jqp 'len(d["json_changes"])')" "4"
is "and reports no status change"      "$(echo "$DIFF" | jqp 'd["status_changed"]')" "False"

echo
echo "── match ────────────────────────────────────────────────────────────"
"$WEBSEC" match res_1 --json-path role=admin --status 200 --session ws-smoke-a >/dev/null 2>&1
is "a hit exits 0" "$?" "0"
"$WEBSEC" match res_1 --contains 'not-in-this-body' --session ws-smoke-a >/dev/null 2>&1
is "a miss exits 1" "$?" "1"
"$WEBSEC" match res_1 --regex '([unclosed' --session ws-smoke-a >/dev/null 2>&1
is "a broken pattern exits 2, not 1" "$?" "2"

echo
echo "── cross-session replay ─────────────────────────────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/login?who=alice" --session ws-smoke-a --json >/dev/null 2>&1
"$H5I" browser navigate "http://127.0.0.1:$PORT/doc?id=1" --session ws-smoke-a >/dev/null 2>&1
"$H5I" browser open "http://127.0.0.1:$PORT/login?who=bob" --session ws-smoke-b --new --capture >/dev/null 2>&1
DOC_SEQ="$("$H5I" browser requests --session ws-smoke-a --url-contains /doc --json 2>/dev/null | jqp 'd["requests"][0]["seq"]')"
is "alice reads her own document" \
   "$("$H5I" browser requests --session ws-smoke-a --url-contains /doc --status 200 --json 2>/dev/null | jqp 'd["shown"]')" "1"
is "bob is refused it" \
   "$("$H5I" browser resend "$DOC_SEQ" --as ws-smoke-b --session ws-smoke-a --json 2>/dev/null | jqp 'd["response"]["status"]')" "403"

echo
echo "── sequences ────────────────────────────────────────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/form" --session ws-smoke-csrf --new --capture >/dev/null 2>&1
"$H5I" browser navigate "http://127.0.0.1:$PORT/settings" --session ws-smoke-csrf >/dev/null 2>&1
is "the protected endpoint refuses a lone replay" \
   "$("$H5I" browser resend 1 --set 'header.X-Role=admin' --create --session ws-smoke-csrf --json 2>/dev/null | jqp 'd["response"]["status"]')" "403"

cat > "/tmp/ws-smoke-flow.$$.json" <<'JSON'
{"steps": [
  {"name": "fetch the form", "resend": 0,
   "extract": {"csrf": "regex:name=\"csrf\" value=\"([^\"]+)\""}},
  {"name": "use the token", "resend": 1, "create": true,
   "set": ["header.X-CSRF-Token=${csrf}", "header.X-Role=admin"]}
]}
JSON
FLOW="$("$H5I" browser sequence "/tmp/ws-smoke-flow.$$.json" --session ws-smoke-csrf --json 2>/dev/null)"
is "the two-step flow succeeds"  "$(echo "$FLOW" | jqp 'd["ok"]')" "True"
is "and the second step is 200"  "$(echo "$FLOW" | jqp 'd["steps"][1]["status"]')" "200"
has "the token was bound"        "$(echo "$FLOW" | jqp 'list(d["steps"][0]["bound"])')" "csrf"

echo
echo "── timing ───────────────────────────────────────────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/slow?wait=0" --session ws-smoke-time --new --capture >/dev/null 2>&1
FAST="$("$H5I" browser resend 0 --repeat 5 --session ws-smoke-time --json 2>/dev/null | jqp 'd["timing"]["ttfb_ms"]["median"]')"
SLOW="$("$H5I" browser resend 0 --set 'query.wait=1' --repeat 5 --session ws-smoke-time --json 2>/dev/null | jqp 'd["timing"]["ttfb_ms"]["median"]')"
if [ -n "$FAST" ] && [ -n "$SLOW" ] && [ "$SLOW" -gt $((FAST + 200)) ]; then
    ok "a 400ms server delay is visible in the median (${FAST}ms vs ${SLOW}ms)"
else
    bad "the delay did not show up (fast '${FAST}', slow '${SLOW}')"
fi
is "every send is sampled" \
   "$("$H5I" browser resend 0 --repeat 3 --session ws-smoke-time --json 2>/dev/null | jqp 'len(d["samples"])')" "3"

echo
echo "── uploads ──────────────────────────────────────────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/page" --session ws-smoke-up --new --capture >/dev/null 2>&1
# Built from nothing: the engine never posts a file itself, so a file-upload
# test has no recorded upload to start from.
UP="$("$H5I" browser resend 0 --create \
    --set 'method=POST' --set 'path=/upload' \
    --set 'multipart.file=<?php system($_GET[0]); ?>' \
    --set 'multipart.file.filename=shell.php' \
    --set 'multipart.file.content_type=text/php' \
    --session ws-smoke-up --json 2>/dev/null)"
is "a type filter refuses the obvious try" "$(echo "$UP" | jqp 'd["response"]["status"]')" "415"

BYPASS="$("$H5I" browser resend 0 --create \
    --set 'method=POST' --set 'path=/upload' \
    --set 'multipart.file=<?php system($_GET[0]); ?>' \
    --set 'multipart.file.filename=../shell.php' \
    --set 'multipart.file.content_type=image/png' \
    --session ws-smoke-up --json 2>/dev/null)"
is "and a lie about the type gets past it" "$(echo "$BYPASS" | jqp 'd["response"]["status"]')" "200"
SEQ="$(echo "$BYPASS" | jqp 'd["seq"]')"
has "with the filename the server stored" \
    "$("$WEBSEC" show "res_$SEQ" --session ws-smoke-up 2>/dev/null)" "../shell.php"

echo
echo "── races ────────────────────────────────────────────────────────────"
# The warm-up spends its own coupon, so the burst is aimed at an unused one.
"$H5I" browser open "http://127.0.0.1:$PORT/redeem?coupon=warmup" \
    --session ws-smoke-race --new --capture >/dev/null 2>&1
BURST="$("$H5I" browser resend 0 --set 'query.coupon=race' --repeat 20 --race \
    --session ws-smoke-race --json 2>/dev/null)"
WON="$(echo "$BURST" | jqp 'sum(1 for s in d["samples"] if s["status"] == 200)')"
is "the whole burst was sent" "$(echo "$BURST" | jqp 'len(d["samples"])')" "20"
if [ -n "$WON" ] && [ "$WON" -gt 1 ]; then
    ok "a one-use coupon was redeemed $WON times (the check-then-act window)"
else
    bad "the window was not reached (won '$WON' of 20)"
fi
is "and every send is in the receipts" \
   "$("$H5I" browser requests --session ws-smoke-race --url-contains coupon=race --json 2>/dev/null | jqp 'd["shown"] == 40')" "True"

echo
echo "── the request log, narrowed ────────────────────────────────────────"
"$H5I" browser open "http://127.0.0.1:$PORT/page" --session ws-smoke-log --new --capture >/dev/null 2>&1
"$H5I" browser navigate "http://127.0.0.1:$PORT/missing" --session ws-smoke-log >/dev/null 2>&1
narrowed() { "$H5I" browser requests --session ws-smoke-log "$@" --json 2>/dev/null | jqp 'd["shown"]'; }
# Two rows, not four: the stylesheet's request and response. A `<script src>`
# is not fetched at all without `--script`, which is the engine's default and
# the reason page-borne script has no delivery channel here.
is "subresources can be picked out" "$(narrowed --initiator subresource)" "2"
is "so can a status"                "$(narrowed --status 404)" "1"
is "so can a URL"                   "$(narrowed --url-contains .css)" "2"
is "a limit is a narrowing"         "$("$H5I" browser requests --session ws-smoke-log --limit 2 --json 2>/dev/null | jqp 'd["narrowed"]')" "True"

echo
echo "── the line protocol ────────────────────────────────────────────────"
RPC=$(printf '{"id":1,"verb":"ping"}\n{"id":2,"verb":"resend","from":0,"raw_target":"/page"}\n{"id":3,"verb":"nope"}\n' \
      | "$H5I" browser rpc --stdio --session ws-smoke-a)
is "ping is answered, with its id" "$(echo "$RPC" | sed -n 1p | jqp "d['id'], d['ok']")" "1 True"
is "a resend answers with the receipt it made" "$(echo "$RPC" | sed -n 2p | jqp "d['response']['status']")" "200"
is "an unknown verb is an error carrying the same id" "$(echo "$RPC" | sed -n 3p | jqp "d['id'], d['error']['code']")" "3 verb"

echo "── the plugin ───────────────────────────────────────────────────────"
PLUGIN="$WEBSEC"
if [ -x "$PLUGIN" ]; then
    "$H5I" plugin install websec --from "$PLUGIN" --force >/dev/null 2>&1
    is "the plugin reports itself installed" \
       "$("$H5I" plugin list --json 2>/dev/null | jqp 'd[0]["installed"]')" "True"
    is "and drives a session" \
       "$("$H5I" websec replay req_0 --set 'query.user_id=2' --session ws-smoke-a 2>/dev/null | jqp 'd["response"]["status"]')" "200"
    # The codes have to survive two hops: h5i -> plugin -> h5i browser.
    "$H5I" websec match res_1 --json-path role=admin --session ws-smoke-a >/dev/null 2>&1
    is "a hit still exits 0 through the plugin" "$?" "0"
    "$H5I" websec match res_1 --contains 'not-in-this-body' --session ws-smoke-a >/dev/null 2>&1
    is "a miss still exits 1" "$?" "1"
    "$H5I" websec show req_42x --session ws-smoke-a >/dev/null 2>&1
    is "and a bad id exits 2" "$?" "2"
else
    echo "  (skipped: no h5i-websec beside $H5I — cargo build --release -p h5i-websec)"
fi

echo
if [ "$FAILED" -eq 0 ]; then echo "all websec smoke checks passed"; else echo "SMOKE FAILURES"; fi
exit "$FAILED"
