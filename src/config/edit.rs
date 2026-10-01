//! Adds one directory to a project's `paths` in config.yml as a text edit, so
//! comments, key order and every other key survive byte for byte. The crate
//! has only serde_yaml, whose round trip drops comments, so the edit is
//! textual and then verified: the result must parse, every key outside
//! `projects` must be unchanged, and `projects` must differ by exactly the
//! one added path.
//!
//! Does NOT read or write files, back up, or decide which project or path.

use crate::config::loader::{ProjectMapping, parse};
use std::path::{Path, PathBuf};

/// `text` with `dir` added to the `paths` of project `key`. Err when the
/// file's shape is one this edit does not handle, with what to do instead.
pub fn add_project_path(text: &str, key: &str, dir: &Path) -> Result<String, String> {
    let before = parse(text).map_err(|e| format!("config.yml does not parse: {e}"))?;
    let item = quoted(&dir.to_string_lossy());
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let edited = match top_level(&lines, "projects") {
        None => append_section(text, key, &item),
        Some(at) => insert_in_section(&lines, at, key, &item)?,
    };
    verify(text, &edited, &before.projects, key, dir)?;
    Ok(edited)
}

const HAND_EDIT: &str = "edit config.yml by hand, or rewrite `projects:` as an indented block";

fn append_section(text: &str, key: &str, item: &str) -> String {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!("projects:\n{}", entry("  ", key, item)));
    out
}

fn entry(indent: &str, key: &str, item: &str) -> String {
    format!("{indent}{}:\n{indent}{indent}paths:\n{indent}{indent}  - {item}\n", yaml_key(key))
}

/// Insert under the `projects:` line at `at`: a new path in an existing
/// entry, or a new entry after the section's last indented line.
fn insert_in_section(lines: &[&str], at: usize, key: &str, item: &str) -> Result<String, String> {
    if !rest_is_empty(lines[at], "projects:") {
        return Err(format!("`projects:` is not an indented block; {HAND_EDIT}"));
    }
    let end = section_end(lines, at + 1, 0);
    let children: Vec<usize> = (at + 1..end).filter(|&i| content(lines[i]).is_some() && indent(lines[i]) > 0).collect();
    let child_indent = children.first().map_or(2, |&i| indent(lines[i]));
    let existing = children.iter().copied().find(|&i| indent(lines[i]) == child_indent && key_of(lines[i]).as_deref() == Some(key));
    let (insert_at, insertion) = match existing {
        Some(k) => add_to_entry(lines, k, item)?,
        None => (children.last().map_or(at + 1, |&i| i + 1), entry(&" ".repeat(child_indent), key, item)),
    };
    let mut out: String = lines[..insert_at].concat();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&insertion);
    out.push_str(&lines[insert_at..].concat());
    Ok(out)
}

/// The line index and text that add `item` after the last `paths` item of
/// the entry whose key line is `k`.
fn add_to_entry(lines: &[&str], k: usize, item: &str) -> Result<(usize, String), String> {
    let end = section_end(lines, k + 1, indent(lines[k]));
    let paths = (k + 1..end)
        .find(|&i| content(lines[i]).is_some_and(|c| c.starts_with("paths:")))
        .ok_or_else(|| format!("the entry has no `paths:` line; {HAND_EDIT}"))?;
    if !rest_is_empty(lines[paths], "paths:") {
        return Err(format!("`paths:` is not an indented list; {HAND_EDIT}"));
    }
    let items_end = section_end(lines, paths + 1, indent(lines[paths]));
    let items: Vec<usize> = (paths + 1..items_end).filter(|&i| content(lines[i]).is_some_and(|c| c.starts_with('-'))).collect();
    let last = items.last().ok_or_else(|| format!("`paths:` has no items; {HAND_EDIT}"))?;
    Ok((last + 1, format!("{}- {item}\n", " ".repeat(indent(lines[*last])))))
}

/// The first line from `from` that has content at or left of `parent`
/// indent: where a block nested under that parent ends.
fn section_end(lines: &[&str], from: usize, parent: usize) -> usize {
    (from..lines.len()).find(|&i| content(lines[i]).is_some() && indent(lines[i]) <= parent).unwrap_or(lines.len())
}

fn top_level(lines: &[&str], key: &str) -> Option<usize> {
    lines.iter().position(|l| indent(l) == 0 && key_of(l).as_deref() == Some(key))
}

