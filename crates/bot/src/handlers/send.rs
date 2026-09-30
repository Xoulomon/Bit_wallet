//! The send flow (PLAN.md §8.3) and `/bumpfee`.
//!
//! Three screens and one rule: money never moves without a confirm card, and
//! only a button press plus an open session or a PIN reaches `confirm_send`
//! (§8.1). The PSBT is never here — the bot holds a `QuoteId` and core
//! re-validates it, which is why a replayed button cannot move money (§8.5).

use crate::{
    Ctx,
    dialogue::{PendingAction, State, WalletDialogue},
    ui,
};
use anyhow::Result;
use std::str::FromStr;
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::bitcoin::{Amount, FeeRate, Txid};
use wallet_core::types::{Auth, QuoteId, SendAmount, SendRequest, UserId};

fn user_of(msg: &Message, ctx: &Ctx) -> Result<UserId> {
    let from = msg
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a private message always has a sender"))?;
    #[allow(clippy::cast_possible_wrap)]
    ctx.users.resolve(from.id.0 as i64)
}

/// `/send <addr|bip21> [amount|max]` (§8.3 step 1).
pub async fn send(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    args: String,
) -> Result<()> {
    let network = ctx.core.network();
    let mut parts = args.split_whitespace();

    let Some(target_arg) = parts.next() else {
        bot.send_message(msg.chat.id, ui::send_usage(network))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    };

    let target = match ctx.core.parse_payment(target_arg) {
        Ok(t) => t,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    // An amount on the command line wins; otherwise the URI's, if it had one.
    let amount = match parts.next() {
        Some(raw) if raw.eq_ignore_ascii_case("max") => Some(None),
        Some(raw) => match raw.parse::<u64>() {
            Ok(sats) => Some(Some(Amount::from_sat(sats))),
            Err(_) => {
                bot.send_message(msg.chat.id, ui::send_usage(network))
                    .parse_mode(ParseMode::Html)
                    .await?;
                return Ok(());
            }
        },
        None => target.amount.map(Some),
    };

    let Some(amount) = amount else {
        bot.send_message(msg.chat.id, ui::send_usage(network))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    };

    let fees = match ctx.core.fee_options().await {
        Ok(f) => f,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    dialogue
        .update(State::AwaitFeeChoice {
            target: target_arg.to_string(),
            amount: amount.map(|a| a.to_sat()),
        })
        .await?;

    let address = target.address.clone().assume_checked().to_string();
    bot.send_message(msg.chat.id, ui::fee_card(network, amount, &address, &fees))
        .parse_mode(ParseMode::Html)
        .reply_markup(ui::fee_keyboard(&fees))
        .await?;

    Ok(())
}

/// A fee button (§8.3 step 2). The callback carries a label, never a rate — so
/// a replayed button cannot raise the fee on a payment (§8.5).
pub async fn choose_fee(
    bot: Bot,
    query: CallbackQuery,
    dialogue: WalletDialogue,
    ctx: Ctx,
    (target, amount): (String, Option<u64>),
) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;

    let Some(data) = query.data.as_deref() else {
        return Ok(());
    };
    let Some(message) = query.message.clone() else {
        return Ok(());
    };
    let chat = message.chat().id;
    let slug = data.trim_start_matches("send:fee:");

    let fees = match ctx.core.fee_options().await {
        Ok(f) => f,
        Err(e) => return reply(&bot, chat, ui::render_error(&e)).await,
    };

    if slug == "custom" {
        dialogue
            .update(State::AwaitCustomFee { target, amount })
            .await?;
        return reply(&bot, chat, ui::ask_custom_fee(fees.floor)).await;
    }

    let Some(label) = ui::fee_from_slug(slug) else {
        return Ok(());
    };
    let Some((_, rate)) = fees.presets.iter().find(|(l, _)| *l == label) else {
        return reply(
            &bot,
            chat,
            ui::render_error(&wallet_core::CoreError::QuoteExpired),
        )
        .await;
    };

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;
    quote_and_show(&bot, chat, &dialogue, &ctx, user, &target, amount, *rate).await
}

/// A typed sat/vB, for mainnet or for anyone who wants an exact rate (§6).
pub async fn receive_custom_fee(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    (target, amount): (String, Option<u64>),
) -> Result<()> {
    let typed = msg.text().unwrap_or_default().trim().to_string();

    let Ok(sat_vb) = typed.parse::<u64>() else {
        let floor = ctx
            .core
            .fee_options()
            .await
            .map(|f| f.floor)
            .unwrap_or(FeeRate::BROADCAST_MIN);
        bot.send_message(msg.chat.id, ui::ask_custom_fee(floor))
            .await?;
        return Ok(());
    };

    let Some(rate) = FeeRate::from_sat_per_vb(sat_vb) else {
        bot.send_message(msg.chat.id, "That rate is out of range.")
            .await?;
        return Ok(());
    };

    let user = user_of(&msg, &ctx)?;
    quote_and_show(
        &bot,
        msg.chat.id,
        &dialogue,
        &ctx,
        user,
        &target,
        amount,
        rate,
    )
    .await
}

/// Price the payment and show the confirm card (§8.3 step 3).
#[allow(clippy::too_many_arguments)]
async fn quote_and_show(
    bot: &Bot,
    chat: ChatId,
    dialogue: &WalletDialogue,
    ctx: &Ctx,
    user: UserId,
    target: &str,
    amount: Option<u64>,
    rate: FeeRate,
) -> Result<()> {
    let parsed = match ctx.core.parse_payment(target) {
        Ok(t) => t,
        Err(e) => return reply(bot, chat, ui::render_error(&e)).await,
    };

    let request = SendRequest {
        target: parsed,
        raw: target.to_string(),
        amount: match amount {
            Some(sats) => SendAmount::Exact(Amount::from_sat(sats)),
            None => SendAmount::Max,
        },
        fee_rate: rate,
    };

    let quote = match ctx.core.quote_send(user, request).await {
        Ok(q) => q,
        Err(e) => {
            dialogue.exit().await?;
            return reply(bot, chat, ui::render_error(&e)).await;
        }
    };

    let card = bot
        .send_message(chat, ui::confirm_card(ctx.core.network(), &quote))
        .parse_mode(ParseMode::Html)
        .reply_markup(ui::confirm_keyboard(&quote))
        .await?;

    dialogue
        .update(State::SendConfirm {
            quote: quote.id.to_string(),
            card: Some(card.id.0),
        })
        .await?;

    Ok(())
}

/// Confirm (§8.3 step 4). An open session signs now; otherwise the PIN is
/// collected by the one handler that collects PINs (§8.4).
pub async fn confirm(
    bot: Bot,
    query: CallbackQuery,
    dialogue: WalletDialogue,
    ctx: Ctx,
) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;

    let Some(data) = query.data.as_deref() else {
        return Ok(());
    };
    let Some(message) = query.message.clone() else {
        return Ok(());
    };
    let chat = message.chat().id;

    let raw = data.trim_start_matches("send:confirm:");
    let Ok(quote) = QuoteId::from_str(raw) else {
        return Ok(());
    };

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;

    if ctx.core.session(user).is_none() {
        dialogue
            .update(State::AwaitPin {
                pending: PendingAction::Send {
                    quote: quote.to_string(),
                },
            })
            .await?;
        return reply(&bot, chat, ui::ask_pin()).await;
    }

    broadcast(&bot, chat, &dialogue, &ctx, user, quote, Auth::Session).await
}

