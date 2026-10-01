#!/usr/bin/env bash
# Mine and fund helpers for regtest (PLAN.md §3).
#
#   ./scripts/regtest-fund.sh mine 101
#   ./scripts/regtest-fund.sh pay bcrt1q... 100000
#   ./scripts/regtest-fund.sh info
#
# Reads REGTEST_RPC_* from .env. Everything here is regtest-only by
# construction: generatetoaddress does not exist anywhere else.

set -euo pipefail

if [[ -f .env ]]; then
  # shellcheck disable=SC1091
  set -a; source .env; set +a
fi

URL="${REGTEST_RPC_URL:-http://127.0.0.1:18443}"
USER="${REGTEST_RPC_USER:-polaruser}"
PASS="${REGTEST_RPC_PASS:-polarpass}"

rpc() {
  local method="$1"; shift
  local params="${1:-[]}"
  curl -s --user "$USER:$PASS" -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"1.0\",\"id\":\"fund\",\"method\":\"$method\",\"params\":$params}" \
    "$URL"
}

result() { python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)["result"]))'; }

# Polar's node may have no loaded wallet; create a throwaway one for mining.
ensure_wallet() {
  rpc createwallet '["regtest-fund"]' >/dev/null 2>&1 || true
  rpc loadwallet '["regtest-fund"]' >/dev/null 2>&1 || true
}

case "${1:-}" in
  mine)
    blocks="${2:-1}"
    ensure_wallet
    address=$(rpc getnewaddress | result | tr -d '"')
    rpc generatetoaddress "[$blocks, \"$address\"]" | result \
      | python3 -c 'import json,sys;print(f"mined {len(json.load(sys.stdin))} block(s)")'
    ;;

  pay)
    address="${2:?usage: regtest-fund.sh pay <address> <sats>}"
    sats="${3:?usage: regtest-fund.sh pay <address> <sats>}"
    ensure_wallet
    btc=$(python3 -c "print(f'{$sats/100000000:.8f}')")
    echo "sending $sats sats to $address"
    rpc sendtoaddress "[\"$address\", $btc]" | result
    # One block so it confirms; comment this out to test unconfirmed handling.
    miner=$(rpc getnewaddress | result | tr -d '"')
    rpc generatetoaddress "[1, \"$miner\"]" >/dev/null
    echo "confirmed"
    ;;

  info)
    rpc getblockchaininfo | result \
      | python3 -c 'import json,sys;d=json.load(sys.stdin);print(f"chain {d[\"chain\"]}  blocks {d[\"blocks\"]}")'
    ;;

  *)
    echo "usage: regtest-fund.sh {mine <n> | pay <address> <sats> | info}" >&2
    exit 2
    ;;
esac
