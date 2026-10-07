//! Push notifications (PLAN.md §8.6, §3a rule 6).
//!
//! A *subscriber*, not a channel owner. One task, one
//! `broadcast::Receiver<CoreEvent>`, one `match`: map `UserId` back to a chat,
//! render, send. Core has no idea this exists, and a CLI front end prints the
//! same events without a line of this file.
//!
//! A sync pass is the one thing that arrives in bursts. Core announces each
//! transaction once (§6), but a single pass can find a hundred of them: `/mine
//! 101` puts a hundred and one coinbases in a hundred and one blocks, and the
//! first sync of a restored wallet finds every payment it ever received. All of
//! those are true things to say at the same instant, and said one per message
//! they are a wall of near-identical lines rather than a notification.
//!
//! So arrivals and confirmations are both held for [`BURST_COALESCE`] and sent
//! as one message each: the chat learns how many and how much, and `/history`
//! has the detail. Everything else goes out the moment it arrives.

use crate::Ctx;
use std::{collections::HashMap, time::Duration};
use teloxide::{prelude::*, types::ParseMode};
use wallet_core::bitcoin::Amount;
use wallet_core::events::{BackendHealth, CoreEvent};
use wallet_core::types::{FiatPrice, TxStatus};

/// How long a burst is gathered before the chat is told about it.
///
/// Comfortably longer than one regtest sync pass, so the blocks from a single
/// `/mine` arrive inside one window rather than dribbling out across several.
const BURST_COALESCE: Duration = Duration::from_secs(2);

/// What one sync pass found, by chat. Both lists render to at most one message,
/// so the worst case for a chat is two.
#[derive(Default)]
struct Burst {
    /// Money that arrived. Only the count and the sum survive grouping, so a
    /// single arrival keeps its own status and the rest become a total.
    incoming: Vec<(Amount, TxStatus)>,
    confirmed: Vec<(String, u32)>,
}

type Pending = HashMap<ChatId, Burst>;

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
                        .confirmed
                        .push((txid.to_string(), confirmations));

                    // The window opens on the first event of a burst and is not
                    // extended by the ones behind it, so a steady trickle can
                    // never hold a message back indefinitely.
                    deadline.get_or_insert_with(|| tokio::time::Instant::now() + BURST_COALESCE);
                }

                // Gathered too: one arrival costs two seconds, which nobody
                // notices, and a hundred cost one message instead of a hundred.
                Ok(CoreEvent::IncomingTx { user, amount, status, .. }) => {
                    let Ok(Some(chat)) = ctx.users.chat_of(user) else { continue };
                    pending
                        .entry(ChatId(chat))
                        .or_default()
                        .incoming
                        .push((amount, status));

                    deadline.get_or_insert_with(|| tokio::time::Instant::now() + BURST_COALESCE);
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
                    flush(&bot, &ctx, &mut pending).await;
                    return;
                }
            },

            // Disabled while nothing is waiting, so an idle bot sets no timers.
            () = sleep_until(deadline), if deadline.is_some() => {
                flush(&bot, &ctx, &mut pending).await;
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

/// Say what the pass found: at most one arrival message and one confirmation
/// message per chat, however many of each there were.
async fn flush(bot: &Bot, ctx: &Ctx, pending: &mut Pending) {
    // Money arriving is the one notification where the dollar figure is the
    // first thing anyone wants, so this is the only event that pays for a price
    // lookup — and a burst of a hundred arrivals pays for it once, here, rather
    // than per transaction.
    let price = if pending.values().any(|b| !b.incoming.is_empty()) {
        ctx.core.price().await
    } else {
        None
    };

    for (chat, burst) in pending.drain() {
        for text in burst.render(price.as_ref()) {
            if let Err(e) = bot
                .send_message(chat, text)
                .parse_mode(ParseMode::Html)
                .await
            {
                tracing::warn!(error = %e, "could not deliver a notification");
            }
        }
    }
}

impl Burst {
    /// Everything one pass found, as the messages it is worth sending: at most
    /// one for arrivals and one for confirmations, whatever the count.
    ///
    /// Pure, and separate from the sending, because "a hundred and one
    /// coinbases produce one message" is the whole point of this module and a
    /// claim that should be tested rather than taken on trust.
    fn render(&self, price: Option<&FiatPrice>) -> Vec<String> {
        let arrival = match self.incoming.as_slice() {
            [] => None,
            [(amount, status)] => Some(crate::ui::incoming(*amount, *status, price)),
            many => {
                // Checked, because an overflowing total would be a panic in a
                // notification task — the one place a wrong number is less bad
                // than a dead subscriber.
                let total = many
                    .iter()
                    .try_fold(Amount::ZERO, |sum, (amount, _)| sum.checked_add(*amount))
                    .unwrap_or(Amount::MAX_MONEY);
                Some(crate::ui::incoming_many(many.len(), total, price))
            }
        };

        let confirmation = match self.confirmed.as_slice() {
            [] => None,
            [(txid, confirmations)] => Some(crate::ui::confirmed(txid, *confirmations)),
            many => Some(crate::ui::confirmed_many(many.len())),
        };

        arrival.into_iter().chain(confirmation).collect()
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
        // Both are handled before this point, so each is gathered with the rest
        // of its burst rather than sent on its own.
        CoreEvent::IncomingTx { .. } | CoreEvent::TxConfirmed { .. } => return,

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

#[cfg(test)]
mod tests {
    use super::*;

    fn coinbase(n: u32) -> (Amount, TxStatus) {
        (
            Amount::from_sat(1_220_703),
            TxStatus::Confirmed {
                height: n,
                confirmations: n,
            },
        )
    }

    /// `/mine 101` mines its blocks in one RPC, so the pass that follows finds
    /// a hundred and one coinbases at once. That is one message, not a screen
    /// of "📥 Received 1,220,703 sats — ✅ 1 conf" repeated until it scrolls.
    #[test]
    fn mine_101_is_one_message() {
        let burst = Burst {
            incoming: (1..=101).map(coinbase).collect(),
            confirmed: Vec::new(),
        };

        let sent = burst.render(None);
        assert_eq!(sent.len(), 1, "one burst, one message: {sent:#?}");
        assert!(sent[0].contains("101 payments"));
        // The total, not a hundred and one separate amounts.
        assert!(sent[0].contains(&crate::ui::group(101 * 1_220_703)));
    }

    /// One payment is still a payment: it keeps the per-transaction line, with
    /// its own status, rather than being summarised into a count of one.
    #[test]
    fn a_single_arrival_is_not_summarised() {
        let burst = Burst {
            incoming: vec![coinbase(1)],
            confirmed: Vec::new(),
        };

        let sent = burst.render(None);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("1,220,703 sats"));
        assert!(!sent[0].contains("payments"));
    }

    /// Arrivals and confirmations are different facts, so a pass that produced
    /// both says both — but still one message each, never one per transaction.
    #[test]
    fn arrivals_and_confirmations_are_two_messages_at_most() {
        let burst = Burst {
            incoming: (1..=50).map(coinbase).collect(),
            confirmed: (1..=50).map(|n| (format!("{n:064x}"), n)).collect(),
        };

        let sent = burst.render(None);
        assert_eq!(sent.len(), 2, "{sent:#?}");
        assert!(sent[0].contains("50 payments"));
        assert!(sent[1].contains("50 transactions confirmed"));
    }

    /// Nothing gathered is nothing said. An empty bucket must not produce a
    /// message, or an idle wallet would ping the chat every window.
    #[test]
    fn an_empty_burst_says_nothing() {
        assert!(Burst::default().render(None).is_empty());
    }
}
