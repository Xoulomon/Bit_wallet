# BitRPC setup (mainnet)

BitRPC is an authenticated proxy in front of a shared Bitcoin Core. No node is
run or synced locally — which is the point, and also the source of every
mainnet limitation this project has.

> **Read the allowlist section before you rely on this backend.** Four missing
> RPC methods change what the wallet can honestly promise, and the design works
> around each one rather than pretending otherwise.

## 1. Get a key

1. Log in at `https://bitrpc.thebuidl.xyz/login` with your registered email.
2. Enter the 6-digit code. It lasts 10 minutes and works once.
3. Create a key at `/dashboard`.
4. **The raw key is shown exactly once.** Put it straight into `.env`:

```
BITRPC_API_KEY=...
```

`.env` is gitignored. Keep it that way — a key in a commit is a key you have to
rotate.

## 2. Confirm the allowlist still matches

Run this **before** trusting the backend. It checks the five things this
project's design depends on, and never echoes your key:

```bash
BITRPC_API_KEY=... ./scripts/bitrpc-smoke.sh
```

It asserts:

1. `getblockchaininfo` reports `chain: "main"`;
2. `getblockcount` → `getblockhash` → `getblock` round-trips one block;
3. `getmempoolinfo` returns the `mempoolminfee` the fee floor depends on;
4. `estimatesmartfee` is **refused with 403** — proving the allowlist is live;
5. a burst past 100 requests returns **429**.

If step 4 returns 200 instead, the allowlist has changed and the fee design in
`rpc/fees.rs` could be simplified. That is worth knowing.

## 3. Turn mainnet on

```
NETWORK=bitcoin
BITRPC_API_KEY=...
MAINNET_I_UNDERSTAND_RISK=true
MAX_SEND_SATS=20000
```

The bot refuses to start on mainnet unless `MAINNET_I_UNDERSTAND_RISK` is
exactly `true`, and it refuses if the key is empty. `MAX_SEND_SATS` is optional
but recommended: without it a single command can spend the whole balance, and
the bot logs a warning saying so.

State is namespaced, so mainnet lands in `data/bitcoin/` and cannot touch
anything your regtest testing produced.

## What is allowed

| Method | What the bot does with it |
|---|---|
| `getblockchaininfo` | Startup network check, `/status` tip |
| `getblockcount` | Birthday for new wallets, restore-depth arithmetic |
| `getblockhash`, `getblock` | Block following, ~2 calls per block |
| `getrawtransaction` | `/tx` for a txid the wallet has not indexed |
| `decoderawtransaction` | Debugging and payjoin inspection |
| `sendrawtransaction` | Broadcast |
| `getmempoolinfo` | `mempoolminfee` → the sat/vB floor |
| `getnetworkinfo` | `/status` diagnostics |

`getwalletinfo`, `getbalance`, `listunspent` and `getnewaddress` are **never
called**. They address the shared node's own wallet rather than your BIP84
descriptors, so touching them would be meaningless at best and would leak the
shared node's state at worst.

BitRPC also proxies a shared LND node. This wallet does not use it: the scope is
on-chain plus payjoin, and that node's keys are not yours — anything routed
through it would be custodial.

## What is missing, and what it costs

| Missing | Consequence |
|---|---|
| `getrawmempool` | The emitter cannot read the mempool. **Unconfirmed incoming payments are invisible until they are mined.** Outgoing transactions still show as pending, because the bot inserts what it just broadcast. |
| `estimatesmartfee` | Fee presets come from mempool.space instead, floored at `mempoolminfee`, with a manual sat/vB always available. If that API is unreachable the bot asks for a rate rather than guessing. |
| `testmempoolaccept` | No dry run before broadcast — a rejection arrives from the broadcast itself, and its reason is shown verbatim. Payjoin's broadcast-suitability check falls back to a documented best-effort substitute. |
| `getblockheader`, `getblockfilter` | The BIP158 compact-filter sync path is unusable. |
| `generatetoaddress` | `/mine` stays regtest-only, as it always was. |

## Errors

| Status | Meaning | Does retrying help? |
|---|---|---|
| 401 | No key sent | No — check `BITRPC_API_KEY` |
| 403 | Invalid key, **or** the method is not allowlisted | No |
| 429 | Over 100 req/min | Yes, after a backoff |
| 502 | Upstream node unavailable | Yes |

Each maps to its own `CoreError` variant, which is what lets the bot retry only
the bottom two.

## Rate limits

**100 requests per minute per key, shared by every user of your instance.** One
limiter gates every mainnet call, and the block emitter draws from a smaller
allowance (`BITRPC_SYNC_BUDGET_PER_MIN`) inside that budget, so a sync backlog
can never starve someone's `/send`.

The poll interval follows the budget rather than being fixed. At the defaults
that is roughly 60 idle calls an hour plus about 12 calls of real blocks —
comfortably inside 100/min, with room for interactive commands.

The public `GET /` and `GET /health` routes are limited separately, at 30
requests per 15 minutes per IP.

## Restores are capped

`MAINNET_MAX_RESCAN_BLOCKS` (default 10 000) is a real constraint, not a
formality. At ~2 calls per block and the default sync budget, that is about 30
blocks a minute:

| Depth | Roughly |
|---|---|
| 2 000 blocks | 1 hour |
| 10 000 blocks (the cap) | 5.6 hours |
| A full SegWit-era rescan (~420 000) | 230 hours |

`/restore` quotes the ETA and refuses beyond the cap. Recovering a genuinely old
wallet needs a different chain source.

## Privacy

BitRPC's operator sees every address this bot queries and every transaction it
broadcasts. Payjoin breaks the common-input-ownership heuristic for *outside*
observers; it hides nothing from the backend. If that matters for your use, run
your own node — the `ChainSource` abstraction is the seam where a different one
would plug in.

## The mainnet smoke test

Small amounts, done by hand:

```
NETWORK=bitcoin
BITRPC_API_KEY=...
MAINNET_I_UNDERSTAND_RISK=true
MAX_SEND_SATS=20000
```

1. `/status` → `chain=main` and a healthy call budget.
2. `/receive` → send yourself a small amount. **Remember: it will not appear
   until it confirms.**
3. Wait for a block, then `/balance`.
4. `/send` with a manually entered sat/vB, and check the fee floor is enforced
   if you try something absurdly low.

Reference: `https://bitrpc.thebuidl.xyz/docs` (public, no key needed).
