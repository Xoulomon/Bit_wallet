//! `wallet-cli` — the second front end (PLAN.md §9 Step 8, §3a).
//!
//! This is not a demo and it is not optional. It is the only honest proof that
//! §3a holds: anything this binary cannot do without reaching past
//! `WalletService` is a leak in the boundary. It is also the headless driver
//! for integration tests, which need no Telegram token.
//!
//! Note what is absent. There is no `bdk_wallet` import, no PSBT, no
//! descriptor, no mnemonic type — the same absences §10's grep test enforces in
//! the bot. The two front ends differ in how they *render*; what they can do is
//! identical, because it is the same facade.
//!
//!     wallet-cli status
//!     wallet-cli create
//!     wallet-cli receive
//!     wallet-cli balance
//!     wallet-cli history
//!     wallet-cli send <address|bip21> <sats|max> <sat/vB>
//!     wallet-cli unlock | lock
//!     wallet-cli events            # print CoreEvents as they arrive
//!
//! The user is chosen with `--user <uuid>`, or `WALLET_CLI_USER`, or a fresh
//! one is minted and printed — the CLI owns its identity map exactly as the bot
//! owns `telegram_users`.

use anyhow::{Context, Result, bail};
use std::{str::FromStr, sync::Arc};
use wallet_core::{
    AppConfig, WalletService,
    bitcoin::{Amount, FeeRate, Network},
    events::CoreEvent,
    types::{Auth, Page, Pin, SendAmount, SendRequest, TxDirection, TxStatus, UserId},
};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let user = take_user(&mut args)?;

    let Some(command) = args.first().cloned() else {
        usage();
        return Ok(());
    };
    let rest = &args[1..];

    let cfg = AppConfig::from_env().context("loading configuration")?;
    let network = cfg.network.network();
    let core = WalletService::new(cfg)
        .await
        .context("connecting to the Bitcoin backend")?;

    let result = run(&core, user, network, &command, rest).await;

    core.shutdown().await;
    result
}

