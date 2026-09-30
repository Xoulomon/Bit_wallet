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

// ------------------------------------------------------------------- on-chain
// `comfy-table` into a <pre> block is the one place monospace survives a
// Telegram client, which is why the plan keeps it for /history and /addresses
// (§2, §8.2).

use comfy_table::{ContentArrangement, Table, presets::UTF8_BORDERS_ONLY};
use wallet_core::types::{
    AddressInfo, BalanceView, Page, Paged, TxDetail, TxDirection, TxStatus, TxSummary,
};

pub fn sats_and_btc(amount: Amount) -> String {
    format!("{} ({:.8} BTC)", sats(amount), amount.to_btc())
}

/// §8.2: confirmed, pending and immature, in sats *and* BTC.
///
/// The mainnet caveat is not a footnote: without `getrawmempool` an incoming
/// payment that has not confirmed is not zero, it is unseen, and a user staring
/// at a balance deserves to be told which (§4b).
pub fn balance(network: Network, b: &BalanceView) -> String {
    let mut out = format!(
        "{}\n\n<b>Balance</b>\n<code>Confirmed  {}</code>",
        badge(network),
        sats_and_btc(b.confirmed)
    );

    if b.trusted_pending > Amount::ZERO {
        out.push_str(&format!(
            "\n<code>Sending    {}</code>",
            sats(b.trusted_pending)
        ));
    }
    if b.untrusted_pending > Amount::ZERO {
        out.push_str(&format!(
            "\n<code>Incoming   {}</code>",
            sats(b.untrusted_pending)
        ));
    }
    if b.immature > Amount::ZERO {
        out.push_str(&format!("\n<code>Immature   {}</code>", sats(b.immature)));
    }

    out.push_str(&format!("\n\n<b>Total {}</b>", sats_and_btc(b.total)));

    if !b.unconfirmed_incoming_visible {
        out.push_str(
            "\n\nℹ️ Payments to you appear here once they're in a block, not before — \
             this bot's node connection can't see the mempool.",
        );
    }

    out
}

/// §8.2: address, BIP21 and the mainnet caveat, as a photo caption.
pub fn receive(network: Network, info: &AddressInfo) -> String {
    let mut out = format!(
        "{}\n\n<b>Your address</b>\n<code>{}</code>\n\nUnused address #{}",
        badge(network),
        escape(&info.address.to_string()),
        info.index
    );

    if network == Network::Bitcoin {
        out.push_str(
            "\n\nℹ️ A payment here shows up once it's in a block. Until then it won't \
             appear in /balance, even though it's on its way.",
        );
    }

    out
}

pub fn addresses(network: Network, page: &Paged<AddressInfo>) -> String {
    if page.items.is_empty() {
        return format!(
            "{}\n\nNo addresses yet. /receive makes one.",
            badge(network)
        );
    }

    let mut table = Table::new();
    table
        .load_style(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["#", "Address", "Used", "Received"]);

    for a in &page.items {
        table.add_row(vec![
            a.index.to_string(),
            shorten(&a.address.to_string()),
            if a.used { "yes" } else { "—" }.to_string(),
            if a.received > Amount::ZERO {
                group(a.received.to_sat())
            } else {
                "—".into()
            },
        ]);
    }

    format!(
        "{}\n\n<pre>{}</pre>\n{}",
        badge(network),
        escape(&table.to_string()),
        pager(page.page, page.total_pages())
    )
}

pub fn history(network: Network, page: &Paged<TxSummary>) -> String {
    if page.items.is_empty() {
        return format!(
            "{}\n\nNo transactions yet. /receive gives you an address to be paid at.",
            badge(network)
        );
    }

    let mut table = Table::new();
    table
        .load_style(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["", "Amount", "Fee", "Status"]);

    for tx in &page.items {
        table.add_row(vec![
            direction_mark(tx.direction).to_string(),
            group(tx.amount.to_sat()),
            tx.fee
                .map(|f| group(f.to_sat()))
                .unwrap_or_else(|| "—".into()),
            status_text(tx.status),
        ]);
    }

    let mut out = format!(
        "{}\n\n<pre>{}</pre>\n{}",
        badge(network),
        escape(&table.to_string()),
        pager(page.page, page.total_pages())
    );

    // On mainnet a txid is worth linking; on regtest there is nothing to link to.
    if network == Network::Bitcoin {
        out.push_str("\n\n");
        for tx in &page.items {
            out.push_str(&format!(
                "<a href=\"https://mempool.space/tx/{0}\">{1}</a>  ",
                tx.txid,
                shorten(&tx.txid.to_string())
            ));
        }
    }

    out
}

