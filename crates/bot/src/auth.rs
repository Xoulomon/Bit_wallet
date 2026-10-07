//! Guards (PLAN.md §8.7).
//!
//! Private chats only, an optional allowlist, and admin-gated commands. All of
//! this is front-end policy: core knows an opaque `UserId` and nothing about who
//! is allowed to hold one.

use std::collections::HashSet;
use teloxide::types::{CallbackQuery, ChatId, Message, UserId as TgUserId};

#[derive(Debug, Clone)]
pub struct Policy {
    pub admins: HashSet<i64>,
    /// Empty means anyone — the multi-user default of §4.
    pub allowed: HashSet<i64>,
}

impl Policy {
    pub fn from_env() -> Self {
        Policy {
            admins: id_list("BOT_ADMIN_IDS"),
            allowed: id_list("ALLOWED_USER_IDS"),
        }
    }

    pub fn is_admin(&self, tg: i64) -> bool {
        self.admins.contains(&tg)
    }

    pub fn is_allowed(&self, tg: i64) -> bool {
        self.allowed.is_empty() || self.allowed.contains(&tg)
    }
}

fn id_list(key: &str) -> HashSet<i64> {
    std::env::var(key)
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

/// Why a message was refused before any handler ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A group or supergroup. A seed phrase must never be one forward away (§5).
    NotPrivate,
    /// Not on `ALLOWED_USER_IDS`.
    NotAllowed,
    /// A channel post or a message with no author.
    NoSender,
    /// Past the per-user command quota (§8.7).
    TooFast,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Refusal::NotPrivate => {
                "This bot only works in a private chat — a wallet command in a group would put \
                 your balance, and possibly your seed phrase, in front of everyone. Message me \
                 directly."
            }
            Refusal::NotAllowed => "This bot is private and your account isn't on its list.",
            Refusal::NoSender => "I can't tell who sent that.",
            Refusal::TooFast => {
                "That's a lot of commands at once — give me a moment and try again."
            }
        }
    }
}

/// The single gate every update passes through (§8.7).
pub fn check(msg: &Message, policy: &Policy) -> Result<(TgUserId, ChatId), Refusal> {
    if !msg.chat.is_private() {
        return Err(Refusal::NotPrivate);
    }
    let from = msg.from.as_ref().ok_or(Refusal::NoSender)?;
    #[allow(clippy::cast_possible_wrap)]
    if !policy.is_allowed(from.id.0 as i64) {
        return Err(Refusal::NotAllowed);
    }
    Ok((from.id, msg.chat.id))
}

/// The same gate for a button press (§8.7).
///
/// Buttons used to skip this entirely — only `Update::filter_message` was
/// guarded — and that was survivable while every button was a step inside a
/// flow a guarded command had already started. It stopped being survivable the
/// moment a button could *run* a command: the allowlist and the throttle have
/// to cover both ways in, or the second one is a way around the first.
///
/// `message` is an `Option` because Telegram drops it once the card is old
/// enough, so the chat can only be checked when it is there. The sender always
/// is, and the sender is what the allowlist is about.
pub fn check_callback(query: &CallbackQuery, policy: &Policy) -> Result<TgUserId, Refusal> {
    if let Some(message) = query.message.as_ref()
        && !message.chat().is_private()
    {
        return Err(Refusal::NotPrivate);
    }
    #[allow(clippy::cast_possible_wrap)]
    if !policy.is_allowed(query.from.id.0 as i64) {
        return Err(Refusal::NotAllowed);
    }
    Ok(query.from.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_allowlist_means_multi_user() {
        let p = Policy {
            admins: HashSet::new(),
            allowed: HashSet::new(),
        };
        assert!(p.is_allowed(1));
        assert!(p.is_allowed(2));
    }

    #[test]
    fn a_populated_allowlist_excludes_everyone_else() {
        let p = Policy {
            admins: HashSet::new(),
            allowed: HashSet::from([7]),
        };
        assert!(p.is_allowed(7));
        assert!(!p.is_allowed(8));
    }

    #[test]
    fn the_group_refusal_explains_why_rather_than_just_refusing() {
        assert!(Refusal::NotPrivate.message().contains("private chat"));
    }
}
