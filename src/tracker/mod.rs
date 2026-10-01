//! Read-only mirror of an external issue tracker into the vault.
//!
//! Each bound project gets an append-only, provider-neutral event log at
//! `<domain>/<project>/tracker.jsonl`. Wardwell never writes back to the
//! tracker; the kanban readonly lock enforces that on the vault side.

pub mod adapter;
pub mod cli;
pub mod compact;
pub mod credential;
pub mod deadline;
pub mod doctor;
pub mod events;
pub mod freshness;
pub mod github;
pub mod items;
pub mod linear;
pub mod lock;
pub mod log;
pub mod pull;
pub mod refresh;
pub mod schedule;
pub mod state;
pub mod trigger;
pub mod view;

/// Provider ids Wardwell has an adapter for.
pub const SUPPORTED_PROVIDERS: &[&str] = &["linear", "github"];

/// The provider id of the merged-change mirror.
pub const GITHUB: &str = "github";

/// Display name for a provider id, for messages a person reads.
pub fn provider_label(provider: &str) -> String {
    match provider {
        "linear" => "Linear".to_string(),
        GITHUB => "GitHub".to_string(),
        other => other.to_string(),
    }
}

/// True for a provider whose mirror holds issues. The merged-change mirror
/// holds no issues, so kanban, session start, the read-only lock, and the
/// gate never read it.
pub fn mirrors_issues(provider: &str) -> bool {
    provider != GITHUB
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

/// Refusal for a kanban write addressed to `key`, an issue the mirror of
/// `binding` holds. Applies whether or not the binding is read-only, since
/// the issue lives in the tracker, not in the kanban.
pub fn mirrored_key_refusal(key: &str, binding: &crate::config::loader::TrackerBinding) -> String {
    let label = provider_label(&binding.provider);
    format!(
        "{key} is mirrored from {label} team {} into {}/{}; it is not a kanban ticket. Edit it in {label}; `wardwell tracker pull` brings the change into the vault.",
        binding.team, binding.domain, binding.project
    )
}

/// The sentence for a binding whose team key equals the native kanban
/// prefix of its project, or None when they differ. Ticket ids of the two
/// would be indistinguishable, so the mirror is left out of kanban reads.
pub fn prefix_collision(binding: &crate::config::loader::TrackerBinding, native_prefix: &str) -> Option<String> {
    let normal = |prefix: &str| prefix.trim_end_matches('-').to_ascii_uppercase();
    (normal(native_prefix) == normal(&binding.team)).then(|| {
        format!(
            "Tracker team key {} of {}/{} equals the native kanban prefix {native_prefix} of project {}. Set a different native prefix for {} in kanban.prefixes.",
            binding.team, binding.domain, binding.project, binding.project, binding.project
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
            gate: false,
            repository: None,
        }
    }

    #[test]
    fn refusal_names_the_provider_and_where_to_edit() {
        let message = readonly_refusal(&binding(true)).unwrap_or_default();
        assert!(message.contains("work/claims is a read-only mirror of Linear team COR"), "{message}");
        assert!(message.contains("Edit it in Linear"), "{message}");
        assert!(readonly_refusal(&binding(false)).is_none());
    }

    #[test]
    fn prefix_collision_compares_without_case_or_hyphen() {
        assert!(prefix_collision(&binding(true), "cor-").is_some());
        assert!(prefix_collision(&binding(true), "CL").is_none());
    }

    #[test]
    fn mirrored_key_refusal_names_the_key_provider_and_where_to_edit() {
        let message = mirrored_key_refusal("COR-12", &binding(false));
        assert!(message.starts_with("COR-12 is mirrored from Linear team COR into work/claims"), "{message}");
        assert!(message.contains("Edit it in Linear"), "{message}");
    }
}