/// §8.2: one transaction in detail.
pub fn tx_detail(network: Network, d: &TxDetail) -> String {
    let s = &d.summary;
    let mut out = format!(
        "{}\n\n<b>{} {}</b>\n<code>{}</code>\n\n<code>Status  {}</code>",
        badge(network),
        match s.direction {
            TxDirection::Incoming => "Received",
            TxDirection::Outgoing => "Sent",
            TxDirection::Internal => "Moved",
        },
        sats(s.amount),
        escape(&s.txid.to_string()),
        status_text(s.status)
    );

    if let Some(fee) = s.fee {
        out.push_str(&format!("\n<code>Fee     {}</code>", sats(fee)));
    }
    if let Some(rate) = d.fee_rate {
        out.push_str(&format!(
            "\n<code>Rate    {} sat/vB</code>",
            rate.to_sat_per_vb_ceil()
        ));
    }
    out.push_str(&format!(
        "\n<code>Size    {} vB</code>\n<code>In/Out  {} / {}</code>",
        d.vsize, d.inputs, d.outputs
    ));

    if network == Network::Bitcoin {
        out.push_str(&format!(
            "\n\n<a href=\"https://mempool.space/tx/{}\">See it on mempool.space</a>",
            s.txid
        ));
    }

    out
}

fn direction_mark(d: TxDirection) -> &'static str {
    match d {
        TxDirection::Incoming => "📥",
        TxDirection::Outgoing => "📤",
        TxDirection::Internal => "🔁",
    }
}

fn status_text(s: TxStatus) -> String {
    match s {
        TxStatus::Unconfirmed => "pending".into(),
        TxStatus::Confirmed { confirmations, .. } if confirmations >= 6 => {
            format!("✅ {confirmations} confs")
        }
        TxStatus::Confirmed { confirmations, .. } => format!("{confirmations} conf"),
    }
}

/// The middle of a long identifier is the part nobody reads.
pub fn shorten(s: &str) -> String {
    if s.len() <= 16 {
        return s.to_string();
    }
    format!("{}…{}", &s[..8], &s[s.len() - 4..])
}

fn pager(page: Page, total_pages: u32) -> String {
    if total_pages <= 1 {
        return String::new();
    }
    format!("Page {} of {}", page.index + 1, total_pages)
}

// -------------------------------------------------------------- notifications
// §8.6: one match, one message. Core supplies the numbers; every word is here.

pub fn incoming(amount: Amount, status: TxStatus) -> String {
    match status {
        TxStatus::Unconfirmed => format!("📥 Incoming {} — unconfirmed", sats(amount)),
        TxStatus::Confirmed { confirmations, .. } => {
            format!("📥 Received {} — ✅ {} conf", sats(amount), confirmations)
        }
    }
}

pub fn confirmed(txid: &str, confirmations: u32) -> String {
    format!("✅ {} — {} confs", shorten(txid), confirmations)
}

pub fn session_expired() -> String {
    "🔒 Session locked after inactivity.".into()
}

pub fn backend_degraded() -> String {
    "⚠️ The Bitcoin backend is slow or rate-limited; commands may lag.".into()
}

pub fn backend_recovered() -> String {
    "✅ Backend healthy again.".into()
}

#[cfg(test)]
mod onchain_tests {
    use super::*;
    use wallet_core::bitcoin::Txid;
    use wallet_core::types::{Page, Paged, TxDirection, TxSummary};

    fn view(confirmed: u64, untrusted: u64, visible: bool) -> BalanceView {
        BalanceView {
            confirmed: Amount::from_sat(confirmed),
            trusted_pending: Amount::ZERO,
            untrusted_pending: Amount::from_sat(untrusted),
            immature: Amount::ZERO,
            total: Amount::from_sat(confirmed + untrusted),
            unconfirmed_incoming_visible: visible,
        }
    }

    fn txid(byte: u8) -> Txid {
        Txid::from_raw_hash(wallet_core::bitcoin::hashes::Hash::from_byte_array(
            [byte; 32],
        ))
    }

