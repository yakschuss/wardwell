//! What `wardwell inject` prints for the vault domain a session starts in:
//! the domain's own state, or each project's summary, as before; then a rot
//! line under each project summary, and the tracker section for each bound
//! project.
//!
//! Does NOT choose the domain (main.rs matches it to the cwd) or pull.

use crate::config::loader::WardwellConfig;
use crate::inject::session::{project_rot_line, project_tracker_lines};
use chrono::{DateTime, NaiveDate, Utc};
use std::path::{Path, PathBuf};

/// The inject output for `domain_dir` at `now` and local date `today`.
/// Tracker credentials are checked, offline, in `config_dir`.
pub fn domain_context(config: &WardwellConfig, config_dir: &Path, domain_dir: &Path, now: DateTime<Utc>, today: NaiveDate) -> String {
    let domain = domain_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    let state = domain_dir.join("current_state.md");
    if state.exists()
        && let Ok(content) = std::fs::read_to_string(&state)
    {
        return with_bound_sections(content, config, config_dir, domain, domain_dir, now);
    }
    let mut out = String::new();
    for p in subdirectories(domain_dir) {
        let project = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
        let printed = push_summary(&mut out, domain, project, &p);
        if skipped(project) {
            continue;
        }
        let tracker = project_tracker_lines(config, config_dir, domain, &p, now);
        if printed {
            push_line(&mut out, &format!("  {}", project_rot_line(&p, today)));
        } else if tracker.is_some() {
            push_line(&mut out, &format!("**{domain}/{project}**"));
        }
        tracker.into_iter().flatten().for_each(|line| push_line(&mut out, &format!("  {line}")));
    }
    out
}

/// The domain state, then a header and tracker section per bound project.
/// The state ends with a newline before anything follows it.
fn with_bound_sections(mut out: String, config: &WardwellConfig, config_dir: &Path, domain: &str, domain_dir: &Path, now: DateTime<Utc>) -> String {
    for p in subdirectories(domain_dir) {
        let project = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
        if skipped(project) {
            continue;
        }
        let Some(lines) = project_tracker_lines(config, config_dir, domain, &p, now) else {
            continue;
        };
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        push_line(&mut out, &format!("**{domain}/{project}**"));
        lines.iter().for_each(|line| push_line(&mut out, &format!("  {line}")));
    }
    out
}

/// Hidden and underscore folders get no rot line or tracker section.
fn skipped(project: &str) -> bool {
    project.starts_with('.') || project.starts_with('_')
}

