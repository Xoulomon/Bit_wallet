# Plan: Telegram Bitcoin Wallet Bot (on-chain + Payjoin), regtest/mainnet

> Working document — keep untracked (add `PLAN.md` to `.gitignore` when you `git init`).

## Context
Capstone project (rust4BTC). `Capstone/` is empty, so this is a new build. The goal is a **non-custodial, multi-user Bitcoin wallet used through a Telegram bot** instead of a CLI or GUI. It must:
- create and restore wallets from a BIP39 mnemonic, use BIP84 derivation, generate and track addresses, sync, show balances and history, sign PSBTs, broadcast, track confirmations, and persist state (the MVP spec);
- support **Payjoin** (BIP77 v2 async send and receive, plus sending to BIP78 v1 endpoints such as BTCPay);
- switch between **regtest (Polar's bitcoind RPC)** and **mainnet (BitRPC's hosted Bitcoin Core)** with one `.env` setting.

Confirmed decisions: **multi-user**, **seed encrypted with a per-user PIN**, **no self-hosted node — mainnet is BitRPC-only**, and **no Lightning — the scope is on-chain plus Payjoin**, matching the MVP brief.

Deliverables: GitHub repo, README, architecture diagram (`docs/architecture.md` with Mermaid + exported SVG).

---

## 0. Requirements traceability (MVP spec → this plan)
Every bullet of the assignment's MVP list, where it is built and how it is proven. Keep this table green; it is the checklist a reviewer will read first.

| # | MVP requirement | Where | Verified by |
|---|---|---|---|
| 1 | Create a new wallet | `keys.rs` mnemonic gen; `/create` dialogue (§5, §8) | BIP84 vector test (§10); manual `/create` |
| 2 | Restore from an existing mnemonic | `keys.rs`; `/restore` + birthday (§5, §6) | Persistence integration test; manual `/restore` |
| 3 | Hierarchical BIP32 derivation, BIP84 native SegWit | `keys.rs` descriptors `wpkh(.../84'/{0,1}'/0'/{0,1}/*)` (§6) | Descriptor + first-address test against BIP84 vectors (§10) |
| 4 | Generate receive addresses, track which are used | `onchain/wallet.rs` `next_unused_address`; `/receive`, `/addresses` via `spk_index().is_used` (§6) | Address-index persistence test (§10) |
| 5 | Sync with the chain to discover incoming transactions | `onchain/sync.rs` `ChainService` + `bdk_bitcoind_rpc::Emitter` (§6) | `corepc-node` fund-and-sync test (§10) |
| 6 | Show confirmed and unconfirmed balance | `/balance` from `wallet.balance()` — confirmed, trusted/untrusted pending, immature (§6) | Balance test; see the mainnet caveat below |
| 7 | List transaction history | `/history` from `transactions()` by chain position (§6, §8) | Manual + integration test |
| 8 | Sign transactions via PSBT | `TxBuilder` → PSBT → PIN-gated signer, behind `quote_send`/`confirm_send` (§3a, §5, §6) | Send integration test (§10) |
| 9 | Broadcast transactions | `rpc/mod.rs` `sendrawtransaction` (§6) | Send integration test; BitRPC smoke test (§10) |
| 10 | Poll and display status: unconfirmed/confirmed + confirmation count | `ChainPosition` → `notify.rs`; `/tx <txid>` (§6, §8) | Confirmation-tracking test (§10) |
| 11 | Persist wallet state between runs | `bdk_wallet` `PersistedWallet` on `rusqlite`; `storage.rs` (§3, §6) | Reload-from-SQLite test (§10) |

Beyond the MVP, as required by the brief: **Payjoin** (§7), **Telegram instead of CLI/GUI** (§8), **regtest↔mainnet switching with RPC config in `.env`** (§4), and **create + import wallet** (rows 1–2).

Every row above is implemented in `wallet-core` and *exposed* through one facade, `WalletService`; the Telegram bot only calls it. §3a states that contract, §8 specifies the UI built on it.

**One caveat on row 6, stated up front:** on mainnet, unconfirmed *incoming* balance is always zero, because BitRPC does not expose `getrawmempool` (§4b). Outgoing transactions do show as pending. Full confirmed/unconfirmed behaviour is demonstrable on regtest. This is the one place where the hosted backend costs a visible MVP behaviour.

---

## 1. System requirements (checked on this machine)
| Requirement | Why | Status / action |
|---|---|---|
| Rust stable ≥ 1.85 (edition 2024) | bdk_wallet 2.x and payjoin MSRV | ✅ rustc 1.97 installed |
| Docker + Docker Compose | Polar runs its nodes in Docker; the local payjoin directory runs in Docker | ✅ docker at `/usr/local/bin/docker`; ensure your user is in the `docker` group |
| Polar ≥ 3.x (AppImage) | Regtest network: a single bitcoind backend. Polar supports bitcoind-only networks, so no Lightning nodes are needed | Install from lightningpolar.com |
| **BitRPC account + API key** | **Mainnet chain source. No local node is run or synced** | Log in at `https://bitrpc.thebuidl.xyz/login` with the instructor-registered email, enter the 6-digit code (10 min, single use), create a key at `/dashboard`. The raw key is shown **once** → store as `BITRPC_API_KEY` in `.env`, never commit it |
| Build packages: `build-essential pkg-config` | rusqlite `bundled` compiles SQLite in C. Using rustls means no OpenSSL needed | `sudo apt install build-essential pkg-config` |
| Telegram bot token | Created with @BotFather; stored in `.env` | Manual |
| Network access | Mainnet: `api.telegram.org`, `bitrpc.thebuidl.xyz`, Payjoin directory `https://payjo.in` + an OHTTP relay | Outbound HTTPS only |
| Disk and RAM | SQLite state only, a few MB per user — no chain data on either network | Scales with user count, not chain size |

---

## 2. Crates (pin exact versions in `Cargo.lock`; verify the latest compatible set when scaffolding)
- **Bitcoin:** `bitcoin` 0.32, `bdk_wallet` 2.x (features `rusqlite`, `keys-bip39`), `bdk_bitcoind_rpc` (block `Emitter`), `bitcoincore-rpc` (broadcast, chain queries), `bip39` (with `zeroize`).
  - Mainnet reaches Bitcoin Core through a **hand-written `jsonrpc::Transport`** wrapped by `bitcoincore_rpc::Client::from_jsonrpc`. This is required, not a preference: neither `jsonrpc`'s `simple_http` nor its `minreq_http` transport can set an arbitrary header — both only ever emit `Authorization` basic auth — and BitRPC authenticates with `X-API-Key`. Wrapping our transport in `from_jsonrpc` gives us a genuine `RpcApi`, which `bdk_bitcoind_rpc::Emitter` then consumes unchanged.
  - `estimatesmartfee`, `testmempoolaccept` and `generatetoaddress` are **regtest only** — BitRPC does not expose them (§4b).
