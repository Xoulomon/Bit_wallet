//! Wallet lifecycle: create, restore, unlock, lock, export, delete (§5, §8.2).
//!
//! Every handler here is *parse → call core → render*. The PIN and the seed
//! phrase pass through as `String`s the moment they arrive and are forwarded
//! into core; the messages carrying them are deleted from the chat first.
//!
//! **One ordering note.** §8.2 draws `/create` as mnemonic → quiz → PIN, but
//! §3a fixes the facade as `create_wallet(u, pin) -> NewWallet`, which needs
//! the PIN before a mnemonic exists. Rather than give core a second, unsealed
//! state to hold between two calls, the PIN is collected first and the mnemonic
//! and quiz follow. The user sees the same steps; only their order differs, and
//! the seed is never unsealed anywhere but inside one call.

use crate::{
    Ctx,
    dialogue::{Intent, PendingAction, State, WalletDialogue},
    ui,
};
use anyhow::Result;
use std::time::Duration;
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::types::{Auth, Pin, UserId};

/// §8.1: a mnemonic lives on screen for a minute and then removes itself.
const MNEMONIC_TTL: Duration = Duration::from_secs(60);

/// Resolve the Telegram id to the opaque `UserId` core understands (§3a rule 3).
fn user_of(msg: &Message, ctx: &Ctx) -> Result<UserId> {
    let from = msg
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a private message always has a sender"))?;
    #[allow(clippy::cast_possible_wrap)]
    ctx.users.resolve(from.id.0 as i64)
}

/// Delete a message the user must not leave lying in the chat (§8.1).
///
/// Best effort: Telegram refuses after 48 hours and in a few other cases, and a
/// failure here must not abort the flow the user is in the middle of.
async fn scrub(bot: &Bot, msg: &Message) {
    if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
        tracing::warn!(error = %e, "could not delete a message carrying a secret");
    }
}

