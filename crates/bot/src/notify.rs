//! Push notifications (PLAN.md §8.6, §3a rule 6).
//!
//! A *subscriber*, not a channel owner. One task, one
//! `broadcast::Receiver<CoreEvent>`, one `match`: map `UserId` back to a chat,
//! render, send. Core has no idea this exists, and a CLI front end prints the
//! same events without a line of this file.
//!
//! Confirmations are the one event that arrives in bursts. Core announces each
//! transaction once, when it lands in a block (§6) — but `/mine 101` puts a
//! hundred and one coinbases in a hundred and one blocks, and that is still a
//! hundred and one true things to say at the same instant. So confirmations are
//! held for [`CONFIRM_COALESCE`] and sent as one message. Everything else goes
//! out the moment it arrives.

use crate::Ctx;
use std::{collections::HashMap, time::Duration};
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::events::{BackendHealth, CoreEvent};

/// How long confirmations are gathered before the chat is told about them.
///
/// Comfortably longer than one regtest sync pass, so the blocks from a single
/// `/mine` arrive inside one window rather than dribbling out across several.
const CONFIRM_COALESCE: Duration = Duration::from_secs(2);

/// Confirmations waiting to be told, by chat.
type Pending = HashMap<ChatId, Vec<(String, u32)>>;

pub async fn run(bot: Bot, ctx: Ctx) {
    let mut rx = ctx.core.subscribe();
    let mut pending: Pending = HashMap::new();
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Ok(CoreEvent::TxConfirmed { user, txid, confirmations }) => {
                    let Ok(Some(chat)) = ctx.users.chat_of(user) else { continue };
                    pending
                        .entry(ChatId(chat))
                        .or_default()
                        .push((txid.to_string(), confirmations));

                    // The window opens on the first confirmation and is not
                    // extended by the ones behind it, so a steady trickle can
                    // never hold a message back indefinitely.
                    deadline.get_or_insert_with(|| tokio::time::Instant::now() + CONFIRM_COALESCE);
                }

                Ok(event) => deliver(&bot, &ctx, event).await,

                // A slow front end lags rather than blocking the chain sync. The
                // honest response is to say so: the user's next /balance is
                // accurate even when a notification was dropped.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "notification backlog overflowed");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("core stopped emitting events");
                    // Whatever was still being gathered is said before going.
                    flush(&bot, &mut pending).await;
                    return;
                }
            },

            // Disabled while nothing is waiting, so an idle bot sets no timers.
            () = sleep_until(deadline), if deadline.is_some() => {
                flush(&bot, &mut pending).await;
                deadline = None;
            }
        }
    }
}

/// The `select!` arm's future. Only ever polled under `deadline.is_some()`,
/// which `select!` checks before this is evaluated.
async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    if let Some(at) = deadline {
        tokio::time::sleep_until(at).await;
    }
}

/// Say what confirmed, one message per chat however many there were.
async fn flush(bot: &Bot, pending: &mut Pending) {
    for (chat, confirmed) in pending.drain() {
        let text = match confirmed.as_slice() {
            [] => continue,
            [(txid, confirmations)] => crate::ui::confirmed(txid, *confirmations),
            many => crate::ui::confirmed_many(many.len()),
        };

        if let Err(e) = bot
            .send_message(chat, text)
            .parse_mode(ParseMode::Html)
            .await
        {
            tracing::warn!(error = %e, "could not deliver a confirmation");
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

        // Handled before this point, so it can be gathered with the rest of
        // its burst rather than sent on its own.
        CoreEvent::TxConfirmed { .. } => return,

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
