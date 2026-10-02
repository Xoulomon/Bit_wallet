//! Command handlers (PLAN.md §8).
//!
//! Every handler reads as *parse → call core → render*. A handler that contains
//! bitcoin logic is a bug in the layering, not a shortcut.

pub mod admin;
pub mod onchain;
pub mod payjoin;
pub mod send;
pub mod start;
pub mod wallet;

use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode};

/// One place turns a `CoreError` into a reply, so no handler has to remember
/// the parse mode or the rendering rule (§8.1).
pub async fn reply_error(bot: &Bot, msg: &Message, e: &wallet_core::CoreError) -> Result<()> {
    // Log it too. An error the user sees but the operator cannot find is one
    // nobody can diagnose — which is how a /bumpfee refusal spent a day
    // looking like a server fault.
    tracing::warn!(
        command = msg.text().unwrap_or_default(),
        error = %e,
        "replied with an error"
    );
    bot.send_message(msg.chat.id, crate::ui::render_error(e))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}
