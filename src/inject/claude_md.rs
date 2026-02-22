use std::path::Path;

const START_MARKER: &str = "<!-- wardwell:start -->";
const END_MARKER: &str = "<!-- wardwell:end -->";

/// Errors from CLAUDE.md injection.
#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Inject content into a CLAUDE.md file between wardwell markers.
/// If markers exist, replaces content between them.
/// If no markers exist, appends markers + content at the end.
/// Content outside the markers is never modified.
pub fn inject(path: &Path, content: &str) -> Result<(), InjectError> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let injected = format!("{START_MARKER}\n{content}\n{END_MARKER}");

    let new_content = if let Some(start_pos) = existing.find(START_MARKER) {
        if let Some(end_pos) = existing.find(END_MARKER) {
            let end_of_marker = end_pos + END_MARKER.len();
            format!("{}{injected}{}", &existing[..start_pos], &existing[end_of_marker..])
        } else {
            // Start marker exists but no end marker — replace from start to end of file
            format!("{}{injected}", &existing[..start_pos])
        }
    } else {
        // No markers — append
        if existing.is_empty() {
            injected
        } else {
            format!("{existing}\n\n{injected}")
        }
    };

    std::fs::write(path, new_content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_into_empty_file() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| std::process::exit(1));
        let path = dir.path().join("CLAUDE.md");

        let result = inject(&path, "wardwell context here");
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains(START_MARKER));
        assert!(content.contains(END_MARKER));
        assert!(content.contains("wardwell context here"));
    }

    #[test]
    fn inject_appends_to_existing_file() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| std::process::exit(1));
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(&path, "# Existing Content\n\nKeep this.\n").ok();

        let result = inject(&path, "injected content");
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("# Existing Content"));
        assert!(content.contains("Keep this."));
        assert!(content.contains("injected content"));
        assert!(content.contains(START_MARKER));
    }

    #[test]
    fn inject_replaces_between_markers() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| std::process::exit(1));
        let path = dir.path().join("CLAUDE.md");
        let existing = format!(
            "# Header\n\n{START_MARKER}\nold content\n{END_MARKER}\n\n# Footer\n"
        );
        std::fs::write(&path, &existing).ok();

        let result = inject(&path, "new content");
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("# Header"));
        assert!(content.contains("# Footer"));
        assert!(content.contains("new content"));
        assert!(!content.contains("old content"));
    }

    #[test]
    fn inject_preserves_content_outside_markers() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| std::process::exit(1));
        let path = dir.path().join("CLAUDE.md");
        let existing = format!(
            "# Before\nImportant stuff.\n\n{START_MARKER}\nold\n{END_MARKER}\n\n# After\nMore stuff.\n"
        );
        std::fs::write(&path, &existing).ok();

        inject(&path, "replaced").ok();

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("# Before"));
        assert!(content.contains("Important stuff."));
        assert!(content.contains("# After"));
        assert!(content.contains("More stuff."));
        assert!(content.contains("replaced"));
    }

    #[test]
    fn inject_is_idempotent() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| std::process::exit(1));
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(&path, "# Existing\n").ok();

        inject(&path, "content v1").ok();
        inject(&path, "content v2").ok();

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("content v2"));
        assert!(!content.contains("content v1"));
        // Should only have one pair of markers
        assert_eq!(content.matches(START_MARKER).count(), 1);
        assert_eq!(content.matches(END_MARKER).count(), 1);
    }
}