/// Sign and broadcast, whichever way the user authorised it.
pub async fn broadcast(
    bot: &Bot,
    chat: ChatId,
    dialogue: &WalletDialogue,
    ctx: &Ctx,
    user: UserId,
    quote: QuoteId,
    auth: Auth,
) -> Result<()> {
    let status = bot.send_message(chat, ui::broadcasting()).await?;

    match ctx.core.confirm_send(user, quote, auth).await {
        Ok(b) => {
            dialogue.exit().await?;
            bot.edit_message_text(chat, status.id, ui::broadcast_done(ctx.core.network(), &b))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            // An expired quote gets its own card: the user needs to know that
            // nothing was sent, not just that something failed (§8.1).
            let text = match e {
                wallet_core::CoreError::QuoteExpired => ui::quote_expired_card(ctx.core.network()),
                ref other => ui::render_error(other),
            };
            // A wrong PIN is worth another try; anything else ends the flow.
            if !matches!(e, wallet_core::CoreError::WrongPin { .. }) {
                dialogue.exit().await?;
            }
            bot.edit_message_text(chat, status.id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

pub async fn cancel(
    bot: Bot,
    query: CallbackQuery,
    dialogue: WalletDialogue,
    ctx: Ctx,
) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;

    let Some(message) = query.message.clone() else {
        return Ok(());
    };

    if let Some(raw) = query
        .data
        .as_deref()
        .map(|d| d.trim_start_matches("send:cancel:"))
        && let Ok(quote) = QuoteId::from_str(raw)
    {
        #[allow(clippy::cast_possible_wrap)]
        let user = ctx.users.resolve(query.from.id.0 as i64)?;
        ctx.core.cancel_quote(user, quote).await;
    }

    dialogue.exit().await?;
    bot.edit_message_text(message.chat().id, message.id(), ui::send_cancelled())
        .await?;
    Ok(())
}

/// `/bumpfee <txid>` (§8.2). The same confirm card, only the header differs.
pub async fn bump_fee(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    txid: String,
) -> Result<()> {
    let Ok(txid) = Txid::from_str(txid.trim()) else {
        bot.send_message(
            msg.chat.id,
            "Which transaction? <code>/bumpfee &lt;txid&gt;</code> — /history lists them.",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    };

    let user = user_of(&msg, &ctx)?;
    let fees = match ctx.core.fee_options().await {
        Ok(f) => f,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    // A bump has to beat the original, so the fastest rate on offer is the
    // sensible default; the floor is the fallback when there are no presets.
    let rate = fees.presets.first().map(|(_, r)| *r).unwrap_or(fees.floor);

    match ctx.core.bump_fee(user, txid, rate).await {
        Ok(quote) => {
            let card = bot
                .send_message(msg.chat.id, ui::confirm_card(ctx.core.network(), &quote))
                .parse_mode(ParseMode::Html)
                .reply_markup(ui::confirm_keyboard(&quote))
                .await?;
            dialogue
                .update(State::SendConfirm {
                    quote: quote.id.to_string(),
                    card: Some(card.id.0),
                })
                .await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}

async fn reply(bot: &Bot, chat: ChatId, text: String) -> Result<()> {
    bot.send_message(chat, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}
