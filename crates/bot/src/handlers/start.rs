//! `/start`, `/help`, `/status`, `/network` (PLAN.md §8.2).
//!
//! Each command is a thin entry point over a `show_*` that takes a `ChatId`
//! rather than a `Message`, because the inline menu runs the same commands from
//! a tap (§8.5) and a tap has no message of its own. The split is the whole
//! mechanism: there is one body per command, reached two ways.

use crate::{Ctx, ui};
use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode, utils::command::BotCommands};
use wallet_core::types::UserId;

/// Whether this user has a wallet, for the cards and the menu that depend on
/// it. A storage error reads as "no wallet", which shows /create and /restore —
/// the two commands that are safe to offer either way.
fn has_wallet(ctx: &Ctx, user: UserId) -> bool {
    matches!(ctx.core.wallet_exists(user), Ok(true))
}

pub async fn start(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = crate::handlers::user_of(&msg, &ctx)?;
    show_welcome(&bot, msg.chat.id, &ctx, user).await
}

pub async fn show_welcome(bot: &Bot, chat: ChatId, ctx: &Ctx, user: UserId) -> Result<()> {
    let network = ctx.core.network();
    let has_wallet = has_wallet(ctx, user);

    bot.send_message(chat, ui::welcome(network, has_wallet))
        .parse_mode(ParseMode::Html)
        .reply_markup(ui::menu_keyboard(network, has_wallet))
        .await?;
    Ok(())
}

pub async fn help(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = crate::handlers::user_of(&msg, &ctx)?;
    show_help(&bot, msg.chat.id, &ctx, user).await
}

/// The full command list, with the menu under it.
///
/// Both, not one or the other: the text is the authoritative surface — it is
/// the only place `/send <address> <amount>` can be shown with its arguments —
/// and the keyboard is the shortcut for the commands that need none.
pub async fn show_help(bot: &Bot, chat: ChatId, ctx: &Ctx, user: UserId) -> Result<()> {
    bot.send_message(chat, crate::commands::Command::descriptions().to_string())
        .reply_markup(ui::menu_keyboard(ctx.core.network(), has_wallet(ctx, user)))
        .await?;
    Ok(())
}

pub async fn status(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = crate::handlers::user_of(&msg, &ctx)?;
    show_status(&bot, msg.chat.id, &ctx, user).await
}

pub async fn show_status(bot: &Bot, chat: ChatId, ctx: &Ctx, user: UserId) -> Result<()> {
    match ctx.core.status().await {
        Ok(s) => {
            let session = ctx.core.session(user).map(|info| info.remaining);
            let price = ctx.core.price().await;
            bot.send_message(chat, ui::status(&s, session, price.as_ref()))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "status failed");
            bot.send_message(chat, ui::render_error(&e))
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

pub async fn network(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    show_network(&bot, msg.chat.id, &ctx).await
}

pub async fn show_network(bot: &Bot, chat: ChatId, ctx: &Ctx) -> Result<()> {
    bot.send_message(chat, ui::network_card(ctx.core.network()))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// Everything §8.2 lists that a later step implements. Saying so is better than
/// silence: the command is registered, so Telegram offers it in the menu.
pub async fn not_yet(bot: Bot, msg: Message) -> Result<()> {
    bot.send_message(
        msg.chat.id,
        "That command isn't wired up in this build yet. /create, /restore, /unlock, \
         /lock, /export, /delete, /status and /network work.",
    )
    .await?;
    Ok(())
}
