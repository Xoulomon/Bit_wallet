//! `/pj_receive` and `/pj_sessions` (PLAN.md §7, §8.2).
//!
//! Core runs the protocol; this file asks it to start, renders the URI it hands
//! back, and lists what is in flight. Nothing here knows what a typestate is.

use crate::{Ctx, ui};
use anyhow::Result;
use std::str::FromStr;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode},
};
use wallet_core::bitcoin::Amount;
use wallet_core::types::{SessionId, UserId};

fn user_of(msg: &Message, ctx: &Ctx) -> Result<UserId> {
    let from = msg
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a private message always has a sender"))?;
    #[allow(clippy::cast_possible_wrap)]
    ctx.users.resolve(from.id.0 as i64)
}

pub async fn receive(bot: Bot, msg: Message, ctx: Ctx, sats: String) -> Result<()> {
    let network = ctx.core.network();

    let Ok(value) = sats.trim().parse::<u64>() else {
        bot.send_message(msg.chat.id, ui::payjoin_usage(network))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    };

    let user = user_of(&msg, &ctx)?;
    let amount = Amount::from_sat(value);

    let receipt = match ctx.core.payjoin_receive(user, amount).await {
        Ok(r) => r,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    let caption = ui::payjoin_receipt(network, amount, &receipt);

    match qr(&receipt.bip21) {
        Ok(png) => {
            bot.send_photo(msg.chat.id, InputFile::memory(png))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "QR rendering failed; sending the URI as text");
            bot.send_message(msg.chat.id, caption)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

pub async fn sessions(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;

    match ctx.core.payjoin_sessions(user).await {
        Ok(list) => {
            let keyboard: Vec<Vec<InlineKeyboardButton>> = list
                .iter()
                .filter(|s| {
                    matches!(
                        s.state,
                        wallet_core::types::PayjoinState::Waiting
                            | wallet_core::types::PayjoinState::ProposalReceived
                    )
                })
                .map(|s| {
                    vec![InlineKeyboardButton::callback(
                        format!("✖ Cancel {}", ui::shorten(&s.id.to_string())),
                        format!("pj:cancel:{}", s.id),
                    )]
                })
                .collect();

            let mut message = bot
                .send_message(msg.chat.id, ui::payjoin_sessions(ctx.core.network(), &list))
                .parse_mode(ParseMode::Html);

            if !keyboard.is_empty() {
                message = message.reply_markup(InlineKeyboardMarkup::new(keyboard));
            }
            message.await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}

pub async fn cancel(bot: Bot, query: CallbackQuery, ctx: Ctx) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;

    let Some(message) = query.message.clone() else {
        return Ok(());
    };
    let Some(raw) = query
        .data
        .as_deref()
        .map(|d| d.trim_start_matches("pj:cancel:"))
    else {
        return Ok(());
    };
    let Ok(session) = SessionId::from_str(raw) else {
        return Ok(());
    };

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;

    match ctx.core.payjoin_cancel(user, session).await {
        Ok(()) => {
            bot.send_message(message.chat().id, ui::payjoin_cancelled())
                .await?;
        }
        Err(e) => {
            bot.send_message(message.chat().id, ui::render_error(&e))
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

fn qr(data: &str) -> Result<Vec<u8>> {
    use image::{ImageFormat, Luma};
    use qrcode::QrCode;

    let code = QrCode::new(data.as_bytes())?;
    let image = code.render::<Luma<u8>>().min_dimensions(512, 512).build();
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, ImageFormat::Png)?;
    Ok(png.into_inner())
}
