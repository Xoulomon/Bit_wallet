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

pub async fn receive(
    bot: Bot,
    msg: Message,
    dialogue: crate::dialogue::WalletDialogue,
    ctx: Ctx,
    sats: String,
) -> Result<()> {
    let network = ctx.core.network();

    let Ok(value) = sats.trim().parse::<u64>() else {
        bot.send_message(msg.chat.id, ui::payjoin_usage(network))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    };

    let user = user_of(&msg, &ctx)?;

    if !ctx.core.wallet_exists(user)? {
        return crate::handlers::reply_error(&bot, &msg, &wallet_core::CoreError::NoWallet).await;
    }

    // The receiver has to contribute an input and sign the proposal, so this
    // needs the seed. Ask for the PIN the way /send does rather than telling
    // the user to go and /unlock first — the single AwaitPin handler exists so
    // every such command behaves the same (§8.4).
    if ctx.core.session(user).is_none() {
        dialogue
            .update(crate::dialogue::State::AwaitPin {
                pending: crate::dialogue::PendingAction::PayjoinReceive { sats: value },
            })
            .await?;
        bot.send_message(msg.chat.id, ui::ask_pin())
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    open_session(&bot, msg.chat.id, &ctx, user, value).await
}

/// Open the session and send the URI.
///
/// The URI is the entire point of the command, so it goes out as text even if
/// the QR cannot be built — a reply with a picture and no address is worse than
/// one with an address and no picture.
pub async fn open_session(
    bot: &Bot,
    chat: ChatId,
    ctx: &Ctx,
    user: UserId,
    sats: u64,
) -> Result<()> {
    let network = ctx.core.network();
    let amount = Amount::from_sat(sats);

    let receipt = match ctx.core.payjoin_receive(user, amount).await {
        Ok(r) => r,
        Err(e) => {
            // A setup failure here is usually the directory or the relay being
            // unreachable, and "couldn't be completed" gives the operator
            // nothing to act on.
            tracing::warn!(error = %e, "payjoin receive could not start");
            bot.send_message(chat, ui::payjoin_unavailable(&e))
                .parse_mode(ParseMode::Html)
                .await?;
            return Ok(());
        }
    };

    let caption = ui::payjoin_receipt(network, amount, &receipt);

    match qr(&receipt.bip21) {
        Ok(png) => {
            let sent = bot
                .send_photo(chat, InputFile::memory(png))
                .caption(caption.clone())
                .parse_mode(ParseMode::Html)
                .await;

            // Telegram can refuse a photo for reasons that have nothing to do
            // with the URI — caption length, media limits, a blocked upload.
            // The URI must not go down with it.
            if let Err(e) = sent {
                tracing::warn!(error = %e, "photo failed; sending the URI as text");
                bot.send_message(chat, caption)
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "QR rendering failed; sending the URI as text");
            bot.send_message(chat, caption)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A real BIP77 v2 URI, from the end-to-end test. Far longer than an
    /// address and mixed-case, because the payjoin crate uppercases the `pj=`
    /// parameter for QR efficiency.
    const REAL_URI: &str = "bitcoin:bcrt1qmg5q0r0wup7vqjdceax67738epkltzg278yjx8?amount=0.00025&pjos=0&pj=HTTPS://PAYJO.IN/8PJCA4X6VZTSC%23EX17V4MU6S-OH1QYPNNJTQFK00ZQ99VKKNGF6M90PKRRPYY6QJHDNC0WLHWR9V74X4P9C-RK1QGDFG9AC3XN7NVDMUH06SMDNQWJHG292DXNN9LPDNEACHTFH9ZTZC";

    /// `/pj_receive` is useless without the URI, so the one thing that must
    /// never fail is producing it. A QR that cannot encode would otherwise take
    /// the whole reply down.
    #[test]
    fn a_real_payjoin_uri_encodes_as_a_qr() {
        let png = qr(REAL_URI).expect("a payjoin URI must encode as a QR");
        assert!(png.len() > 100, "the PNG has content");
        assert_eq!(&png[1..4], b"PNG", "it really is a PNG");
    }

    /// Telegram caps a photo caption at 1024 characters and rejects the whole
    /// send if it is longer — which would lose the URI along with it.
    #[test]
    fn the_caption_fits_telegrams_photo_limit_on_both_networks() {
        use wallet_core::bitcoin::{Amount, Network};
        use wallet_core::types::{PayjoinReceipt, SessionId};

        let receipt = PayjoinReceipt {
            session_id: SessionId::new(),
            bip21: REAL_URI.into(),
            expires_at: std::time::SystemTime::now(),
        };

        for network in [Network::Regtest, Network::Bitcoin] {
            let caption = crate::ui::payjoin_receipt(network, Amount::from_sat(25_000), &receipt);
            assert!(
                caption.chars().count() <= 1024,
                "{network} caption is {} chars, over Telegram's photo limit",
                caption.chars().count()
            );
            // And the URI itself survives, escaped but recoverable.
            assert!(
                caption.contains("pj=") || caption.contains("pj%3D"),
                "the caption must carry the payjoin endpoint"
            );
        }
    }

    /// The URI contains `&` and `%`, and an unescaped `&` makes Telegram reject
    /// the message as malformed HTML — losing the URI for a punctuation bug.
    #[test]
    fn the_uri_is_html_escaped_in_the_caption() {
        use wallet_core::bitcoin::{Amount, Network};
        use wallet_core::types::{PayjoinReceipt, SessionId};

        let receipt = PayjoinReceipt {
            session_id: SessionId::new(),
            bip21: REAL_URI.into(),
            expires_at: std::time::SystemTime::now(),
        };
        let caption =
            crate::ui::payjoin_receipt(Network::Regtest, Amount::from_sat(25_000), &receipt);

        assert!(caption.contains("&amp;"), "ampersands must be escaped");
        // No bare `&` that is not the start of an entity.
        for (i, _) in caption.match_indices('&') {
            let tail = &caption[i..];
            assert!(
                tail.starts_with("&amp;") || tail.starts_with("&lt;") || tail.starts_with("&gt;"),
                "bare ampersand at {i} would make Telegram reject the HTML"
            );
        }
    }
}