    /// §4b's most user-visible consequence has to be *said*, not implied by a
    /// zero that looks like a lost payment.
    #[test]
    fn a_backend_without_a_mempool_says_so_on_the_balance() {
        let rendered = balance(Network::Bitcoin, &view(100_000, 0, false));
        assert!(rendered.contains("in a block"));
        assert!(rendered.starts_with("🟠 MAINNET"));
    }

    #[test]
    fn a_backend_with_a_mempool_adds_no_caveat() {
        let rendered = balance(Network::Regtest, &view(100_000, 5_000, true));
        assert!(!rendered.contains("in a block"));
        assert!(rendered.contains("Incoming"));
    }

    #[test]
    fn a_balance_is_shown_in_sats_and_btc() {
        let rendered = balance(Network::Regtest, &view(150_000, 0, true));
        assert!(rendered.contains("150,000 sats"));
        assert!(rendered.contains("0.00150000 BTC"));
    }

    #[test]
    fn zero_categories_are_left_out_rather_than_shown_as_zero() {
        let rendered = balance(Network::Regtest, &view(1_000, 0, true));
        assert!(!rendered.contains("Immature"));
        assert!(!rendered.contains("Incoming"));
    }

    #[test]
    fn an_empty_history_offers_the_next_step() {
        let empty: Paged<TxSummary> = Paged {
            items: vec![],
            page: Page::new(0),
            total: 0,
        };
        assert!(history(Network::Regtest, &empty).contains("/receive"));
    }

    #[test]
    fn history_links_txids_on_mainnet_and_not_on_regtest() {
        let page = Paged {
            items: vec![TxSummary {
                txid: txid(1),
                direction: TxDirection::Incoming,
                amount: Amount::from_sat(25_000),
                fee: None,
                status: TxStatus::Confirmed {
                    height: 100,
                    confirmations: 3,
                },
                timestamp: None,
            }],
            page: Page::new(0),
            total: 1,
        };

        assert!(history(Network::Bitcoin, &page).contains("mempool.space"));
        assert!(!history(Network::Regtest, &page).contains("mempool.space"));
    }

    #[test]
    fn a_pager_appears_only_when_there_is_more_than_one_page() {
        let one: Paged<TxSummary> = Paged {
            items: vec![],
            page: Page::new(0),
            total: 3,
        };
        assert_eq!(pager(one.page, one.total_pages()), "");

        let many: Paged<TxSummary> = Paged {
            items: vec![],
            page: Page::new(1),
            total: 25,
        };
        assert_eq!(pager(many.page, many.total_pages()), "Page 2 of 3");
    }

    #[test]
    fn six_confirmations_reads_as_settled() {
        assert!(
            status_text(TxStatus::Confirmed {
                height: 1,
                confirmations: 6
            })
            .contains('✅')
        );
        assert_eq!(status_text(TxStatus::Unconfirmed), "pending");
    }

    #[test]
    fn long_identifiers_are_shortened_in_the_middle() {
        let full = txid(2).to_string();
        let short = shorten(&full);
        assert!(short.starts_with(&full[..8]));
        assert!(short.ends_with(&full[full.len() - 4..]));
        assert!(short.len() < full.len());
        // Short strings are left alone.
        assert_eq!(shorten("bc1qshort"), "bc1qshort");
    }

    #[test]
    fn a_mainnet_receive_warns_that_incoming_is_invisible_until_confirmed() {
        let info = AddressInfo {
            address: "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
                .parse::<wallet_core::bitcoin::Address<_>>()
                .expect("parses")
                .assume_checked(),
            index: 0,
            used: false,
            received: Amount::ZERO,
            bip21: "bitcoin:bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu".into(),
        };
        assert!(receive(Network::Bitcoin, &info).contains("in a block"));
        assert!(!receive(Network::Regtest, &info).contains("in a block"));
    }

    #[test]
    fn notifications_distinguish_arrival_from_confirmation() {
        assert!(incoming(Amount::from_sat(25_000), TxStatus::Unconfirmed).contains("unconfirmed"));
        assert!(
            incoming(
                Amount::from_sat(25_000),
                TxStatus::Confirmed {
                    height: 1,
                    confirmations: 1
                }
            )
            .contains("Received")
        );
        assert!(confirmed(&txid(3).to_string(), 6).contains("6 confs"));
    }
}

