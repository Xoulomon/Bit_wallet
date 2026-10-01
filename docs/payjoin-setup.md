# Payjoin setup

Short version: **leave the defaults alone.** `REGTEST_PAYJOIN_DIRECTORY` and
`REGTEST_OHTTP_RELAY` point at public infrastructure, and that is the
configuration that works.

```
REGTEST_PAYJOIN_DIRECTORY=https://payjo.in
REGTEST_OHTTP_RELAY=https://pj.benalleng.com
```

Then `/pj_receive 50000` in the bot, or:

```bash
cargo run -p wallet-cli -- pj-receive 50000
```

Nothing about the chain is involved. The directory is a store-and-forward
mailbox for PSBTs; it neither knows nor cares that the addresses are regtest,
and a regtest session through it moves no real funds.

## Why not a local directory?

`PLAN.md` §7 assumed a local `payjoin-directory` plus `ohttp-relay` behind
Redis, which is how the project shipped them at the time. That is no longer
possible, for a reason worth understanding rather than working around.

**The two crates were combined and renamed.** `payjoin-directory` and
`ohttp-relay` are now one binary, `payjoin-mailroom`, which is both halves and
needs no Redis. The published `payjoin-directory` 0.0.3 is stale — it pairs
with `payjoin` 0.24, and this project uses 1.1.

**OHTTP requires the two halves to be different operators.** Run one mailroom
and point both URLs at it, and it refuses:

```
Rejected OHTTP request from same-instance relay
Forbidden: Relay and gateway must be operated by different entities
```

That is not an inconvenience, it is the point. In OHTTP the relay sees your IP
but not your request, and the gateway sees your request but not your IP. One
operator who is both sees everything, and the privacy guarantee is gone. The
mailroom detects this with a per-instance sentinel header and declines.

**And two local instances do not fix it.** Two instances have different
sentinel tags, so they get past that check — but the relay half of
`payjoin-mailroom` 0.1.2 has no configuration for its gateway. Its `Config`
struct has `listener`, `storage_dir`, `timeout`, `mailbox_ttl`, `v1`,
`telemetry`, `acme` and `access_control`, and no gateway origin; the relay is
hard-wired to `DEFAULT_GATEWAY`, which is `https://payjo.in`. Ask it to proxy a
key fetch for a local directory and it answers with *its own* key
configuration, so the client encrypts to the wrong key and the directory
reports:

```
Bad request: Key configuration rejected: a problem occurred with HPKE: Failed to open ciphertext
```

The relay also assumes HTTPS for `CONNECT` targets, so a plaintext local
gateway could not be tunnelled to even if the origin were configurable.

So a fully local pair cannot complete a session with this release. When
upstream makes the relay's gateway configurable, `scripts/payjoin-regtest.sh`
already starts a correctly separated pair and will need only that one setting.

## Public relays

The directory `https://payjo.in` is the default gateway in the payjoin
codebase itself. Relays are operated separately; these three were verified to
proxy the directory's key configuration correctly:

| Relay | |
|---|---|
| `https://pj.benalleng.com` | the default here |
| `https://pj.bobspacebkk.com` | |
| `https://payjoin.achow101.com` | |

Note the spelling of the second one. `PLAN.md` and earlier drafts of
`.env.example` had `pj.bobspacebind.com`, which does not resolve — a stale name
that would have failed every payjoin attempt with a key-fetch error.

To check a relay yourself, fetch the directory's keys through it and compare
with fetching them directly. The hashes must match; if the relay answers with
its own keys instead, HPKE will fail later with no obvious cause:

```bash
curl -s -H 'Accept: application/ohttp-keys' \
  https://payjo.in/.well-known/ohttp-gateway | sha256sum

curl -s -H 'Accept: application/ohttp-keys' \
  --proxy https://pj.benalleng.com \
  https://payjo.in/.well-known/ohttp-gateway | sha256sum
```

## Proving it works

The round trip has a test. It is `#[ignore]`d because it reaches the public
directory, so it is opt-in rather than part of `cargo test`:

```bash
cargo test -p wallet-core --test payjoin_e2e -- --ignored --nocapture
```

It runs both sides in one process against a real directory and relay: the
receiver opens a session and polls, the sender posts the Original PSBT, the
receiver walks the §7 checks and contributes an input, the sender signs the
proposal and broadcasts it. It then asserts the transaction has inputs from
both wallets — which is the whole privacy claim — and that the balances moved
by the right amounts. A second test covers the case §7 cares about just as
much: with nobody listening, the sender falls back to an ordinary payment and
reports it as a fallback rather than a failure.

## Privacy, stated plainly

Payjoin breaks the common-input-ownership heuristic for anyone analysing the
chain afterwards. It does not hide the payment from:

- **the directory operator**, who sees the session and the PSBTs passing
  through it — the relay is what keeps your IP from them, which is why the two
  must be separate parties;
- **your chain source**, which on mainnet is BitRPC, and which sees every
  address this bot queries and every transaction it broadcasts.

On regtest none of this matters. On mainnet, decide whether it does before
relying on it.

## Running a local pair anyway

`scripts/payjoin-regtest.sh` starts two separated mailroom instances, and
`docker-compose.payjoin.yml` is the Docker equivalent for a native Linux
engine. Both work as far as the protocol lets them: each serves its own OHTTP
keys and the same-instance check passes. Neither can complete a session until
the relay's gateway is configurable, so they are kept for that day and for
inspecting the directory side.

```bash
cargo install payjoin-mailroom --version 0.1.2 --locked
./scripts/payjoin-regtest.sh up
./scripts/payjoin-regtest.sh status
./scripts/payjoin-regtest.sh down
```

A note on Docker: `docker-compose.payjoin.yml` uses host networking so that
`localhost:8080` means the same thing to the bot and to the relay. Under Docker
Desktop — including on Linux — containers run in a VM, so host networking binds
the VM's loopback and nothing appears on yours. Use the script there.