async fn run(
    core: &Arc<WalletService>,
    user: UserId,
    network: Network,
    command: &str,
    args: &[String],
) -> Result<()> {
    match command {
        "status" => {
            let s = core.status().await?;
            println!("network  {}", chain(network));
            println!("tip      {}", s.tip_height);
            println!("latency  {} ms", s.latency.as_millis());
            if let (Some(used), Some(budget)) = (s.calls_used, s.call_budget) {
                println!("calls    {used} / {budget} per minute");
            }
            println!(
                "session  {}",
                match core.session(user) {
                    Some(info) => format!("unlocked, {}s left", info.remaining.as_secs()),
                    None => "locked".into(),
                }
            );
        }

        "create" => {
            let pin = read_pin("Choose a PIN (6-8 digits): ")?;
            let wallet = core.create_wallet(user, &pin).await?;

            println!("\nWrite these words down, in order:\n");
            println!("  {}\n", *wallet.mnemonic);
            println!("Anyone with them has your coins. They are shown once.\n");

            // §5: core issued the challenge, so core checks the answers.
            let mut answers = Vec::new();
            for index in wallet.confirm_challenge {
                answers.push(prompt(&format!("Word number {}: ", index as usize + 1))?);
            }
            core.confirm_backup(
                user,
                [answers[0].clone(), answers[1].clone(), answers[2].clone()],
            )
            .await?;

            println!("Wallet ready. Birthday block {}.", wallet.birthday);
        }

        "restore" => {
            let words = prompt("Seed phrase: ")?;
            let birthday = prompt("Birthday height (blank to scan from the start): ")?;
            let birthday = birthday.trim().parse::<u32>().ok();

            let plan = core.restore_preflight(birthday).await?;
            println!(
                "Scanning {} blocks, about {} minutes.",
                plan.depth,
                plan.eta.as_secs() / 60
            );

            let pin = read_pin("PIN: ")?;
            core.restore_wallet(user, zeroize::Zeroizing::new(words), birthday, &pin)
                .await?;
            println!("Restored.");
        }

        "unlock" => {
            let pin = read_pin("PIN: ")?;
            let info = core.unlock(user, &pin).await?;
            println!("Unlocked for {}s.", info.remaining.as_secs());
        }

        "lock" => {
            core.lock(user);
            println!("Locked.");
        }

        "receive" => {
            let info = core.next_address(user).await?;
            println!("{}", info.address);
            println!("{}", info.bip21);
            if network == Network::Bitcoin {
                println!("\nNote: a payment here is invisible until it confirms in a block.");
            }
        }

        "balance" => {
            core.sync_now(user).await.ok();
            let b = core.balance(user).await?;
            println!("confirmed  {} sats", b.confirmed.to_sat());
            println!("pending    {} sats", b.trusted_pending.to_sat());
            println!("incoming   {} sats", b.untrusted_pending.to_sat());
            println!("total      {} sats", b.total.to_sat());
            if !b.unconfirmed_incoming_visible {
                println!("\nNote: unconfirmed incoming payments are not visible on this backend.");
            }
        }

        "addresses" => {
            let page = core.addresses(user, Page::new(page_arg(args))).await?;
            for a in &page.items {
                println!(
                    "{:>4}  {}  {}  {} sats",
                    a.index,
                    a.address,
                    if a.used { "used  " } else { "unused" },
                    a.received.to_sat()
                );
            }
            println!("page {} of {}", page.page.index + 1, page.total_pages());
        }

        "history" => {
            core.sync_now(user).await.ok();
            let page = core.history(user, Page::new(page_arg(args))).await?;
            for tx in &page.items {
                println!(
                    "{}  {:>12} sats  {}  {}",
                    match tx.direction {
                        TxDirection::Incoming => "in ",
                        TxDirection::Outgoing => "out",
                        TxDirection::Internal => "self",
                    },
                    tx.amount.to_sat(),
                    match tx.status {
                        TxStatus::Unconfirmed => "pending".to_string(),
                        TxStatus::Confirmed { confirmations, .. } =>
                            format!("{confirmations} confs"),
                    },
                    tx.txid
                );
            }
            println!("page {} of {}", page.page.index + 1, page.total_pages());
        }

        "fees" => {
            let f = core.fee_options().await?;
            for (label, rate) in &f.presets {
                println!("{label:?}  {} sat/vB", rate.to_sat_per_vb_ceil());
            }
            println!("floor  {} sat/vB", f.floor.to_sat_per_vb_ceil());
            println!("source {:?}", f.source);
        }

        "send" => {
            let [target, amount, rate] = match args {
                [t, a, r] => [t.clone(), a.clone(), r.clone()],
                _ => bail!("usage: wallet-cli send <address|bip21> <sats|max> <sat/vB>"),
            };

            let parsed = core.parse_payment(&target)?;
            let fee_rate =
                FeeRate::from_sat_per_vb(rate.parse()?).context("that fee rate is out of range")?;

            // §3a rule 4: quote first, then confirm. The CLI puts a human in
            // the loop exactly as the bot does, and core blocked on neither.
            let quote = core
                .quote_send(
                    user,
                    SendRequest {
                        target: parsed,
                        raw: target.clone(),
                        amount: if amount.eq_ignore_ascii_case("max") {
                            SendAmount::Max
                        } else {
                            SendAmount::Exact(Amount::from_sat(amount.parse()?))
                        },
                        fee_rate,
                    },
                )
                .await?;

            println!("To      {}", quote.recipient);
            println!("Amount  {} sats", quote.amount.to_sat());
            println!("Fee     {} sats", quote.fee.to_sat());
            println!("Total   {} sats", quote.total.to_sat());
            println!("Change  {} sats", quote.change.to_sat());
            if quote.is_payjoin {
                println!("Payjoin will be attempted.");
            }

            if !prompt("Type yes to sign and broadcast: ")?
                .trim()
                .eq_ignore_ascii_case("yes")
            {
                core.cancel_quote(user, quote.id).await;
                println!("Cancelled. Nothing was sent.");
                return Ok(());
            }

            let auth = match core.session(user) {
                Some(_) => Auth::Session,
                None => Auth::Pin(read_pin("PIN: ")?),
            };

            let sent = core.confirm_send(user, quote.id, auth).await?;
            println!("\nBroadcast {}", sent.txid);
            if quote.is_payjoin && !sent.payjoin {
                println!("Payjoin didn't complete; sent as a regular transaction.");
            }
        }

        "pj-receive" => {
            let sats: u64 = args
                .first()
                .context("usage: wallet-cli pj-receive <sats>")?
                .parse()?;

            // The receiver contributes an input and signs the proposal, so
            // this needs the seed. Every CLI command is its own process, so
            // there is never a session to inherit — the PIN is collected here
            // rather than widening the facade (§3a fixes its signature).
            ensure_unlocked(core, user).await?;

            let receipt = core.payjoin_receive(user, Amount::from_sat(sats)).await?;
            println!("{}", receipt.bip21);
            println!(
                "\nSession {}. Run `wallet-cli events` to watch it.",
                receipt.session_id
            );
        }

        "pj-sessions" => {
            for s in core.payjoin_sessions(user).await? {
                println!("{}  {:?}  {:?}", s.id, s.role, s.state);
            }
        }

        "mine" => {
            let blocks: u32 = args.first().map(|s| s.parse()).transpose()?.unwrap_or(1);
            let hashes = core.mine(blocks, None).await?;
            println!("mined {} block(s)", hashes.len());
        }

        // The same events the bot renders as push notifications (§3a rule 6).
        "events" => {
            let mut rx = core.subscribe();
            println!("Watching. Ctrl-C to stop.");
            loop {
                tokio::select! {
                    event = rx.recv() => match event {
                        Ok(event) => print_event(&event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            println!("(missed {n} events)");
                        }
                        Err(_) => return Ok(()),
                    },
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
        }

        other => bail!("unknown command `{other}` — run with no arguments for usage"),
    }

    Ok(())
}

fn print_event(event: &CoreEvent) {
    match event {
        CoreEvent::IncomingTx { amount, status, .. } => {
            println!("incoming {} sats ({status:?})", amount.to_sat())
        }
        CoreEvent::TxConfirmed {
            txid,
            confirmations,
            ..
        } => println!("confirmed {txid} ({confirmations} confs)"),
        CoreEvent::SyncProgress { height, tip, .. } => println!("sync {height}/{tip}"),
        CoreEvent::SessionExpired { .. } => println!("session expired"),
        CoreEvent::Payjoin { state, .. } => println!("payjoin {state:?}"),
        CoreEvent::BackendHealth(h) => println!("backend {h:?}"),
        other => println!("{other:?}"),
    }
}

/// The CLI's identity map, such as it is: core knows an opaque `UserId` and
/// this front end decides how to supply one (§3a rule 3).
fn take_user(args: &mut Vec<String>) -> Result<UserId> {
    if let Some(i) = args.iter().position(|a| a == "--user") {
        let raw = args.get(i + 1).cloned().context("--user needs a UUID")?;
        args.drain(i..=i + 1);
        return UserId::from_str(&raw).context("--user must be a UUID");
    }

    match std::env::var("WALLET_CLI_USER") {
        Ok(raw) => UserId::from_str(&raw).context("WALLET_CLI_USER must be a UUID"),
        Err(_) => {
            let fresh = UserId::new();
            eprintln!("No --user given; using {fresh}");
            eprintln!("Set WALLET_CLI_USER={fresh} to keep this wallet.\n");
            Ok(fresh)
        }
    }
}

/// Open a session if one is not already open.
///
/// The bot keeps a session across commands because it is one long-lived
/// process. A CLI cannot, so any command that needs the seed asks for the PIN
/// first. That is a front-end concern, which is why it lives here and not on
/// the facade.
async fn ensure_unlocked(core: &Arc<WalletService>, user: UserId) -> Result<()> {
    if core.session(user).is_some() {
        return Ok(());
    }
    let pin = read_pin("PIN: ")?;
    core.unlock(user, &pin).await?;
    Ok(())
}

fn page_arg(args: &[String]) -> u32 {
    args.first()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|n| n.saturating_sub(1))
        .unwrap_or(0)
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write as _;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Read a PIN without echoing it. The same rule as the bot deleting the
/// message: a secret must not be left on screen (§8.1).
///
/// With no terminal attached, read it from stdin instead. That is not a
/// weakening — there is no screen to leave it on — and it is what lets this
/// binary be the headless driver §9 asks for: a test or a script can pipe a
/// PIN in, which `rpassword` alone refuses with `ENXIO`.
fn read_pin(label: &str) -> Result<Pin> {
    use std::io::IsTerminal as _;

    if std::io::stdin().is_terminal() {
        return Ok(Pin::new(rpassword::prompt_password(label)?));
    }
    Ok(Pin::new(prompt(label)?))
}

fn chain(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "mainnet",
        Network::Regtest => "regtest",
        Network::Testnet => "testnet",
        Network::Signet => "signet",
        _ => "unknown",
    }
}

fn usage() {
    eprintln!(
        "wallet-cli — the same wallet as the Telegram bot, over the same facade.

  wallet-cli [--user <uuid>] <command>

  status                 backend, tip, call budget, session
  create                 new wallet (seed shown once, then a 3-word check)
  restore                restore from a seed phrase
  unlock | lock          open or close a signing session
  receive                next unused address, as text and BIP21
  addresses [page]       revealed addresses with used/unused
  balance                confirmed, pending, incoming, total
  history [page]         transactions, newest first
  fees                   presets, floor and where they came from
  send <to> <sats|max> <sat/vB>
  pj-receive <sats>      ask to be paid with payjoin
  pj-sessions            payjoin sessions and their state
  mine <n>               regtest only
  events                 print CoreEvents as they arrive
"
    );
}
