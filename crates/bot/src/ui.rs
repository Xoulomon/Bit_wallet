//! Rendering (PLAN.md §8, §3a rule 2).
//!
//! Every emoji, badge, table and human sentence in this project is written
//! here. Core hands over `Amount`, `FeeRate`, `Txid` and typed enums; this
//! module turns them into Telegram HTML. Nothing in here may contain bitcoin
//! logic, and nothing below the boundary may contain a word of this.

use wallet_core::bitcoin::{Amount, Network};
use wallet_core::types::BackendStatus;
use wallet_core::{BackendError, CoreError};

/// The network badge of §8.1. On mainnet it leads the first line, so a mistaken
/// network is visible before an amount is read.
pub fn badge(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "🟠 MAINNET",
        Network::Regtest => "🧪 REGTEST",
        Network::Testnet => "🧪 TESTNET",
        Network::Signet => "🧪 SIGNET",
        _ => "❔ UNKNOWN",
    }
}

/// Sats with thin separators, plus BTC — every balance is shown both ways (§8.2).
pub fn sats(amount: Amount) -> String {
    format!("{} sats", group(amount.to_sat()))
}

/// Thousands separators without pulling in a formatting crate.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Telegram HTML is a small allowlist of tags; anything user-supplied that
/// reaches a message must go through here or it can break the parse.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn welcome(network: Network, has_wallet: bool) -> String {
    let badge = badge(network);
    if has_wallet {
        format!(
            "{badge}\n\n<b>Your wallet is ready.</b>\n\n\
             /balance — what you hold\n\
             /receive — an address to be paid at\n\
             /send — pay someone\n\
             /help — everything else"
        )
    } else {
        format!(
            "{badge}\n\n<b>A non-custodial Bitcoin wallet.</b>\n\n\
             Your seed phrase is generated on this server, encrypted with a PIN only you know, \
             and never leaves it. Nobody can move your coins without that PIN — and nobody can \
             recover it for you either.\n\n\
             /create — a new wallet\n\
             /restore — an existing seed phrase"
        )
    }
}

/// `/status` (§8.2). The call budget appears only where there is one to spend.
pub fn status(s: &BackendStatus, session: Option<std::time::Duration>) -> String {
    let mut out = format!(
        "{}\n\n<b>Backend</b>\nTip      {}\nLatency  {} ms",
        badge(s.network),
        group(u64::from(s.tip_height)),
        s.latency.as_millis()
    );

    if let (Some(used), Some(budget)) = (s.calls_used, s.call_budget) {
        out.push_str(&format!("\nCalls    {used} / {budget} per minute"));
    }

    if s.degraded {
        out.push_str("\n\n⚠️ The backend is slow or rate-limited; commands may lag.");
    }

    out.push_str(&match session {
        Some(left) => format!(
            "\n\n<b>Session</b>\n🔓 Unlocked, {} min left",
            left.as_secs() / 60
        ),
        None => "\n\n<b>Session</b>\n🔒 Locked".to_string(),
    });

    out
}

/// What this instance is bound to, and that its state is namespaced (§8.2).
pub fn network_card(network: Network) -> String {
    let body = match network {
        Network::Bitcoin => {
            "Real bitcoin, on the real chain. Transactions cannot be reversed.\n\n\
             Because this instance reaches Core through a hosted, allowlisted proxy, two \
             things differ from a full node:\n\
             • incoming payments are invisible until they confirm in a block;\n\
             • fee estimates come from an external API, and every rate is floored at the \
             node's own minimum."
        }
        _ => {
            "A private test chain. These coins are worth nothing — which is exactly what \
             makes it the right place to learn the flows.\n\n\
             /mine <n> mints blocks (admins only)."
        }
    };
    format!(
        "{}\n\n{body}\n\nState for each network is stored separately; the two can never mix.",
        badge(network)
    )
}

