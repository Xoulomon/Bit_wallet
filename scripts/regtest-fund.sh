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

# A throwaway wallet on the node, used only to mine and to send us coins. It
# has nothing to do with the bot's wallets, which are BIP84 descriptors the
# node never sees.
FUND_WALLET="regtest-fund"

# Chain-level calls go to the bare endpoint.
rpc() { call "$URL" "$@"; }

# Wallet-level calls must name the wallet: Polar loads several, and Core then
# refuses a bare `sendtoaddress` with -19 rather than guessing which to use.
wrpc() { call "$URL/wallet/$FUND_WALLET" "$@"; }

call() {
  local endpoint="$1" method="$2"
  local params="${3:-[]}"
  curl -s --user "$USER:$PASS" -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"1.0\",\"id\":\"fund\",\"method\":\"$method\",\"params\":$params}" \
    "$endpoint"
}

# Extract the `result` field, or report the node's error rather than printing
# `null` and carrying on as though the call had worked.
result() {
  python3 -c 'import json,sys
body = json.load(sys.stdin)
if body.get("error"):
    sys.exit("rpc error: " + str(body["error"]))
print(json.dumps(body["result"]))'
}

# Polar's node may have no loaded wallet; create a throwaway one for mining.
ensure_wallet() {
  rpc createwallet "[\"$FUND_WALLET\"]" >/dev/null 2>&1 || true
  rpc loadwallet "[\"$FUND_WALLET\"]" >/dev/null 2>&1 || true
}

case "${1:-}" in
  mine)
    blocks="${2:-1}"
    ensure_wallet
    address=$(wrpc getnewaddress | result | tr -d '"')
    wrpc generatetoaddress "[$blocks, \"$address\"]" | result \
      | python3 -c 'import json,sys; print("mined", len(json.load(sys.stdin)), "block(s)")'
    ;;

  pay)
    address="${2:?usage: regtest-fund.sh pay <address> <sats>}"
    sats="${3:?usage: regtest-fund.sh pay <address> <sats>}"
    ensure_wallet
    btc=$(python3 -c "print(f'{$sats/100000000:.8f}')")
    echo "sending $sats sats to $address"
    wrpc sendtoaddress "[\"$address\", $btc]" | result
    # One block so it confirms; comment this out to test unconfirmed handling.
    miner=$(wrpc getnewaddress | result | tr -d '"')
    wrpc generatetoaddress "[1, \"$miner\"]" >/dev/null
    echo "confirmed"
    ;;

  info)
    rpc getblockchaininfo | result \
      | python3 -c 'import json,sys
d = json.load(sys.stdin)
print("chain", d["chain"], " blocks", d["blocks"])'
    ;;

  *)
    echo "usage: regtest-fund.sh {mine <n> | pay <address> <sats> | info}" >&2
    exit 2
    ;;
esac
