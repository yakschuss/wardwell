//! Read-only mirror of an external issue tracker into the vault.
//!
//! Each bound project gets an append-only, provider-neutral event log at
//! `<domain>/<project>/tracker.jsonl`. Wardwell never writes back to the
//! tracker; the kanban readonly lock enforces that on the vault side.

pub mod events;
pub mod log;
pub mod credential;
pub mod adapter;
pub mod linear;
pub mod pull;
pub mod cli;

/// Display name for a provider id, for messages a person reads.
pub fn provider_label(provider: &str) -> String {
    match provider {
        "linear" => "Linear".to_string(),
        other => other.to_string(),
    }
}