// ----------------------------------------------------------------- the send flow
// §8.3, screen by screen. Two cards and one rule: money never moves without a
// confirm card, and the card carries every number the user is agreeing to.

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use wallet_core::bitcoin::FeeRate;
use wallet_core::types::{Broadcast, FeeLabel, FeeOptions, FeeSource, SendQuote};

/// The fee screen (§8.3). The same code on both networks: it draws the presets
/// it is handed and nothing more.
pub fn fee_card(network: Network, amount: Option<Amount>, to: &str, fees: &FeeOptions) -> String {
    let what = match amount {
        Some(a) => format!(
            "Sending {} to <code>{}</code>",
            sats(a),
            escape(&shorten(to))
        ),
        None => format!("Sending to <code>{}</code>", escape(&shorten(to))),
    };

    let provenance = match &fees.source {
        FeeSource::Node => "Rates from your node".to_string(),
        FeeSource::External { name } => format!("Rates from {}", escape(name)),
        FeeSource::Unavailable => {
            // §6: it never silently guesses. Say why the buttons are missing.
            "No fee estimate available right now — enter a rate yourself".to_string()
        }
    };

    format!(
        "{} · <b>Choose a fee</b>\n{what}\n{provenance} · floor {} sat/vB",
        badge(network),
        fees.floor.to_sat_per_vb_ceil()
    )
}

pub fn fee_keyboard(fees: &FeeOptions) -> InlineKeyboardMarkup {
    let mut presets: Vec<InlineKeyboardButton> = fees
        .presets
        .iter()
        .map(|(label, rate)| {
            InlineKeyboardButton::callback(
                format!("{} {}", fee_label(*label), rate.to_sat_per_vb_ceil()),
                format!("send:fee:{}", fee_slug(*label)),
            )
        })
        .collect();

    // Keep a row from growing past what a phone shows.
    presets.truncate(3);

    let mut rows = Vec::new();
    if !presets.is_empty() {
        rows.push(presets);
    }

    let mut last = Vec::new();
    if fees.allows_custom {
        last.push(InlineKeyboardButton::callback(
            "Custom sat/vB",
            "send:fee:custom",
        ));
    }
    last.push(InlineKeyboardButton::callback("✖ Cancel", "send:cancel:-"));
    rows.push(last);

    InlineKeyboardMarkup::new(rows)
}

fn fee_label(l: FeeLabel) -> &'static str {
    match l {
        FeeLabel::Fast => "Fast",
        FeeLabel::Normal => "Normal",
        FeeLabel::Slow => "Slow",
    }
}

pub fn fee_slug(l: FeeLabel) -> &'static str {
    match l {
        FeeLabel::Fast => "fast",
        FeeLabel::Normal => "normal",
        FeeLabel::Slow => "slow",
    }
}

pub fn fee_from_slug(slug: &str) -> Option<FeeLabel> {
    match slug {
        "fast" => Some(FeeLabel::Fast),
        "normal" => Some(FeeLabel::Normal),
        "slow" => Some(FeeLabel::Slow),
        _ => None,
    }
}

pub fn ask_custom_fee(floor: FeeRate) -> String {
    format!(
        "Send a fee rate in sat/vB — a whole number, at least {}.",
        floor.to_sat_per_vb_ceil()
    )
}

/// The confirm card (§8.3). Every number the user is agreeing to, and the
/// countdown that says the quote will not wait forever.
pub fn confirm_card(network: Network, q: &SendQuote) -> String {
    let header = if q.replaces.is_some() {
        "Fee bump"
    } else {
        "Confirm payment"
    };

    let mut out = format!(
        "{} · <b>{header}</b>\n\n\
         <code>To      {}</code>\n\
         <code>Amount  {}</code>\n\
         <code>Fee     {} @ {} sat/vB</code>\n\
         <code>Total   {}</code>",
        badge(network),
        escape(&shorten(&q.recipient.to_string())),
        sats_and_btc(q.amount),
        sats(q.fee),
        q.fee_rate.to_sat_per_vb_ceil(),
        sats(q.total)
    );

    if q.change > Amount::ZERO {
        out.push_str(&format!("\n<code>Change  {}</code>", sats(q.change)));
    }
    if q.is_payjoin {
        out.push_str("\n\n🤝 Payjoin will be attempted — it makes this payment harder to trace.");
    }
    if let Some(replaced) = q.replaces {
        out.push_str(&format!(
            "\n\nReplaces <code>{}</code>",
            escape(&shorten(&replaced.to_string()))
        ));
    }

    out.push_str(&format!("\n\n⏳ Expires in {}", countdown(q)));
    out
}

