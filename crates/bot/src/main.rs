//! The Telegram front end (PLAN.md §8).
//!
//! A thin client over `WalletService`. Its job is three translations and nothing
//! else: chat input → a service call, service data → a rendered message,
//! `CoreEvent` → a push notification. It cannot reach past the facade, and §10
//! has a grep test that proves it: no `bdk_wallet`, `Psbt` or `Mnemonic` import
//! appears anywhere in this crate.

mod auth;
mod commands;
mod dialogue;
mod handlers;
mod notify;
mod throttle;
mod ui;
mod users;

use anyhow::{Context, Result};
use commands::Command;
use dialogue::{SqliteDialogueStore, State};
use std::sync::{Arc, Mutex};
use teloxide::{dispatching::UpdateFilterExt, prelude::*, utils::command::BotCommands};
use wallet_core::{AppConfig, WalletService};

/// What every handler needs. Note what is absent: no seed, no PSBT, no
/// descriptor. The bot holds a handle on the service and its own two maps.
#[derive(Clone)]
pub struct Ctx {
    pub core: Arc<WalletService>,
    pub users: Arc<users::UserStore>,
    pub policy: Arc<auth::Policy>,
    /// §8.7: per-user *command* throttling, a separate concern from core's
    /// RPC budget.
    pub throttle: Arc<throttle::Throttle>,
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

    // One connection, two tables: the tg_id map and the dialogue states. They
    // share a file because they are the same kind of thing — front-end state
    // that core must never see (§3a rule 3, §8.4).
    let bot_conn = Arc::new(Mutex::new(users::open_bot_db(&bot_db)?));
    let ctx = Ctx {
        core,
        users: Arc::new(users::UserStore::new(Arc::clone(&bot_conn))?),
        policy: Arc::new(auth::Policy::from_env()),
        throttle: Arc::new(throttle::Throttle::new()),
    };
    let dialogues = SqliteDialogueStore::new(bot_conn)?;

    let bot = Bot::from_env();
    bot.set_my_commands(Command::bot_commands())
        .await
        .context("registering the command menu")?;

    // §8.6: one subscriber, running beside the dispatcher for as long as it does.
    tokio::spawn(notify::run(bot.clone(), ctx.clone()));

    tracing::info!("dispatching");

    Dispatcher::builder(bot, schema())
        .dependencies(dptree::deps![ctx.clone(), dialogues])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    // Let the chain follower finish its pass, and drop every unlocked seed
    // rather than leaving one in a process that is on its way out.
    ctx.core.shutdown().await;

    Ok(())
}

/// The update tree (§8).
///
/// The guard of §8.7 runs *before* anything else, so a group chat never reaches
/// wallet code at all. Commands are matched before dialogue states, which is
/// what lets a user type /start to escape a flow they no longer want.
fn schema() -> teloxide::dispatching::UpdateHandler<anyhow::Error> {
    use dptree::case;

    let commands = teloxide::filter_command::<Command, _>()
        .branch(case![Command::Start].endpoint(handlers::start::start))
        .branch(case![Command::Help].endpoint(handlers::start::help))
        .branch(case![Command::Status].endpoint(handlers::start::status))
        .branch(case![Command::Network].endpoint(handlers::start::network))
        .branch(case![Command::Create].endpoint(handlers::wallet::create))
        .branch(case![Command::Restore].endpoint(handlers::wallet::restore))
        .branch(case![Command::Unlock].endpoint(handlers::wallet::unlock))
        .branch(case![Command::Lock].endpoint(handlers::wallet::lock))
        .branch(case![Command::Export].endpoint(handlers::wallet::export))
        .branch(case![Command::Delete].endpoint(handlers::wallet::delete))
        .branch(case![Command::Receive].endpoint(handlers::onchain::receive))
        .branch(case![Command::Balance].endpoint(handlers::onchain::balance))
        .branch(case![Command::Addresses { page }].endpoint(handlers::onchain::addresses))
        .branch(case![Command::History { page }].endpoint(handlers::onchain::history))
        .branch(case![Command::Tx { txid }].endpoint(handlers::onchain::tx))
        .branch(case![Command::Send { args }].endpoint(handlers::send::send))
        .branch(case![Command::Bumpfee { txid }].endpoint(handlers::send::bump_fee))
        .branch(case![Command::Mine { blocks }].endpoint(handlers::admin::mine))
        .branch(case![Command::Faucet { sats }].endpoint(handlers::admin::faucet))
        .branch(case![Command::PjReceive { sats }].endpoint(handlers::payjoin::receive))
        .branch(case![Command::PjSessions].endpoint(handlers::payjoin::sessions))
        .endpoint(handlers::start::not_yet);

    // §8.4: every state that expects text has exactly one endpoint, so
    // delete-on-receipt and the retry counter are written once each.
    let states = dptree::entry()
        .branch(case![State::RestoreMnemonic].endpoint(handlers::wallet::receive_mnemonic))
        .branch(
            case![State::RestoreBirthday { words }].endpoint(handlers::wallet::receive_birthday),
        )
        .branch(case![State::SetPin { intent }].endpoint(handlers::wallet::receive_new_pin))
        .branch(
            case![State::ConfirmPin { intent, first }]
                .endpoint(handlers::wallet::receive_pin_confirmation),
        )
        .branch(
            case![State::CreateConfirmWords {
                challenge,
                answered
            }]
            .endpoint(handlers::wallet::receive_backup_word),
        )
        .branch(case![State::AwaitPin { pending }].endpoint(handlers::wallet::receive_pin))
        .branch(case![State::DeleteTypeConfirm].endpoint(handlers::wallet::receive_delete_word))
        .branch(case![State::AwaitCustomFee { what }].endpoint(handlers::send::receive_custom_fee));

    let messages = Update::filter_message()
        .branch(dptree::filter_map(guard).endpoint(refuse))
        .enter_dialogue::<Message, SqliteDialogueStore, State>()
        .branch(commands)
        .branch(states)
        // Nothing above matched. Without this the update is dropped and the
        // user gets silence, which is indistinguishable from a broken bot —
        // one mistyped command and they have no idea why.
        .endpoint(unrecognised);

    // §8.5: callback data is `action:subject:arg`, and every id in it is one
    // core minted — so a replayed button can only reference something core will
    // re-validate or reject.
    let callbacks = Update::filter_callback_query()
        // §8.7, for taps as well as for messages. A button can now run a
        // command, so the gate that covers typing one has to cover tapping it.
        .branch(dptree::filter_map(guard_callback).endpoint(refuse_tap))
        .branch(
            dptree::filter(|q: CallbackQuery| q.data.as_deref() == Some("bal:refresh"))
                .endpoint(handlers::onchain::refresh_balance),
        )
        // Paging /history. Outside the dialogue, like Refresh: turning a page
        // reads, so it has nothing to say to a flow the user is in the middle
        // of and must not be blocked by one either.
        .branch(
            dptree::filter(starts_with(ui::HISTORY_PAGE_PREFIX))
                .endpoint(handlers::onchain::turn_history_page),
        )
        .enter_dialogue::<CallbackQuery, SqliteDialogueStore, State>()
        // The inline menu (§8.2): every button here runs the same function the
        // typed command runs. It is inside the dialogue so /create and /unlock
        // can open their flows from a tap.
        .branch(dptree::filter(starts_with(ui::MENU_PREFIX)).endpoint(handlers::menu::tap))
        .branch(
            dptree::filter(starts_with("send:fee:"))
                .branch(case![State::AwaitFeeChoice { what }].endpoint(handlers::send::choose_fee)),
        )
        // Gated on the state, not only on the id: `confirm` rewrites the
        // dialogue to ask for a PIN, so a tap on a card the flow has moved
        // past must not reach it. The fallback endpoint is what stops a stale
        // card being answered with silence.
        .branch(
            dptree::filter(starts_with("send:confirm:"))
                .branch(case![State::SendConfirm { quote, card }].endpoint(handlers::send::confirm))
                .endpoint(handlers::send::confirm_stale),
        )
        .branch(dptree::filter(starts_with("send:cancel:")).endpoint(handlers::send::cancel))
        .branch(dptree::filter(starts_with("pj:cancel:")).endpoint(handlers::payjoin::cancel));

    dptree::entry().branch(messages).branch(callbacks)
}

