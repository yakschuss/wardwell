pub mod audit;
pub mod events;
pub mod jsonl;
pub mod plan;
pub mod prefix;
pub mod proposals;
pub mod questions;
pub mod reality_check;
pub mod relationships;
pub mod status;
pub mod store;
pub mod verification;

/// The `(domain, project)` of a vault-relative path that is native kanban
/// text: the ticket log, the event, proposal, question, relationship and
/// verification logs, and the status snapshots. None for any other file,
/// including the tracker mirror, which stays readable when the board is off.
pub fn native_file_project(rel_path: &str) -> Option<(&str, &str)> {
    let mut parts = rel_path.split('/');
    let (domain, project, file) = (parts.next()?, parts.next()?, parts.next()?);
    let native = matches!(
        file,
        "tickets.md" | "kanban.jsonl" | "proposals.jsonl" | "questions.jsonl" | "relationships.jsonl" | "verifications.jsonl"
    ) || (file == "status" && parts.next().is_some());
    (native && !domain.is_empty() && !project.is_empty()).then_some((domain, project))
}

#[cfg(test)]
mod tests {
    use super::native_file_project;

    #[test]
    fn native_files_name_their_project_and_the_mirror_is_not_native() {
        assert_eq!(native_file_project("work/claims/tickets.md"), Some(("work", "claims")));
        assert_eq!(native_file_project("work/claims/kanban.jsonl"), Some(("work", "claims")));
        assert_eq!(native_file_project("work/claims/status/x-status-2026-10-01.md"), Some(("work", "claims")));
        assert_eq!(native_file_project("work/claims/tracker.jsonl"), None);
        assert_eq!(native_file_project("work/claims/current_state.md"), None);
        assert_eq!(native_file_project("work/claims/status"), None);
        assert_eq!(native_file_project("tickets.md"), None);
    }
}
