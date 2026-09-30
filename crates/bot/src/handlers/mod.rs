//! Command handlers (PLAN.md §8).
//!
//! Every handler reads as *parse → call core → render*. A handler that contains
//! bitcoin logic is a bug in the layering, not a shortcut.

pub mod admin;
pub mod onchain;
pub mod send;
pub mod start;
pub mod wallet;

use anyhow::Result;
use teloxide::{prelude::*, types::ParseMode};

/// One place turns a `CoreError` into a reply, so no handler has to remember
/// the parse mode or the rendering rule (§8.1).
pub async fn reply_error(bot: &Bot, msg: &Message, e: &wallet_core::CoreError) -> Result<()> {
    bot.send_message(msg.chat.id, crate::ui::render_error(e))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}
