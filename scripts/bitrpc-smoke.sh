#!/usr/bin/env bash
# PLAN.md §10 — BitRPC mainnet smoke test.
#
# Confirms the §4b allowlist still matches what the plan assumes, BEFORE any code
# is written against the backend. Needs only BITRPC_API_KEY in the environment
# (or in .env); makes no wallet calls and moves no funds.
#
#   BITRPC_API_KEY=... ./scripts/bitrpc-smoke.sh
#
# The key is never echoed.

set -euo pipefail

BITRPC_URL="${BITRPC_URL:-https://bitrpc.thebuidl.xyz}"
ENDPOINT="$BITRPC_URL/bitcoin"

if [[ -z "${BITRPC_API_KEY:-}" && -f .env ]]; then
  # shellcheck disable=SC1091
  BITRPC_API_KEY="$(grep -E '^BITRPC_API_KEY=' .env | head -1 | cut -d= -f2-)"
fi

if [[ -z "${BITRPC_API_KEY:-}" ]]; then
  echo "BITRPC_API_KEY is not set (export it, or put it in .env). Aborting." >&2
  exit 2
fi

pass=0
fail=0

ok()   { echo "  PASS  $1"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $1"; fail=$((fail + 1)); }

# rpc <method> [params-json] -> prints "<http-status>\n<body>"
rpc() {
  local method="$1" params="${2:-[]}"
  curl -s -o /tmp/bitrpc-smoke.body -w '%{http_code}' \
    -X POST "$ENDPOINT" \
    -H 'Content-Type: application/json' \
    -H "X-API-Key: $BITRPC_API_KEY" \
    --data "{\"jsonrpc\":\"1.0\",\"id\":\"smoke\",\"method\":\"$method\",\"params\":$params}"
}

jqr() { python3 -c 'import json,sys;d=json.load(sys.stdin);print(json.dumps(d.get("result")))' < /tmp/bitrpc-smoke.body; }

echo "BitRPC smoke test against $ENDPOINT"
echo

# 1. getblockchaininfo -> chain must be "main"
echo "1. getblockchaininfo"
code=$(rpc getblockchaininfo)
if [[ "$code" == "200" ]]; then
  chain=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["chain"])' < /tmp/bitrpc-smoke.body)
  [[ "$chain" == "main" ]] && ok "chain = main" || bad "chain = $chain (expected main)"
else
  bad "HTTP $code — 401 means a missing key, 403 an invalid one"
fi

# 2. getblockcount, then getblockhash/getblock round-trip one block
echo "2. getblockcount + getblockhash/getblock round trip"
code=$(rpc getblockcount)
if [[ "$code" == "200" ]]; then
  height=$(jqr)
  ok "getblockcount = $height"
  code=$(rpc getblockhash "[$height]")
  if [[ "$code" == "200" ]]; then
    hash=$(jqr)
    ok "getblockhash($height) = $hash"
    code=$(rpc getblock "[$hash]")
    if [[ "$code" == "200" ]]; then
      back=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["height"])' < /tmp/bitrpc-smoke.body)
      [[ "$back" == "$height" ]] && ok "getblock round-trips to height $back" \
                                 || bad "getblock height $back != $height"
    else
      bad "getblock -> HTTP $code"
    fi
  else
    bad "getblockhash -> HTTP $code"
  fi
else
  bad "getblockcount -> HTTP $code"
fi

# 3. getmempoolinfo -> the mempoolminfee our fee floor depends on (§6)
echo "3. getmempoolinfo (source of the mainnet fee floor)"
code=$(rpc getmempoolinfo)
if [[ "$code" == "200" ]]; then
  minfee=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["mempoolminfee"])' < /tmp/bitrpc-smoke.body)
  ok "mempoolminfee = $minfee BTC/kvB"
else
  bad "getmempoolinfo -> HTTP $code (the fee floor would fall back to MAINNET_MIN_FEE_SAT_VB)"
fi

# 4. a deliberately disallowed method must be refused, proving the allowlist is live
echo "4. estimatesmartfee must be refused (allowlist enforcement)"
code=$(rpc estimatesmartfee '[6]')
if [[ "$code" == "403" ]]; then
  ok "estimatesmartfee -> 403, allowlist enforced as §4b assumes"
elif [[ "$code" == "200" ]]; then
  bad "estimatesmartfee returned 200 — the allowlist has CHANGED; §6's fee design can be simplified"
else
  bad "estimatesmartfee -> HTTP $code (expected 403)"
fi

# 5. burst past the quota must yield 429, proving the limiter in rpc/bitrpc.rs is needed
echo "5. burst past 100 req/min must yield 429"
saw429=0
for _ in $(seq 1 110); do
  c=$(rpc getblockcount)
  if [[ "$c" == "429" ]]; then saw429=1; break; fi
done
if [[ "$saw429" == "1" ]]; then
  ok "429 observed — the shared rate limiter is load-bearing"
else
  bad "no 429 after 110 requests; the documented 100/min limit may have changed"
fi

rm -f /tmp/bitrpc-smoke.body
echo
echo "$pass passed, $fail failed"
[[ "$fail" -eq 0 ]]
