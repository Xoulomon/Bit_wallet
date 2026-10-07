//! `/mine` and `/faucet` (PLAN.md §8.2, §8.7).
//!
//! Two gates, deliberately in different places: the front end decides *who*
//! may call it, and core decides *whether it exists at all* — both return
//! `UnsupportedOnNetwork` off regtest whatever the bot thinks (§3a).
//!
//! The two commands are gated differently on purpose. `/mine` is an admin
//! command because it moves the chain itself, and a chain is shared. `/faucet`
//! only moves worthless regtest coins into one caller's own wallet, so the
//! network check is the only guard it needs — anyone trying the bot can fund
//! themselves without leaving the chat.

use crate::{Ctx, ui};
use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::types::UserId;
use wallet_core::{CoreError, bitcoin::Amount};

/// Above this many blocks, `/mine` says it is working before it starts.
const MANY_BLOCKS: u32 = 10;

/// What `/faucet` hands out when no amount is given: enough to pay a few
/// times over, and far too little to be worth a second thought.
const FAUCET_DEFAULT_SATS: u64 = 100_000;

/// The bounds on an explicit amount. The ceiling is not a safety rule — these
/// coins are worthless — it is there so a typo asks for a block subsidy rather
/// than draining the node wallet in one go.
const FAUCET_MIN_SATS: u64 = 1_000;
const FAUCET_MAX_SATS: u64 = 10_000_000;

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

    // A big mine takes real time at the node, and silence reads as a hang. Say
    // so first and edit that same message into the result, so `/mine 500`
    // still leaves exactly one message behind (§8.1).
    let status = if count > MANY_BLOCKS {
        Some(
            bot.send_message(msg.chat.id, ui::mining(ctx.core.network(), count))
                .parse_mode(ParseMode::Html)
                .await?,
        )
    } else {
        None
    };

    let hashes = match ctx.core.mine(count, to).await {
        Ok(hashes) => hashes,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    // Pull the new blocks into the caller's wallet before reading the balance,
    // or the number reported is the one from before they mined.
    let balance = if to_self {
        let _ = ctx.core.sync_now(user).await;
        ctx.core.balance(user).await.ok()
    } else {
        None
    };
    let price = ctx.core.price().await;

    let card = ui::mined(
        ctx.core.network(),
        hashes.len(),
        to_self,
        balance.as_ref(),
        price.as_ref(),
    );

    match status {
        Some(status) => {
            bot.edit_message_text(msg.chat.id, status.id, card)
                .parse_mode(ParseMode::Html)
                .await?;
        }
        None => {
            bot.send_message(msg.chat.id, card)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

/// `/faucet [sats]` — credit the caller's own wallet on regtest.
///
/// No admin gate (see the module note). No PIN either: revealing the next
/// address is a watch-only read, and nothing here spends the caller's coins.
pub async fn faucet(bot: Bot, msg: Message, ctx: Ctx, sats: String) -> Result<()> {
    let user = crate::handlers::user_of(&msg, &ctx)?;

    let trimmed = sats.trim();
    let amount = if trimmed.is_empty() {
        FAUCET_DEFAULT_SATS
    } else {
        match trimmed.parse::<u64>() {
            Ok(n) if (FAUCET_MIN_SATS..=FAUCET_MAX_SATS).contains(&n) => n,
            _ => {
                bot.send_message(
                    msg.chat.id,
                    ui::faucet_usage(FAUCET_MIN_SATS, FAUCET_MAX_SATS),
                )
                .parse_mode(ParseMode::Html)
                .await?;
                return Ok(());
            }
        }
    };

    run_faucet(&bot, msg.chat.id, &ctx, user, amount).await
}

/// The default handout, for the inline button — which has no argument to carry
/// and so asks for exactly what a bare `/faucet` asks for.
pub const fn default_handout() -> u64 {
    FAUCET_DEFAULT_SATS
}

/// Everything `/faucet` does once the amount is settled.
pub async fn run_faucet(bot: &Bot, chat: ChatId, ctx: &Ctx, user: UserId, sats: u64) -> Result<()> {
    let amount = Amount::from_sat(sats);

    if !matches!(ctx.core.wallet_exists(user), Ok(true)) {
        bot.send_message(chat, ui::faucet_needs_wallet())
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let to = match ctx.core.next_address(user).await {
        Ok(address) => address,
        Err(e) => return crate::handlers::reply_error_at(bot, chat, "/faucet", &e).await,
    };

    let txid = match ctx.core.faucet(&to.address, amount).await {
        Ok(txid) => txid,

        // Core's `InsufficientFunds` here is about the *node's* wallet, and the
        // generic rendering would report it as "you have 0" — which is both
        // wrong and the opposite of actionable. Only this handler knows whose
        // funds they were, so only this handler can say what to do about it.
        Err(CoreError::InsufficientFunds { available, .. }) => {
            bot.send_message(chat, ui::faucet_dry(available))
                .parse_mode(ParseMode::Html)
                .await?;
            return Ok(());
        }

        Err(e) => return crate::handlers::reply_error_at(bot, chat, "/faucet", &e).await,
    };

    // Core mined a block to confirm the payment, so the coins are already
    // spendable — but this wallet has not looked at that block yet.
    let _ = ctx.core.sync_now(user).await;
    let balance = ctx.core.balance(user).await.ok();
    let price = ctx.core.price().await;

    bot.send_message(
        chat,
        ui::faucet_sent(
            ctx.core.network(),
            &to.address.to_string(),
            amount,
            &txid.to_string(),
            balance.as_ref(),
            price.as_ref(),
        ),
    )
    .parse_mode(ParseMode::Html)
    .await?;
    Ok(())
}
