//! Push notifications (PLAN.md §8.6, §3a rule 6).
//!
//! A *subscriber*, not a channel owner. One task, one
//! `broadcast::Receiver<CoreEvent>`, one `match`: map `UserId` back to a chat,
//! render, send. Core has no idea this exists, and a CLI front end prints the
//! same events without a line of this file.

use crate::Ctx;
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::events::{BackendHealth, CoreEvent};

pub async fn run(bot: Bot, ctx: Ctx) {
    let mut rx = ctx.core.subscribe();

    loop {
        match rx.recv().await {
            Ok(event) => deliver(&bot, &ctx, event).await,

            // A slow front end lags rather than blocking the chain sync. The
            // honest response is to say so: the user's next /balance is
            // accurate even when a notification was dropped.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "notification backlog overflowed");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("core stopped emitting events");
                return;
            }
        }
    }
}

async fn deliver(bot: &Bot, ctx: &Ctx, event: CoreEvent) {
    // A per-user event goes to that user's chat; `BackendHealth` concerns
    // everyone, and this build logs it rather than waking every chat at once.
    let chat = match event.user() {
        Some(user) => match ctx.users.chat_of(user) {
            Ok(Some(chat)) => Some(ChatId(chat)),
            _ => return,
        },
        None => None,
    };

    let text = match &event {
        CoreEvent::IncomingTx { amount, status, .. } => crate::ui::incoming(*amount, *status),

        CoreEvent::TxConfirmed {
            txid,
            confirmations,
            ..
        } => crate::ui::confirmed(&txid.to_string(), *confirmations),

        CoreEvent::SyncProgress { height, tip, .. } => {
            // §8.1 says progress owns one message that is edited in place, and
            // that message is the restore card — which only exists while a
            // restore is running. Until there is one to edit, a message per
            // pass would be worse than silence.
            tracing::debug!(height, tip, "sync progress");
            return;
        }

        CoreEvent::SessionExpired { .. } => crate::ui::session_expired(),

        CoreEvent::Payjoin { state, .. } => match crate::ui::payjoin_event(state) {
            Some(line) => line,
            // Waiting and ProposalSent are visible in /pj_sessions; a message
            // for every transition would be noise (§7's "no scary errors").
            None => return,
        },

        CoreEvent::BackendHealth(health) => {
            // This concerns everyone, so it goes to the people who can act on
            // it rather than waking every chat (§8.7 owns the admin list).
            let (line, level) = match health {
                BackendHealth::Healthy => (crate::ui::backend_recovered(), false),
                BackendHealth::Degraded { reason } => {
                    tracing::warn!(%reason, "backend degraded");
                    (crate::ui::backend_degraded(), true)
                }
                BackendHealth::RateLimited => {
                    tracing::warn!("backend rate limited");
                    (crate::ui::backend_degraded(), true)
                }
            };
            if !level {
                tracing::info!("backend recovered");
            }
            for admin in &ctx.policy.admins {
                let _ = bot.send_message(ChatId(*admin), line.clone()).await;
            }
            return;
        }

        _ => return,
    };

    let Some(chat) = chat else { return };

    if let Err(e) = bot
        .send_message(chat, text)
        .parse_mode(ParseMode::Html)
        .await
    {
        // A user who blocked the bot is not an error worth retrying.
        tracing::warn!(error = %e, "could not deliver a notification");
    }
}
