#!/usr/bin/env bash
# Print the .env lines to copy from a running Polar bitcoind (PLAN.md §3).
#
#   ./scripts/polar-env.sh [container-name]
#
# Polar names its backend containers `polar-n<N>-<name>`. With no argument this
# finds the first bitcoind container it can.

set -euo pipefail

container="${1:-}"

if [[ -z "$container" ]]; then
  container=$(docker ps --format '{{.Names}}' \
    | grep -E '^polar-n[0-9]+-' \
    | while read -r name; do
        if docker exec "$name" which bitcoin-cli >/dev/null 2>&1; then
          echo "$name"
          break
        fi
      done)
fi

if [[ -z "$container" ]]; then
  echo "No running Polar bitcoind container found." >&2
  echo "Start a Polar network with a bitcoind backend, or pass the name:" >&2
  echo "  ./scripts/polar-env.sh polar-n1-backend1" >&2
  exit 1
fi

# Polar puts the credentials in the container's bitcoin.conf.
conf=$(docker exec "$container" cat /home/bitcoin/.bitcoin/bitcoin.conf 2>/dev/null || true)
user=$(grep -E '^rpcuser=' <<<"$conf" | head -1 | cut -d= -f2- || true)
pass=$(grep -E '^rpcpassword=' <<<"$conf" | head -1 | cut -d= -f2- || true)
port=$(docker port "$container" 18443/tcp 2>/dev/null | head -1 | awk -F: '{print $NF}')

echo "# Copy these into .env — from container $container"
echo "NETWORK=regtest"
echo "REGTEST_RPC_URL=http://127.0.0.1:${port:-18443}"
echo "REGTEST_RPC_USER=${user:-polaruser}"
echo "REGTEST_RPC_PASS=${pass:-polarpass}"
echo
echo "# Check it:"
echo "#   curl -s --user ${user:-polaruser}:${pass:-polarpass} \\"
echo "#     --data '{\"method\":\"getblockchaininfo\",\"params\":[]}' \\"
echo "#     http://127.0.0.1:${port:-18443}"
