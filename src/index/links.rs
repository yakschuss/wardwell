use crate::index::store::Link;
use regex::Regex;
use std::sync::LazyLock;

static WIKI_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]*)?\]\]")
        .unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| std::process::exit(1)))
});

static CALLSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z]{2,8})-(\d{1,5})\b")
        .unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| std::process::exit(1)))
});

/// Extract all links from a vault file's body and frontmatter related field.
pub fn extract_links(
    body: &str,
    related: &[String],
    all_paths: &[String],
) -> Vec<Link> {
    let mut links = Vec::new();

    // 1. Frontmatter `related:` entries — direct path references
    for path in related {
        links.push(Link {
            target_path: normalize_path(path),
            link_type: "related".to_string(),
            line_number: None,
            context: None,
        });
    }

    // 2. Wiki-links [[target]] or [[target|display]]
    for (line_num, line) in body.lines().enumerate() {
        for cap in WIKI_LINK.captures_iter(line) {
            if let Some(m) = cap.get(1) {
                let raw_target = m.as_str().trim();
                if let Some(resolved) = resolve_wiki_link(raw_target, all_paths) {
                    let ctx = extract_context(line, 120);
                    links.push(Link {
                        target_path: resolved,
                        link_type: "wiki".to_string(),
                        line_number: Some(line_num + 1),
                        context: Some(ctx),
                    });
                }
            }
        }

        // 3. Callsign references (e.g. PROJ-12)
        for cap in CALLSIGN.captures_iter(line) {
            if let Some(m) = cap.get(0) {
                let callsign = m.as_str();
                let ctx = extract_context(line, 120);
                links.push(Link {
                    target_path: format!("kanban:{callsign}"),
                    link_type: "callsign".to_string(),
                    line_number: Some(line_num + 1),
                    context: Some(ctx),
                });
            }
        }
    }

    // Dedup by (target_path, link_type, line_number)
    let mut seen = std::collections::HashSet::new();
    links.retain(|l| {
        seen.insert((l.target_path.clone(), l.link_type.clone(), l.line_number))
    });

    links
}

fn resolve_wiki_link(target: &str, all_paths: &[String]) -> Option<String> {
    let target_lower = target.to_lowercase();

    // Exact path match (e.g. [[domain/project/file.md]])
    for path in all_paths {
        if path.to_lowercase() == target_lower
            || path.to_lowercase() == format!("{target_lower}.md")
        {
            return Some(path.clone());
        }
    }

    // Filename match (e.g. [[file]] matches domain/project/file.md)
    for path in all_paths {
        let filename = path.rsplit('/').next().unwrap_or(path);
        let stem = filename.strip_suffix(".md").unwrap_or(filename);
        if stem.to_lowercase() == target_lower {
            return Some(path.clone());
        }
    }

    // Title-based match would require index lookup — return unresolved for now
    None
}

fn normalize_path(path: &str) -> String {
    let clean = path.strip_prefix('/').unwrap_or(path);
    clean.to_string()
}

fn extract_context(line: &str, max_len: usize) -> String {
    let trimmed = line.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let end = trimmed.floor_char_boundary(max_len);
        format!("{}...", &trimmed[..end])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn extract_wiki_links() {
        let body = "See [[myapp]] for details.\nAlso check [[auth|Auth System]].";
        let paths = vec!["myapp.md".to_string(), "myapp/auth.md".to_string()];
        let links = extract_links(body, &[], &paths);
        let wiki_links: Vec<_> = links.iter().filter(|l| l.link_type == "wiki").collect();
        assert_eq!(wiki_links.len(), 2);
        assert_eq!(wiki_links[0].target_path, "myapp.md");
        assert_eq!(wiki_links[1].target_path, "myapp/auth.md");
    }

    #[test]
    fn extract_callsign_refs() {
        let body = "Working on PROJ-12 and WARD-3 today.";
        let links = extract_links(body, &[], &[]);
        let callsigns: Vec<_> = links.iter().filter(|l| l.link_type == "callsign").collect();
        assert_eq!(callsigns.len(), 2);
        assert_eq!(callsigns[0].target_path, "kanban:PROJ-12");
        assert_eq!(callsigns[1].target_path, "kanban:WARD-3");
    }

    #[test]
    fn extract_related_refs() {
        let related = vec!["myapp.md".to_string(), "wardwell.md".to_string()];
        let links = extract_links("", &related, &[]);
        let related_links: Vec<_> = links.iter().filter(|l| l.link_type == "related").collect();
        assert_eq!(related_links.len(), 2);
    }

    #[test]
    fn wiki_link_resolves_by_filename() {
        let paths = vec!["personal/journal/notes.md".to_string()];
        let body = "Check [[notes]] for reference.";
        let links = extract_links(body, &[], &paths);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_path, "personal/journal/notes.md");
    }

    #[test]
    fn unresolved_wiki_link_skipped() {
        let body = "See [[nonexistent]] here.";
        let links = extract_links(body, &[], &[]);
        let wiki_links: Vec<_> = links.iter().filter(|l| l.link_type == "wiki").collect();
        assert_eq!(wiki_links.len(), 0);
    }

    #[test]
    fn line_numbers_are_1_indexed() {
        let body = "line one\n[[myapp]] on line two\nline three";
        let paths = vec!["myapp.md".to_string()];
        let links = extract_links(body, &[], &paths);
        assert_eq!(links[0].line_number, Some(2));
    }

    #[test]
    fn dedup_same_link_same_line() {
        let body = "See [[myapp]] and also [[myapp]] again.";
        let paths = vec!["myapp.md".to_string()];
        let links = extract_links(body, &[], &paths);
        let wiki_links: Vec<_> = links.iter().filter(|l| l.link_type == "wiki").collect();
        assert_eq!(wiki_links.len(), 1);
    }
}
