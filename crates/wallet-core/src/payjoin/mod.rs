//! Payjoin (PLAN.md §7): BIP77 v2 send and receive, plus sending to BIP78 v1
//! endpoints such as BTCPay.
//!
//! Core runs the protocol and publishes `CoreEvent::Payjoin` at each state
//! change; the front end renders a badge and never touches a typestate (§3a).

pub mod persist;
pub mod receive;
pub mod send;