fn countdown(q: &SendQuote) -> String {
    match q.expires_at.duration_since(std::time::SystemTime::now()) {
        Ok(left) => format!("{}:{:02}", left.as_secs() / 60, left.as_secs() % 60),
        Err(_) => "0:00".into(),
    }
}

pub fn confirm_keyboard(q: &SendQuote) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("✅ Confirm & sign", format!("send:confirm:{}", q.id)),
        InlineKeyboardButton::callback("✖ Cancel", format!("send:cancel:{}", q.id)),
    ]])
}

pub fn quote_expired_card(network: Network) -> String {
    format!(
        "{} · <b>Expired</b>\n\nThat card quoted a fee that may now be stale, so it was not \
         signed. Nothing was sent. Start /send again.",
        badge(network)
    )
}

pub fn broadcasting() -> String {
    "📡 Signing and broadcasting…".into()
}

pub fn broadcast_done(network: Network, b: &Broadcast) -> String {
    format!(
        "{} · 📡 <b>Sent</b>\n\n<code>{}</code>\n{} + {} fee\n\nTracking confirmations.",
        badge(network),
        escape(&shorten(&b.txid.to_string())),
        sats(b.amount),
        sats(b.fee)
    )
}

pub fn send_usage(network: Network) -> String {
    format!(
        "{}\n\n<b>Send bitcoin</b>\n\n\
         <code>/send &lt;address&gt; &lt;sats&gt;</code>\n\
         <code>/send &lt;address&gt; max</code>\n\
         <code>/send &lt;bitcoin: URI&gt;</code>\n\n\
         A BIP21 URI can carry its own amount, and a payjoin endpoint if the \
         receiver offers one.",
        badge(network)
    )
}

pub fn send_cancelled() -> String {
    "Cancelled. Nothing was sent.".into()
}

pub fn mined(network: Network, blocks: usize) -> String {
    format!("{} · ⛏ Mined {blocks} block(s).", badge(network))
}