- **Payjoin:** `payjoin` (features `v2`, `io`; `_danger-local-https` in tests only), `payjoin-test-utils` (dev), payjoin's `Uri` for BIP21 parsing.
- **Telegram:** `teloxide` (feature `macros`), dialogue state machine backed by SQLite storage.
- **Shared:** `tokio`, `serde`/`serde_json`, `tracing`/`tracing-subscriber`, `thiserror` (core), `anyhow` (binaries).
- **`wallet-core` only** — nothing here knows what a chat is: `reqwest` (rustls), `ureq` (rustls — the blocking client inside the JSON-RPC transport; the `Emitter` runs inside `spawn_blocking`, so a blocking client avoids the nested-runtime hazard `reqwest::blocking` has), `rusqlite` (`bundled`), `zeroize`, `argon2`, `chacha20poly1305`, `rand`, `uuid`/`ulid` (opaque `UserId`, `QuoteId`, `SessionId`), `governor` (the shared BitRPC 100 req/min budget), `toml`, `dotenvy`.
- **`bot` only** — nothing here knows what a descriptor is: `teloxide` (`macros`), `qrcode` + `image` (QR photos), `comfy-table` (tables), `governor` (per-user *command* throttling, a separate concern from the RPC budget), `rusqlite` for its own `bot.sqlite` (dialogue state + the `tg_id` ↔ `UserId` map).
- **The dependency direction is a rule, not an accident:** `bot` depends on `wallet-core`; `wallet-core` must not be able to name `teloxide`. §3a states the contract and §10 tests it.
- **Dev:** `corepc-node` (spawns a throwaway bitcoind for tests), `tempfile`, `tokio-test`, `httpmock` (blocking mock server for the BitRPC transport tests).
- **Where the brief offers a choice:** persistence uses **BDK's SQLite (`rusqlite`)** rather than `bdk_file_store` — one file per user, and the bot already needs SQLite for dialogue state and payjoin sessions. RPC uses **`bitcoincore-rpc`** rather than `corepc-client`, because `Client::from_jsonrpc` is what lets us inject the BitRPC transport (above) while still satisfying the `RpcApi` trait `bdk_bitcoind_rpc` requires.
- **On the brief's CLI-output crates.** `owo-colors`/`colored` and `indicatif` assume a terminal; this wallet's front end is Telegram, so they have no target. Their jobs are done by the bot crate's `ui.rs` instead (presentation lives above the boundary, §3a): ANSI colour → Telegram HTML plus the 🧪/🟠 network badges, and progress bars → a single status message edited in place (used for the restore rescan, §6). **`comfy-table` is still used**: `/history` and `/addresses` render through it into a Telegram `<pre>` block, which is the one place monospace tables survive the client.

---

## 3. Repository layout (Cargo workspace)
```
Capstone/
├── Cargo.toml                 # workspace
├── .env.example               # every setting, documented
├── config.example.toml        # optional non-secret tuning (fees, limits, timeouts)
├── docker-compose.payjoin.yml # local payjoin-directory + ohttp-relay for regtest
├── crates/
│   ├── wallet-core/           # library: no Telegram code, uses thiserror
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── config.rs      # Network enum, backend config, loads .env for the active network
│   │       ├── error.rs       # CoreError (thiserror)
│   │       ├── crypto.rs      # Argon2id + XChaCha20-Poly1305 seed vault, Zeroizing types
│   │       ├── keys.rs        # mnemonic gen/validate, BIP84 descriptors (84'/0' main, 84'/1' test)
│   │       ├── rpc/
│   │       │   ├── mod.rs     # ChainSource: build a bitcoincore_rpc::Client for the active network
│   │       │   ├── bitrpc.rs  # jsonrpc::Transport impl: X-API-Key, ureq, shared rate limiter,
│   │       │   │              # 401/403/429/502 -> typed CoreError variants
│   │       │   ├── polar.rs   # regtest: stock Auth::UserPass client
│   │       │   └── fees.rs    # FeePolicy: regtest estimatesmartfee+fallback
│   │       │                  #          | mainnet mempoolminfee floor + manual sat/vB
│   │       ├── onchain/
│   │       │   ├── wallet.rs  # load/create PersistedWallet, addresses, balance, history, build/sign
│   │       │   └── sync.rs    # ChainService: one shared Emitter feeding every loaded wallet
│   │       ├── payjoin/
│   │       │   ├── send.rs    # v2 + v1 sender
│   │       │   ├── receive.rs # v2 receiver state machine
│   │       │   └── persist.rs # SQLite SessionPersister impls
│   │       ├── session.rs     # unlocked-session cache + idle auto-lock (secrets never leave core)
│   │       ├── storage.rs     # app.sqlite: users, vaults, sessions, labels, quotes
│   │       └── service/
│   │           ├── mod.rs     # WalletService: the ONLY entry point a front end may call (§3a)
│   │           ├── types.rs   # BalanceView, TxSummary, SendQuote, FeeOptions … (data, never prose)
│   │           └── events.rs  # CoreEvent + the broadcast channel front ends subscribe to
│   ├── wallet-cli/            # second front end: ~200 lines, proves core is UI-agnostic (§9)
│   │   └── src/main.rs        # create / balance / receive / send / status over the same facade
│   └── bot/                   # binary: uses anyhow; a *client* of wallet-core
│       └── src/
│           ├── main.rs        # load config, init tracing, start ChainService + bot dispatcher
│           ├── commands.rs    # teloxide BotCommands enum
│           ├── dialogue.rs    # State enum: create/restore/PIN/fee/send-confirm flows
│           ├── handlers/{start,wallet,onchain,payjoin,admin}.rs
│           ├── auth.rs        # private-chat guard, admin check, per-user command throttling
│           ├── users.rs       # bot.sqlite: tg_id <-> UserId map; core never sees a Telegram id
│           ├── notify.rs      # subscribes to CoreEvent, routes each to a chat, renders it
│           └── ui.rs          # message formatting, inline keyboards, QR, tables, error prose
├── scripts/
│   ├── polar-env.sh           # prints the .env values to copy from Polar's Connect tab
│   ├── regtest-fund.sh        # mine and fund helpers via bitcoin-cli against Polar
│   └── bitrpc-smoke.sh        # §10 mainnet smoke test, needs only BITRPC_API_KEY
├── tests/                     # integration tests (see §10)
└── docs/
    ├── architecture.md        # Mermaid diagrams + explanation
    ├── architecture.svg
    ├── polar-setup.md
    └── bitrpc-setup.md        # login, key creation, allowlist, rate limit, what it means for us
```
Reference **bitmask-core** for its module split (`bitcoin/`, `constants`, encrypted secret storage keyed by a password hash). We copy the structure, not the code: it is RGB-focused and uses a different BDK version.

---

## 3a. The crate boundary: `wallet-core` is the wallet, `bot` is one of its front ends

The split in §3 is not cosmetic. `wallet-core` must be a wallet library that a CLI, an HTTP daemon or a second bot could drive without changing a line of it; the Telegram crate is a **presentation layer** that turns chat messages into service calls and service data into rendered replies. A reviewer should be able to delete `crates/bot/` and still have a working, testable wallet.

