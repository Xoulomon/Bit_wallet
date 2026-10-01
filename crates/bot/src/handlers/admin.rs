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

    // Mine to the caller's own wallet. Two reasons, and the second is the one
    // that bites: `/mine 101` then actually funds the person who typed it,
    // which is what they wanted; and supplying an address means core never
    // calls `getnewaddress`, so it never has to pick among the node's own
    // wallets — Polar loads several, and Core refuses a bare wallet call.
    //
    // If the caller has no wallet yet, fall through to core's own choice: an
    // admin mining before /create is setting up a chain, not funding himself.
    let user = ctx.users.resolve(tg)?;
    let to = match ctx.core.wallet_exists(user) {
        Ok(true) => ctx.core.next_address(user).await.ok().map(|a| a.address),
        _ => None,
    };
    let to_self = to.is_some();

    match ctx.core.mine(count, to).await {
        Ok(hashes) => {
            bot.send_message(
                msg.chat.id,
                ui::mined(ctx.core.network(), hashes.len(), to_self),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}
