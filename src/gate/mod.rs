//! Hook gates: each reads a Claude Code PreToolUse payload and returns a deny
//! decision or nothing. A gate never blocks on a payload it cannot read.
//! Does NOT install hooks; `wardwell setup` wires them.

pub mod linear;
pub mod ruleset;
