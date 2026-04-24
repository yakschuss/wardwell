use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use chrono::Local;

fn now_mmdd() -> String {
    Local::now().format("%m/%d").to_string()
}

pub fn append_ticket_log(
    vault_root: &Path,
    domain: &str,
    project: &str,
    line: &str,
) -> Result<(), io::Error> {
    let dir = vault_root.join(domain).join(project);
    fs::create_dir_all(&dir)?;

    let path = dir.join("tickets.md");

    let needs_header = !path.exists() || path.metadata()?.len() == 0;

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;

    if needs_header {
        writeln!(file, "# {project} Tickets")?;
        writeln!(file)?;
    }

    writeln!(file, "- {} {}", now_mmdd(), line)?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn creates_file_with_header() {
        let tmp = tempdir().unwrap();
        append_ticket_log(tmp.path(), "work", "myproject", "ticket created").unwrap();

        let content = fs::read_to_string(tmp.path().join("work/myproject/tickets.md")).unwrap();
        assert!(content.contains("# myproject Tickets"), "missing header");
        assert!(content.contains("ticket created"), "missing event line");
    }

    #[test]
    fn appends_to_existing_file() {
        let tmp = tempdir().unwrap();
        append_ticket_log(tmp.path(), "work", "proj", "first event").unwrap();
        append_ticket_log(tmp.path(), "work", "proj", "second event").unwrap();

        let content = fs::read_to_string(tmp.path().join("work/proj/tickets.md")).unwrap();
        assert!(content.contains("first event"), "missing first event");
        assert!(content.contains("second event"), "missing second event");

        // Header should appear exactly once
        let header_count = content.matches("# proj Tickets").count();
        assert_eq!(header_count, 1, "header should appear exactly once");
    }

    #[test]
    fn creates_project_dir_if_missing() {
        let tmp = tempdir().unwrap();
        // Do NOT pre-create the directory — the function must handle it
        let result = append_ticket_log(tmp.path(), "personal", "newproj", "dir auto-created");
        assert!(result.is_ok(), "expected Ok but got: {result:?}");

        let path = tmp.path().join("personal/newproj/tickets.md");
        assert!(path.exists(), "tickets.md should have been created");
    }
}
