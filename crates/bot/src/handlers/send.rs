//! The send flow (PLAN.md §8.3) and `/bumpfee`.
//!
//! Three screens and one rule: money never moves without a confirm card *and*
//! a freshly typed PIN (§8.1). An open session is not enough — it authorises
//! reading and drafting, never a signature — and core enforces that itself, by
//! taking a `&Pin` rather than anything a session could satisfy. The PSBT is
//! never here: the bot holds a `QuoteId` and core re-validates it, which is why
//! a replayed button cannot move money (§8.5).

use crate::{
    Ctx,
    dialogue::{FeeFor, PendingAction, State, WalletDialogue},
    ui,
};
use anyhow::Result;
use std::str::FromStr;
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::bitcoin::{Amount, FeeRate, Txid};
use wallet_core::types::{FeeOptions, Pin, QuoteId, SendAmount, SendRequest, UserId};

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
            what: FeeFor::Pay {
                target: target_arg.to_string(),
                amount: amount.map(|a| a.to_sat()),
            },
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
    what: FeeFor,
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

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;

    let fees = match fee_menu(&ctx, user, &what).await {
        Ok(f) => f,
        Err(e) => return reply(&bot, chat, ui::render_error(&e)).await,
    };

    if slug == "custom" {
        dialogue.update(State::AwaitCustomFee { what }).await?;
        return reply(&bot, chat, ui::ask_custom_fee(fees.floor)).await;
    }

    // `min` exists only on a bump, where the floor is not a boring lower
    // bound but the actual answer: the one rate that is certain to beat the
    // original. Without it a fresh regtest chain offers an empty keyboard,
    // because every preset it has is below the replacement minimum.
    let rate = if slug == "min" {
        fees.floor
    } else {
        let Some(label) = ui::fee_from_slug(slug) else {
            return Ok(());
        };
        match fees.presets.iter().find(|(l, _)| *l == label) {
            Some((_, rate)) => *rate,
            None => {
                return reply(
                    &bot,
                    chat,
                    ui::render_error(&wallet_core::CoreError::QuoteExpired),
                )
                .await;
            }
        }
    };

    price(&bot, chat, &dialogue, &ctx, user, &what, rate).await
}

/// A typed sat/vB, for mainnet or for anyone who wants an exact rate (§6).
pub async fn receive_custom_fee(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    what: FeeFor,
) -> Result<()> {
    let typed = msg.text().unwrap_or_default().trim().to_string();
    let user = user_of(&msg, &ctx)?;

    let Ok(sat_vb) = typed.parse::<u64>() else {
        // Re-prompt with the floor that applies to *this* job. A bump's floor
        // is the replacement minimum, not the network's, and asking again for
        // "at least 1 sat/vB" when 3 is needed sends the user round the loop.
        let floor = fee_menu(&ctx, user, &what)
            .await
            .map(|f| f.floor)
            .unwrap_or(FeeRate::BROADCAST_MIN);
        bot.send_message(msg.chat.id, ui::ask_custom_fee(floor))
            .await?;
        return Ok(());
    };

    let Some(rate) = FeeRate::from_sat_per_vb(sat_vb) else {
        bot.send_message(msg.chat.id, ui::fee_rate_out_of_range())
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    };

    price(&bot, msg.chat.id, &dialogue, &ctx, user, &what, rate).await
}

/// The fee menu for whatever is being priced.
///
/// A bump's menu is not the network's: it is the network's with everything
/// below the replacement minimum removed, which core works out by asking BDK.
async fn fee_menu(
    ctx: &Ctx,
    user: UserId,
    what: &FeeFor,
) -> std::result::Result<FeeOptions, wallet_core::CoreError> {
    match what {
        FeeFor::Pay { .. } => ctx.core.fee_options().await,
        FeeFor::Bump { txid } => match Txid::from_str(txid) {
            Ok(txid) => ctx.core.bump_fee_options(user, txid).await.map(|b| b.fees),
            // The txid was parsed before it was ever put in the state.
            Err(_) => Err(wallet_core::CoreError::QuoteExpired),
        },
    }
}

/// Price it, whichever kind of thing it is, and show the confirm card.
async fn price(
    bot: &Bot,
    chat: ChatId,
    dialogue: &WalletDialogue,
    ctx: &Ctx,
    user: UserId,
    what: &FeeFor,
    rate: FeeRate,
) -> Result<()> {
    match what {
        FeeFor::Pay { target, amount } => {
            quote_and_show(bot, chat, dialogue, ctx, user, target, *amount, rate).await
        }
        FeeFor::Bump { txid } => bump_and_show(bot, chat, dialogue, ctx, user, txid, rate).await,
    }
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

    let price = ctx.core.price().await;
    let card = bot
        .send_message(
            chat,
            ui::confirm_card(ctx.core.network(), &quote, price.as_ref()),
        )
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

/// Confirm (§8.3 step 4). Always asks for the PIN, and the one handler that
/// collects PINs collects this one too (§8.4).
///
/// State-gated on `SendConfirm`, which matters now that this writes
/// `AwaitPin` unconditionally: without the gate a tap on a card left over from
/// an earlier `/send` would yank someone out of whatever they were typing —
/// including a seed phrase mid-`/restore`.
pub async fn confirm(
    bot: Bot,
    query: CallbackQuery,
    dialogue: WalletDialogue,
    ctx: Ctx,
    (parked, card): (String, Option<i32>),
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

    // Defence in depth: the state and the button must name the same quote.
    // Core would refuse a mismatch anyway, but refusing here keeps the dialogue
    // from being rewritten on behalf of a card nobody is looking at.
    if parked != quote.to_string() {
        return reply(&bot, chat, ui::quote_expired_card(ctx.core.network())).await;
    }

    // No `UserId` is resolved here: this handler no longer reaches core at
    // all. It parks the quote and asks for a PIN, and `receive_pin` resolves
    // the user when it has something to authorise.

    // Take the buttons away before asking for the PIN, so a second tap cannot
    // start a second prompt for a quote that is already being confirmed.
    if let Some(card) = card {
        let _ = bot
            .edit_message_reply_markup(chat, teloxide::types::MessageId(card))
            .await;
    }

    dialogue
        .update(State::AwaitPin {
            pending: PendingAction::Send {
                quote: quote.to_string(),
            },
        })
        .await?;
    reply(&bot, chat, ui::ask_pin()).await
}

/// A tap on a confirm card the dialogue has moved on from.
///
/// Reachable whenever a card outlives its flow — after a cancel, after a PIN
/// lockout, or simply from scrolling up. Silence here would be the same bug
/// `/pj_receive` had, so it answers.
pub async fn confirm_stale(bot: Bot, query: CallbackQuery, ctx: Ctx) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;
    let Some(message) = query.message.clone() else {
        return Ok(());
    };
    reply(
        &bot,
        message.chat().id,
        ui::quote_expired_card(ctx.core.network()),
    )
    .await
}

