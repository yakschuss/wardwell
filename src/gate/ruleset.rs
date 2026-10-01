//! The "linear-updates" ruleset, held as data: the comment and issue shapes,
//! the Simplified Technical English limits, the rejected items, the status
//! and priority rule, and the destructive tools the deny list blocks.
//! Does NOT evaluate anything; `gate::linear` applies it.

/// One comment shape: its first-line prefix and the fields its team section needs.
pub struct Shape {
    pub name: &'static str,
    pub fields: &'static [&'static str],
}

/// A named check: the label used in the deny reason and its pattern.
pub struct Pattern {
    pub label: &'static str,
    pub regex: &'static str,
}

/// A named, versioned set of rules for writes to one tracker.
pub struct Ruleset {
    pub name: &'static str,
    pub version: u32,
    /// The heading line that opens the plain-words section.
    pub team_heading: &'static str,
    /// The line that ends the team section.
    pub divider: &'static str,
    pub comment_shapes: &'static [Shape],
    pub issue_fields: &'static [&'static str],
    /// The team section must have fewer words than this.
    pub team_word_limit: usize,
    /// A sentence in the team section may have at most this many words.
    pub sentence_word_limit: usize,
    /// Punctuation the team section must not contain (case-insensitive).
    pub forbidden_punctuation: &'static [Pattern],
    /// Technical items the team section must not contain (case-sensitive).
    pub rejected_items: &'static [Pattern],
    /// Whole words the team section must not contain (case-insensitive).
    pub banned_words: &'static [&'static str],
    /// The tracker key prefix, e.g. `COR` for `COR-12`.
    pub key_prefix: &'static str,
    /// The only `state` values a session may set, and only on create.
    pub create_states: &'static [&'static str],
    /// The only `state` values a session may set on an existing issue (the owner's rule of 2026-10-01).
    pub update_states: &'static [&'static str],
    /// Destructive tools the client deny list blocks outright.
    pub denied_tools: &'static [&'static str],
}

const FILE_EXTENSIONS: &str = r"\b\w+\.(?:rb|py|ts|tsx|js|jsx|md|yml|yaml|json|erb|css|scss|html|sql|csv|pdf|txt|lock|sh|toml|rake)\b";

/// The rules in corrtex `docs/operations/LINEAR_UPDATES.md`, version 2.
pub const LINEAR_UPDATES: Ruleset = Ruleset {
    name: "linear-updates",
    version: 2,
    team_heading: "for the team",
    divider: "---",
    comment_shapes: &[
        Shape { name: "Shipped", fields: &["What changed", "How to check it", "How we know it works", "Live in Corrtex since"] },
        Shape { name: "Needs info", fields: &["Question for", "Why it matters", "If we hear nothing"] },
        Shape { name: "Blocked", fields: &["Blocked on", "Needed from", "What is delayed", "What we are doing meanwhile"] },
        Shape { name: "Follow-up", fields: &["Not in this change", "Tracked as", "Why not now", "Next step"] },
    ],
    issue_fields: &["Asked by", "What changes for whom", "Done when"],
    team_word_limit: 80,
    sentence_word_limit: 20,
    forbidden_punctuation: &[
        Pattern { label: "a parenthesis", regex: r"[()]" },
        Pattern { label: "a semicolon", regex: r";" },
        Pattern { label: "a dash", regex: "\u{2014}|\u{2013}| - | -- " },
        Pattern { label: "'e.g.' or 'i.e.'", regex: r"\b(?:e\.g|i\.e)\." },
    ],
    rejected_items: &[
        Pattern { label: "a backtick", regex: r"`" },
        Pattern { label: "a URL", regex: r"https?://|www\." },
        Pattern { label: "a file path (contains /)", regex: r"\S/|/\S" },
        Pattern { label: "a # followed by digits", regex: r"#\d" },
        Pattern { label: "a snake_case token", regex: r"\b[A-Za-z0-9]+(?:_[A-Za-z0-9]+)+\b" },
        Pattern { label: "a CamelCase token", regex: r"\b[A-Za-z]*[a-z][A-Z][A-Za-z0-9]*\b|\b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+\b" },
        Pattern { label: "a file extension", regex: FILE_EXTENSIONS },
    ],
    banned_words: &["PR", "merge", "deploy", "migration", "webhook", "adapter"],
    key_prefix: "COR",
    create_states: &["Triage"],
    update_states: &["Done"],
    denied_tools: &[
        "mcp__linear__delete_comment",
        "mcp__linear__delete_attachment",
        "mcp__linear__retire_issue_label",
        "mcp__linear__retire_project_label",
        "mcp__linear__save_project",
        "mcp__linear__delete_status_update",
        "mcp__linear__delete_diff_comment",
    ],
};
