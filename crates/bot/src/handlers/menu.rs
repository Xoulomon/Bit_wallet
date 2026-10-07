//! The inline menu (PLAN.md §8.2, §8.5).
//!
//! One endpoint behind every button on the `/start` and `/help` cards. It
//! reimplements nothing: each arm calls the very function the typed command
//! calls, so a tap and the command cannot drift apart — there is no second copy
//! of a handler to forget about when one of them changes.
//!
//! The split that makes that possible lives in the command modules: each one
//! has a `show_*`/`begin_*` taking a `ChatId` and a `UserId`, with the command
//! handler as a two-line entry point over it. A tap has no `Message` of its
//! own, and that is the only difference between the two routes.
//!
//! What a button may stand for is decided in [`crate::ui::MenuCommand`], and
//! the reasons some commands are absent are written there.

use crate::{
    Ctx,
    dialogue::WalletDialogue,
    handlers,
    ui::{MENU_PREFIX, MenuCommand},
};
use anyhow::Result;
use teloxide::prelude::*;
use wallet_core::types::Page;

/// The first page, which is what a button asks for: `/history` and `/addresses`
/// take a page number and a button carries no argument, so it gets the same
/// page a bare command does.
fn first_page() -> Page {
    Page::new(0)
}

pub async fn tap(bot: Bot, query: CallbackQuery, dialogue: WalletDialogue, ctx: Ctx) -> Result<()> {
    // Answer first, and whatever happens next. An unanswered callback leaves
    // the button spinning for a minute, which reads as a bot that has died.
    bot.answer_callback_query(query.id.clone()).await?;

    // No message means the card is older than Telegram keeps, and there is no
    // chat to answer in. Nothing to do but let the tap go.
    let Some(message) = query.message.as_ref() else {
        return Ok(());
    };
    let chat = message.chat().id;

    let Some(command) = parse(query.data.as_deref()) else {
        // A card from an older build can outlive the slug it carries. Say so in
        // the log rather than in the chat: the user tapped a button that is no
        // longer a command, and /start draws them a current one.
        tracing::warn!(data = ?query.data, "a menu button carried an unknown command");
        return Ok(());
    };

    #[allow(clippy::cast_possible_wrap)]
    let user = ctx.users.resolve(query.from.id.0 as i64)?;

    tracing::debug!(command = command.slug(), "menu button");

    match command {
        MenuCommand::Create => {
            handlers::wallet::begin_create(&bot, chat, &dialogue, &ctx, user).await
        }
        MenuCommand::Restore => {
            handlers::wallet::begin_restore(&bot, chat, &dialogue, &ctx, user).await
        }
        MenuCommand::Unlock => {
            handlers::wallet::begin_unlock(&bot, chat, &dialogue, &ctx, user).await
        }
        MenuCommand::Lock => handlers::wallet::do_lock(&bot, chat, &ctx, user).await,
        MenuCommand::Balance => handlers::onchain::show_balance(&bot, chat, &ctx, user).await,
        MenuCommand::Receive => handlers::onchain::show_receive(&bot, chat, &ctx, user).await,
        MenuCommand::Send => handlers::send::show_usage(&bot, chat, &ctx).await,
        MenuCommand::History => {
            handlers::onchain::show_history(&bot, chat, &ctx, user, first_page()).await
        }
        MenuCommand::Addresses => {
            handlers::onchain::show_addresses(&bot, chat, &ctx, user, first_page()).await
        }
        MenuCommand::PjSessions => handlers::payjoin::show_sessions(&bot, chat, &ctx, user).await,
        MenuCommand::Faucet => {
            handlers::admin::run_faucet(&bot, chat, &ctx, user, handlers::admin::default_handout())
                .await
        }
        MenuCommand::Status => handlers::start::show_status(&bot, chat, &ctx, user).await,
        MenuCommand::Network => handlers::start::show_network(&bot, chat, &ctx).await,
        MenuCommand::Help => handlers::start::show_help(&bot, chat, &ctx, user).await,
    }
}

/// `cmd:<slug>:-` → the command, or `None` for anything else.
///
/// The argument field is read and discarded rather than ignored outright: §8.5
/// fixes the shape as `action:subject:arg`, and taking only the subject is what
/// keeps a future `cmd:history:3` from being mistaken for `cmd:history`.
fn parse(data: Option<&str>) -> Option<MenuCommand> {
    data?
        .strip_prefix(MENU_PREFIX)
        .and_then(|rest| rest.split(':').next())
        .and_then(MenuCommand::from_slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip the dispatcher depends on: what `ui` draws is what this
    /// module routes. A mismatch is a button that answers with silence.
    #[test]
    fn every_button_this_build_draws_parses_back_to_its_command() {
        for command in [
            MenuCommand::Create,
            MenuCommand::Restore,
            MenuCommand::Unlock,
            MenuCommand::Lock,
            MenuCommand::Balance,
            MenuCommand::Receive,
            MenuCommand::Send,
            MenuCommand::History,
            MenuCommand::Addresses,
            MenuCommand::PjSessions,
            MenuCommand::Faucet,
            MenuCommand::Status,
            MenuCommand::Network,
            MenuCommand::Help,
        ] {
            let data = command.callback_data();
            assert_eq!(
                parse(Some(&data)),
                Some(command),
                "`{data}` is drawn on a button but routes to nothing"
            );
        }
    }

    /// Everything that is not one of our buttons, so this endpoint cannot
    /// swallow a tap that belongs to the send flow or to payjoin.
    #[test]
    fn nothing_else_is_claimed() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(Some("cmd:")), None);
        assert_eq!(parse(Some("cmd:nonsense:-")), None);
        assert_eq!(parse(Some("send:confirm:01HXYZ")), None);
        assert_eq!(parse(Some("bal:refresh")), None);
        assert_eq!(parse(Some("pj:cancel:abc")), None);
    }

    /// The commands that must stay typed-only: §8.1 wants a word for a
    /// destructive action, and the rest need an argument a tap cannot carry.
    #[test]
    fn the_commands_held_back_are_not_reachable_by_a_tap() {
        for slug in ["export", "delete", "mine", "tx", "bumpfee", "pj_receive"] {
            assert_eq!(
                parse(Some(&format!("cmd:{slug}:-"))),
                None,
                "/{slug} must not be reachable from a button"
            );
        }
    }
}
