//! `/start`, `/help`, `/status`, `/network` (PLAN.md §8.2).

use crate::{Ctx, ui};
use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode, utils::command::BotCommands};

pub async fn start(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    // Step 1: no wallet can exist yet, because no create path does. Step 3
    // replaces this with `ctx.core.wallet_exists(user)?` and the menu branch.
    let text = ui::welcome(ctx.core.network(), false);
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

pub async fn help(bot: Bot, msg: Message) -> Result<()> {
    bot.send_message(
        msg.chat.id,
        crate::commands::Command::descriptions().to_string(),
    )
    .await?;
    Ok(())
}

pub async fn status(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    match ctx.core.status().await {
        Ok(s) => {
            // Step 3 fills in the session; there is no session store yet.
            bot.send_message(msg.chat.id, ui::status(&s, None))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "status failed");
            bot.send_message(msg.chat.id, ui::render_error(&e))
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

pub async fn network(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    bot.send_message(msg.chat.id, ui::network_card(ctx.core.network()))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// Everything §8.2 lists that a later step implements. Saying so is better than
/// silence: the command is registered, so Telegram offers it in the menu.
pub async fn not_yet(bot: Bot, msg: Message) -> Result<()> {
    bot.send_message(
        msg.chat.id,
        "That command isn't wired up in this build yet. /status and /network work.",
    )
    .await?;
    Ok(())
}
