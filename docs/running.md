# Running it

Three crates. `wallet-core` is a library, so you never run it — you test it.
`bot` and `wallet-cli` are the two front ends, and they drive the same core over
the same facade.

Everything below assumes `.env` is filled in and a Polar network with a bitcoind
backend is running. `./scripts/regtest-fund.sh info` is the one-line check:

```bash
./scripts/regtest-fund.sh info
# chain regtest  blocks 1731
```

---

## wallet-core — the library

Nothing to run. What you do instead:

```bash
# The gate. Everything must pass before a commit.
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test

# One suite at a time, when something fails
cargo test -p wallet-core --lib                  # 121 unit tests
cargo test -p wallet-core --test boundary        # the §3a layering rules
cargo test -p wallet-core --test onchain         # against a real bitcoind
cargo test -p wallet-core --test cli_boundary    # the facade, end to end
cargo test -p wallet-core --test bitrpc_transport

# The payjoin round trip. Ignored by default because it needs the public
# directory — see docs/payjoin-setup.md.
cargo test -p wallet-core --test payjoin_e2e -- --ignored --nocapture

# Prove the boundary holds: delete the Telegram front end entirely.
# (CI does this on every push.)
cargo test -p wallet-core --lib     # after removing crates/bot from Cargo.toml
```

The integration tests spawn their own `bitcoind` through `corepc-node`, so they
need neither Polar nor a BitRPC key. The first run downloads the binary.

---

## bot — the Telegram front end

```bash
cargo run -p bot                 # foreground, Ctrl-C to stop
cargo run --release -p bot       # what you want for anything long-lived
```

It refuses to start if the backend is unreachable or is serving a different
chain than `NETWORK` says — that refusal is deliberate, because mixing regtest
and mainnet state is the one mistake that cannot be undone.

Leave it running and talk to it in Telegram. More logs when you need them:

```bash
RUST_LOG=bot=debug,wallet_core=debug cargo run -p bot
```

### In the chat

Private chats only; it refuses in groups before any handler runs.

| | |
|---|---|
| `/start` `/help` | welcome, command list |
| `/create` | PIN, then the seed phrase, then a three-word check |
| `/restore` | from a seed phrase, with an optional birthday height |
| `/unlock` `/lock` | open or close a signing session |
| `/export` `/delete` | show the seed again, or destroy the wallet |
| `/receive` | next unused address, QR + BIP21 |
| `/addresses [page]` `/balance` `/history [page]` `/tx <txid>` | reading |
| `/send <address\|bip21> [sats\|max]` | fee card → confirm card → PIN |
| `/bumpfee <txid>` | raise the fee on a stuck transaction — same fee card as `/send` |
| `/pj_receive <sats>` `/pj_sessions` | payjoin |
| `/status` `/network` | backend tip, latency, call budget, session |
| `/mine <n>` | regtest, admins only — mines to *your* wallet |
| `/faucet [sats]` | regtest — funds your wallet from the node and confirms it |

Signing always costs a PIN, whether or not `/unlock` left a session open. The
session buys you reading and drafting; it does not buy you a signature. One
exception, and it is structural rather than an oversight: a payjoin *receive*
signs later and on its own, when the sender's proposal arrives, so there is no
moment at which anyone could be asked. What bounds that is
`SESSION_IDLE_TIMEOUT_SECS`, not a PIN.

`/balance`, the send confirm card, `/tx` and the send receipt carry an
approximate dollar value, from `PRICE_API` (mempool.space by default) and
cached for five minutes. On regtest the figure is the real mainnet price
applied to coins that are worth nothing, and the card says so. If the price
API cannot be reached the lines are simply absent — no command fails over it,
and no fee or amount is ever derived from a price.

A freshly mined coinbase needs 100 more blocks before it is spendable, so
`/mine 101` is the useful number — and `/mine` reports one result, not one
message per block. For coins you can spend immediately, `/faucet` is the short
way: it pays your next address from the node's own wallet and mines a block so
the coins are usable at once. It needs the node to have mined first, so on a
brand-new chain the order is `/mine 101`, then `/faucet`.

---

## wallet-cli — the same wallet, from a terminal

Not a toy: it is the proof the boundary holds, since anything it could not do
without reaching past `WalletService` would be a leak. It is also how you drive
two wallets at once without two Telegram accounts.

Each command is its own process, so the user is chosen explicitly:

