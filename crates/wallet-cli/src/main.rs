//! The second front end (PLAN.md §9, Step 8).
//!
//! Deliberately tiny, and deliberately not optional: anything this binary
//! cannot do without reaching past `WalletService` is a leak in the boundary
//! (§3a). It is also the headless driver for the integration tests, which need
//! no Telegram token.
//!
//! Step 8 fills it in — `create`, `receive`, `balance`, `send`, `status` over
//! the same facade, printing the same `CoreEvent`s to stdout.

fn main() {
    eprintln!("wallet-cli is implemented in Step 8; see PLAN.md §9.");
}