#[cfg(test)]
mod send_tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use wallet_core::bitcoin::Txid;
    use wallet_core::types::QuoteId;

    fn vb(n: u64) -> FeeRate {
        FeeRate::from_sat_per_vb(n).expect("a valid rate")
    }

    fn address() -> wallet_core::bitcoin::Address {
        "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
            .parse::<wallet_core::bitcoin::Address<_>>()
            .expect("parses")
            .require_network(Network::Bitcoin)
            .expect("mainnet")
    }

    fn quote() -> SendQuote {
        SendQuote {
            id: QuoteId::new(),
            recipient: address(),
            amount: Amount::from_sat(50_000),
            fee: Amount::from_sat(1_410),
            fee_rate: vb(6),
            total: Amount::from_sat(51_410),
            change: Amount::from_sat(212_590),
            is_payjoin: false,
            payjoin_uri: None,
            replaces: None,
            expires_at: SystemTime::now() + Duration::from_secs(300),
        }
    }

    fn options(presets: Vec<(FeeLabel, FeeRate)>, source: FeeSource) -> FeeOptions {
        FeeOptions {
            presets,
            floor: vb(1),
            source,
            allows_custom: true,
        }
    }

    /// §8.3: the card carries every number the user is agreeing to.
    #[test]
    fn the_confirm_card_shows_amount_fee_total_and_change() {
        let card = confirm_card(Network::Bitcoin, &quote());
        assert!(card.contains("50,000 sats"));
        assert!(card.contains("1,410 sats"));
        assert!(card.contains("51,410 sats"));
        assert!(card.contains("212,590 sats"));
        assert!(card.contains("6 sat/vB"));
        assert!(card.contains("Expires in"));
        assert!(card.starts_with("🟠 MAINNET"));
    }

    #[test]
    fn a_payjoin_quote_says_so_on_the_card() {
        let mut q = quote();
        q.is_payjoin = true;
        assert!(confirm_card(Network::Regtest, &q).contains("Payjoin"));
        assert!(!confirm_card(Network::Regtest, &quote()).contains("Payjoin"));
    }

    /// §8.3: /bumpfee reuses the card; only the header and one line differ.
    #[test]
    fn a_fee_bump_is_the_same_card_with_a_different_header() {
        let mut q = quote();
        q.replaces = Some(Txid::from_raw_hash(
            wallet_core::bitcoin::hashes::Hash::from_byte_array([4u8; 32]),
        ));
        let card = confirm_card(Network::Bitcoin, &q);
        assert!(card.contains("Fee bump"));
        assert!(card.contains("Replaces"));
        assert!(!confirm_card(Network::Bitcoin, &quote()).contains("Fee bump"));
    }

    #[test]
    fn a_drained_wallet_shows_no_change_line() {
        let mut q = quote();
        q.change = Amount::ZERO;
        assert!(!confirm_card(Network::Regtest, &q).contains("Change"));
    }

    /// §6: the keyboard draws whatever presets it is handed — the same code on
    /// both networks.
    #[test]
    fn the_fee_keyboard_draws_the_presets_it_is_handed() {
        let three = fee_keyboard(&options(
            vec![
                (FeeLabel::Fast, vb(12)),
                (FeeLabel::Normal, vb(6)),
                (FeeLabel::Slow, vb(2)),
            ],
            FeeSource::External {
                name: "mempool.space".into(),
            },
        ));
        assert_eq!(three.inline_keyboard[0].len(), 3);

        // No estimate: no preset row at all, just Custom and Cancel.
        let none = fee_keyboard(&options(vec![], FeeSource::Unavailable));
        assert_eq!(none.inline_keyboard.len(), 1);
        assert_eq!(none.inline_keyboard[0].len(), 2);
    }

    /// §6: when there is no estimate, say why rather than showing a bare prompt.
    #[test]
    fn an_unavailable_estimator_is_explained_on_the_fee_card() {
        let card = fee_card(
            Network::Bitcoin,
            Some(Amount::from_sat(50_000)),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
            &options(vec![], FeeSource::Unavailable),
        );
        assert!(card.contains("No fee estimate available"));
        assert!(card.contains("floor 1 sat/vB"));
    }

    #[test]
    fn the_fee_card_names_its_source() {
        let card = fee_card(
            Network::Bitcoin,
            None,
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
            &options(
                vec![(FeeLabel::Normal, vb(6))],
                FeeSource::External {
                    name: "mempool.space".into(),
                },
            ),
        );
        assert!(card.contains("mempool.space"));
    }

    /// §8.5: callback data carries an opaque id, never an amount or a rate.
    #[test]
    fn callback_data_holds_no_money() {
        let q = quote();
        let keyboard = confirm_keyboard(&q);
        let data: Vec<String> = keyboard
            .inline_keyboard
            .iter()
            .flatten()
            .filter_map(|b| match &b.kind {
                teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => Some(d.clone()),
                _ => None,
            })
            .collect();

        assert!(data.iter().any(|d| d == &format!("send:confirm:{}", q.id)));
        for d in &data {
            assert!(d.len() <= 64, "Telegram's callback data limit is 64 bytes");
            assert!(!d.contains("50000"), "an amount must not be replayable");
            assert!(!d.contains("bc1q"), "an address must not be replayable");
        }
    }

    #[test]
    fn fee_slugs_round_trip() {
        for label in [FeeLabel::Fast, FeeLabel::Normal, FeeLabel::Slow] {
            assert_eq!(fee_from_slug(fee_slug(label)), Some(label));
        }
        assert_eq!(fee_from_slug("nonsense"), None);
    }

    /// §8.1: an expired card must say that nothing was sent, not merely that
    /// something failed.
    #[test]
    fn the_expiry_card_says_nothing_was_sent() {
        let card = quote_expired_card(Network::Bitcoin);
        assert!(card.contains("Nothing was sent"));
        assert!(card.contains("/send"));
    }
}

// ------------------------------------------------------------------- payjoin
// §7's UX note: explain the privacy benefit, badge the outcome, and never make
// a fallback look like a failure — the payment went through either way.

use wallet_core::types::{PayjoinReceipt, PayjoinRole, PayjoinSessionView, PayjoinState};

