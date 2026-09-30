//! The Telegram front end (PLAN.md §8).
//!
//! A thin client over `WalletService`. Its job is three translations and nothing
//! else: chat input → a service call, service data → a rendered message,
//! `CoreEvent` → a push notification. It cannot reach past the facade, and §10
//! has a grep test that proves it: no `bdk_wallet`, `Psbt` or `Mnemonic` import
//! appears anywhere in this crate.

mod auth;
mod commands;
mod handlers;
mod ui;
mod users;

use anyhow::{Context, Result};
use commands::Command;
use std::sync::Arc;
use teloxide::{dispatching::UpdateFilterExt, prelude::*, utils::command::BotCommands};
use wallet_core::{AppConfig, WalletService};

/// What every handler needs. Note what is absent: no seed, no PSBT, no
/// descriptor. The bot holds a handle on the service and its own two maps.
#[derive(Clone)]
pub struct Ctx {
    pub core: Arc<WalletService>,
    pub users: Arc<users::UserStore>,
    pub policy: Arc<auth::Policy>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing();

    let cfg = AppConfig::from_env().context("loading configuration")?;
    let network = cfg.network;
    let bot_db = cfg.network_dir().join("bot.sqlite");

    tracing::info!(network = network.namespace(), "starting");

    // Fails fast if the backend is unreachable or is serving the wrong chain (§4).
    let core = WalletService::new(cfg)
        .await
        .context("connecting to the Bitcoin backend")?;

    let ctx = Ctx {
        core,
        users: Arc::new(users::UserStore::open(&bot_db)?),
        policy: Arc::new(auth::Policy::from_env()),
    };

    let bot = Bot::from_env();
    bot.set_my_commands(Command::bot_commands())
        .await
        .context("registering the command menu")?;

    tracing::info!("dispatching");

    Dispatcher::builder(bot, schema())
        .dependencies(dptree::deps![ctx])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// The update tree. The guard of §8.7 runs *before* any handler, so a group
/// chat never reaches wallet code at all.
fn schema() -> teloxide::dispatching::UpdateHandler<anyhow::Error> {
    use dptree::case;

    let commands = teloxide::filter_command::<Command, _>()
        .branch(case![Command::Start].endpoint(handlers::start::start))
        .branch(case![Command::Help].endpoint(handlers::start::help))
        .branch(case![Command::Status].endpoint(handlers::start::status))
        .branch(case![Command::Network].endpoint(handlers::start::network))
        .endpoint(handlers::start::not_yet);

    Update::filter_message()
        .branch(dptree::filter_map(guard).endpoint(refuse))
        .branch(commands)
}

/// Yields a refusal only when the message must not be served (§8.7); `None`
/// lets it fall through to the command branch.
fn guard(msg: Message, ctx: Ctx) -> Option<auth::Refusal> {
    auth::check(&msg, &ctx.policy).err()
}

async fn refuse(bot: Bot, msg: Message, refusal: auth::Refusal) -> Result<()> {
    bot.send_message(msg.chat.id, refusal.message()).await?;
    Ok(())
}

/// `RUST_LOG` filtering, with the mnemonic and the API key structurally absent:
/// neither is ever a tracing field, and both are redacted in every `Debug` impl
/// that could carry one (§3a rule 3, §5).
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,bot=debug,wallet_core=debug"));

    fmt().with_env_filter(filter).with_target(true).init();
}
