use std::path::PathBuf;

/// Find the wardwell binary path for MCP config.
pub fn find_binary_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        return exe;
    }

    let candidates = [
        dirs::home_dir().map(|h| h.join(".cargo/bin/wardwell")),
        Some(PathBuf::from("/opt/homebrew/bin/wardwell")),
        Some(PathBuf::from("/usr/local/bin/wardwell")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return candidate;
        }
    }

    PathBuf::from("wardwell")
}

/// Find all CLAUDE.md files in domain paths.
pub fn find_claude_md_files(domain_paths: &[String]) -> Vec<PathBuf> {
    let mut found = Vec::new();

    if let Some(home) = dirs::home_dir() {
        let global = home.join(".claude/CLAUDE.md");
        if global.exists() {
            found.push(global);
        }
    }

    for path_glob in domain_paths {
        let expanded = expand_tilde(path_glob);
        let base = expanded.split('*').next().unwrap_or(&expanded);
        let base_dir = base.trim_end_matches('/');
        let claude_md = PathBuf::from(base_dir).join("CLAUDE.md");
        if claude_md.exists() && !found.contains(&claude_md) {
            found.push(claude_md);
        }
        let dot_claude_md = PathBuf::from(base_dir).join(".claude/CLAUDE.md");
        if dot_claude_md.exists() && !found.contains(&dot_claude_md) {
            found.push(dot_claude_md);
        }
    }

    found
}

/// Scan for Obsidian vaults (directories containing .obsidian/).
pub fn scan_obsidian_vaults() -> Vec<PathBuf> {
    let mut vaults = Vec::new();

    let candidates = [
        // macOS iCloud
        dirs::home_dir().map(|h| h.join("Library/Mobile Documents/iCloud~md~obsidian/Documents")),
        // Standard locations
        dirs::home_dir().map(|h| h.join("Documents/Obsidian")),
        dirs::home_dir().map(|h| h.join("Obsidian")),
        dirs::home_dir().map(|h| h.join("Documents")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if !candidate.exists() {
            continue;
        }

        // Check if this dir itself is an Obsidian vault
        if candidate.join(".obsidian").exists() {
            let canonical = std::fs::canonicalize(&candidate).unwrap_or(candidate);
            if !vaults.contains(&canonical) {
                vaults.push(canonical);
            }
            continue;
        }

        // Check one level deep
        let entries = match std::fs::read_dir(&candidate) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(".obsidian").exists() {
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                if !vaults.contains(&canonical) {
                    vaults.push(canonical);
                }
            }
        }
    }

    vaults
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return format!("{}/{rest}", home.display());
    }
    path.to_string()
}

