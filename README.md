# Telegram Bitcoin Wallet Bot

A non-custodial, multi-user Bitcoin wallet you use through a Telegram chat
instead of a CLI or a GUI. On-chain payments plus **payjoin** (BIP77 v2 send and
receive, BIP78 v1 sending), switchable between **regtest** (a Polar bitcoind)
and **mainnet** (BitRPC's hosted Bitcoin Core) with one setting.

Each user gets their own BIP84 wallet. The seed is generated on the server,
encrypted with a PIN only that user knows, and never leaves it — see
[Security model](#security-model) for exactly what that does and does not buy
you.

---

## Features

| | |
|---|---|
| **Create / restore** | 12-word BIP39 mnemonic, shown once in a self-deleting message, then a three-word check. Restore takes an optional birthday height. |
| **BIP84 native SegWit** | `wpkh(.../84'/{0,1}'/0'/{0,1}/*)`, coin type 0 on mainnet and 1 on regtest. Verified against the BIP84 specification's own test vectors. |
| **Addresses** | `/receive` gives the next unused address as a QR and a BIP21 URI; `/addresses` lists them with used/unused status. |
| **Sync** | One shared block emitter serves every user, so a block is fetched once however many wallets exist. |
| **Balance and history** | Confirmed, pending and immature, in sats and BTC, with an approximate `≈ $` value; history with fees and confirmation counts. |
| **Send** | Fee presets or a typed sat/vB, a confirmation card with every number on it, PIN-gated signing, broadcast, confirmation tracking. `/bumpfee` for a rate that turned out too low. |
| **Payjoin** | `/pj_receive` opens a BIP77 v2 session; paying a `pj=` URI uses the payjoin sender, falling back to an ordinary transaction if the receiver never answers. |
| **Two front ends** | The Telegram bot and `wallet-cli` drive the same `WalletService`. Deleting either leaves a working wallet. |

---

## Requirements

| | Why |
|---|---|
| Rust stable ≥ 1.85 (edition 2024) | `bdk_wallet` and `payjoin` MSRV |
| `build-essential`, `pkg-config` | `rusqlite` compiles SQLite in C; rustls means no OpenSSL |
| Docker + Docker Compose | Polar runs its nodes in Docker; the local payjoin directory too |
| [Polar](https://lightningpolar.com) ≥ 3.x | The regtest network — a single bitcoind backend, no Lightning nodes needed |
| A Telegram bot token | From [@BotFather](https://t.me/botfather) |
| A BitRPC API key | **Mainnet only.** See [`docs/bitrpc-setup.md`](docs/bitrpc-setup.md) |

Disk and RAM scale with the number of users, not with the chain: no chain data
is stored on either network.

```bash
sudo apt install build-essential pkg-config
cargo build --release
```

---

## Quick start (regtest)

```bash
# 1. A Polar network with one bitcoind backend, then:
./scripts/polar-env.sh              # prints the .env lines to copy

cp .env.example .env                # fill in TELOXIDE_TOKEN and the REGTEST_* block

# 2. Run it (payjoin needs no local setup — see docs/payjoin-setup.md)
cargo run -p bot
```

Then message your bot: `/start`, `/create`, `/mine 101`, `/faucet`, `/balance`.
`/mine 101` is needed once per chain, to give the node's own wallet something
to hand out; `/faucet` then pays you from it and mines a block so the coins are
spendable immediately.

The same wallet from a terminal:

```bash
cargo run -p wallet-cli -- status
cargo run -p wallet-cli -- create
```

[`docs/running.md`](docs/running.md) is the full command reference for all
three crates, [`docs/polar-setup.md`](docs/polar-setup.md) walks through the
regtest setup, and [`docs/payjoin-setup.md`](docs/payjoin-setup.md) covers
payjoin.

---

## Configuration

Every setting lives in `.env`, documented in
[`.env.example`](.env.example). `.env` is gitignored and must stay that way: it
holds a bot token and, on mainnet, an API key that is shown to you exactly once.

### The one that matters

```
NETWORK=regtest      # regtest | bitcoin
```

`NETWORK` picks the backend block and namespaces **all** state under
`data/{regtest,bitcoin}/`. Regtest and mainnet data can never mix, and a wallet
file refuses to load under the wrong chain.

### Reference

| Setting | Default | What it does |
|---|---|---|
| `NETWORK` | — | `regtest` or `bitcoin`. Required. |
| `TELOXIDE_TOKEN` | — | Bot token from @BotFather. |
| `DATA_DIR` | `./data` | Root of the state tree. |
| `BOT_ADMIN_IDS` | empty | Telegram ids that may use `/mine`. |
| `ALLOWED_USER_IDS` | empty | Allowlist; empty means anyone. |
| `REGTEST_RPC_URL` | `http://127.0.0.1:18443` | Polar's bitcoind. |
| `REGTEST_RPC_USER` / `REGTEST_RPC_PASS` | — | From Polar's Connect tab. Required on regtest. |
| `REGTEST_PAYJOIN_DIRECTORY` | `https://payjo.in` | Public even on regtest — a local pair cannot work yet, see [`docs/payjoin-setup.md`](docs/payjoin-setup.md). |
| `REGTEST_OHTTP_RELAY` | `https://pj.benalleng.com` | Must be a *different* operator from the directory; OHTTP depends on it. |
| `REGTEST_FALLBACK_FEE_SAT_VB` | `2` | Used when `estimatesmartfee` has no data — every fresh regtest chain. |
| `BITRPC_URL` | `https://bitrpc.thebuidl.xyz` | Mainnet chain source. |
| `BITRPC_API_KEY` | — | Required on mainnet. Never commit it. |
| `BITRPC_RATE_LIMIT_PER_MIN` | `90` | Headroom under the hard 100/min. |
| `BITRPC_SYNC_BUDGET_PER_MIN` | `60` | The emitter's share, so a sync backlog cannot starve a `/send`. |
| `MAINNET_MAX_RESCAN_BLOCKS` | `10000` | `/restore` refuses a deeper birthday. |
| `MAINNET_MIN_FEE_SAT_VB` | `1` | Hard floor when `getmempoolinfo` is unavailable. |
| `MAINNET_FEE_API` | `https://mempool.space/api` | BitRPC has no `estimatesmartfee`. |
| `MAINNET_PAYJOIN_DIRECTORY` | `https://payjo.in` | |
| `MAINNET_OHTTP_RELAY` | `https://pj.benalleng.com` | Alternatives: `pj.bobspacebkk.com`, `payjoin.achow101.com`. |
| `MAINNET_I_UNDERSTAND_RISK` | `false` | Must be `true` or the bot refuses to start on mainnet. |
| `SESSION_IDLE_TIMEOUT_SECS` | `600` | How long a wallet stays unlocked. |
| `FEE_CACHE_SECS` | `60` | How long a fee estimate is reused. |
| `PRICE_API` | `https://api.coingecko.com/api/v3` | BTC/USD for the `≈ $` lines, with mempool.space as an automatic fallback. Used on both chains; if neither answers the lines are absent and `/status` says so. |
| `MAX_SEND_SATS` | empty | Optional per-payment cap. Running mainnet without one logs a warning. |

---

## Commands

| Command | What it does |
|---|---|
| `/start` | Welcome, or the main menu if you have a wallet |
| `/help` | This list, in the chat |
| `/create` | New wallet: PIN, then the seed phrase, then a three-word check |
| `/restore` | Restore from a seed phrase, with an optional birthday height |
| `/unlock` · `/lock` | Open or close a session. A session unlocks reading and drafting — signing always costs a PIN |
| `/export` | Show your seed phrase again (PIN required, self-deleting) |
| `/delete` | Delete your wallet — type `DELETE`, then the PIN |
| `/receive` | Next unused address, as a QR and a BIP21 URI |
| `/addresses [page]` | Revealed addresses with used/unused and amounts received |
| `/balance` | Confirmed, pending, incoming and immature, with a Refresh button |
| `/history [page]` | Transactions, newest first, with fees and confirmations |
| `/tx <txid>` | One transaction in detail |
| `/send <address\|bip21> [sats\|max]` | Fee choice → confirmation card → PIN → broadcast |
| `/bumpfee <txid>` | Raise the fee on a stuck transaction — the same fee card as `/send` |
| `/pj_receive <sats>` | Ask to be paid with payjoin |
| `/pj_sessions` | Payjoin sessions and their state, with cancel |
| `/status` | Backend tip, latency, call budget, session state |
| `/network` | Which chain this instance is bound to |
| `/mine <n>` | Regtest only, admins only. One message, with the new balance |
| `/faucet [sats]` | Regtest only. Funds your wallet from the node and mines a block so it is spendable |

`wallet-cli` offers the same set; run it with no arguments for its usage.

---

## Architecture

Three crates, one direction of dependency, enforced by tests rather than by
convention:

```
bot  ·  wallet-cli          front ends: chat/terminal in, rendered output out
        │
        ▼
   WalletService            the only entry point a front end may call
        │
   keys · onchain · payjoin · rpc · session · storage
        │
   Polar bitcoind  |  BitRPC
```

`wallet-core` is the wallet. It cannot name `teloxide`, never returns a
formatted string or an emoji, has never heard of a Telegram id, and never blocks
waiting for a human — every human decision is a `quote_*` / `confirm_*` pair.
`wallet-cli` exists to prove that: anything it could not do without reaching
past the facade would be a leak.

[`docs/architecture.md`](docs/architecture.md) has the layer diagram (also
exported to [`docs/architecture.svg`](docs/architecture.svg)) and sequence
diagrams for send and payjoin receive.

---

## Security model

**What protects your coins.** The seed is encrypted with
XChaCha20-Poly1305 under a key derived by Argon2id (64 MiB, 3 passes) from your
PIN. A 6–8 digit PIN is a small search space, so the cost of one guess is the
whole defence — together with a lockout after five failures, counted in the
database and shared by every front end.

**A PIN is required for every signature**, not merely when the wallet is locked.
`confirm_send` takes a `&Pin` rather than anything a session could satisfy, so a
front end cannot broadcast without one. An open session buys reading and
drafting; it is not a bearer token for spending. The one exception is
structural: a payjoin *receive* signs later and on its own, when the sender's
proposal arrives and nobody is in the chat, so what bounds that is
`SESSION_IDLE_TIMEOUT_SECS` rather than a PIN.

**What is persisted.** The BDK wallet on disk holds only *public* descriptors.
Syncing, balances, addresses and history therefore need no PIN, and a stolen
wallet file reveals your transaction history but cannot spend a satoshi.

**What happens on screen.** Seed phrases are shown once and delete themselves
after 30 seconds, and the card says so using the same constant the deleter
uses. PIN messages are deleted the moment they arrive. The bot refuses to work
in group chats at all, before any handler runs.

**What is never logged.** The mnemonic and `BITRPC_API_KEY` are redacted from
every `Debug` impl and every error message, and a test greps the source to keep
it that way.

**What this is not.** See the first limitation below.

---

## Known limitations

- The bot server sees the seed while a session is unlocked, so this is
  "PIN-protected self-hosted", not hardware-grade non-custodial. Each user
  should run their own bot instance for maximum trust.
- **Mainnet unconfirmed incoming payments are not detected.** `getrawmempool`
  is not on BitRPC's allowlist, so incoming funds appear only once they are
  mined. Outgoing transactions do show as pending.
- **Mainnet fee estimation is third-party.** BitRPC does not expose
  `estimatesmartfee`, so Fast/Normal/Slow come from mempool.space rather than
  from a node we trust, and a manual sat/vB entry is always available as a
  fallback. If that API is unreachable the bot asks for the rate instead of
  guessing. Every rate is floored at the node's own `mempoolminfee`;
  `/bumpfee` is the remedy for a rate that turns out too low.
- **No `testmempoolaccept` on mainnet:** broadcasts go out without a dry run,
  and payjoin's broadcast-suitability check is best-effort rather than
  authoritative.
- **Mainnet restores are capped** at `MAINNET_MAX_RESCAN_BLOCKS` (~10 000
  blocks, ~10 weeks). Recovering a genuinely old wallet needs a different chain
  source; a full SegWit-era rescan through BitRPC would take roughly 230 hours.
- **All bot users share one 100 req/min BitRPC budget**, so sync pace and
  command latency degrade as users are added.
- **BitRPC is a single point of failure and a privacy trade-off:** its operator
  can see every address the bot queries and every transaction it broadcasts.
  Payjoin protects against outside chain analysis, not against the backend.
- **Payjoin uses a public directory and relay, on both networks.** OHTTP
  requires those two to be separate operators, and `payjoin-mailroom` 0.1.2
  offers no way to point its relay half at a local directory — so a fully
  local pair cannot complete a session, and the directory operator sees
  session metadata. [`docs/payjoin-setup.md`](docs/payjoin-setup.md) has the
  diagnosis and the evidence.

---

## Testing

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Integration tests spawn a throwaway `bitcoind` through `corepc-node`, so they
need neither Polar nor a BitRPC key and run unchanged in CI. The first run
downloads the binary.

The boundary tests are worth knowing about: `cargo tree` proves `wallet-core`
cannot name `teloxide`, `qrcode`, `image` or `comfy-table`; a grep proves
neither front end holds a `bdk_wallet` type, a `Psbt` or a `Mnemonic`; another
proves no secret reaches a log line; and `cargo test -p wallet-core` passes with
`crates/bot/` removed from the workspace entirely.

Before writing code against the mainnet backend, confirm its allowlist still
matches what this project assumes:

```bash
BITRPC_API_KEY=... ./scripts/bitrpc-smoke.sh
```

The payjoin round trip has its own end-to-end test, which is `#[ignore]`d
because it needs the public directory:

```bash
cargo test -p wallet-core --test payjoin_e2e -- --ignored --nocapture
```

Two wallets complete a real BIP77 v2 payjoin — the receiver contributes an
input, the final transaction has inputs from both, and the balances move by the
right amounts — plus the case where nobody is listening and the sender falls
back to an ordinary payment. §10 asks for `payjoin-test-utils` instead, which
would keep this off the network; its only published version depends on
`payjoin` 0.24 while this project uses 1.1, so it cannot be used yet.
[`docs/payjoin-setup.md`](docs/payjoin-setup.md) explains why a local directory
is not an option either.

---

## Licence

MIT.
