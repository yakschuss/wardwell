//! Read-only mirror of an external issue tracker into the vault.
//!
//! Each bound project gets an append-only, provider-neutral event log at
//! `<domain>/<project>/tracker.jsonl`. Wardwell never writes back to the
//! tracker; the kanban readonly lock enforces that on the vault side.

pub mod adapter;
pub mod cli;
pub mod credential;
pub mod events;
pub mod linear;
pub mod log;
pub mod pull;
pub mod schedule;

/// Display name for a provider id, for messages a person reads.
pub fn provider_label(provider: &str) -> String {
    match provider {
        "linear" => "Linear".to_string(),
        other => other.to_string(),
    }
}

/// Kanban actions that append to any file under the project's folder or its
/// ticket audit log, or (export_roadmap) have the roadmap service save a PDF
/// there. A readonly tracker binding refuses all of them; reads are unaffected.
pub const LOCKED_KANBAN_ACTIONS: &[&str] = &[
    "create", "update", "move", "note", "attach", "detach", "sequence", "groom",
    "relationship_create", "relationship_delete",
    "question_create", "question_update", "question_answer", "question_invalidate",
    "proposal_create", "proposal_approve", "proposal_reject", "proposal_apply",
    "verify", "status", "export_roadmap",
];

/// Refusal for a kanban write on a project mirrored read-only, or None when
/// the binding allows writes.
pub fn readonly_refusal(binding: &crate::config::loader::TrackerBinding) -> Option<String> {
    let label = provider_label(&binding.provider);
    binding.readonly.then(|| {
        format!(
            "{}/{} is a read-only mirror of {label} team {}. Edit it in {label}; `wardwell tracker pull` brings the change into the vault.",
            binding.domain, binding.project, binding.team
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::loader::TrackerBinding;

    fn binding(readonly: bool) -> TrackerBinding {
        TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "c".into(),
            readonly,
        }
    }

    #[test]
    fn refusal_names_the_provider_and_where_to_edit() {
        let message = readonly_refusal(&binding(true)).unwrap_or_default();
        assert!(message.contains("work/claims is a read-only mirror of Linear team COR"), "{message}");
        assert!(message.contains("Edit it in Linear"), "{message}");
        assert!(readonly_refusal(&binding(false)).is_none());
    }
}