```bash
export WALLET_CLI_USER=$(uuidgen)      # keep this to keep the wallet
cargo run -p wallet-cli -- status
```

Or per command: `cargo run -p wallet-cli -- --user <uuid> balance`.

```bash
cargo run -p wallet-cli -- status                  # backend, tip, session
cargo run -p wallet-cli -- create                  # seed shown once, 3-word check
cargo run -p wallet-cli -- restore
cargo run -p wallet-cli -- unlock                  # or lock
cargo run -p wallet-cli -- receive                 # address + BIP21
cargo run -p wallet-cli -- addresses 1
cargo run -p wallet-cli -- balance
cargo run -p wallet-cli -- history 1
cargo run -p wallet-cli -- fees                    # presets, floor, source
cargo run -p wallet-cli -- send <address> 50000 2  # to, sats|max, sat/vB
cargo run -p wallet-cli -- pj-receive 25000        # waits for the sender
cargo run -p wallet-cli -- pj-sessions
cargo run -p wallet-cli -- mine 101
cargo run -p wallet-cli -- events                  # CoreEvents as they arrive
```

PINs are read without echoing when there is a terminal, and from stdin when
there is not — which is what makes it scriptable:

```bash
printf '864213\n864213\n' | cargo run -q -p wallet-cli -- create

# `send` asks to confirm and then for the PIN, every time — an open session
# does not sign.
printf 'yes\n864213\n' | cargo run -q -p wallet-cli -- send <address> 50000 2
```

---

## Helpers

```bash
./scripts/polar-env.sh                              # .env lines from a running Polar
./scripts/regtest-fund.sh info                      # chain and height
./scripts/regtest-fund.sh mine 101                  # mine to a throwaway node wallet
./scripts/regtest-fund.sh pay <address> 250000      # fund an address, then confirm it

BITRPC_API_KEY=... ./scripts/bitrpc-smoke.sh        # check the mainnet allowlist

./scripts/payjoin-regtest.sh up|status|down|logs    # local payjoin pair (see the caveat)
```

---

## A full regtest run, start to finish

```bash
# 1. Polar is up and reachable
./scripts/regtest-fund.sh info

# 2. The bot, left running in its own terminal
cargo run -p bot

# 3. In Telegram: /create — the seed phrase is on screen for 15 seconds

# 4. Fund it, without leaving the chat
#    /mine 101     (admins only, and only needed once per chain)
#    /faucet 250000
#
#    Or from here, which is the same thing and works for any address:
#    ./scripts/regtest-fund.sh pay <address from /receive> 250000

# 5. Back in Telegram: /balance, /history, /send, /tx <txid>
```

### Two wallets, for a payjoin

The bot is one wallet; the CLI is the other.

```bash
# Receiver: the bot. /pj_receive 25000 → copy the bitcoin:… URI
#   (its wallet needs a confirmed UTXO to contribute, so fund it first)

# Sender: the CLI, funded the same way
export WALLET_CLI_USER=$(uuidgen)
cargo run -p wallet-cli -- create
cargo run -p wallet-cli -- receive
./scripts/regtest-fund.sh pay <cli address> 300000
cargo run -p wallet-cli -- send '<the bitcoin:… URI>' 25000 2
```

Quote the URI: it contains `&`, which a shell would otherwise read as "run this
in the background".

The sender reports whether it was a payjoin or fell back to an ordinary
payment. A fallback is a completed payment, not a failure — only the privacy
gain is lost. `/pj_sessions` and `pj-sessions` show where a session got to.

---

## When something is wrong

| | |
|---|---|
| `connecting to the Bitcoin backend` | Polar is down, or its port moved — re-run `./scripts/polar-env.sh`. Polar reassigns ports when you recreate a network. |
| `network mismatch` | `.env` and the node disagree about the chain. A refusal, not a bug. |
| Nothing confirms | Regtest only makes blocks when asked: `./scripts/regtest-fund.sh mine 1`. |
| `A network error … api.telegram.org` | A transient connect timeout. The bot logs it and carries on; no restart needed. |
| Bot silent on a command | It replies to anything it cannot route, so silence means the process is gone — check it is still running. |
| Payjoin will not start | The directory or relay did not answer; the bot now says so and keeps the reason. See `docs/payjoin-setup.md`. |

Wallet state lives in `data/{regtest,bitcoin}/`. Deleting it destroys those
wallets — only a seed phrase brings one back.