### The six rules
1. **No Telegram types below the boundary.** `wallet-core` never depends on `teloxide`. No `ChatId`, no `Message`, no `InlineKeyboard`, no bot token. Enforced by a test (§10).
2. **No presentation below the boundary.** Core returns *data*: `Amount`, `FeeRate`, `Txid`, typed enums. It never returns a formatted string, HTML, emoji, a network badge, a QR image or a table. `qrcode`, `image` and `comfy-table` are **bot-only** dependencies.
3. **No user identity below the boundary.** Core knows an opaque `UserId(Uuid)`. It has never heard of a Telegram id. The bot owns `bot.sqlite` with `telegram_users(tg_id PRIMARY KEY, user_id, is_admin, created_at)` and translates on every call. This is what makes "a second front end" true rather than aspirational — and it means a user could later be reachable from two front ends at once.
4. **Core never waits for a human.** Anything needing a decision is split into two calls: a pure, side-effect-free *quote* the UI renders, then a *confirm* carrying an id. No callbacks into the UI, no blocking prompts. This is why `/send` is `quote_send` → `confirm_send` rather than one function with a closure.
5. **Core owns every secret.** The mnemonic, the seed, the Argon2 vault, the unlocked-session cache and its idle timer all live in core. The bot's only contact with a secret is forwarding the PIN and chat text the user typed, as `Zeroizing<String>`, straight into a core call, and displaying the one-time mnemonic that `create_wallet`/`export_mnemonic` hand back. **`session.rs` therefore moves from `bot/` into `wallet-core/`** (it was in the wrong crate in the original layout): a front end must not be able to hold a decrypted seed, because a second front end would then need its own copy of that logic and its own chance to leak it.
6. **Core pushes events, it does not send messages.** `notify.rs` in the bot is a *subscriber*, not a channel owner. Core exposes `subscribe() -> broadcast::Receiver<CoreEvent>`; every event carries the `UserId` it concerns, and the bot decides which chat to deliver it to and how it reads.

### `WalletService` — the whole API surface
One facade, `wallet_core::service::WalletService`, held as an `Arc` and shared by every front end. Every method takes the `UserId` it acts for. This *is* the MVP: if a capability is not on this list, the bot cannot offer it.

```rust
impl WalletService {
    pub async fn new(cfg: AppConfig) -> Result<Arc<Self>, CoreError>;
    pub fn network(&self) -> Network;
    pub async fn status(&self) -> Result<BackendStatus, CoreError>;   // tip, latency, call budget
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent>;

    // --- wallet lifecycle (MVP 1, 2, 3) ---
    pub async fn create_wallet(&self, u: UserId, pin: &Pin) -> Result<NewWallet, CoreError>;
    pub async fn restore_preflight(&self, birthday: Option<u32>) -> Result<RestorePlan, CoreError>;
    pub async fn restore_wallet(&self, u: UserId, words: Zeroizing<String>,
                                birthday: Option<u32>, pin: &Pin) -> Result<(), CoreError>;
    pub async fn export_mnemonic(&self, u: UserId, pin: &Pin) -> Result<Zeroizing<Mnemonic>, CoreError>;
    pub async fn delete_wallet(&self, u: UserId, pin: &Pin) -> Result<(), CoreError>;
    pub fn wallet_exists(&self, u: UserId) -> Result<bool, CoreError>;

    // --- session ---
    pub async fn unlock(&self, u: UserId, pin: &Pin) -> Result<SessionInfo, CoreError>;
    pub fn lock(&self, u: UserId);
    pub fn session(&self, u: UserId) -> Option<SessionInfo>;          // remaining idle time

    // --- watch-only, no PIN needed (MVP 4, 6, 7, 10) ---
    pub async fn next_address(&self, u: UserId) -> Result<AddressInfo, CoreError>;
    pub async fn addresses(&self, u: UserId, page: Page) -> Result<Paged<AddressInfo>, CoreError>;
    pub async fn balance(&self, u: UserId) -> Result<BalanceView, CoreError>;
    pub async fn history(&self, u: UserId, page: Page) -> Result<Paged<TxSummary>, CoreError>;
    pub async fn tx(&self, u: UserId, txid: Txid) -> Result<TxDetail, CoreError>;

    // --- spending (MVP 8, 9) ---
    pub fn parse_payment(&self, input: &str) -> Result<PaymentTarget, CoreError>;  // address | BIP21 (+ pj=)
    pub async fn fee_options(&self) -> Result<FeeOptions, CoreError>;
    pub async fn quote_send(&self, u: UserId, req: SendRequest) -> Result<SendQuote, CoreError>;
    pub async fn confirm_send(&self, u: UserId, q: QuoteId, auth: Auth) -> Result<Broadcast, CoreError>;
    pub async fn cancel_quote(&self, u: UserId, q: QuoteId);
    pub async fn bump_fee(&self, u: UserId, txid: Txid, rate: FeeRate) -> Result<SendQuote, CoreError>;

    // --- payjoin (§7) ---
    pub async fn payjoin_receive(&self, u: UserId, amount: Amount) -> Result<PayjoinReceipt, CoreError>;
    pub async fn payjoin_sessions(&self, u: UserId) -> Result<Vec<PayjoinSessionView>, CoreError>;
    pub async fn payjoin_cancel(&self, u: UserId, id: SessionId) -> Result<(), CoreError>;

    // --- regtest only; the front end decides who may call it ---
    pub async fn mine(&self, blocks: u32, to: Option<Address>) -> Result<Vec<BlockHash>, CoreError>;
}
```

Notes on the shapes that carry the design:
- **`Auth`** is `Auth::Session` or `Auth::Pin(Zeroizing<String>)`. The bot passes a PIN when the session is locked and never learns whether a seed was decrypted.
- **`SendQuote`** holds `QuoteId`, recipient, amount, fee, feerate, total, change, `is_payjoin`, and the drafted PSBT **kept inside core** — the PSBT never crosses the boundary. It expires (default 5 min) so an abandoned confirm card cannot be replayed at a stale feerate.
- **`NewWallet`** returns `Zeroizing<Mnemonic>` *and* a `confirm_challenge: [u8; 3]` of word indices, so the "confirm 3 random words" step (§5) is core's rule, not the bot's invention. Verification is `core.confirm_backup(u, answers)`.
- **`RestorePlan`** returns `{ depth, eta: Duration, verdict: Proceed | Warn | Refuse { max } }`. The mainnet depth cap (§6) is a core policy; the bot only renders the verdict.
- **`FeeOptions`** returns `{ presets: [(Label, FeeRate)], floor: FeeRate, source: Node | External { name } | Unavailable, allows_custom: bool }`. Regtest and mainnet differ (§6) but the bot's rendering code is identical — it draws whatever presets it is handed.
- **`CoreError`** is exhaustive and typed (`InsufficientFunds { needed, available }`, `FeeBelowFloor { given, floor }`, `PinLocked { until }`, `RestoreTooDeep { eta }`, `Backend(BackendError)`, …). `ui::render_error` in the bot is a `match` over it, which is what keeps user-facing prose out of core.

### `CoreEvent` — what the front end subscribes to
```rust
pub enum CoreEvent {
    IncomingTx   { user: UserId, txid: Txid, amount: Amount, status: TxStatus },
    TxConfirmed  { user: UserId, txid: Txid, confirmations: u32 },
    SyncProgress { user: UserId, height: u32, tip: u32 },
    SessionExpired { user: UserId },
    Payjoin      { user: UserId, session: SessionId, state: PayjoinState },
    BackendHealth(BackendHealth),   // degraded / recovered / rate-limited
}
```
The bot's `notify.rs` is then a single `while let Ok(ev) = rx.recv().await` loop: map `UserId` → `tg_id`, render, send. A CLI front end would print the same events. Core has no idea either exists.