/// Subdirectories in directory order, as the summaries always listed them.
fn subdirectories(domain_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(domain_dir)
        .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

/// A project's status, focus and next action from its current_state.md.
/// False when it has none.
fn push_summary(out: &mut String, domain: &str, project: &str, p: &Path) -> bool {
    let state = p.join("current_state.md");
    if !state.exists() {
        return false;
    }
    let Ok(vf) = crate::vault::reader::read_file(&state) else {
        return false;
    };
    let status = vf.frontmatter.status.as_ref().map(|s| s.to_string()).unwrap_or_else(|| "active".to_string());
    let focus = extract_section(&vf.body, "Focus");
    let next = extract_section(&vf.body, "Next Action");
    push_line(out, &format!("**{domain}/{project}** ({status}): {focus}"));
    if !next.is_empty() {
        push_line(out, &format!("  Next: {next}"));
    }
    true
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// Text under `## <heading>` up to the next `## ` heading.
fn extract_section(body: &str, heading: &str) -> String {
    let marker = format!("## {heading}");
    let start = match body.find(&marker) {
        Some(pos) => pos + marker.len(),
        None => return String::new(),
    };
    let rest = body[start..].trim_start();
    let end = rest.find("\n## ").unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// The inject output before rot lines and tracker sections, kept
    /// verbatim to prove the new output only adds lines.
    fn old_output(domain_dir: &Path) -> String {
        let mut out = String::new();
        let domain = domain_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
        let state = domain_dir.join("current_state.md");
        if state.exists()
            && let Ok(content) = std::fs::read_to_string(&state)
        {
            out.push_str(&content);
            return out;
        }
        if let Ok(entries) = std::fs::read_dir(domain_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let state = p.join("current_state.md");
                    if state.exists()
                        && let Ok(vf) = crate::vault::reader::read_file(&state)
                    {
                        let project = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
                        let status = vf.frontmatter.status.as_ref().map(|s| s.to_string()).unwrap_or_else(|| "active".to_string());
                        let focus = extract_section(&vf.body, "Focus");
                        let next = extract_section(&vf.body, "Next Action");
                        out.push_str(&format!("**{domain}/{project}** ({status}): {focus}\n"));
                        if !next.is_empty() {
                            out.push_str(&format!("  Next: {next}\n"));
                        }
                    }
                }
            }
        }
        out
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    }

    /// A config over `vault` binding each `domain/project` in `bound`.
    fn config(dir: &Path, vault: &Path, bound: &[&str]) -> WardwellConfig {
        let mut yaml = format!("vault_path: {}\nsession_sources: []\n", vault.display());
        if !bound.is_empty() {
            yaml.push_str("trackers:\n");
            for key in bound {
                yaml.push_str(&format!("  {key}:\n    provider: linear\n    team: COR\n    credential: c\n"));
            }
        }
        std::fs::write(dir.join("config.yml"), yaml).unwrap();
        let credential = crate::tracker::credential::path_in(dir, "c").unwrap();
        if !credential.exists() {
            crate::tracker::credential::save(&credential, "t").unwrap();
        }
        crate::config::loader::load(Some(&dir.join("config.yml"))).unwrap()
    }

    /// A mirror at `project` whose last pull completed an hour before `now`.
    fn pulled_mirror(project: &Path) {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(
            project.join("tracker.jsonl"),
            "{\"_schema\":\"tracker\",\"_version\":\"1.0\"}\n{\"kind\":\"pull_completed\",\"id\":\"p\",\"provider\":\"linear\",\"external_key\":\"COR\",\"external_id\":\"COR\",\"occurred_at\":\"2026-09-30T11:00:00Z\",\"title\":\"p\"}\n",
        )
        .unwrap();
    }

    const SECTION: &str = "  Tracker mirror. Last pulled 1 hour ago. Not authoritative.\n";

    fn state(path: &Path, n: usize) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("current_state.md"),
            format!("---\nstatus: active\n---\n# proj{n}\n\n## Focus\nShip the thing number {n}.\n\n## Next Action\nOpen the PR.\n"),
        )
        .unwrap();
    }

    /// Sixty projects with state, history and decisions, plus a hidden
    /// folder, an archive with no vault files, and an empty attachments folder.
    fn sixty_project_vault(root: &Path) -> PathBuf {
        let work = root.join("vault/work");
        for n in 0..60 {
            let p = work.join(format!("proj{n:02}"));
            state(&p, n);
            std::fs::write(p.join("history.jsonl"), "{\"date\":\"2026-09-18\",\"title\":\"x\"}\n").unwrap();
            std::fs::write(p.join("decisions.md"), "# Decisions\n\n## 2026-09-27 — Pick\n").unwrap();
        }
        std::fs::create_dir_all(work.join(".obsidian")).unwrap();
        std::fs::create_dir_all(work.join("archive/oldproj")).unwrap();
        std::fs::create_dir_all(work.join("attachments")).unwrap();
        work
    }

    #[test]
    fn sixty_projects_print_the_old_output_plus_one_rot_line_each() {
        let dir = tempfile::tempdir().unwrap();
        let work = sixty_project_vault(dir.path());
        let config = config(dir.path(), &dir.path().join("vault"), &[]);
        let new = domain_context(&config, dir.path(), &work, now(), today());
        let rot = "  Last history entry 12 days ago. Last decision 3 days ago.";
        let mut expected = String::new();
        for line in old_output(&work).lines() {
            if !expected.is_empty() && line.starts_with("**") {
                expected.push_str(rot);
                expected.push('\n');
            }
            expected.push_str(line);
            expected.push('\n');
        }
        expected.push_str(rot);
        expected.push('\n');
        assert_eq!(new, expected);
        assert_eq!(new.matches(rot).count(), 60);
        assert!(!new.contains("archive") && !new.contains("attachments") && !new.contains(".obsidian"));
    }

    #[test]
    fn a_bound_project_adds_its_tracker_section_under_its_rot_line() {
        let dir = tempfile::tempdir().unwrap();
        let work = sixty_project_vault(dir.path());
        pulled_mirror(&work.join("proj03"));
        let config = config(dir.path(), &dir.path().join("vault"), &["work/proj03"]);
        let new = domain_context(&config, dir.path(), &work, now(), today());
        let block = "**work/proj03** (active): Ship the thing number 3.\n  Next: Open the PR.\n  Last history entry 12 days ago. Last decision 3 days ago.\n  Tracker mirror. Last pulled 1 hour ago. Not authoritative.\n";
        assert!(new.contains(block), "{new}");
        assert_eq!(new.matches("Tracker mirror.").count(), 1);
        let without: String = new.lines().filter(|l| !l.starts_with("  Tracker mirror.") && !l.starts_with("  Last history")).map(|l| format!("{l}\n")).collect();
        assert_eq!(without, old_output(&work));
    }

    #[test]
    fn domain_state_prints_the_old_output_plus_bound_projects_only() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("vault/work");
        state(&work.join("alpha"), 1);
        pulled_mirror(&work.join("beta"));
        pulled_mirror(&work.join("_templates"));
        std::fs::write(work.join("current_state.md"), "# Work\n\n## Focus\nDomain focus line.\n").unwrap();
        let config = config(dir.path(), &dir.path().join("vault"), &["work/beta", "work/_templates"]);
        let new = domain_context(&config, dir.path(), &work, now(), today());
        assert_eq!(new, format!("{}**work/beta**\n{SECTION}", old_output(&work)));

        let unbound = domain_context(&config_without_trackers(dir.path()), dir.path(), &work, now(), today());
        assert_eq!(unbound, old_output(&work), "no binding, no change at all");
    }

    fn config_without_trackers(dir: &Path) -> WardwellConfig {
        let other = dir.join("plain");
        std::fs::create_dir_all(&other).unwrap();
        config(&other, &dir.join("vault"), &[])
    }

    #[test]
    fn a_domain_state_without_a_trailing_newline_does_not_glue_to_the_next_header() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("vault/work");
        pulled_mirror(&work.join("beta"));
        std::fs::write(work.join("current_state.md"), "# Work\n\n## Next Action\nLAST LINE NO NEWLINE").unwrap();
        let config = config(dir.path(), &dir.path().join("vault"), &["work/beta"]);
        let new = domain_context(&config, dir.path(), &work, now(), today());
        assert!(new.starts_with("# Work\n\n## Next Action\nLAST LINE NO NEWLINE\n**work/beta**\n"), "{new}");
    }

    #[test]
    fn a_bound_project_without_state_gets_a_header_and_its_section() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("vault/work");
        state(&work.join("alpha"), 1);
        pulled_mirror(&work.join("beta"));
        let config = config(dir.path(), &dir.path().join("vault"), &["work/beta"]);
        let new = domain_context(&config, dir.path(), &work, now(), today());
        assert!(new.contains(&format!("**work/beta**\n{SECTION}")), "{new}");
        assert!(!new.contains("**work/beta**\n  No history"), "no rot line for a project the summaries did not print");
    }
}
