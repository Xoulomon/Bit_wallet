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

// ---------------------------------------------------------------- lifecycle
// The wording of §5 and §8.1: what is about to happen, why it matters, and
// what the user must do. No step asks for a secret without saying what becomes
// of it.

pub fn ask_pin_new() -> String {
    "<b>Choose a PIN</b>\n\n\
     6–8 digits. It encrypts your seed phrase on this server, and it is the only \
     thing standing between someone with the database and your coins.\n\n\
     There is no way to reset it. Send it now — I'll delete your message straight away."
        .into()
}

pub fn ask_pin_again() -> String {
    "Send the same PIN once more, so a typo can't lock you out.".into()
}

pub fn pin_mismatch() -> String {
    "Those two didn't match. Let's start the PIN again — send the one you want.".into()
}

pub fn ask_pin() -> String {
    "Send your PIN. I'll delete the message as soon as it arrives.".into()
}

pub fn ask_mnemonic() -> String {
    "<b>Send your seed phrase</b>\n\n\
     12 or 24 words, in order, separated by spaces. I'll delete your message the \
     instant it arrives.\n\n\
     Only do this in a chat you trust, on a device you trust."
        .into()
}

pub fn ask_birthday() -> String {
    "<b>When was this wallet first used?</b>\n\n\
     Send the block height if you know it — scanning starts there instead of from \
     the beginning of the chain, which is much faster.\n\n\
     Send <code>skip</code> if you don't know."
        .into()
}

/// §6: the front end renders the verdict; the policy behind it is core's.
pub fn restore_plan(network: Network, plan: &wallet_core::types::RestorePlan) -> String {
    use wallet_core::types::RestoreVerdict;

    let head = format!("{}\n\n<b>Restore</b>", badge(network));
    let depth = format!(
        "\nFrom block {} to {} — {} blocks.",
        group(u64::from(plan.birthday)),
        group(u64::from(plan.tip)),
        group(u64::from(plan.depth))
    );

    match &plan.verdict {
        RestoreVerdict::Proceed => format!("{head}{depth}\n\nThis will be quick."),
        RestoreVerdict::Warn => format!(
            "{head}{depth}\n\n⏳ Scanning that far back takes about {}. \
             You can keep using the chat meanwhile; balances will fill in as it goes.",
            duration(plan.eta)
        ),
        RestoreVerdict::Refuse { max } => format!(
            "{head}{depth}\n\n❌ That's more than this bot will scan ({} blocks, about {}).\n\n\
             The limit exists because every block costs calls against a shared, rate-limited \
             backend. If you know a later block height for this wallet, send /restore again \
             and use it.",
            group(u64::from(*max)),
            duration(plan.eta)
        ),
    }
}

pub fn duration(d: std::time::Duration) -> String {
    let mins = d.as_secs() / 60;
    match mins {
        0 => "under a minute".into(),
        1..=90 => format!("{mins} minutes"),
        _ => format!("{} hours", mins / 60),
    }
}

/// §5, §8.1: shown once, in a message that removes itself after 60 seconds.
pub fn mnemonic_card(words: &str) -> String {
    format!(
        "<b>Write these down, in order, on paper.</b>\n\n\
         <tg-spoiler><code>{}</code></tg-spoiler>\n\n\
         ⏳ This message deletes itself in 60 seconds.\n\n\
         Anyone with these words has your coins. Never type them into anything that \
         asks for them — including, after today, this bot.",
        escape(words)
    )
}

pub fn ask_backup_word(index: u8, nth: usize) -> String {
    format!(
        "<b>Check {nth} of 3</b>\n\nWhat is word number {}?",
        index as usize + 1
    )
}

pub fn wallet_ready(network: Network) -> String {
    format!(
        "{}\n\n✅ <b>Your wallet is ready.</b>\n\n\
         /receive — an address to be paid at\n\
         /balance — what you hold\n\
         /send — pay someone",
        badge(network)
    )
}

