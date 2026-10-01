# Polar setup (regtest)

Polar gives you a throwaway Bitcoin network in Docker. This project needs
exactly one node from it — a bitcoind backend. No Lightning nodes are involved:
the scope is on-chain plus payjoin.

## 1. Install and start a network

1. Download the AppImage from [lightningpolar.com](https://lightningpolar.com),
   make it executable, run it.
2. **Create Network** → name it → drag **one Bitcoin Core** node onto the
   canvas. You can remove any Lightning nodes Polar adds by default.
3. **Start** the network and wait for the node to go green.

Your user must be in the `docker` group, or Polar cannot talk to the daemon:

```bash
sudo usermod -aG docker "$USER"   # then log out and back in
```

## 2. Copy the RPC settings

Click the bitcoind node → **Connect** tab. You want the RPC host, port, user and
password. Or let the script read them out of the container:

```bash
./scripts/polar-env.sh
```

It prints the four lines to paste into `.env`:

```
NETWORK=regtest
REGTEST_RPC_URL=http://127.0.0.1:18443
REGTEST_RPC_USER=polaruser
REGTEST_RPC_PASS=polarpass
```

Check the bot can reach it — this is the same call the bot makes at startup,
and it refuses to start if the chain does not match `NETWORK`:

```bash
./scripts/regtest-fund.sh info
# chain regtest  blocks 0
```

## 3. Mine some coins

A fresh regtest chain has no spendable coins: coinbase outputs need 100
confirmations to mature.

```bash
./scripts/regtest-fund.sh mine 101
```

Or, once the bot is running and your Telegram id is in `BOT_ADMIN_IDS`, use
`/mine 101` in the chat.

## 4. Run the bot

```bash
cargo run -p bot
```

Then, in a private chat with your bot:

```
/start
/create          → PIN, seed phrase, three-word check
/receive         → an address and a QR
```

Fund that address:

```bash
./scripts/regtest-fund.sh pay bcrt1q... 500000
```

`/balance` should show it. Try `/send`, `/history`, `/tx <txid>`.

## 5. Payjoin on regtest

Payjoin needs no local setup: the defaults in `.env.example` point at the
public directory and a public relay, which is the only configuration that
works today. [`docs/payjoin-setup.md`](payjoin-setup.md) explains why, and it is
worth reading before changing those two settings.

Then `/pj_receive 50000` on one account and pay the URI from another. Two
Telegram accounts is the easiest way to see both sides; `wallet-cli` with a
different `--user` works too.

## 6. Two front ends, one wallet

This is worth doing once, because it is the clearest demonstration that the
boundary is real:

```bash
export WALLET_CLI_USER=$(uuidgen)
cargo run -p wallet-cli -- create
cargo run -p wallet-cli -- receive
./scripts/regtest-fund.sh pay <that address> 200000
cargo run -p wallet-cli -- balance
cargo run -p wallet-cli -- send <address> 50000 2
```

Same core, same database layout, same events — no Telegram token anywhere.

## Troubleshooting

**"connecting to the Bitcoin backend" at startup.** Polar is not running, or
the port moved. Re-run `./scripts/polar-env.sh`; Polar assigns a new port when
you recreate a network.

**"network mismatch".** `.env` says one chain and the node is serving another.
This is a refusal, not a bug: mixing regtest and mainnet state is the one
mistake that cannot be undone.

**Nothing confirms.** Regtest only makes blocks when you ask.
`./scripts/regtest-fund.sh mine 1`, or `/mine 1`.

**A restart lost my Polar chain.** Polar's networks are disposable by design. So
is the wallet state that tracked it — delete `data/regtest/` and start over.