/// `CoreError` → "what happened + what to do" (§8.1).
///
/// This `match` is the reason no user-facing prose exists below the boundary.
/// A raw backend string is surfaced in exactly one place — a broadcast
/// rejection — where the node's own reason is the useful part.
pub fn render_error(e: &CoreError) -> String {
    match e {
        CoreError::NoWallet =>
            "You don't have a wallet yet. /create makes one, /restore brings an existing seed phrase.".into(),
        CoreError::WalletExists =>
            "You already have a wallet here. /delete removes it first — make sure your seed phrase is written down.".into(),
        CoreError::BackupCheckFailed =>
            "Those words don't match. Check the numbered words against what you wrote down, then try again.".into(),
        CoreError::InvalidMnemonic =>
            "That isn't a valid seed phrase. Check the spelling and the word order, then try /restore again.".into(),
        CoreError::InvalidPin { min, max } =>
            format!("A PIN must be {min}–{max} digits. Try again."),
        CoreError::WrongPin { remaining } =>
            format!("Wrong PIN. {remaining} attempt(s) left before a lockout."),
        CoreError::PinLocked { .. } =>
            "Too many wrong PINs. Wait for the lockout to clear, then try again.".into(),
        CoreError::Locked =>
            "Your wallet is locked. /unlock first.".into(),
        CoreError::RestoreTooDeep { depth, max, eta } =>
            format!(
                "That birthday is {} blocks back — over the {} block limit, and about {} hours of \
                 scanning. Use a later birthday if you know one.",
                group(u64::from(*depth)), group(u64::from(*max)), eta.as_secs() / 3600
            ),
        CoreError::InvalidPaymentTarget { network } =>
            format!("That isn't a valid address or BIP21 URI for {}.", badge(*network)),
        CoreError::InsufficientFunds { needed, available } =>
            format!(
                "Not enough funds: this would cost {}, and you have {}.",
                sats(*needed), sats(*available)
            ),
        CoreError::FeeBelowFloor { given, floor } =>
            format!(
                "{} sat/vB is below the network's current minimum of {} sat/vB, so it would never \
                 relay. Choose a higher rate.",
                given.to_sat_per_vb_ceil(), floor.to_sat_per_vb_ceil()
            ),
        CoreError::OverSendCap { amount, cap } =>
            format!(
                "{} is over this bot's per-payment cap of {}.",
                sats(*amount), sats(*cap)
            ),
        CoreError::QuoteExpired =>
            "That payment card has expired, so the fee it quoted may be stale. Start /send again.".into(),
        CoreError::BroadcastRejected { reason } =>
            // The node's own words: the one place a raw string is the useful part.
            format!("The network rejected the transaction:\n<code>{}</code>", escape(reason)),
        CoreError::UnsupportedOnNetwork { network } =>
            format!("That command isn't available on {}.", badge(*network)),
        CoreError::Payjoin(_) =>
            "The payjoin couldn't be completed. Your funds are untouched — /pj_sessions shows what happened.".into(),
        CoreError::NoSuchSession =>
            "No such payjoin session.".into(),
        CoreError::Backend(b) => render_backend_error(b),
        CoreError::NetworkMismatch { .. } | CoreError::MainnetNotAcknowledged
        | CoreError::MissingConfig(_) | CoreError::InvalidConfig { .. } =>
            "This bot is misconfigured and can't serve that safely. Tell whoever runs it.".into(),
        // `CoreError` is `#[non_exhaustive]`, so core can grow a variant without
        // breaking this crate. A new one reads as an internal fault until it is
        // given its own sentence here, which is the safe direction to fail.
        CoreError::Storage(_) | CoreError::Wallet(_) | CoreError::Crypto(_) | _ =>
            "Something went wrong on this server. Nothing was sent. Try again in a moment.".into(),
    }
}

fn render_backend_error(e: &BackendError) -> String {
    match e {
        BackendError::MissingApiKey | BackendError::Forbidden { .. } => {
            "This bot can't reach the Bitcoin backend — check its BITRPC_API_KEY. Nothing was sent."
                .into()
        }
        BackendError::RateLimited { .. } => {
            "The backend is rate-limited right now. Try again in a minute; nothing was sent.".into()
        }
        BackendError::NodeUnavailable => {
            "The Bitcoin node is unavailable. Try again shortly; nothing was sent.".into()
        }
        BackendError::Rpc { message, .. } => format!(
            "The node refused that request:\n<code>{}</code>",
            escape(message)
        ),
        BackendError::Transport(_) | _ => {
            "Couldn't reach the Bitcoin backend. Try again shortly; nothing was sent.".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_is_badged_first_so_the_chain_is_read_before_the_amount() {
        let card = welcome(Network::Bitcoin, true);
        assert!(card.starts_with("🟠 MAINNET"));
    }

    #[test]
    fn amounts_are_grouped() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(2_100_000_000_000_000), "2,100,000,000,000,000");
    }

    #[test]
    fn html_from_a_node_message_cannot_break_the_parse() {
        let rendered = render_error(&CoreError::BroadcastRejected {
            reason: "bad-txns <script>".into(),
        });
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(!rendered.contains("<script>"));
    }

    #[test]
    fn every_error_renders_to_a_sentence_with_a_next_step() {
        let rendered = render_error(&CoreError::Locked);
        assert!(rendered.contains("/unlock"));
    }
}
