//! Command handlers (PLAN.md §8).
//!
//! Every handler reads as *parse → call core → render*. A handler that contains
//! bitcoin logic is a bug in the layering, not a shortcut.

pub mod start;
pub mod wallet;
