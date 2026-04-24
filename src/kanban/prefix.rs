use std::collections::HashMap;

/// Derives a 2-character uppercase prefix from a project slug.
///
/// Candidates (in priority order):
/// 1. First two chars uppercased
/// 2. First + third char uppercased (only when slug has 3+ chars)
/// 3. First + last char uppercased (only when not a duplicate of an earlier candidate)
///
/// Returns the first candidate not already in `existing`, or None if all collide.
pub fn derive_prefix(slug: &str, existing: &[String]) -> Option<String> {
    if slug.chars().count() < 2 {
        return None;
    }

    let chars: Vec<char> = slug.chars().collect();
    let first = chars[0].to_uppercase().next().unwrap_or(chars[0]);

    let mut candidates: Vec<String> = Vec::new();

    // Candidate 1: first two chars
    let second = chars[1].to_uppercase().next().unwrap_or(chars[1]);
    candidates.push(format!("{first}{second}"));

    // Candidate 2: first + third (only if 3+ chars)
    if chars.len() >= 3 {
        let third = chars[2].to_uppercase().next().unwrap_or(chars[2]);
        candidates.push(format!("{first}{third}"));
    }

    // Candidate 3: first + last (skip if already in candidate list)
    let last = chars[chars.len() - 1]
        .to_uppercase()
        .next()
        .unwrap_or(chars[chars.len() - 1]);
    let first_last = format!("{first}{last}");
    if !candidates.contains(&first_last) {
        candidates.push(first_last);
    }

    candidates.into_iter().find(|c| !existing.contains(c))
}

/// Resolves a prefix for `slug`, preferring an explicit config entry over derivation.
/// Config overrides are still checked against `existing_prefixes` to catch collisions.
pub fn resolve_prefix(
    slug: &str,
    config_prefixes: &HashMap<String, String>,
    existing_prefixes: &[String],
) -> Option<String> {
    if let Some(p) = config_prefixes.get(slug) {
        if existing_prefixes.contains(p) {
            return None;
        }
        return Some(p.clone());
    }
    derive_prefix(slug, existing_prefixes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // "shulops" chars: s(0) h(1) u(2) l(3) o(4) p(5) s(6)
    // candidates: "SH" (first+second), "SU" (first+third), "SS" (first+last)

    #[test]
    fn derive_basic() {
        let result = derive_prefix("shulops", &[]);
        assert_eq!(result, Some("SH".to_string()));
    }

    #[test]
    fn derive_collision_first_two() {
        let existing = vec!["SH".to_string()];
        let result = derive_prefix("shulops", &existing);
        assert_eq!(result, Some("SU".to_string()));
    }

    #[test]
    fn derive_collision_first_two_and_first_third() {
        let existing = vec!["SH".to_string(), "SU".to_string()];
        let result = derive_prefix("shulops", &existing);
        assert_eq!(result, Some("SS".to_string()));
    }

    #[test]
    fn derive_all_collide() {
        // "ab": candidates are "AB" only (len=2, no third; first+last == first+second, skipped)
        let existing = vec!["AB".to_string()];
        let result = derive_prefix("ab", &existing);
        assert_eq!(result, None);
    }

    #[test]
    fn derive_single_char_slug() {
        let result = derive_prefix("a", &[]);
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_config_override_wins() {
        let mut config = HashMap::new();
        config.insert("shulops".to_string(), "XY".to_string());
        let result = resolve_prefix("shulops", &config, &[]);
        assert_eq!(result, Some("XY".to_string()));
    }

    #[test]
    fn resolve_falls_back_to_derive() {
        let config = HashMap::new();
        let result = resolve_prefix("shulops", &config, &[]);
        assert_eq!(result, Some("SH".to_string()));
    }

    #[test]
    fn resolve_config_collision() {
        // config says "XY" for project, but "XY" already exists → None
        let mut config = HashMap::new();
        config.insert("shulops".to_string(), "XY".to_string());
        let existing = vec!["XY".to_string()];
        let result = resolve_prefix("shulops", &config, &existing);
        assert_eq!(result, None);
    }
}
