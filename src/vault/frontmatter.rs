use crate::vault::types::{Frontmatter, VaultError};

/// Parse frontmatter from a vault file's content.
/// Expects `---` delimiters. Returns (Frontmatter, body).
/// `type` is required; all other fields are optional.
/// Unknown fields are ignored (forward compatible).
pub fn parse_frontmatter(content: &str) -> Result<(Frontmatter, String), VaultError> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Err(VaultError::NoFrontmatter);
    }

    // Find the closing ---
    let after_opening = &trimmed[3..];
    let closing_pos = after_opening
        .find("\n---")
        .ok_or(VaultError::UnclosedFrontmatter)?;

    let yaml_str = &after_opening[..closing_pos];
    let body_start = closing_pos + 4; // skip \n---
    let body = after_opening[body_start..].trim_start_matches('\n').to_string();

    let frontmatter: Frontmatter = serde_yaml::from_str(yaml_str)?;

    Ok((frontmatter, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vault::types::*;

    #[test]
    fn parse_project() {
        let content = "---\ntype: project\ndomain: myapp\nstatus: active\nsummary: Project management tool\n---\n## Summary\nSome body here.\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, body) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Project);
        assert_eq!(fm.domain.as_deref(), Some("myapp"));
        assert_eq!(fm.status, Some(Status::Active));
        assert!(body.contains("## Summary"));
    }

    #[test]
    fn parse_decision() {
        let content = "---\ntype: decision\nstatus: resolved\nconfidence: confirmed\n---\n## Context\nDecision body.\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Decision);
        assert_eq!(fm.status, Some(Status::Resolved));
        assert_eq!(fm.confidence, Some(Confidence::Confirmed));
    }

    #[test]
    fn parse_insight() {
        let content = "---\ntype: insight\nconfidence: inferred\ntags: [rust, debugging]\n---\n## Pattern\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Insight);
        assert_eq!(fm.confidence, Some(Confidence::Inferred));
        assert_eq!(fm.tags, vec!["rust", "debugging"]);
    }

    #[test]
    fn parse_thread() {
        let content = "---\ntype: thread\nstatus: active\n---\n## Question\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Thread);
    }

    #[test]
    fn parse_reference() {
        let content = "---\ntype: reference\nrelated: [myapp.md, auth.md]\n---\n## Source\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Reference);
        assert_eq!(fm.related, vec!["myapp.md", "auth.md"]);
    }

    #[test]
    fn parse_all_status_variants() {
        for status in ["active", "resolved", "abandoned", "superseded"] {
            let content = format!("---\ntype: project\nstatus: {status}\n---\nbody\n");
            let result = parse_frontmatter(&content);
            assert!(result.is_ok(), "failed to parse status: {status}");
        }
    }

    #[test]
    fn parse_all_confidence_variants() {
        for conf in ["inferred", "proposed", "confirmed"] {
            let content = format!("---\ntype: insight\nconfidence: {conf}\n---\nbody\n");
            let result = parse_frontmatter(&content);
            assert!(result.is_ok(), "failed to parse confidence: {conf}");
        }
    }

    #[test]
    fn parse_with_date() {
        let content = "---\ntype: project\nupdated: 2026-02-15\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert!(fm.updated.is_some());
        assert_eq!(fm.updated.map(|d| d.to_string()), Some("2026-02-15".to_string()));
    }

    #[test]
    fn missing_type_defaults_to_reference() {
        let content = "---\ndomain: test\nstatus: active\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Reference);
    }

    #[test]
    fn unknown_fields_ignored() {
        let content = "---\ntype: project\nfuture_field: something\nalso_unknown: 42\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Project);
    }

    #[test]
    fn no_frontmatter_errors() {
        let content = "Just some markdown without frontmatter.\n";
        let result = parse_frontmatter(content);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn unclosed_frontmatter_errors() {
        let content = "---\ntype: project\nNo closing delimiter\n";
        let result = parse_frontmatter(content);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn parse_datetime_updated() {
        let content = "---\ntype: project\nupdated: 2026-02-15 11:00\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.updated.map(|d| d.to_string()), Some("2026-02-15".to_string()));
    }

    #[test]
    fn unknown_type_becomes_reference() {
        let content = "---\ntype: exploration\nstatus: active\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Reference);
        assert_eq!(fm.status, Some(Status::Active));
    }

    #[test]
    fn unknown_status_becomes_none() {
        let content = "---\ntype: project\nstatus: draft\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Project);
        assert!(fm.status.is_none());
    }

    #[test]
    fn minimal_frontmatter() {
        let content = "---\ntype: insight\n---\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, body) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Insight);
        assert!(fm.domain.is_none());
        assert!(fm.status.is_none());
        assert!(fm.confidence.is_none());
        assert!(fm.updated.is_none());
        assert!(fm.summary.is_none());
        assert!(fm.related.is_empty());
        assert!(fm.tags.is_empty());
        assert!(body.is_empty());
    }

    #[test]
    fn parse_domain_with_can_read() {
        let content = "---\ntype: domain\ndomain: wardwell\nconfidence: confirmed\ncan_read: [personal, general]\n---\n## Paths\n- ~/Code/wardwell/*\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert_eq!(fm.file_type, VaultType::Domain);
        assert_eq!(fm.can_read, vec!["personal", "general"]);
    }

    #[test]
    fn parse_without_can_read_defaults_empty() {
        let content = "---\ntype: project\ndomain: test\n---\nbody\n";
        let result = parse_frontmatter(content);
        assert!(result.is_ok(), "{result:?}");
        let (fm, _) = result.unwrap();
        assert!(fm.can_read.is_empty());
    }
}