pub async fn create(bot: Bot, msg: Message, dialogue: WalletDialogue, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;

    if ctx.core.wallet_exists(user)? {
        bot.send_message(
            msg.chat.id,
            ui::render_error(&wallet_core::CoreError::WalletExists),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    dialogue
        .update(State::SetPin {
            intent: Intent::Create,
        })
        .await?;

    bot.send_message(msg.chat.id, ui::ask_pin_new())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

pub async fn restore(bot: Bot, msg: Message, dialogue: WalletDialogue, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;

    if ctx.core.wallet_exists(user)? {
        bot.send_message(
            msg.chat.id,
            ui::render_error(&wallet_core::CoreError::WalletExists),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    dialogue.update(State::RestoreMnemonic).await?;
    bot.send_message(msg.chat.id, ui::ask_mnemonic())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// The seed phrase arrives. Delete first, validate second (§8.1).
pub async fn receive_mnemonic(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
) -> Result<()> {
    let Some(text) = msg.text().map(str::to_owned) else {
        bot.send_message(msg.chat.id, "Send your seed phrase as text.")
            .await?;
        return Ok(());
    };
    scrub(&bot, &msg).await;

    let user = user_of(&msg, &ctx)?;
    let _ = user;

    // A preflight with no birthday tells the user what a full scan would cost
    // before they decide whether they know a better one.
    dialogue
        .update(State::RestoreBirthday { words: text })
        .await?;
    bot.send_message(msg.chat.id, ui::ask_birthday())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

pub async fn receive_birthday(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    words: String,
) -> Result<()> {
    let raw = msg.text().unwrap_or_default().trim().to_string();
    let birthday = if raw.eq_ignore_ascii_case("skip") || raw.is_empty() {
        None
    } else {
        match raw.parse::<u32>() {
            Ok(h) => Some(h),
            Err(_) => {
                bot.send_message(msg.chat.id, ui::ask_birthday()).await?;
                return Ok(());
            }
        }
    };

    let plan = ctx.core.restore_preflight(birthday).await?;
    let card = bot
        .send_message(msg.chat.id, ui::restore_plan(ctx.core.network(), &plan))
        .parse_mode(ParseMode::Html)
        .await?;

    // A refusal is the end of the flow, not a step in it (§6).
    if matches!(
        plan.verdict,
        wallet_core::types::RestoreVerdict::Refuse { .. }
    ) {
        dialogue.exit().await?;
        return Ok(());
    }

    dialogue
        .update(State::SetPin {
            intent: Intent::Restore { words, birthday },
        })
        .await?;
    let _ = card;

    bot.send_message(msg.chat.id, ui::ask_pin_new())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// The first half of setting a PIN. Deleted on receipt, then echoed back only
/// as a request to repeat it.
pub async fn receive_new_pin(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    intent: Intent,
) -> Result<()> {
    let typed = msg.text().unwrap_or_default().trim().to_string();
    scrub(&bot, &msg).await;

    if !Pin::new(typed.clone()).is_well_formed() {
        bot.send_message(
            msg.chat.id,
            ui::render_error(&wallet_core::CoreError::InvalidPin { min: 6, max: 8 }),
        )
        .await?;
        return Ok(());
    }

    dialogue
        .update(State::ConfirmPin {
            intent,
            first: typed,
        })
        .await?;
    bot.send_message(msg.chat.id, ui::ask_pin_again())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

pub async fn receive_pin_confirmation(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    (intent, first): (Intent, String),
) -> Result<()> {
    let typed = msg.text().unwrap_or_default().trim().to_string();
    scrub(&bot, &msg).await;

    if typed != first {
        dialogue.update(State::SetPin { intent }).await?;
        bot.send_message(msg.chat.id, ui::pin_mismatch())
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let user = user_of(&msg, &ctx)?;
    let pin = Pin::new(typed);

    match intent {
        Intent::Create => {
            let new_wallet = match ctx.core.create_wallet(user, &pin).await {
                Ok(w) => w,
                Err(e) => return fail(&bot, &msg, &dialogue, &e).await,
            };

            // Shown once, then removed by us rather than left to the user (§5).
            let shown = bot
                .send_message(msg.chat.id, ui::mnemonic_card(&new_wallet.mnemonic))
                .parse_mode(ParseMode::Html)
                .await?;
            schedule_deletion(bot.clone(), shown.chat.id, shown.id);

            dialogue
                .update(State::CreateConfirmWords {
                    challenge: new_wallet.confirm_challenge,
                    answered: Vec::new(),
                })
                .await?;

            bot.send_message(
                msg.chat.id,
                ui::ask_backup_word(new_wallet.confirm_challenge[0], 1),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Intent::Restore { words, birthday } => {
            let words = zeroize::Zeroizing::new(words);
            match ctx.core.restore_wallet(user, words, birthday, &pin).await {
                Ok(()) => {
                    dialogue.exit().await?;
                    bot.send_message(msg.chat.id, ui::restored(ctx.core.network()))
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
                Err(e) => return fail(&bot, &msg, &dialogue, &e).await,
            }
        }
    }
    Ok(())
}

/// The three-word quiz of §5. Core holds the challenge and checks the answers.
pub async fn receive_backup_word(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    (challenge, mut answered): ([u8; 3], Vec<String>),
) -> Result<()> {
    answered.push(msg.text().unwrap_or_default().trim().to_string());

    if answered.len() < 3 {
        let next = answered.len();
        dialogue
            .update(State::CreateConfirmWords {
                challenge,
                answered,
            })
            .await?;
        bot.send_message(msg.chat.id, ui::ask_backup_word(challenge[next], next + 1))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let user = user_of(&msg, &ctx)?;
    let answers: [String; 3] = [
        answered[0].clone(),
        answered[1].clone(),
        answered[2].clone(),
    ];

    match ctx.core.confirm_backup(user, answers).await {
        Ok(()) => {
            dialogue.exit().await?;
            bot.send_message(msg.chat.id, ui::wallet_ready(ctx.core.network()))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Err(e) => {
            // Wrong words are not fatal: the wallet exists and the words are
            // still on the user's paper. Start the quiz again.
            dialogue
                .update(State::CreateConfirmWords {
                    challenge,
                    answered: Vec::new(),
                })
                .await?;
            bot.send_message(msg.chat.id, ui::render_error(&e))
                .parse_mode(ParseMode::Html)
                .await?;
            bot.send_message(msg.chat.id, ui::ask_backup_word(challenge[0], 1))
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

pub async fn unlock(bot: Bot, msg: Message, dialogue: WalletDialogue, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    if !ctx.core.wallet_exists(user)? {
        return reply_error(&bot, &msg, &wallet_core::CoreError::NoWallet).await;
    }
    ask_pin_for(bot, msg, dialogue, PendingAction::Unlock).await
}

pub async fn lock(bot: Bot, msg: Message, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    ctx.core.lock(user);
    bot.send_message(msg.chat.id, ui::locked()).await?;
    Ok(())
}

pub async fn export(bot: Bot, msg: Message, dialogue: WalletDialogue, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    if !ctx.core.wallet_exists(user)? {
        return reply_error(&bot, &msg, &wallet_core::CoreError::NoWallet).await;
    }
    ask_pin_for(bot, msg, dialogue, PendingAction::Export).await
}

/// §8.1: a destructive action needs a typed word, not just a button.
pub async fn delete(bot: Bot, msg: Message, dialogue: WalletDialogue, ctx: Ctx) -> Result<()> {
    let user = user_of(&msg, &ctx)?;
    if !ctx.core.wallet_exists(user)? {
        return reply_error(&bot, &msg, &wallet_core::CoreError::NoWallet).await;
    }
    dialogue.update(State::DeleteTypeConfirm).await?;
    bot.send_message(msg.chat.id, ui::ask_delete_word())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

pub async fn receive_delete_word(bot: Bot, msg: Message, dialogue: WalletDialogue) -> Result<()> {
    if msg.text().unwrap_or_default().trim() != "DELETE" {
        dialogue.exit().await?;
        bot.send_message(msg.chat.id, ui::delete_cancelled())
            .await?;
        return Ok(());
    }
    ask_pin_for(bot, msg, dialogue, PendingAction::Delete).await
}

async fn ask_pin_for(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    pending: PendingAction,
) -> Result<()> {
    dialogue.update(State::AwaitPin { pending }).await?;
    bot.send_message(msg.chat.id, ui::ask_pin())
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// The single place a PIN is collected for an existing wallet (§8.4).
pub async fn receive_pin(
    bot: Bot,
    msg: Message,
    dialogue: WalletDialogue,
    ctx: Ctx,
    pending: PendingAction,
) -> Result<()> {
    let typed = msg.text().unwrap_or_default().trim().to_string();
    scrub(&bot, &msg).await;

    let user = user_of(&msg, &ctx)?;
    let pin = Pin::new(typed);

    match pending {
        PendingAction::Unlock => match ctx.core.unlock(user, &pin).await {
            Ok(info) => {
                dialogue.exit().await?;
                bot.send_message(msg.chat.id, ui::unlocked(info.remaining))
                    .await?;
            }
            Err(e) => return retry_or_fail(&bot, &msg, &dialogue, &e).await,
        },

        PendingAction::Export => match ctx.core.export_mnemonic(user, &pin).await {
            Ok(words) => {
                dialogue.exit().await?;
                let shown = bot
                    .send_message(msg.chat.id, ui::mnemonic_card(&words))
                    .parse_mode(ParseMode::Html)
                    .await?;
                schedule_deletion(bot.clone(), shown.chat.id, shown.id);
            }
            Err(e) => return retry_or_fail(&bot, &msg, &dialogue, &e).await,
        },

        PendingAction::Delete => match ctx.core.delete_wallet(user, &pin).await {
            Ok(()) => {
                dialogue.exit().await?;
                bot.send_message(msg.chat.id, ui::deleted())
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            Err(e) => return retry_or_fail(&bot, &msg, &dialogue, &e).await,
        },

        PendingAction::Send { quote } => {
            // Step 5 signs and broadcasts here; the PIN plumbing is already in
            // place, which is the point of collecting it in one handler.
            let _ = (quote, Auth::Pin(pin));
            dialogue.exit().await?;
            bot.send_message(msg.chat.id, "Sending is implemented in Step 5.")
                .await?;
        }
    }
    Ok(())
}

/// A wrong PIN leaves the user in `AwaitPin` so they can simply try again; a
/// lockout or a missing wallet ends the flow, because retrying cannot help.
async fn retry_or_fail(
    bot: &Bot,
    msg: &Message,
    dialogue: &WalletDialogue,
    e: &wallet_core::CoreError,
) -> Result<()> {
    if !matches!(e, wallet_core::CoreError::WrongPin { .. }) {
        dialogue.exit().await?;
    }
    bot.send_message(msg.chat.id, ui::render_error(e))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

async fn fail(
    bot: &Bot,
    msg: &Message,
    dialogue: &WalletDialogue,
    e: &wallet_core::CoreError,
) -> Result<()> {
    dialogue.exit().await?;
    reply_error(bot, msg, e).await
}

async fn reply_error(bot: &Bot, msg: &Message, e: &wallet_core::CoreError) -> Result<()> {
    bot.send_message(msg.chat.id, ui::render_error(e))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// §8.1: the mnemonic removes itself after a minute, whether or not the user
/// acts. Spawned rather than awaited, so the flow continues meanwhile.
fn schedule_deletion(bot: Bot, chat: teloxide::types::ChatId, message: teloxide::types::MessageId) {
    tokio::spawn(async move {
        tokio::time::sleep(MNEMONIC_TTL).await;
        if let Err(e) = bot.delete_message(chat, message).await {
            tracing::warn!(error = %e, "a self-deleting mnemonic outlived its message");
        }
    });
}