### What lives where, at a glance
| Concern | `wallet-core` | `bot` |
|---|---|---|
| Descriptors, PSBT build/sign, broadcast | ✅ | — |
| Chain sync, persistence, rate-limit budget | ✅ | — |
| Seed vault, PIN verification, lockout, unlocked session | ✅ | — |
| Payjoin state machines and their persistence | ✅ | — |
| Fee policy and the `mempoolminfee` floor | ✅ | — |
| Restore depth cap, quote expiry, `MAX_SEND_SATS` | ✅ | — |
| Dialogue state, keyboards, pagination, message deletion | — | ✅ |
| QR images, tables, emoji, network badge, HTML | — | ✅ |
| `tg_id` ↔ `UserId`, admin list, private-chat guard | — | ✅ |
| Per-user *command* rate limiting | — | ✅ |
| Error → human sentence | — | ✅ |


## 4. Configuration and network switching
`.env` (with `.env.example` committed):
```
NETWORK=regtest                    # regtest | bitcoin
TELOXIDE_TOKEN=...
DATA_DIR=./data
BOT_ADMIN_IDS=123456789            # can use /mine and /status
ALLOWED_USER_IDS=                  # empty = anyone (multi-user); otherwise an allowlist

# Regtest (Polar backend bitcoind → Connect tab)
REGTEST_RPC_URL=http://127.0.0.1:18443
REGTEST_RPC_USER=polaruser
REGTEST_RPC_PASS=polarpass
REGTEST_PAYJOIN_DIRECTORY=http://localhost:8080
REGTEST_OHTTP_RELAY=http://localhost:3000
REGTEST_FALLBACK_FEE_SAT_VB=2

# Mainnet (BitRPC — no local node required)
BITRPC_URL=https://bitrpc.thebuidl.xyz
BITRPC_API_KEY=                    # from /dashboard; shown once; never commit
BITRPC_RATE_LIMIT_PER_MIN=90       # headroom under the hard 100/min
BITRPC_SYNC_BUDGET_PER_MIN=60      # cap for the block emitter so /send is never starved
MAINNET_MAX_RESCAN_BLOCKS=10000    # /restore refuses birthdays deeper than this
MAINNET_MIN_FEE_SAT_VB=1           # hard floor if getmempoolinfo is unavailable
MAINNET_FEE_API=https://mempool.space/api  # fee estimation; BitRPC has no estimatesmartfee
FEE_CACHE_SECS=60                  # cache window for fee estimates
MAINNET_PAYJOIN_DIRECTORY=https://payjo.in
MAINNET_OHTTP_RELAY=https://pj.bobspacebind.com
MAINNET_I_UNDERSTAND_RISK=false    # must be true or the bot refuses to start on mainnet

SESSION_IDLE_TIMEOUT_SECS=600
MAX_SEND_SATS=                     # optional safety cap
```
- `config.rs` reads `NETWORK` and picks the matching `REGTEST_*` / BitRPC block, returning a typed `AppConfig`; it fails fast if a setting is missing, and specifically if `NETWORK=bitcoin` with an empty `BITRPC_API_KEY`.
- On startup it runs `getblockchaininfo` through the active backend and checks `.chain` against `NETWORK`. On mismatch the bot refuses to start. A 401 or 403 from BitRPC is reported as "check your `BITRPC_API_KEY`" rather than a generic RPC error.
- **The 100 req/min limit is per API key, shared by every bot user.** One `Arc<RateLimiter>` in `rpc/bitrpc.rs` gates every mainnet call: `check()`-and-sleep on the blocking emitter path, `until_ready()` on async paths. The emitter is additionally capped at `BITRPC_SYNC_BUDGET_PER_MIN` so a sync backlog can never starve an interactive `/send`.
- **All data is namespaced by network:** `data/{regtest|bitcoin}/app.sqlite`, `data/{net}/users/{tg_id}/wallet.sqlite`. Regtest and mainnet state can never mix.
- Every bot reply that shows funds carries a network badge (🧪 REGTEST / 🟠 MAINNET).

---

## 4b. BitRPC backend (mainnet)
BitRPC is an authenticated proxy in front of a shared Bitcoin Core, so no node is run or synced locally. Auth is the `X-API-Key` header on every request.

**Bitcoin Core — `POST /bitcoin`, JSON-RPC 1.0.** Allowed methods and what the bot does with them:

| Method | Used for |
|---|---|
| `getblockchaininfo` | Startup network check, `/status` tip height |
| `getblockcount` | Birthday for new wallets, restore-depth arithmetic |
| `getblockhash`, `getblock` | `bdk_bitcoind_rpc::Emitter` block following (~2 calls/block) |
| `getrawtransaction` | `/tx` lookups for txids the wallet has not indexed |
| `decoderawtransaction` | Debug and payjoin proposal inspection |
| `sendrawtransaction` | Broadcast |
| `getmempoolinfo` | `mempoolminfee` → the sat/vB floor for `/send` and for payjoin's broadcast check |
| `getnetworkinfo` | `/status` diagnostics |
| `getwalletinfo`, `getbalance`, `listunspent`, `getnewaddress` | **Never called.** These address the shared node's own wallet, not our BIP84 descriptors, so touching them would be meaningless at best and leak the shared node's state at worst |

**LND.** BitRPC also proxies a shared LND node at `/lnd/*`. **This wallet does not use it** — the scope is on-chain plus Payjoin, and that node's keys are not the user's, so anything routed through it would be custodial.

**Not available, and the consequences** — these drive the design changes in §6, §7 and §7:

| Missing | Consequence |
|---|---|
| `getrawmempool` | `Emitter::mempool()` cannot run. No unconfirmed **incoming** detection on mainnet (§6) |
| `estimatesmartfee` | No fee presets. `/send` prompts for sat/vB, floored at `mempoolminfee` (§6) |
| `testmempoolaccept` | No pre-broadcast dry run; payjoin's `check_broadcast_suitability` is best-effort (§7) |
| `getblockheader`, `getblockfilter` | The BIP158 compact-filter path in `bdk_bitcoind_rpc::bip158` is unusable |
| `generatetoaddress` | `/mine` stays regtest-only (it already was) |

**Errors:** 401 missing key, 403 invalid key *or* method/path not permitted, 429 rate limited, 502 node unavailable. `rpc/bitrpc.rs` maps each to a distinct `CoreError` variant so the bot can give a useful message and retry only where retrying helps (429, 502).

**Rate limits:** 100 req/min per key on authenticated routes; 30 req/15 min per IP on the public `GET /` and `GET /health`. Full reference: `https://bitrpc.thebuidl.xyz/docs` (public, no key needed).

---

