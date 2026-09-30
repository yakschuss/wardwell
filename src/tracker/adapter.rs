//! The one seam between Wardwell and a tracker provider.
//!
//! An adapter reads from its provider and translates into Wardwell events.
//! Does NOT write to the provider, the vault, or any cursor store.

use crate::tracker::events::Event;
use chrono::{DateTime, Utc};

/// Receives one page of events as the adapter reads it. An error stops the pull.
pub type Sink<'a> = dyn FnMut(Vec<Event>) -> Result<(), String> + 'a;

/// A read-only source of Wardwell events for one provider team.
pub trait Adapter {
    /// Hand `sink` the events for issues updated at or after `since`, page by
    /// page; every issue (including archived) when `full` is true or `since`
    /// is None. Ok means every page was delivered.
    fn pull(&self, since: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String>;
}
