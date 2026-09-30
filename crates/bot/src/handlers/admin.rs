//! `/mine` (PLAN.md §8.2, §8.7).
//!
//! Two gates, deliberately in different places: the front end decides *who*
//! may call it, and core decides *whether it exists at all* — `mine` returns
//! `UnsupportedOnNetwork` off regtest whatever the bot thinks (§3a).

use crate::{Ctx, ui};
use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode};

pub async fn mine(bot: Bot, msg: Message, ctx: Ctx, blocks: String) -> Result<()> {
    let Some(from) = msg.from.as_ref() else {
        return Ok(());
    };

    #[allow(clippy::cast_possible_wrap)]
    let tg = from.id.0 as i64;
    if !ctx.policy.is_admin(tg) {
        bot.send_message(msg.chat.id, "That command is for this bot's admins.")
            .await?;
        return Ok(());
    }

    let count: u32 = blocks.trim().parse().unwrap_or(1);
    if count == 0 || count > 500 {
        bot.send_message(msg.chat.id, "Mine between 1 and 500 blocks.")
            .await?;
        return Ok(());
    }

    match ctx.core.mine(count, None).await {
        Ok(hashes) => {
            bot.send_message(msg.chat.id, ui::mined(ctx.core.network(), hashes.len()))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}