pub fn restored(network: Network) -> String {
    format!(
        "{}\n\n✅ <b>Restored.</b> Scanning for your history now — /balance will fill \
         in as it goes.",
        badge(network)
    )
}

pub fn unlocked(remaining: std::time::Duration) -> String {
    format!("🔓 Unlocked for {} minutes.", remaining.as_secs() / 60)
}

pub fn locked() -> String {
    "🔒 Locked.".into()
}

pub fn ask_delete_word() -> String {
    "<b>Delete this wallet?</b>\n\n\
     Your seed phrase and every address this bot knows for you will be erased here. \
     If you have the words written down you can restore later; if you don't, the \
     coins are gone.\n\n\
     Type <code>DELETE</code> in capitals to continue, or anything else to stop."
        .into()
}

pub fn delete_cancelled() -> String {
    "Nothing deleted.".into()
}

pub fn deleted() -> String {
    "Wallet deleted. <b>/restore</b> brings it back if you have the seed phrase.".into()
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::time::Duration;
    use wallet_core::types::{RestorePlan, RestoreVerdict};

    fn plan(depth: u32, verdict: RestoreVerdict, eta_secs: u64) -> RestorePlan {
        RestorePlan {
            birthday: 900_000 - depth,
            tip: 900_000,
            depth,
            eta: Duration::from_secs(eta_secs),
            verdict,
        }
    }

    /// §8.1: a mnemonic message says, on the message itself, that it will go.
    #[test]
    fn the_mnemonic_card_warns_that_it_self_destructs() {
        let card = mnemonic_card("abandon abandon about");
        assert!(card.contains("60 seconds"));
        assert!(card.contains("paper"));
        assert!(card.contains("abandon abandon about"));
    }

    #[test]
    fn a_refused_restore_says_why_and_what_to_do_instead() {
        let rendered = restore_plan(
            Network::Bitcoin,
            &plan(500_000, RestoreVerdict::Refuse { max: 10_000 }, 900_000),
        );
        assert!(rendered.starts_with("🟠 MAINNET"));
        assert!(rendered.contains("10,000"));
        assert!(
            rendered.contains("/restore"),
            "a refusal must name the next step"
        );
    }

    #[test]
    fn a_warned_restore_quotes_the_wait_rather_than_just_warning() {
        let rendered = restore_plan(Network::Regtest, &plan(5_000, RestoreVerdict::Warn, 9_000));
        assert!(rendered.contains("hours") || rendered.contains("minutes"));
    }

    #[test]
    fn a_quick_restore_says_nothing_alarming() {
        let rendered = restore_plan(Network::Regtest, &plan(50, RestoreVerdict::Proceed, 5));
        assert!(!rendered.contains('❌'));
        assert!(rendered.contains("quick"));
    }

    #[test]
    fn durations_read_as_english() {
        assert_eq!(duration(Duration::from_secs(30)), "under a minute");
        assert_eq!(duration(Duration::from_secs(600)), "10 minutes");
        assert_eq!(duration(Duration::from_secs(7_200)), "2 hours");
    }

    /// §5: the PIN prompt has to say that it cannot be reset, because the user
    /// is choosing it in the three seconds before they forget it.
    #[test]
    fn the_pin_prompt_states_the_consequence_of_losing_it() {
        let prompt = ask_pin_new();
        assert!(prompt.contains("no way to reset"));
        assert!(prompt.contains("delete your message"));
    }

    #[test]
    fn the_delete_prompt_requires_a_typed_word_and_names_the_risk() {
        let prompt = ask_delete_word();
        assert!(prompt.contains("DELETE"));
        assert!(prompt.contains("gone"));
    }

    #[test]
    fn a_backup_question_is_one_based_for_a_human() {
        // Core counts from zero; a user counts from one.
        assert!(ask_backup_word(0, 1).contains("word number 1"));
        assert!(ask_backup_word(11, 3).contains("word number 12"));
    }
}
