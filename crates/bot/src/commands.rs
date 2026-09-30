//! The command surface (PLAN.md §8.2).
//!
//! Registered with `BotCommands` so Telegram shows the native menu. One intent
//! per command — no overloaded `/wallet do-thing` verbs (§8.1). Every variant
//! maps to one or more calls on `WalletService` and nothing else.

use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug, PartialEq)]
#[command(
    rename_rule = "lowercase",
    description = "A non-custodial Bitcoin wallet. Commands:"
)]
pub enum Command {
    #[command(description = "start here, or return to the main menu")]
    Start,
    #[command(description = "show this list")]
    Help,
    #[command(description = "create a new wallet")]
    Create,
    #[command(description = "restore a wallet from a seed phrase")]
    Restore,
    #[command(description = "unlock for signing")]
    Unlock,
    #[command(description = "lock immediately")]
    Lock,
    #[command(description = "show your seed phrase (PIN required)")]
    Export,
    #[command(description = "delete your wallet (PIN required)")]
    Delete,
    #[command(description = "show a receiving address")]
    Receive,
    #[command(description = "list your addresses")]
    Addresses { page: String },
    #[command(description = "show your balance")]
    Balance,
    #[command(description = "list your transactions")]
    History { page: String },
    #[command(description = "show one transaction")]
    Tx { txid: String },
    #[command(description = "send bitcoin: /send <address|bip21> [amount]")]
    Send { args: String },
    #[command(description = "raise the fee on a stuck transaction")]
    Bumpfee { txid: String },
    #[command(description = "receive via payjoin: /pj_receive <sats>")]
    PjReceive { sats: String },
    #[command(description = "list your payjoin sessions")]
    PjSessions,
    #[command(description = "backend and session status")]
    Status,
    #[command(description = "which chain this bot is bound to")]
    Network,
    #[command(description = "regtest only, admins only: /mine <n>")]
    Mine { blocks: String },
}