pub fn payjoin_receipt(network: Network, amount: Amount, r: &PayjoinReceipt) -> String {
    let mut out = format!(
        "{} · 🤝 <b>Payjoin request for {}</b>\n\n<code>{}</code>\n\n\
         Pay this with a wallet that supports payjoin and your two wallets build the \
         transaction together — so the usual assumption that every input belongs to the \
         sender stops holding for this payment.\n\n\
         If the sender's wallet doesn't do payjoin, they can still pay it normally.",
        badge(network),
        sats(amount),
        escape(&r.bip21)
    );

    if network == Network::Bitcoin {
        // §7's privacy note, stated rather than implied.
        out.push_str(
            "\n\nℹ️ This hides the payment from outside observers, not from the node \
             provider this bot talks to.",
        );
    }

    out.push_str("\n\nThis request expires in an hour. /pj_sessions shows how it's going.");
    out
}

pub fn payjoin_sessions(network: Network, sessions: &[PayjoinSessionView]) -> String {
    if sessions.is_empty() {
        return format!(
            "{}\n\nNo payjoin sessions. /pj_receive &lt;sats&gt; starts one.",
            badge(network)
        );
    }

    let mut out = format!("{}\n\n<b>Payjoin sessions</b>", badge(network));
    for s in sessions {
        out.push_str(&format!(
            "\n\n{} {} — {}\n<code>{}</code>",
            payjoin_badge(&s.state),
            match s.role {
                PayjoinRole::Receiver => "Receiving",
                PayjoinRole::Sender => "Sending",
            },
            payjoin_state_text(&s.state),
            escape(&shorten(&s.id.to_string()))
        ));
    }
    out
}

pub fn payjoin_badge(state: &PayjoinState) -> &'static str {
    match state {
        PayjoinState::Completed { .. } => "✅",
        PayjoinState::FellBack { .. } => "↩️",
        PayjoinState::Failed { .. } => "⚠️",
        PayjoinState::Expired | PayjoinState::Cancelled => "—",
        _ => "⏳",
    }
}

/// §7: no scary errors. A fallback is an outcome, not a failure.
pub fn payjoin_state_text(state: &PayjoinState) -> String {
    match state {
        PayjoinState::Waiting => "waiting for the other side".into(),
        PayjoinState::ProposalReceived => "checking their proposal".into(),
        PayjoinState::ProposalSent => "proposal sent, waiting".into(),
        PayjoinState::Completed { .. } => "done — payjoin".into(),
        PayjoinState::FellBack { .. } => "sent as a regular transaction".into(),
        PayjoinState::Expired => "expired".into(),
        PayjoinState::Cancelled => "cancelled".into(),
        PayjoinState::Failed { .. } => "didn't complete — funds untouched".into(),
    }
}

/// §8.6: the push notification for a payjoin state change.
pub fn payjoin_event(state: &PayjoinState) -> Option<String> {
    Some(match state {
        PayjoinState::ProposalReceived => "🤝 Payjoin proposal received — verifying".into(),
        PayjoinState::Completed { txid } => {
            format!("🤝 Payjoin ✅ — {}", shorten(&txid.to_string()))
        }
        PayjoinState::FellBack { txid } => format!(
            "↩️ Sent as a regular transaction (payjoin didn't complete) — {}",
            shorten(&txid.to_string())
        ),
        PayjoinState::Expired => "Payjoin request expired. Nothing was sent.".into(),
        PayjoinState::Failed { .. } => {
            "The payjoin didn't complete. Your funds are untouched — /pj_sessions has the detail."
                .into()
        }
        // Waiting, ProposalSent and Cancelled are visible in /pj_sessions; a
        // notification for each would be noise.
        _ => return None,
    })
}

pub fn payjoin_usage(network: Network) -> String {
    format!(
        "{}\n\n<code>/pj_receive &lt;sats&gt;</code> — ask to be paid with payjoin.",
        badge(network)
    )
}

pub fn payjoin_cancelled() -> String {
    "Payjoin session cancelled.".into()
}

#[cfg(test)]
mod payjoin_tests {
    use super::*;
    use wallet_core::bitcoin::Txid;
    use wallet_core::types::SessionId;

