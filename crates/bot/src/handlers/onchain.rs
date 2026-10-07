//! `/receive`, `/addresses`, `/balance`, `/history`, `/tx` (PLAN.md §8.2).
//!
//! Pure rendering on top of the facade: parse the argument, call one method,
//! hand the result to `ui`. None of these needs a PIN, because the persisted
//! descriptors are public (§5) — that is the property that makes a wallet
//! usable as a chat without unlocking it every few minutes.
//!
//! Each command is a thin entry point over a `show_*` taking a `ChatId`, so the
//! inline menu (§8.5) runs the command itself rather than a copy of it. The
//! page argument becomes a `Page` at the boundary, which is what lets a button
//! ask for the first page without parsing a string it never had.

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
    show_receive(&bot, msg.chat.id, &ctx, user).await
}

pub async fn show_receive(bot: &Bot, chat: ChatId, ctx: &Ctx, user: UserId) -> Result<()> {
    let info = match ctx.core.next_address(user).await {
        Ok(info) => info,
        Err(e) => return crate::handlers::reply_error_at(bot, chat, "/receive", &e).await,
    };

    let caption = ui::receive(ctx.core.network(), &info, ctx.core.capabilities().mempool);

    // A QR is the whole reason this command exists on a phone. If rendering it
    // fails, the address still has to arrive — as text rather than not at all.
    match qr_png(&info) {
        Ok(png) => {
            bot.send_photo(chat, InputFile::memory(png))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "QR rendering failed; sending the address as text");
            bot.send_message(chat, caption)
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
    show_balance(&bot, msg.chat.id, &ctx, user).await
}

pub async fn show_balance(bot: &Bot, chat: ChatId, ctx: &Ctx, user: UserId) -> Result<()> {
    let price = ctx.core.price().await;
    match ctx.core.balance(user).await {
        Ok(b) => {
            bot.send_message(chat, ui::balance(ctx.core.network(), &b, price.as_ref()))
                .parse_mode(ParseMode::Html)
                .reply_markup(refresh_keyboard())
                .await?;
        }
        Err(e) => return crate::handlers::reply_error_at(bot, chat, "/balance", &e).await,
    }
    Ok(())
}

/// The one button on a balance card. Written once, because the refresh handler
/// has to redraw the identical keyboard or the card loses it on the first tap.
fn refresh_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[InlineKeyboardButton::callback("🔄 Refresh", "bal:refresh")]])
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
                .reply_markup(refresh_keyboard())
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
    show_addresses(&bot, msg.chat.id, &ctx, user, page_arg(&page)).await
}

pub async fn show_addresses(
    bot: &Bot,
    chat: ChatId,
    ctx: &Ctx,
    user: UserId,
    page: Page,
) -> Result<()> {
    match ctx.core.addresses(user, page).await {
        Ok(listing) => {
            bot.send_message(chat, ui::addresses(ctx.core.network(), &listing))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => return crate::handlers::reply_error_at(bot, chat, "/addresses", &e).await,
    }
    Ok(())
}

pub async fn history(bot: Bot, msg: Message, ctx: Ctx, page: String) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    show_history(&bot, msg.chat.id, &ctx, user, page_arg(&page)).await
}

pub async fn show_history(
    bot: &Bot,
    chat: ChatId,
    ctx: &Ctx,
    user: UserId,
    page: Page,
) -> Result<()> {
    match ctx.core.history(user, page).await {
        Ok(listing) => {
            let mut card = bot
                .send_message(chat, ui::history(ctx.core.network(), &listing))
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview());

            // Only where there is somewhere to go. A single page gets no row.
            if let Some(keyboard) = ui::history_keyboard(&listing) {
                card = card.reply_markup(keyboard);
            }
            card.await?;
        }
        Err(e) => return crate::handlers::reply_error_at(bot, chat, "/history", &e).await,
    }
    Ok(())
}

/// The Previous and Next buttons of `/history` (§8.2).
///
/// Edits the card in place rather than posting another one (§8.1): paging
/// through six screens of history should leave one message behind, not six.
pub async fn turn_history_page(bot: Bot, query: CallbackQuery, ctx: Ctx) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;

    let Some(message) = query.message.as_ref() else {
        return Ok(());
    };
    let Some(index) = query.data.as_deref().and_then(ui::history_page_from_data) else {
        tracing::warn!(data = ?query.data, "a history button carried no page number");
        return Ok(());
    };

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;
    let chat = message.chat().id;

    let listing = match ctx.core.history(user, Page::new(index)).await {
        Ok(listing) => listing,
        Err(e) => return crate::handlers::reply_error_at(&bot, chat, "/history", &e).await,
    };

    let edit = bot
        .edit_message_text(
            chat,
            message.id(),
            ui::history(ctx.core.network(), &listing),
        )
        .parse_mode(ParseMode::Html)
        .link_preview_options(no_preview())
        // Always, even when it is empty. An edit that omits the markup leaves
        // the old one in place, so the last page would keep a Next button
        // pointing past the end of the history.
        .reply_markup(ui::history_keyboard(&listing).unwrap_or_default())
        .await;

    // The same card again — a tap on a page the screen is already showing.
    // Nothing is wrong, so nothing is reported.
    if let Err(e) = edit
        && !is_unmodified(&e)
    {
        return Err(e.into());
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
