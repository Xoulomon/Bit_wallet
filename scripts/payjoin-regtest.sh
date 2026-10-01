#!/usr/bin/env bash
# Run a local payjoin directory and OHTTP relay for regtest (PLAN.md §7).
#
# NOTE: this cannot complete a payjoin session with payjoin-mailroom 0.1.2 —
# the relay half's gateway is not configurable and is hard-wired to
# https://payjo.in, so it answers a key fetch for the local directory with its
# own keys. Both halves start and serve keys correctly, so this is ready for the
# release that adds that setting. Regtest payjoin meanwhile uses the public
# directory with a live public relay; docs/payjoin-setup.md has the diagnosis.
#
#   ./scripts/payjoin-regtest.sh up      # start both, in the background
#   ./scripts/payjoin-regtest.sh status  # are they answering?
#   ./scripts/payjoin-regtest.sh down    # stop both
#   ./scripts/payjoin-regtest.sh logs    # tail both logs
#
# Two instances, and that is not redundancy. `payjoin-mailroom` is directory
# and relay in one binary, but it refuses to serve a request where both halves
# are itself:
#
#     Forbidden: Relay and gateway must be operated by different entities
#
# which is OHTTP's whole point — one operator who is both relay and gateway
# sees the client's address *and* the decrypted request, so the privacy
# guarantee evaporates. In production they are different companies; locally
# they are two processes with different sentinel tags, which is the least that
# lets the protocol run honestly.
#
# Native processes rather than containers because Docker Desktop runs a VM:
# `network_mode: host` there means the VM, so nothing lands on your loopback,
# and the directory URL embedded in a payjoin URI has to resolve identically
# for the bot (on the host) and for the relay (resolving the gateway). Two
# plain processes on real localhost make that true by construction.
# docker-compose.payjoin.yml is the equivalent for a native Linux Docker engine.

set -euo pipefail

VERSION="0.1.2"
DIR_PORT="${PJ_DIRECTORY_PORT:-8080}"
RELAY_PORT="${PJ_RELAY_PORT:-8081}"
RUN_DIR="${PJ_RUN_DIR:-./data/payjoin}"

mkdir -p "$RUN_DIR"/{directory,relay}

bin() {
  if command -v payjoin-mailroom >/dev/null 2>&1; then
    echo payjoin-mailroom
  elif [[ -x "$HOME/.cargo/bin/payjoin-mailroom" ]]; then
    echo "$HOME/.cargo/bin/payjoin-mailroom"
  else
    echo "payjoin-mailroom is not installed. Run:" >&2
    echo "  cargo install payjoin-mailroom --version $VERSION --locked" >&2
    exit 1
  fi
}

start_one() {
  local name="$1" port="$2"
  local pidfile="$RUN_DIR/$name.pid"

  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
    echo "  $name already running (pid $(cat "$pidfile")) on :$port"
    return
  fi

  PJ_LISTENER="127.0.0.1:$port" \
  PJ_STORAGE_DIR="$RUN_DIR/$name" \
  RUST_LOG="${RUST_LOG:-info}" \
    nohup "$(bin)" > "$RUN_DIR/$name.log" 2>&1 &

  echo $! > "$pidfile"
  echo "  $name started (pid $!) on :$port"
}

stop_one() {
  local name="$1"
  local pidfile="$RUN_DIR/$name.pid"
  if [[ -f "$pidfile" ]]; then
    local pid
    pid=$(cat "$pidfile")
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      echo "  $name stopped (pid $pid)"
    fi
    rm -f "$pidfile"
  else
    echo "  $name was not running"
  fi
}

# The key endpoint is the one the bot actually calls at startup, so it is the
# honest thing to check.
probe() {
  local name="$1" port="$2"
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 \
    "http://127.0.0.1:$port/ohttp-keys" 2>/dev/null || echo 000)
  if [[ "$code" == "200" ]]; then
    echo "  $name  :$port  serving OHTTP keys"
  else
    echo "  $name  :$port  NOT answering (http $code)"
    return 1
  fi
}

case "${1:-}" in
  up)
    echo "Starting the payjoin directory and relay:"
    start_one directory "$DIR_PORT"
    start_one relay "$RELAY_PORT"

    # Give them a moment to bind, then prove they are up rather than assuming.
    for _ in $(seq 1 20); do
      if probe directory "$DIR_PORT" >/dev/null 2>&1 \
         && probe relay "$RELAY_PORT" >/dev/null 2>&1; then
        break
      fi
      sleep 0.5
    done

    echo
    probe directory "$DIR_PORT"
    probe relay "$RELAY_PORT"
    echo
    echo "Put these in .env:"
    echo "  REGTEST_PAYJOIN_DIRECTORY=http://localhost:$DIR_PORT"
    echo "  REGTEST_OHTTP_RELAY=http://localhost:$RELAY_PORT"
    ;;

  down)
    echo "Stopping:"
    stop_one relay
    stop_one directory
    ;;

  status)
    probe directory "$DIR_PORT" || true
    probe relay "$RELAY_PORT" || true
    ;;

  logs)
    tail -f "$RUN_DIR"/directory.log "$RUN_DIR"/relay.log
    ;;

  *)
    echo "usage: payjoin-regtest.sh {up|down|status|logs}" >&2
    exit 2
    ;;
esac