/// The line without indent, or None for blank and comment lines.
fn content(line: &str) -> Option<&str> {
    let c = line.trim();
    (!c.is_empty() && !c.starts_with('#')).then_some(c)
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// The mapping key on a `key:` line, unquoted.
fn key_of(line: &str) -> Option<String> {
    let c = content(line)?;
    let (raw, _) = if let Some(rest) = c.strip_prefix('"') {
        let close = rest.find('"')?;
        return rest[close + 1..].starts_with(':').then(|| rest[..close].to_string());
    } else if let Some(rest) = c.strip_prefix('\'') {
        let close = rest.find('\'')?;
        return rest[close + 1..].starts_with(':').then(|| rest[..close].to_string());
    } else {
        c.split_once(':')?
    };
    Some(raw.trim().to_string())
}

/// True when nothing but a comment follows `prefix` on the line.
fn rest_is_empty(line: &str, prefix: &str) -> bool {
    let rest = line.trim().strip_prefix(prefix).unwrap_or("x").trim();
    rest.is_empty() || rest.starts_with('#')
}

fn yaml_key(key: &str) -> String {
    let plain = key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    if plain { key.to_string() } else { quoted(key) }
}

/// A YAML double-quoted scalar. JSON string syntax is valid YAML.
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

fn verify(old: &str, new: &str, before: &std::collections::BTreeMap<String, ProjectMapping>, key: &str, dir: &Path) -> Result<(), String> {
    let failed = |what: &str| format!("the edit {what}; nothing written. {HAND_EDIT}");
    let after = parse(new).map_err(|_| failed("would not parse"))?;
    if without_projects(old) != without_projects(new) {
        return Err(failed("would change keys outside `projects`"));
    }
    let mut expected = before.clone();
    let (domain, project) = key.split_once('/').ok_or_else(|| failed("has no <domain>/<project> key"))?;
    let mapping = expected.entry(key.to_string()).or_insert_with(|| ProjectMapping { domain: domain.into(), project: project.into(), paths: vec![] });
    mapping.paths.push(PathBuf::from(dir));
    if after.projects != expected {
        return Err(failed("would not add exactly this one path"));
    }
    Ok(())
}

fn without_projects(text: &str) -> Option<serde_yaml::Value> {
    let mut value: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    value.as_mapping_mut()?.remove("projects");
    Some(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BASE: &str = "# Wardwell config\nvault_path: /tmp/v # the vault\nsession_sources: []\n\n# trackers below\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n";

    #[test]
    fn a_config_without_projects_gains_the_section_at_the_end_and_keeps_every_byte() {
        let out = add_project_path(BASE, "personal/corr-platform", Path::new("/code/corrtex")).unwrap();
        assert_eq!(out, format!("{BASE}\nprojects:\n  personal/corr-platform:\n    paths:\n      - \"/code/corrtex\"\n"));
    }

    #[test]
    fn a_file_without_a_trailing_newline_is_not_glued() {
        let base = BASE.trim_end();
        let out = add_project_path(base, "personal/corr-platform", Path::new("/code/corrtex")).unwrap();
        assert!(out.starts_with(&format!("{base}\n\nprojects:\n")), "{out}");
    }

    #[test]
    fn a_new_project_goes_under_the_existing_section_with_its_indent_and_comments_kept() {
        let text = "vault_path: /tmp/v\nsession_sources: []\nprojects:   # mapped repos\n    # the old repo\n    work/old:\n        paths:\n            - /code/old\n\n# after\nexclude: []\n";
        let out = add_project_path(text, "work/new", Path::new("/code/new")).unwrap();
        assert_eq!(
            out,
            "vault_path: /tmp/v\nsession_sources: []\nprojects:   # mapped repos\n    # the old repo\n    work/old:\n        paths:\n            - /code/old\n    work/new:\n        paths:\n          - \"/code/new\"\n\n# after\nexclude: []\n"
        );
    }

    #[test]
    fn a_second_path_joins_the_existing_list() {
        let text = "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/old:\n    paths:\n      - /code/old # main\n      # spare\n  work/other:\n    paths: [/x]\n";
        let out = add_project_path(text, "work/old", Path::new("/code/old-2")).unwrap();
        assert_eq!(out, "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/old:\n    paths:\n      - /code/old # main\n      - \"/code/old-2\"\n      # spare\n  work/other:\n    paths: [/x]\n");
    }

    #[test]
    fn flow_style_is_refused_with_what_to_do() {
        let text = "vault_path: /tmp/v\nsession_sources: []\nprojects: {work/a: {paths: [/a]}}\n";
        let error = add_project_path(text, "work/b", Path::new("/b")).unwrap_err();
        assert!(error.contains("edit config.yml by hand"), "{error}");
        let text = "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/a:\n    paths: [/a]\n";
        let error = add_project_path(text, "work/a", Path::new("/b")).unwrap_err();
        assert!(error.contains("not an indented list"), "{error}");
    }

    #[test]
    fn a_path_with_yaml_special_characters_round_trips() {
        let dir = Path::new("/code/a: b #c \"q\"");
        let out = add_project_path(BASE, "personal/x", dir).unwrap();
        let config = parse(&out).unwrap();
        assert_eq!(config.projects["personal/x"].paths, vec![dir.to_path_buf()]);
    }

    #[test]
    fn a_config_that_does_not_parse_is_refused() {
        let error = add_project_path("vault_path: [\n", "work/a", Path::new("/a")).unwrap_err();
        assert!(error.contains("does not parse"), "{error}");
    }
}