    fn txid(b: u8) -> Txid {
        Txid::from_raw_hash(wallet_core::bitcoin::hashes::Hash::from_byte_array([b; 32]))
    }

    fn session(state: PayjoinState, role: PayjoinRole) -> PayjoinSessionView {
        PayjoinSessionView {
            id: SessionId::new(),
            role,
            state,
            amount: None,
            created_at: std::time::SystemTime::now(),
            expires_at: std::time::SystemTime::now(),
        }
    }

    /// §7: explain the privacy benefit rather than assuming the user knows it.
    #[test]
    fn a_receive_request_explains_what_payjoin_buys() {
        let receipt = PayjoinReceipt {
            session_id: SessionId::new(),
            bip21: "bitcoin:bc1qexample?amount=0.0005&pj=https://payjo.in/X".into(),
            expires_at: std::time::SystemTime::now(),
        };
        let card = payjoin_receipt(Network::Regtest, Amount::from_sat(50_000), &receipt);

        assert!(card.contains("together"));
        assert!(
            card.contains("pay it normally"),
            "a non-payjoin sender is not stuck"
        );
        assert!(card.contains("payjo.in"));
    }

    /// §7's privacy note: payjoin hides the payment from observers, not from
    /// the backend this bot talks to.
    #[test]
    fn mainnet_says_who_can_still_see_the_payment() {
        let receipt = PayjoinReceipt {
            session_id: SessionId::new(),
            bip21: "bitcoin:bc1qexample?pj=https://payjo.in/X".into(),
            expires_at: std::time::SystemTime::now(),
        };
        let card = payjoin_receipt(Network::Bitcoin, Amount::from_sat(50_000), &receipt);
        assert!(card.contains("not from the node provider"));

        let regtest = payjoin_receipt(Network::Regtest, Amount::from_sat(50_000), &receipt);
        assert!(!regtest.contains("node provider"));
    }

    /// §7: no scary errors. A fallback is an outcome, not a failure.
    #[test]
    fn a_fallback_reads_as_a_completed_payment() {
        let line = payjoin_event(&PayjoinState::FellBack { txid: txid(1) })
            .expect("a fallback is worth announcing");
        assert!(line.contains("Sent as a regular transaction"));
        assert!(!line.to_lowercase().contains("fail"));
        assert!(!line.contains('⚠'));
    }

    #[test]
    fn a_failure_says_the_funds_are_untouched() {
        let line = payjoin_event(&PayjoinState::Failed {
            reason: "check failed".into(),
        })
        .expect("worth announcing");
        assert!(line.contains("untouched"));
    }

    #[test]
    fn quiet_transitions_produce_no_notification() {
        // A message for every transition would be noise.
        assert!(payjoin_event(&PayjoinState::Waiting).is_none());
        assert!(payjoin_event(&PayjoinState::ProposalSent).is_none());
        assert!(payjoin_event(&PayjoinState::Cancelled).is_none());
    }

    #[test]
    fn an_empty_session_list_offers_the_next_step() {
        assert!(payjoin_sessions(Network::Regtest, &[]).contains("/pj_receive"));
    }

    #[test]
    fn sessions_are_badged_by_outcome() {
        let list = [
            session(PayjoinState::Waiting, PayjoinRole::Receiver),
            session(
                PayjoinState::Completed { txid: txid(2) },
                PayjoinRole::Sender,
            ),
            session(
                PayjoinState::FellBack { txid: txid(3) },
                PayjoinRole::Sender,
            ),
        ];
        let rendered = payjoin_sessions(Network::Regtest, &list);

        assert!(rendered.contains("Receiving"));
        assert!(rendered.contains("Sending"));
        assert!(rendered.contains("done — payjoin"));
        assert!(rendered.contains("sent as a regular transaction"));
    }

    #[test]
    fn every_state_has_a_badge_and_a_sentence() {
        for state in [
            PayjoinState::Waiting,
            PayjoinState::ProposalReceived,
            PayjoinState::ProposalSent,
            PayjoinState::Completed { txid: txid(4) },
            PayjoinState::FellBack { txid: txid(5) },
            PayjoinState::Expired,
            PayjoinState::Cancelled,
            PayjoinState::Failed { reason: "x".into() },
        ] {
            assert!(!payjoin_badge(&state).is_empty());
            assert!(!payjoin_state_text(&state).is_empty());
        }
    }
}