## 5. Key management and security (multi-user, PIN-encrypted)
- **Create:** generate a 12-word mnemonic, send it once in a message that self-deletes after 60 s, then have the user confirm 3 random words. The user then sets a 6–8 digit PIN; PIN messages are deleted immediately with `delete_message`.
- **Vault:** `seed_ct = XChaCha20Poly1305(key = Argon2id(PIN, salt, m=64MiB, t=3), nonce, mnemonic_entropy)`. Stored in `users(tg_id, network, salt, nonce, seed_ct, created_at, birthday_height, failed_attempts, locked_until)`. Plaintext always held in `Zeroizing<>`.
- **Watch-only by default:** the BDK wallet is persisted with **public** descriptors (BDK never saves the keymap). Syncing, balances, addresses and history work without the PIN. Signing loads the wallet with `.descriptor(..., Some(priv))` + `.extract_keys()`, or signs with a transient signer built from the decrypted seed.
- **Unlocked session** (`wallet-core/session.rs` — **in core, not the bot**, §3a rule 5): after a correct PIN, keep `Zeroizing<Mnemonic>` in memory until `SESSION_IDLE_TIMEOUT_SECS` elapses or the user calls `lock()`. Needed to sign and to let the payjoin receiver contribute an input. A front end can ask *whether* a session is open (`session()`) but can never hold its contents, so no front end is in a position to leak a seed.
- **PIN brute-force protection:** 5 failures → lockout with exponential backoff, counted **in core** and stored in the DB, so every front end shares one lockout. `unlock` returns `CoreError::PinLocked { until }` and the bot renders it. Per-user *command* throttling is separate and lives in the bot.
- **Chat rules:** private chats only, refuses in groups. The mnemonic is never logged; `tracing` fields are redacted, and `BITRPC_API_KEY` is redacted from every log line and error message. `/export` reshows the mnemonic only after the PIN, in a self-deleting message.
- **Restore:** `/restore` asks for the mnemonic (message deleted immediately), an optional birthday height, then the PIN. Birthday handling and the mainnet depth cap are in §6.

---

## 6. On-chain wallet (MVP)
- **Descriptors:** `wpkh(xprv/84'/{0|1}'/0'/0/*)` receive and `/1/*` change. Coin type 1 on regtest, 0 on mainnet.
- **Addresses:** `/receive` returns `next_unused_address(External)`, persists it, and sends the address with a QR photo and a BIP21 URI. `/addresses` lists revealed addresses with used/unused status via `spk_index().is_used`.
- **Sync (`ChainService`):** one background task runs a `bdk_bitcoind_rpc::Emitter` from the lowest checkpoint among loaded wallets. Each block is applied to every wallet via `apply_block_connected_to`. Every block is fetched once, however many users there are. Wallets still catching up (e.g. a restore from birthday) get a dedicated emitter until they reach the tip, then join the shared one.
  - **Regtest:** blocks every 5 s, plus `emitter.mempool()` applied via `apply_unconfirmed_txs`.
  - **Mainnet:** blocks **only** — `getrawmempool` is not on BitRPC's allowlist, so `emitter.mempool()` cannot run. The poll interval is derived from `BITRPC_SYNC_BUDGET_PER_MIN` rather than being a fixed 30 s. Each new block costs ~2 calls (`getblock` for the header/`nextblockhash`, then the full block); each poll that finds no new block still costs 1. At a 60 s poll interval that is ~60 calls/hour idle plus ~12 calls/hour of actual blocks — comfortably inside the budget.
- **Unconfirmed visibility on mainnet** — the most user-visible consequence of the allowlist:
  - **Outgoing** is fine: after `sendrawtransaction` we insert the tx we just built straight into the wallet with `apply_unconfirmed_txs`, so it shows as pending immediately.
  - **Incoming** unconfirmed payments from third parties are **invisible until they confirm in a block**. `/receive` says so, and the README states it.
- **Confirmations** are read from the wallet's own `ChainPosition` as blocks are applied, not from `getrawtransaction` — that avoids depending on `txindex` being enabled at the shared node.
- **Notifications:** diffs of each wallet's `ChangeSet` / `tx_graph` produce "📥 Incoming X sats" (on regtest, "(unconfirmed)" first), then "✅ confirmed (1 conf)" and "6 confs".
- **Balance:** `/balance` shows confirmed, trusted-pending, untrusted-pending and immature amounts from `wallet.balance()`.
- **History:** `/history [page]` lists txid (shortened, linked to mempool.space on mainnet), net amount, fee, and status/confirmation count, from `transactions()` sorted by chain position.
- **Birthday and restore depth:**
  - New wallets get `birthday = getblockcount()`, so creation is instant on mainnet.
  - `/restore` takes a birthday height and computes `tip - birthday`. Under ~2 000 blocks it proceeds silently; up to `MAINNET_MAX_RESCAN_BLOCKS` it proceeds with an ETA warning; beyond that it **refuses**, quoting the computed ETA and the configured maximum.
  - The arithmetic to quote: ~2 calls/block at `BITRPC_SYNC_BUDGET_PER_MIN=60` ≈ 30 blocks/min, so 10 000 blocks ≈ 5.6 h and a full SegWit-era rescan (~420 000 blocks from 481824) ≈ 230 h — which is why the cap exists.
  - On regtest the birthday defaults to 0 and there is no cap.
- **Send flow (`/send <addr|bip21> <amount|max>`):**
  1. Parse the input. If the BIP21 has a `pj=` parameter, use the payjoin sender (§7).
  2. Fee rate:
     - **Regtest:** `estimatesmartfee` with targets 1/6/144 as inline buttons Fast/Normal/Slow, falling back to `REGTEST_FALLBACK_FEE_SAT_VB` on error.
     - **Mainnet:** BitRPC blocks `estimatesmartfee`, but the MVP requires the wallet to *estimate fees*, so estimation moves to an external source rather than disappearing. `rpc/fees.rs` fetches `GET {MAINNET_FEE_API}/v1/fees/recommended` (mempool.space), caches it for `FEE_CACHE_SECS`, and offers the same **Fast / Normal / Slow** buttons as regtest (`fastestFee` / `halfHourFee` / `hourFee`), plus a fourth **Custom sat/vB** button for manual entry.
       - Every rate, estimated or typed, is floored at `getmempoolinfo.mempoolminfee` (or `MAINNET_MIN_FEE_SAT_VB` if that call fails) and rejected below it.
       - If the fee API is unreachable, the bot degrades to the manual prompt and says why — it never silently guesses.
       - The estimator is a small trait (`FeeEstimator`) so the API is swappable and mockable in tests. Whatever it yields is flattened into one `FeeOptions` value (§3a), so the bot's fee keyboard is the same code on both networks — it draws the presets it is handed and nothing more.
  3. **`quote_send`** builds with `TxBuilder` (default BnB coin selection, RBF enabled, drain for `max`) → PSBT, which stays inside core; the caller gets a `SendQuote` of plain numbers and a `QuoteId` (§3a).
  4. The front end renders the confirmation card from that quote — recipient, amount, fee, feerate, total, change (§8.3). Core does no formatting.
  5. **`confirm_send(quote, auth)`** takes either an open session or the PIN, signs the PSBT, finalizes, then broadcasts:
     - **Regtest:** `testmempoolaccept`, then `sendrawtransaction`.
     - **Mainnet:** straight to `sendrawtransaction` — there is no dry run — and surface its rejection reason verbatim to the user.
  6. Insert the tx as unconfirmed, persist, then emit `CoreEvent::TxConfirmed` from ChainService as blocks land. `/tx <txid>` shows confirmations.
  - Steps 3 and 5 being separate calls is what lets any front end put a human in the loop without core ever blocking on one (§3a rule 4).
- **Stretch:** `/bumpfee <txid>` using `build_fee_bump`. This matters *more* on mainnet than the original plan assumed, since there is no fee estimator to get the first attempt right.

