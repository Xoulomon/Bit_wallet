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
    // `rename_rule = "lowercase"` would make these `/pjreceive` and
    // `/pjsessions`. §8.2 specifies the underscores, so name them explicitly
    // rather than let the rule mangle them — an unregistered command is a
    // command that silently does nothing.
    #[command(
        rename = "pj_receive",
        description = "receive via payjoin: /pj_receive <sats>"
    )]
    PjReceive { sats: String },
    #[command(rename = "pj_sessions", description = "list your payjoin sessions")]
    PjSessions,
    #[command(description = "backend and session status")]
    Status,
    #[command(description = "which chain this bot is bound to")]
    Network,
    #[command(description = "regtest only, admins only: /mine <n>")]
    Mine { blocks: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §8.2 names every command, and a name that does not round-trip is a
    /// command that silently does nothing.
    ///
    /// Regression: `rename_rule = "lowercase"` turned `PjReceive` into
    /// `pjreceive`, so `/pj_receive` was an unhandled update — no error, no
    /// reply, nothing. Parsing is the honest check: it is exactly what the
    /// dispatcher does with an incoming message.
    #[test]
    fn every_command_in_the_spec_parses() {
        let spec = [
            "/start",
            "/help",
            "/create",
            "/restore",
            "/unlock",
            "/lock",
            "/export",
            "/delete",
            "/receive",
            "/addresses",
            "/balance",
            "/history",
            "/tx abc",
            "/send bc1qxxx 1000",
            "/bumpfee abc",
            "/pj_receive 25000",
            "/pj_sessions",
            "/status",
            "/network",
            "/mine 1",
        ];

        for input in spec {
            assert!(
                Command::parse(input, "bitgram_wallet_bot").is_ok(),
                "§8.2 lists `{input}` but it does not parse, so it would be ignored in silence"
            );
        }
    }

    /// The underscored names specifically, since a rename rule is what broke
    /// them and a rule applies to everything at once.
    #[test]
    fn the_payjoin_commands_keep_their_underscores() {
        assert_eq!(
            Command::parse("/pj_receive 25000", "b").expect("parses"),
            Command::PjReceive {
                sats: "25000".into()
            }
        );
        assert_eq!(
            Command::parse("/pj_sessions", "b").expect("parses"),
            Command::PjSessions
        );

        // And the mangled forms are not what we answer to.
        assert!(Command::parse("/pjreceive 25000", "b").is_err());
    }

    /// The menu Telegram is given must be the set that parses, or the native
    /// command list offers a user something the bot ignores.
    #[test]
    fn the_registered_menu_matches_what_parses() {
        for command in Command::bot_commands() {
            let name = &command.command;
            // Bare, or with an argument for the variants that take one.
            let bare = Command::parse(name, "b").is_ok();
            let with_arg = Command::parse(&format!("{name} 1"), "b").is_ok();
            assert!(
                bare || with_arg,
                "`{name}` is offered in Telegram's menu but parses as nothing, \
                 so tapping it would do nothing at all"
            );
        }
    }

    #[test]
    fn an_unknown_command_does_not_parse_so_the_fallback_catches_it() {
        assert!(Command::parse("/nonsense", "b").is_err());
        assert!(Command::parse("hello", "b").is_err());
    }
}
