//! The one seam between Wardwell and a tracker provider.
//!
//! An adapter reads from its provider and translates into Wardwell events.
//! Does NOT write to the provider, the vault, or any cursor store.

use crate::tracker::events::Event;
use chrono::{DateTime, Utc};

/// A read-only source of Wardwell events for one provider team.
pub trait Adapter {
    /// Return events for issues updated at or after `since`; every issue
    /// (including archived) when `full` is true or `since` is None.
    fn pull(&self, since: Option<DateTime<Utc>>, full: bool) -> Result<Vec<Event>, String>;
}