/// Sign and broadcast. Only ever reached with a PIN the user just typed.
pub async fn broadcast(
    bot: &Bot,
    chat: ChatId,
    dialogue: &WalletDialogue,
    ctx: &Ctx,
    user: UserId,
    quote: QuoteId,
    pin: &Pin,
) -> Result<()> {
    let status = bot.send_message(chat, ui::broadcasting()).await?;

    match ctx.core.confirm_send(user, quote, pin).await {
        Ok(b) => {
            dialogue.exit().await?;
            let price = ctx.core.price().await;
            bot.edit_message_text(
                chat,
                status.id,
                ui::broadcast_done(ctx.core.network(), &b, price.as_ref()),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Err(e) => {
            // An expired quote gets its own card: the user needs to know that
            // nothing was sent, not just that something failed (§8.1).
            let text = match e {
                wallet_core::CoreError::QuoteExpired => ui::quote_expired_card(ctx.core.network()),
                // Now the common path rather than a rare one, so it says what
                // did *not* happen as well as what went wrong.
                wallet_core::CoreError::WrongPin { remaining } => ui::wrong_pin_retry(remaining),
                ref other => ui::render_error(other),
            };
            crate::handlers::keep_or_exit(dialogue, &e).await?;
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

/// `/bumpfee <txid>` (§8.2). The same three screens as `/send`, because a
/// replacement is a payment and deserves the same deliberation.
///
/// It used to pick the rate itself — the fastest preset, or the floor when
/// there were none — and that is why it never worked. On a chain with one
/// flat preset, the fastest rate on offer is the rate the original already
/// paid, BDK refuses anything that does not beat it, and the refusal came
/// back as a generic wallet error. Now core says what the minimum is and the
/// user picks from rates that can actually be accepted.
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

    // This is also where "already confirmed" and "not replaceable" surface,
    // before a card is drawn — rather than on a card whose every button
    // dead-ends.
    let options = match ctx.core.bump_fee_options(user, txid).await {
        Ok(o) => o,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    dialogue
        .update(State::AwaitFeeChoice {
            what: FeeFor::Bump {
                txid: txid.to_string(),
            },
        })
        .await?;

    bot.send_message(msg.chat.id, ui::bump_fee_card(ctx.core.network(), &options))
        .parse_mode(ParseMode::Html)
        .reply_markup(ui::bump_fee_keyboard(&options))
        .await?;

    Ok(())
}

/// Price a replacement and show the confirm card — the same card a send gets,
/// which is what `SendQuote::replaces` is for.
async fn bump_and_show(
    bot: &Bot,
    chat: ChatId,
    dialogue: &WalletDialogue,
    ctx: &Ctx,
    user: UserId,
    txid: &str,
    rate: FeeRate,
) -> Result<()> {
    let Ok(txid) = Txid::from_str(txid) else {
        dialogue.exit().await?;
        return reply(bot, chat, ui::quote_expired_card(ctx.core.network())).await;
    };

    let quote = match ctx.core.bump_fee(user, txid, rate).await {
        Ok(q) => q,

        // "Too low" is answerable: the message says what rate would work, and
        // leaving the dialogue where it is means the user types a number
        // rather than starting again. It is also the backstop for the race
        // between reading the minimum and drafting at it — a descendant
        // arriving in between raises the real minimum.
        Err(e) if is_rate_too_low(&e) => {
            return reply(bot, chat, ui::render_error(&e)).await;
        }

        Err(e) => {
            dialogue.exit().await?;
            return reply(bot, chat, ui::render_error(&e)).await;
        }
    };

    let price = ctx.core.price().await;
    let card = bot
        .send_message(
            chat,
            ui::confirm_card(ctx.core.network(), &quote, price.as_ref()),
        )
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

/// A bump refusal the user can answer by naming a bigger number.
fn is_rate_too_low(e: &wallet_core::CoreError) -> bool {
    use wallet_core::error::FeeBumpRefusal as Refusal;
    matches!(
        e,
        wallet_core::CoreError::CannotBumpFee {
            reason: Refusal::RateTooLow { .. } | Refusal::AbsoluteFeeTooLow { .. }
        }
    )
}

async fn reply(bot: &Bot, chat: ChatId, text: String) -> Result<()> {
    bot.send_message(chat, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}