/// §8.5: `action:subject:arg`. Matching on the prefix keeps the routing in one
/// place and the ids opaque.
fn starts_with(prefix: &'static str) -> impl Fn(CallbackQuery) -> bool + Clone {
    move |q: CallbackQuery| q.data.as_deref().is_some_and(|d| d.starts_with(prefix))
}

/// Yields a refusal only when the message must not be served (§8.7); `None`
/// lets it fall through to the command branch.
fn guard(msg: Message, ctx: Ctx) -> Option<auth::Refusal> {
    let (tg, _) = match auth::check(&msg, &ctx.policy) {
        Ok(ok) => ok,
        Err(refusal) => return Some(refusal),
    };

    #[allow(clippy::cast_possible_wrap)]
    if !ctx.throttle.allow(tg.0 as i64) {
        return Some(auth::Refusal::TooFast);
    }
    None
}

async fn refuse(bot: Bot, msg: Message, refusal: auth::Refusal) -> Result<()> {
    bot.send_message(msg.chat.id, refusal.message()).await?;
    Ok(())
}

/// [`guard`] for a tap. Same policy, different update: a callback carries its
/// own sender and its own throttle slot.
fn guard_callback(query: CallbackQuery, ctx: Ctx) -> Option<auth::Refusal> {
    if let Err(refusal) = auth::check_callback(&query, &ctx.policy) {
        return Some(refusal);
    }
    #[allow(clippy::cast_possible_wrap)]
    if !ctx.throttle.allow(query.from.id.0 as i64) {
        return Some(auth::Refusal::TooFast);
    }
    None
}

/// An alert on the button rather than a message in the chat.
///
/// The refusal belongs to the tap, and in the one case that matters — a card
/// forwarded into a group — the chat it would be posted in is the group the tap
/// is being refused for.
async fn refuse_tap(bot: Bot, query: CallbackQuery, refusal: auth::Refusal) -> Result<()> {
    // Telegram caps an alert at 200 characters and rejects a longer one, which
    // would turn a refusal into a dispatcher error and no answer at all.
    let text: String = refusal.message().chars().take(200).collect();
    bot.answer_callback_query(query.id)
        .text(text)
        .show_alert(true)
        .await?;
    Ok(())
}

/// A message no command and no dialogue state claimed.
async fn unrecognised(bot: Bot, msg: Message) -> Result<()> {
    let text = msg.text().unwrap_or_default();
    bot.send_message(msg.chat.id, ui::unrecognised(text))
        .parse_mode(teloxide::types::ParseMode::Html)
        .await?;
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
