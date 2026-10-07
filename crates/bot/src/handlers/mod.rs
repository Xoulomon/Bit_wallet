//! Command handlers (PLAN.md §8).
//!
//! Every handler reads as *parse → call core → render*. A handler that contains
//! bitcoin logic is a bug in the layering, not a shortcut.

pub mod admin;
pub mod menu;
pub mod onchain;
pub mod payjoin;
pub mod send;
pub mod start;
pub mod wallet;

use crate::Ctx;
use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode};

/// The Telegram id → the opaque `UserId` core understands (§3a rule 3).
///
/// Infallible in practice: §8.7's guard rejects an authorless update before any
/// handler runs, so by the time one of these is called `from` is always there.
/// The error arm is here because "always" is a property of the dispatcher, not
/// of the type.
pub fn user_of(msg: &Message, ctx: &Ctx) -> Result<wallet_core::types::UserId> {
    let from = msg
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a private message always has a sender"))?;
    #[allow(clippy::cast_possible_wrap)]
    ctx.users.resolve(from.id.0 as i64)
}

/// One place turns a `CoreError` into a reply, so no handler has to remember
/// the parse mode or the rendering rule (§8.1).
pub async fn reply_error(bot: &Bot, msg: &Message, e: &wallet_core::CoreError) -> Result<()> {
    reply_error_at(bot, msg.chat.id, msg.text().unwrap_or_default(), e).await
}

/// The same, where the action came from a tap rather than a message.
///
/// A button carries no text to quote, so the caller names the command it was
/// standing in for. That keeps the log field meaningful either way: an error
/// the user sees but the operator cannot find is one nobody can diagnose —
/// which is how a /bumpfee refusal spent a day looking like a server fault.
pub async fn reply_error_at(
    bot: &Bot,
    chat: ChatId,
    command: &str,
    e: &wallet_core::CoreError,
) -> Result<()> {
    tracing::warn!(command, error = %e, "replied with an error");
    bot.send_message(chat, crate::ui::render_error(e))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// Whether a failed PIN attempt is worth repeating.
///
/// A wrong PIN is: the user mistyped and the next try may work. A lockout is
/// not — the whole point of a lockout is that trying again cannot help — and
/// neither is an expired quote, a missing wallet or a backend fault.
///
/// This exists as one named predicate because the rule was written out twice,
/// in `wallet::retry_or_fail` and in `send::broadcast`. Two copies of a policy
/// drift the moment someone decides a second error is retryable and only finds
/// one of them. Everything around the two call sites genuinely differs — one
/// sends a message, the other edits a status card in place — but *this* does
/// not, so this is the part that is shared.
pub fn is_worth_retrying(e: &wallet_core::CoreError) -> bool {
    matches!(e, wallet_core::CoreError::WrongPin { .. })
}

/// Apply [`is_worth_retrying`] to the dialogue: stay put, or end the flow.
pub async fn keep_or_exit(
    dialogue: &crate::dialogue::WalletDialogue,
    e: &wallet_core::CoreError,
) -> Result<()> {
    if !is_worth_retrying(e) {
        dialogue.exit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wallet_core::CoreError;

    /// The cases spelled out, so a change to the rule is a change to this
    /// list rather than something that happens in one handler by accident.
    #[test]
    fn only_a_wrong_pin_keeps_the_user_in_the_flow() {
        assert!(is_worth_retrying(&CoreError::WrongPin { remaining: 3 }));

        for ended in [
            CoreError::PinLocked {
                until: std::time::SystemTime::now(),
            },
            CoreError::QuoteExpired,
            CoreError::NoWallet,
            CoreError::Locked,
            CoreError::BroadcastRejected {
                reason: "min relay fee not met".into(),
            },
        ] {
            assert!(
                !is_worth_retrying(&ended),
                "{ended} cannot be fixed by typing the PIN again"
            );
        }
    }
}