---

## 7. Payjoin (BIP77 v2, plus BIP78 v1 sending)
**Infrastructure** (independent of BitRPC)
- Regtest: `docker-compose.payjoin.yml` runs `payjoin-directory` (with Redis) and `ohttp-relay`. OHTTP keys are fetched at startup with `payjoin::io::fetch_ohttp_keys(relay, directory)`.
- Mainnet: directory `https://payjo.in` plus a public OHTTP relay (configurable).

**Receiver** (`/pj_receive <sats>`, requires an unlocked session):
1. `ReceiverBuilder::new(address, directory, ohttp_keys)` with a 1 h expiry, then `.build()`. Persist the session with the SQLite `SessionPersister`.
2. `payjoin_receive` returns a `PayjoinReceipt { session_id, bip21, expires_at }`. Rendering it as text plus a QR is the front end's job (§8.2) — core returns the URI string, never an image.
3. A background task **inside core** polls `extract_req(relay)` → `process_res` until the Original PSBT arrives, emitting `CoreEvent::Payjoin` at each state change so any front end can follow along, then runs the typestate checks in order:
   - `check_broadcast_suitability` — **this is the step that loses `testmempoolaccept` on mainnet.** Regtest keeps the real dry run. On mainnet, substitute a documented best-effort check: fee rate at or above `getmempoolinfo.mempoolminfee`, no dust outputs, sane weight, and the sender's inputs not owned by us. This is **weaker than a real mempool dry-run** — it cannot detect a non-standard script, a missing parent, or an already-spent input — so the fallback tx in step 4 carries more weight than it otherwise would;
   - `check_inputs_not_owned` (against the wallet's `is_mine`);
   - `check_no_inputs_seen_before` (SQLite table of seen outpoints);
   - `identify_receiver_outputs`;
   - `commit_outputs`;
   - `contribute_inputs` (pick 1 UTXO with payjoin-aware input selection);
   - `commit_inputs`;
   - `apply_fee_range`;
   - `finalize_proposal` (sign with the BDK wallet);
   - post the proposal.
4. Watch for the payjoin txid or the fallback txid and emit `PayjoinState::Completed` or `FellBack` either way. If the session expires, or the sender never finishes and the fallback has been held long enough, broadcast the fallback. `/pj_sessions` renders `payjoin_sessions()`.

**Sender** (via `/send` on a BIP21 with `pj=`):
1. Build and sign the Original PSBT at the chosen feerate (manual sat/vB on mainnet, §6), then `SenderBuilder::new(psbt, uri).build_recommended(feerate)`.
2. v2: post through the OHTTP relay, then poll for the proposal. v1 (BTCPay/BIP78 URL): a synchronous POST.
3. `process_response` validates the proposal (the crate checks the sender's outputs and fee contribution). Then sign our inputs, finalize and broadcast.
4. On timeout or error, broadcast the Original PSBT's tx as fallback and tell the user.

- Sessions are persisted so a bot restart resumes polling (`pj_sessions` table: id, user, role, state blob, expiry).
- Also parse `pjos=0` (output substitution disabled).
- UX follows the bitcoin.design payjoin case study: explain the privacy benefit, show a "Payjoin ✅" or "fell back to a regular tx" badge, no scary errors. All of that is §8 — core supplies only the `PayjoinState`.
- **Privacy note:** on mainnet, BitRPC's operator sees every address we query and every transaction we broadcast. Payjoin still breaks the common-input-ownership heuristic for *outside* observers, but it does not hide anything from the backend. Say so next to the privacy explanation.

---

## 8. The Telegram front end (`bot` crate) — UI specification

The bot is a **thin client over `WalletService`** (§3a). Its job is three translations and nothing else: chat input → a service call, service data → a rendered message, `CoreEvent` → a push notification. Every handler in `handlers/` should read as *parse → call core → render*; any handler that contains bitcoin logic is a bug in the layering.

### 8.1 Interaction principles
- **One intent per command.** No overloaded `/wallet do-thing` verbs.
- **Money never moves without a confirm card.** `quote_send` renders a card; only a button press plus PIN reaches `confirm_send`. Quotes expire in 5 minutes and the card is edited to "expired" rather than silently failing.
- **Every message showing funds carries the network badge** (🧪 REGTEST / 🟠 MAINNET) from `service.network()`. On mainnet the badge is in the *first* line, so a mistaken network is visible before an amount is read.
- **Secrets are ephemeral on screen.** Mnemonics self-delete after 60 s; PIN messages are deleted on receipt with `delete_message`; neither is ever edited into a card that stays in history.
- **Destructive actions need a typed word**, not just a button: `/delete` requires the user to type `DELETE`, then the PIN.
- **Edit, don't spam.** Long operations (restore rescan, payjoin waiting) own one status message that is edited in place — this is the plan's replacement for `indicatif` (§2).
- **Errors are sentences with a next step.** `ui::render_error` maps each `CoreError` variant to "what happened + what to do"; raw RPC strings appear only for a broadcast rejection, where the node's reason is the useful part.

### 8.2 Command surface
Every row is a direct call into the §3a API. Commands are registered with `BotCommands` so Telegram shows the native menu.

| Command | Core call(s) | Rendered as |
|---|---|---|
| `/start` | `wallet_exists` | Welcome + network badge; buttons **Create wallet** / **Restore wallet**, or the main menu if one exists |
| `/help` | — | Grouped command list |
| `/create` | `create_wallet` → `confirm_backup` | Mnemonic (self-deleting) → 3-word quiz → PIN → PIN confirm → done card |
| `/restore` | `restore_preflight` → `restore_wallet` | Mnemonic prompt → birthday → verdict card → PIN → live rescan progress |
| `/unlock` | `unlock` | PIN prompt → "🔓 Unlocked for 10 min" |
| `/lock` | `lock` | "🔒 Locked" |
| `/export` | `export_mnemonic` | PIN prompt → mnemonic, self-deleting after 60 s |
| `/delete` | `delete_wallet` | Typed `DELETE` → PIN → confirmation |
| `/receive` | `next_address` | QR photo + caption: address, BIP21 URI, "unused address #n", mainnet mempool caveat |
| `/addresses [page]` | `addresses` | `comfy-table` in `<pre>`: index, address, used/unused, received; ◀ ▶ pagination |
| `/balance` | `balance` | Confirmed / pending / immature in sats **and** BTC; button **Refresh** |
| `/history [page]` | `history` | Table: date, direction, amount, fee, confs; txid links to mempool.space on mainnet |
| `/tx <txid>` | `tx` | Detail card: status, confirmations, amount, fee, feerate, inputs/outputs count |
| `/send <addr\|bip21> [amount]` | `parse_payment` → `fee_options` → `quote_send` → `confirm_send` | The send flow (8.3) |
| `/bumpfee <txid>` | `bump_fee` → `confirm_send` | Same confirm card, labelled "Fee bump" |
| `/pj_receive <sats>` | `payjoin_receive` | QR + BIP21 with `pj=`, a short "what payjoin does" line, live status message |
| `/pj_sessions` | `payjoin_sessions` | List with state badges; ✖ Cancel button per row |
| `/status` | `status` | Network, tip height, backend latency, calls used / budget, session state |
| `/network` | `network` | Which chain this instance is bound to, and that state is namespaced |
| `/mine <n>` | `mine` | Admin + regtest only; blocks mined, then the resulting balance |

### 8.3 The send flow, screen by screen
```
 user: /send bc1q…k7 50000
        │
        ├─ parse_payment  ── invalid ──▶ "That isn't a valid address or BIP21 URI for 🟠 MAINNET."
        │                   has pj=  ──▶ card shows "Payjoin ✅ will be attempted"
        ├─ fee_options
        ▼
┌──────────────────────────────────────┐
│ 🟠 MAINNET · Choose a fee            │
│ Sending 50,000 sats to bc1q…k7       │
│ Rates from mempool.space · floor 1.0 │
├──────────────────────────────────────┤
│ [ Fast 12 ] [ Normal 6 ] [ Slow 2 ]  │
│ [ Custom sat/vB ]      [ Cancel ]    │
└──────────────────────────────────────┘
        │ callback  send:fee:<quote_seed>:<rate>
        ▼ quote_send
┌──────────────────────────────────────┐
│ 🟠 Confirm payment                   │
│ To      bc1q…k7                      │
│ Amount  50,000 sats  (0.00050000 BTC)│
│ Fee     1,410 sats @ 6.0 sat/vB      │
│ Total   51,410 sats                  │
│ Change  212,590 sats                 │
│ Payjoin attempt: yes                 │
│ Expires in 5:00                      │
├──────────────────────────────────────┤
│ [ ✅ Confirm & sign ]   [ ✖ Cancel ] │
└──────────────────────────────────────┘
        │ session unlocked? ── no ──▶ "Enter your PIN" (message deleted on receipt)
        ▼ confirm_send(quote, Auth::…)
   "📡 Broadcast — a1b2…9f · tracking confirmations"
        │
        ▼ CoreEvent::TxConfirmed
   "✅ a1b2…9f confirmed (1 conf)"   … later …  "✅ 6 confs"
```
The same card serves `/bumpfee` and the payjoin sender — the only difference is the header and the `is_payjoin` line, because all three come back as a `SendQuote`.

### 8.4 Dialogue states (`dialogue.rs`)
Pure UI state, stored in `bot.sqlite` so flows survive a restart. No state variant holds a seed, a PSBT or a key — only ids and rendering context.
```
Start
 ├ CreateShowMnemonic { challenge }
 ├ CreateConfirmWords { challenge, answered }
 ├ SetPin { intent: Create | Restore }      → ConfirmPin { intent, first_hash }
 ├ RestoreMnemonic → RestoreBirthday → RestoreConfirmDepth { plan }
 ├ AwaitFeeChoice { target, amount }        → AwaitCustomFee { target, amount }
 ├ SendConfirm { quote: QuoteId, card_msg }
 ├ AwaitPin { pending: PendingAction }      // Send(QuoteId) | Export | Delete | Unlock
 └ DeleteTypeConfirm
```
`AwaitPin` is the single place a PIN is collected, for every action that needs one — so the delete-on-receipt, the lockout message and the retry counter are written once.

### 8.5 Callback data
`action:subject:arg` inside Telegram's 64-byte limit, e.g. `send:confirm:<QuoteId>`, `send:fee:custom`, `hist:page:3`, `pj:cancel:<SessionId>`, `bal:refresh`. Ids are opaque ULIDs minted by core; the bot never puts an amount, an address or a feerate in callback data, so a replayed button cannot alter a payment — it can only reference a quote that core will re-validate or reject as expired.

### 8.6 Push notifications (`notify.rs`)
One task, one `broadcast::Receiver<CoreEvent>`, one `match`:

| Event | Message |
|---|---|
| `IncomingTx` (unconfirmed, regtest) | 📥 Incoming 25,000 sats — unconfirmed |
| `IncomingTx` (confirmed) | 📥 Received 25,000 sats — ✅ 1 conf |
| `TxConfirmed` | ✅ a1b2…9f — 6 confs (edits the earlier message where possible) |
| `SyncProgress` | edits the single restore-progress message: `Rescanning 812,400 / 823,109 · ~18 min left` |
| `SessionExpired` | 🔒 Session locked after 10 minutes of inactivity |
| `Payjoin{Proposal}` | 🤝 Payjoin proposal received — verifying |
| `Payjoin{Completed}` | Payjoin ✅ — a1b2…9f |
| `Payjoin{FellBack}` | Sent as a regular transaction (payjoin timed out) — a1b2…9f |
| `BackendHealth(Degraded)` | ⚠️ Backend is slow or rate-limited; commands may lag |

### 8.7 Guards (`auth.rs`)
Private chats only — refuse in groups and supergroups before any handler runs. `ALLOWED_USER_IDS` allowlist when set. Per-user command rate limiting with `governor` (this is UI throttling; the BitRPC budget is core's, §4). Admin-only commands checked against `BOT_ADMIN_IDS`, and `/mine` additionally refused unless `service.network() == Regtest`. Unknown `tg_id`s are minted a fresh `UserId` on first contact and recorded in `telegram_users`.

---

## 9. Implementation milestones
1. **Scaffold and the boundary:** workspace, config and network switching, tracing, `.env.example`, RPC health check against Polar. **Write `service/mod.rs`, `types.rs` and `events.rs` first — signatures and `todo!()` bodies** — so the bot is compiled against the finished facade from day one and can never grow a shortcut into BDK. Bot answers `/start` and `/status`.
2. **BitRPC transport:** the `jsonrpc::Transport` impl with `X-API-Key`, the shared rate limiter, typed 401/403/429/502 errors, and `Client::from_jsonrpc` wiring. Prove it with `getblockchaininfo` against mainnet and `getblockcount` against Polar through the same `ChainSource` interface.
3. **Keys and vault:** mnemonic create/restore, Argon2 + AEAD vault, `storage.rs` schema and migrations, create/restore/PIN dialogues, message auto-deletion, restore-depth cap.
4. **On-chain core:** BDK wallet persistence, addresses, `ChainService` sync (mempool on regtest, blocks-only on mainnet), balance/history as `BalanceView`/`TxSummary` DTOs, and `CoreEvent` emission on the broadcast channel. The bot's `/balance`, `/receive`, `/addresses`, `/history` and `notify.rs` are pure rendering on top of this.
5. **Send:** fee policy per network (presets on regtest, manual sat/vB + `mempoolminfee` floor on mainnet), PSBT build, confirm card, PIN sign, broadcast, unconfirmed insertion, confirmation tracking, `/mine` for regtest.
6. **Payjoin:** local directory compose, receiver state machine, sender (v2 + v1), persistence and resume, fallback handling, the mainnet best-effort broadcast check.
7. **Hardening:** per-user rate limiting, PIN lockouts, `MAX_SEND_SATS`, mainnet guardrails, **BitRPC 429 backoff and 502 retry**, API-key and mnemonic log redaction, graceful shutdown (flush persisters).
8. **Second front end (`wallet-cli`):** ~200 lines over the same `WalletService` — `create`, `receive`, `balance`, `send`, `status` — printing the same `CoreEvent`s to stdout. This is cheap, and it is the only honest proof that §3a holds: anything the CLI cannot do without reaching past the facade is a leak in the boundary, and it gives integration tests a headless driver that needs no Telegram token.
9. **Docs:** README (features, requirements, Polar setup, BitRPC setup, `.env`, command reference, security model and limitations), `docs/bitrpc-setup.md`, `docs/architecture.md` with Mermaid — a **layer diagram** (`bot` / `wallet-cli` → `WalletService` → keys · onchain · payjoin · rpc → Polar | BitRPC) plus sequence diagrams for send and payjoin receive — exported SVG, demo GIF or screenshots.

---

## 10. Verification
- **Unit tests** (`cargo test -p wallet-core`):
  - vault encrypt/decrypt round trip; a wrong PIN fails;
  - descriptor and first-address derivation match the BIP84 test vectors;
  - config chooses the right backend per `NETWORK`, rejects a network mismatch, and rejects `NETWORK=bitcoin` with an empty API key;
  - **BitRPC transport:** the `X-API-Key` header is set on every request; 401/403/429/502 map to the right `CoreError` variants; the rate limiter blocks once the quota is spent (against a local mock HTTP server);
  - **fee policy:** regtest falls back to `REGTEST_FALLBACK_FEE_SAT_VB` when `estimatesmartfee` errors; mainnet maps a recorded mempool.space payload onto Fast/Normal/Slow, degrades to the manual prompt when that API errors, and rejects any rate — estimated or typed — below the `mempoolminfee` floor, or below `MAINNET_MIN_FEE_SAT_VB` when that call fails;
  - restore-depth cap arithmetic and its refusal message;
  - BIP21 and `pj=` parsing;
  - **quote lifecycle:** a `SendQuote` past its expiry is rejected by `confirm_send`, and a `QuoteId` belonging to another `UserId` is rejected.
- **Boundary tests** (they enforce §3a, and they are cheap):
  - `cargo tree -p wallet-core` must not contain `teloxide`, `qrcode`, `image` or `comfy-table` — a `#[test]` that shells out and asserts this fails the build if anyone adds the dependency;
  - `cargo build -p wallet-core` and `cargo test -p wallet-core` must pass with `crates/bot/` absent from the workspace;
  - every `WalletService` method is exercised by `wallet-cli` or by an integration test, with no test reaching into `onchain::`, `keys::` or `payjoin::` directly — if a test needs a private module, the facade is missing a method;
  - the bot crate contains no `bdk_wallet`, `bitcoin::Psbt` or `Mnemonic` import (grep test).
- **Integration tests** (`tests/`, using `corepc-node` to spawn bitcoind regtest, so neither Polar nor BitRPC is needed in CI):
  - create a wallet, fund it with `generatetoaddress`, sync and check the balance;
  - send between two wallets and check that confirmations are tracked;
  - persistence: reload the wallet from SQLite and get the same balance and address index;
  - **payjoin end to end** with `payjoin-test-utils` (in-process directory and relay): receiver and sender wallets complete a v2 payjoin, the final tx has inputs from both, and the fallback path works on timeout;
- **BitRPC smoke test** (`scripts/bitrpc-smoke.sh`, needs only `BITRPC_API_KEY`) — run this **before** writing code against the backend, to confirm the allowlist still matches what this plan assumes:
  - `getblockchaininfo` returns `chain: "main"`;
  - `getblockcount`, then `getblockhash`/`getblock` round-trip one block;
  - a deliberately disallowed method (`estimatesmartfee`) returns **403**, confirming the allowlist is enforced;
  - a burst past 100 req/min returns **429**, and the in-app limiter prevents ever reaching it.
- **Manual end to end with Polar (regtest):**
  1. Start a Polar network with a single bitcoind backend and copy the RPC settings into `.env`.
  2. Run `docker compose -f docker-compose.payjoin.yml up`.
  3. Run `cargo run -p bot`.
  4. In Telegram, from two different accounts:
     - `/create` and `/restore`;
     - `/receive` then `/mine 101` (or mine in Polar);
     - `/balance` and `/send` between users;
     - `/pj_receive` on one user, pay the URI from the other;
  5. Repeat `create` → `receive` → `balance` → `send` through `wallet-cli` against the same regtest backend, to show the two front ends are interchangeable over one core.
- **Mainnet smoke test** (optional, tiny amounts): there is no local node to point at — set `NETWORK=bitcoin`, `BITRPC_API_KEY`, `MAINNET_I_UNDERSTAND_RISK=true` and `MAX_SEND_SATS=20000`. Check `/status` shows `chain=main` and a healthy BitRPC call budget, then `/receive`, wait for a confirmation (remember: unconfirmed incoming is invisible here), and do a small `/send` with a manually entered sat/vB.
- **CI:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` in GitHub Actions (the bitcoind binary comes from `corepc-node`'s download feature). No BitRPC key in CI.

---

## Known limitations (to state in the README)
- The bot server sees the seed while a session is unlocked, so this is "PIN-protected self-hosted", not hardware-grade non-custodial. Each user should run their own bot instance for maximum trust.
- **Mainnet unconfirmed incoming payments are not detected.** `getrawmempool` is not on BitRPC's allowlist, so incoming funds appear only once they are mined. Outgoing transactions do show as pending.
- **Mainnet fee estimation is third-party.** BitRPC does not expose `estimatesmartfee`, so Fast/Normal/Slow come from mempool.space rather than from a node we trust, and a manual sat/vB entry is always available as a fallback. If that API is unreachable the bot asks for the rate instead of guessing. Every rate is floored at the node's own `mempoolminfee`; `/bumpfee` is the remedy for a rate that turns out too low.
- **No `testmempoolaccept` on mainnet:** broadcasts go out without a dry run, and payjoin's broadcast-suitability check is best-effort rather than authoritative.
- **Mainnet restores are capped** at `MAINNET_MAX_RESCAN_BLOCKS` (~10 000 blocks, ~10 weeks). Recovering a genuinely old wallet needs a different chain source; a full SegWit-era rescan through BitRPC would take roughly 230 hours.
- **All bot users share one 100 req/min BitRPC budget**, so sync pace and command latency degrade as users are added.
- **BitRPC is a single point of failure and a privacy trade-off:** its operator can see every address the bot queries and every transaction it broadcasts. Payjoin protects against outside chain analysis, not against the backend.

---

## References
Sources this plan is built on; keep the list in the README too.

**Wallet architecture**
- bitmask-core — module split and encrypted secret storage: <https://github.com/bitmask-stack/bitmask-core> (§3)

**Payjoin**
- Receive Payjoin v2 (BIP77 receiver state machine): <https://payjoin.org/docs/tutorials/receive-payjoin-v2> (§7)
- PDK — send and receive test payjoins: <https://payjoindevkit.org/send-receive-test-payjoins.html> (§10)
- PDK — send a payjoin: <https://payjoindevkit.org/tutorials/send-payjoin-with-pdk.html> (§7)
- Payjoin UX case study: <https://bitcoin.design/guide/case-studies/payjoin/> (§7)
- BTCPay Server payjoin guide — the BIP78 v1 endpoint we send to: <https://docs.btcpayserver.org/Payjoin/> (§7)

**Backend**
- BitRPC quickstart and API reference: <https://bitrpc.thebuidl.xyz/docs> (§4b)
- Polar — runs the local regtest bitcoind: <https://lightningpolar.com> (§1)
