//! Read-only mirror of an external issue tracker into the vault.
//!
//! Each bound project gets an append-only, provider-neutral event log at
//! `<domain>/<project>/tracker.jsonl`. Wardwell never writes back to the
//! tracker; the kanban readonly lock enforces that on the vault side.

pub mod events;
pub mod log;
pub mod credential;
