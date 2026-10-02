//! `/receive`, `/addresses`, `/balance`, `/history`, `/tx` (PLAN.md §8.2).
//!
//! Pure rendering on top of the facade: parse the argument, call one method,
//! hand the result to `ui`. None of these needs a PIN, because the persisted
//! descriptors are public (§5) — that is the property that makes a wallet
//! usable as a chat without unlocking it every few minutes.

use crate::{Ctx, ui};
use anyhow::Result;
use std::str::FromStr;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, LinkPreviewOptions, ParseMode},
};
use wallet_core::bitcoin::Txid;
use wallet_core::types::{AddressInfo, Page, UserId};

fn user_of(msg: &Message, ctx: &Ctx) -> Result<UserId> {
    let from = msg
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a private message always has a sender"))?;
    #[allow(clippy::cast_possible_wrap)]
    ctx.users.resolve(from.id.0 as i64)
}

/// Telegram's "message is not modified" is the one edit failure that means
/// the screen is already right.
fn is_unmodified(e: &teloxide::RequestError) -> bool {
    e.to_string().contains("message is not modified")
}

/// A history full of mempool.space links would otherwise be a wall of
/// previews.
fn no_preview() -> LinkPreviewOptions {
    LinkPreviewOptions {
        is_disabled: true,
        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}

/// `[page]` arguments are 1-based for a human and 0-based for core.
fn page_arg(raw: &str) -> Page {
    Page::new(
        raw.trim()
            .parse::<u32>()
            .ok()
            .map(|n| n.saturating_sub(1))
            .unwrap_or(0),
    )
}

pub async fn receive(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    let info = match ctx.core.next_address(user).await {
        Ok(info) => info,
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    };

    let caption = ui::receive(ctx.core.network(), &info);

    // A QR is the whole reason this command exists on a phone. If rendering it
    // fails, the address still has to arrive — as text rather than not at all.
    match qr_png(&info) {
        Ok(png) => {
            bot.send_photo(msg.chat.id, InputFile::memory(png))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "QR rendering failed; sending the address as text");
            bot.send_message(msg.chat.id, caption)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

/// Render the BIP21 URI — not the bare address — so a wallet scanning it gets
/// the amount and the payjoin endpoint too when they are there (§7).
fn qr_png(info: &AddressInfo) -> Result<Vec<u8>> {
    use image::{ImageFormat, Luma};
    use qrcode::QrCode;

    let code = QrCode::new(info.bip21.as_bytes())?;
    let image = code.render::<Luma<u8>>().min_dimensions(512, 512).build();

    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, ImageFormat::Png)?;
    Ok(png.into_inner())
}

pub async fn balance(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    let price = ctx.core.price().await;
    match ctx.core.balance(user).await {
        Ok(b) => {
            bot.send_message(
                msg.chat.id,
                ui::balance(ctx.core.network(), &b, price.as_ref()),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(InlineKeyboardMarkup::new([[
                InlineKeyboardButton::callback("🔄 Refresh", "bal:refresh"),
            ]]))
            .await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}

/// The Refresh button of §8.2. Syncs first, then edits the card in place
/// rather than posting another one (§8.1).
pub async fn refresh_balance(bot: Bot, query: CallbackQuery, ctx: Ctx) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;

    if let Err(e) = ctx.core.sync_now(user).await {
        tracing::warn!(error = %e, "refresh sync failed; showing the last known balance");
    }

    let Some(message) = query.message else {
        return Ok(());
    };

    let price = ctx.core.price().await;
    match ctx.core.balance(user).await {
        Ok(b) => {
            let edit = bot
                .edit_message_text(
                    message.chat().id,
                    message.id(),
                    ui::balance(ctx.core.network(), &b, price.as_ref()),
                )
                .parse_mode(ParseMode::Html)
                .reply_markup(InlineKeyboardMarkup::new([[
                    InlineKeyboardButton::callback("🔄 Refresh", "bal:refresh"),
                ]]))
                .await;

            // A refresh that changes nothing produces a byte-identical card,
            // and Telegram rejects that edit. The balance is already correct on
            // screen, so this is success — not something to log as a failure.
            if let Err(e) = edit
                && !is_unmodified(&e)
            {
                return Err(e.into());
            }
        }
        Err(e) => {
            bot.send_message(message.chat().id, ui::render_error(&e))
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

pub async fn addresses(bot: Bot, msg: Message, ctx: Ctx, page: String) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    match ctx.core.addresses(user, page_arg(&page)).await {
        Ok(listing) => {
            bot.send_message(msg.chat.id, ui::addresses(ctx.core.network(), &listing))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}

pub async fn history(bot: Bot, msg: Message, ctx: Ctx, page: String) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    match ctx.core.history(user, page_arg(&page)).await {
        Ok(listing) => {
            bot.send_message(msg.chat.id, ui::history(ctx.core.network(), &listing))
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview())
                .await?;
        }
        Err(e) => return crate::handlers::reply_error(&bot, &msg, &e).await,
    }
    Ok(())
}

pub async fn tx(bot: Bot, msg: Message, ctx: Ctx, txid: String) -> Result<()> {
    let Ok(txid) = Txid::from_str(txid.trim()) else {
        bot.send_message(
            msg.chat.id,
            "That doesn't look like a transaction id. Try <code>/tx &lt;txid&gt;</code>.",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    };

    let user = user_of(&msg, &ctx)?;
    let price = ctx.core.price().await;
    match ctx.core.tx(user, txid).await {
        Ok(detail) => {
            bot.send_message(
                msg.chat.id,
                ui::tx_detail(ctx.core.network(), &detail, price.as_ref()),
            )
            .parse_mode(ParseMode::Html)
            .link_preview_options(no_preview())
            .await?;
        }
        Err(_) => {
            bot.send_message(
                msg.chat.id,
                "I don't know that transaction. /history lists the ones this wallet has.",
            )
            .await?;
        }
    }
    Ok(())
}
