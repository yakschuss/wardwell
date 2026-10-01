//! `wardwell gate linear`: checks Linear comment and issue writes against the
//! "linear-updates" ruleset and returns the PreToolUse deny JSON, or nothing to
//! allow. Fails open: a malformed payload or an internal error allows.
//! A payload this parser cannot read is allowed. Three known cases differ
//! from the Python hook it replaces, which read them: a lone surrogate escape
//! such as `\ud83d` (serde_json rejects it), nesting deeper than serde_json's
//! recursion limit of 128, and input over the 1 MiB size cap in `main`.
//! Line breaks are `\r\n`, `\n`, and a bare `\r`, as the Python hook's
//! `splitlines` treated them; its other Unicode separators are not.
//! Does NOT read project files, call Linear, or install the hook.

use super::ruleset::{LINEAR_UPDATES, Ruleset};
use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};

pub const COMMENT_TOOL: &str = "mcp__linear__save_comment";
pub const ISSUE_TOOL: &str = "mcp__linear__save_issue";
/// The PreToolUse matcher that routes both checked tools to the gate.
pub const MATCHER: &str = "mcp__linear__save_comment|mcp__linear__save_issue";

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
    "November", "December",
];
const MONTH_ABBREVIATIONS: &str = "Jan|Feb|Mar|Apr|Jun|Jul|Aug|Sep|Sept|Oct|Nov|Dec";

/// A reason to deny, nothing to allow, or a pattern that failed to compile.
type Verdict = Result<Option<String>, regex::Error>;

/// The deny JSON for one raw stdin payload, or None to allow.
pub fn evaluate(input: &str) -> Option<Value> {
    let payload: Value = serde_json::from_str(input).ok()?;
    let reason = decide(&LINEAR_UPDATES, &payload).ok()??;
    Some(deny(&reason))
}

fn deny(reason: &str) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": format!("Linear gate: {reason}"),
    }})
}

fn decide(rules: &Ruleset, payload: &Value) -> Verdict {
    let (Some(name), Some(input)) = (payload.get("tool_name"), payload.get("tool_input")) else {
        return Ok(None);
    };
    let Some(input) = input.as_object() else {
        return Ok(None);
    };
    match name.as_str() {
        Some(COMMENT_TOOL) => match input.get("body").and_then(Value::as_str) {
            Some(body) => check_comment(rules, body),
            None => Ok(Some(format!("comment body is required. Templates: {}", shape_names(rules)))),
        },
        Some(ISSUE_TOOL) => check_issue(rules, input),
        _ => Ok(None),
    }
}

fn shape_names(rules: &Ruleset) -> String {
    rules.comment_shapes.iter().map(|shape| shape.name).collect::<Vec<_>>().join(", ")
}

fn check_comment(rules: &Ruleset, body: &str) -> Verdict {
    let names = shape_names(rules);
    let first = split_lines(body.trim()).first().copied().unwrap_or("");
    let found = rules
        .comment_shapes
        .iter()
        .find_map(|shape| first.strip_prefix(&format!("{}:", shape.name)).map(|rest| (shape, rest)));
    let Some((shape, rest)) = found else {
        return Ok(Some(format!("first line must start with one of the template prefixes; {names}")));
    };
    if rest.trim().is_empty() {
        return Ok(Some(format!("first line needs text after '{}:'. Templates: {names}", shape.name)));
    }
    let Some(team) = team_section(rules, body) else {
        return Ok(Some(format!("missing a 'For the team' section. Templates: {names}")));
    };
    let missing = missing_fields(&team, shape.fields)?;
    if !missing.is_empty() {
        return Ok(Some(format!(
            "{} comment is missing or has empty field(s): {}. Templates: {names}",
            shape.name,
            missing.join(", ")
        )));
    }
    let words = team.split_whitespace().count();
    if words >= rules.team_word_limit {
        return Ok(Some(format!(
            "the team section must be under {} words (has {words}). Templates: {names}",
            rules.team_word_limit
        )));
    }
    let problem = match style_problem(rules, &team)? {
        Some(problem) => Some(problem),
        None => vocabulary_problem(rules, &team)?,
    };
    let problem = match problem {
        Some(problem) => Some(problem),
        None => key_or_date_problem(rules, body)?,
    };
    Ok(problem.map(|problem| format!("{problem}. Templates: {names}")))
}

fn check_issue(rules: &Ruleset, input: &Map<String, Value>) -> Verdict {
    let fields = rules.issue_fields.join(", ");
    let creating = !input.contains_key("id");
    if input.contains_key("priority") {
        return Ok(Some(locked_reason(rules, "priority")));
    }
    if let Some(value) = input.get("state") {
        let allowed = if creating { rules.create_states } else { rules.update_states };
        if !value.as_str().is_some_and(|state| allowed.contains(&state)) {
            return Ok(Some(locked_reason(rules, "state")));
        }
    }
    if !creating {
        return Ok(None);
    }
    let description = match input.get("description") {
        Some(Value::String(text)) => text.as_str(),
        Some(value) if truthy(value) => return Ok(None),
        _ => "",
    };
    let Some(team) = team_section(rules, description) else {
        return Ok(Some(format!("issue description needs a 'For the team' section with: {fields}")));
    };
    let missing = missing_fields(&team, rules.issue_fields)?;
    if !missing.is_empty() {
        return Ok(Some(format!(
            "issue 'For the team' fields missing or empty: {}. Required: {fields}",
            missing.join(", ")
        )));
    }
    if let Some(problem) = style_problem(rules, &team)? {
        return Ok(Some(format!("{problem}. Required fields: {fields}")));
    }
    if !(input.get("parentId").is_some_and(truthy) || input.get("template").is_some_and(truthy) || input.get("project").is_some_and(truthy)) {
        return Ok(Some(format!("a created issue needs parentId, project, or template. Description fields required: {fields}")));
    }
    Ok(None)
}

fn locked_reason(_rules: &Ruleset, key: &str) -> String {
    let base = format!("save_issue must not set '{key}' from a session; the PR and weekly triage set it");
    match key {
        "state" => format!("{base}. A new issue may be created in Triage. An existing issue may be set to Done."),
        _ => base,
    }
}

/// Python truthiness, so the gate decides exactly as the hook it replaces.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn regex(pattern: &str, case_insensitive: bool) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern).case_insensitive(case_insensitive).build()
}

/// Lines split at `\r\n`, `\n`, or a bare `\r`; no empty line after a final break.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(['\r', '\n']) {
        lines.push(&rest[..at]);
        let width = if rest[at..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[at + width..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

/// The text between the team heading and the divider, or None without a heading.
fn team_section(rules: &Ruleset, body: &str) -> Option<String> {
    let lines = split_lines(body);
    let start = lines.iter().position(|line| line.trim().to_lowercase() == rules.team_heading)?;
    let section: Vec<&str> =
        lines.iter().skip(start + 1).take_while(|line| line.trim() != rules.divider).copied().collect();
    Some(section.join("\n"))
}

/// The text after `label...:` on a line starting with the label, or None.
fn field_value(section: &str, label: &str) -> Result<Option<String>, regex::Error> {
    let pattern = format!(r"(?m)^\s*{}[^:\n]*:(.*)$", regex::escape(label));
    Ok(regex(&pattern, true)?
        .captures(section)
        .map(|found| found.get(1).map_or("", |m| m.as_str()).trim().to_string()))
}

fn missing_fields(section: &str, labels: &[&str]) -> Result<Vec<String>, regex::Error> {
    let mut missing = Vec::new();
    for label in labels {
        if field_value(section, label)?.is_none_or(|value| value.is_empty()) {
            missing.push((*label).to_string());
        }
    }
    Ok(missing)
}

fn date_pattern() -> String {
    format!(r"\b(?:{}|{MONTH_ABBREVIATIONS})\.? \d{{1,2}},? \d{{4}}\b", MONTHS.join("|"))
}

/// Simplified Technical English: forbidden punctuation and sentence length.
fn style_problem(rules: &Ruleset, team: &str) -> Verdict {
    for pattern in rules.forbidden_punctuation {
        if regex(pattern.regex, true)?.is_match(team) {
            return Ok(Some(format!("the team section contains {}", pattern.label)));
        }
    }
    let date = regex(&date_pattern(), false)?;
    let label = regex(r"^\s*[^:]{1,60}:", false)?;
    let sentence_end = regex(r"[.?!](?:\s+|$)", false)?;
    for line in split_lines(team) {
        let line = date.replace_all(line, "DATE");
        let line = label.replace(&line, "");
        for sentence in sentence_end.split(&line) {
            let words: Vec<&str> = sentence.split_whitespace().collect();
            if words.len() > rules.sentence_word_limit {
                let start: Vec<&str> = words.iter().take(6).copied().collect();
                return Ok(Some(format!(
                    "a sentence in the team section has more than {} words: '{}...'",
                    rules.sentence_word_limit,
                    start.join(" ")
                )));
            }
        }
    }
    Ok(None)
}

fn vocabulary_problem(rules: &Ruleset, team: &str) -> Verdict {
    for pattern in rules.rejected_items {
        if regex(pattern.regex, false)?.is_match(team) {
            return Ok(Some(format!("the team section contains {}", pattern.label)));
        }
    }
    for word in rules.banned_words {
        if regex(&format!(r"\b{}\b", regex::escape(word)), true)?.is_match(team) {
            return Ok(Some(format!("the team section contains the banned word '{word}'")));
        }
    }
    Ok(None)
}

fn key_or_date_problem(rules: &Ruleset, body: &str) -> Verdict {
    let prefix = regex::escape(rules.key_prefix);
    let exact = regex(&format!(r"^{prefix}-\d+$"), false)?;
    for found in regex(&format!(r"\b{prefix}-(\S*)"), true)?.find_iter(body) {
        let raw = found.as_str();
        if !exact.is_match(raw.trim_end_matches(['.', ',', ';', ':', ')'])) {
            return Ok(Some(format!("malformed Linear key '{raw}' (expected {}-<digits>)", rules.key_prefix)));
        }
    }
    let padded = regex(r"^\d{4}-\d{2}-\d{2}$", false)?;
    for found in regex(r"\b\d{4}-\d{1,2}-\d{1,2}\b", false)?.find_iter(body) {
        let text = found.as_str();
        let valid = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok_and(|date| chrono::Datelike::year(&date) >= 1);
        if !padded.is_match(text) || !valid {
            return Ok(Some(format!("unparseable date '{text}' (use YYYY-MM-DD)")));
        }
    }
    for found in regex(&date_pattern(), false)?.find_iter(body) {
        if written_date(found.as_str()).is_none() {
            return Ok(Some(format!("unparseable date '{}' (use Month D, YYYY)", found.as_str())));
        }
    }
    Ok(None)
}

/// `October 5, 2026` or `Oct 5 2026` as a date. `Sept` is not a month name.
fn written_date(text: &str) -> Option<chrono::NaiveDate> {
    let cleaned = text.replace([',', '.'], "");
    let parts: Vec<&str> = cleaned.split_whitespace().collect();
    let [month, day, year] = parts.as_slice() else { return None };
    let month = MONTHS
        .iter()
        .position(|name| name.eq_ignore_ascii_case(month) || name.get(..3).is_some_and(|abbr| abbr.eq_ignore_ascii_case(month)))?;
    let year: i32 = year.parse().ok().filter(|year| *year >= 1)?;
    chrono::NaiveDate::from_ymd_opt(year, u32::try_from(month).ok()? + 1, day.parse().ok()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! A port of every case in the Python hook's suite, plus the Triage rule.
    use super::*;

    const SHIPPED: &str = "Shipped: Claims screen now shows the payer name in the Candid column

For the team
What changed: Billing staff see the payer name in the Candid column on the claims screen.
How to check it: Open the claims screen in Corrtex and look at the Candid column.
How we know it works: Jack saw it on real claims on October 5, 2026.
What did not change: Other columns look the same.
Live in Corrtex since: 2026-10-05

---
Engineering notes
PR #872 moved payer_name into candid_column.rb.";

    const NEEDS: &str = "Needs info: Which payers show first?

For the team
Question for Lars: Should Medicare claims sort above commercial claims? Reply with: A, B, or one sentence.
Why it matters: It decides the default order of the claims queue.
If we hear nothing: Medicare first, starting 2026-10-07. Can it be changed afterward: yes, easily / no.

---
Engineering notes
Sort lives in the claims query.";

    const BLOCKED: &str = "Blocked: Waiting on the Candid test login

For the team
Blocked on: A test login from Candid.
Needed from: Kelly, by October 9, 2026. Reply with: the login in the team chat.
What is delayed: Sending the first claim.
What we are doing meanwhile: nothing until this clears

---
Engineering notes
Sandbox returns 401.";

    const FOLLOWUP: &str = "Follow-up: Payer names on old claims

For the team
Not in this change: Fixing payer names on claims sent before September.
Tracked as: COR-12, Fix payer names on old claims
Why not now: The new column had to ship first.
Next step: Lars says when to start it.

---
Engineering notes
Backfill excluded.";

    const ISSUE_BODY: &str = "For the team
Asked by: Lars
What changes for whom: Coaches see the next visit date on the patient list.
Done when: The patient list shows a next visit column.
How to check it: Open the patient list in Corrtex.

---
Engineering notes
Display only.";

    fn run(tool: &str, input: Value) -> Option<Value> {
        evaluate(&json!({"tool_name": tool, "tool_input": input}).to_string())
    }

    fn allowed(tool: &str, input: Value) {
        let out = run(tool, input);
        assert!(out.is_none(), "expected allow, got {out:?}");
    }

    fn denied(tool: &str, input: Value, text: &str) -> String {
        let out = run(tool, input).expect("expected deny");
        let hook = &out["hookSpecificOutput"];
        assert_eq!(hook["permissionDecision"], "deny");
        assert_eq!(hook["hookEventName"], "PreToolUse");
        let reason = hook["permissionDecisionReason"].as_str().unwrap().to_string();
        assert!(reason.contains(text), "{reason} lacks {text}");
        reason
    }

    fn comment(body: &str) -> Value {
        json!({"body": body})
    }

    fn shipped_with(replacement: &str) -> String {
        SHIPPED.replace("Other columns look the same.", replacement)
    }

    // CommentTests

    #[test]
    fn valid_templates() {
        for body in [SHIPPED, NEEDS, BLOCKED, FOLLOWUP] {
            allowed(COMMENT_TOOL, json!({"issueId": "COR-1", "body": body}));
        }
    }

    #[test]
    fn missing_prefix() {
        let reason = denied(COMMENT_TOOL, comment(&SHIPPED.replacen("Shipped:", "Done:", 1)), "");
        for name in ["Shipped", "Needs info", "Blocked", "Follow-up"] {
            assert!(reason.contains(name), "{reason}");
        }
    }

    #[test]
    fn missing_field() {
        let body = SHIPPED.replace("How to check it: Open the claims screen in Corrtex and look at the Candid column.\n", "");
        denied(COMMENT_TOOL, comment(&body), "How to check it");
    }

    #[test]
    fn empty_field() {
        let body = BLOCKED.replace("Blocked on: A test login from Candid.", "Blocked on:");
        denied(COMMENT_TOOL, comment(&body), "Blocked on");
    }

    #[test]
    fn over_80_words() {
        denied(COMMENT_TOOL, comment(&shipped_with(&vec!["word"; 80].join(" "))), "80 words");
    }

    #[test]
    fn backtick() {
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("the Candid column on", "the `Candid` column on")), "backtick");
    }

    #[test]
    fn path() {
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("Other columns", "Look in app/models for it. Other columns")), "path");
    }

    #[test]
    fn pr_number() {
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("Other columns", "See #875. Other columns")), "#");
    }

    #[test]
    fn snake_case() {
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("Other columns", "The payer_name field. Other columns")), "snake_case");
    }

    #[test]
    fn banned_word_deploy() {
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("Other columns", "We did a deploy. Other columns")), "deploy");
    }

    #[test]
    fn sentence_length() {
        let twenty = format!("{} end.", vec!["word"; 19].join(" "));
        let twenty_one = format!("{} end.", vec!["word"; 20].join(" "));
        allowed(COMMENT_TOOL, comment(&shipped_with(&twenty)));
        denied(COMMENT_TOOL, comment(&shipped_with(&twenty_one)), "20 words");
    }

    #[test]
    fn style_punctuation() {
        for (bad, text) in [
            ("It is (small).", "parenthesis"),
            ("It is small; it is fast.", "semicolon"),
            ("It is small \u{2014} and fast.", "dash"),
            ("It is small \u{2013} and fast.", "dash"),
            ("It is small - and fast.", "dash"),
            ("Use e.g. one claim.", "e.g."),
            ("Use i.e. one claim.", "e.g."),
        ] {
            denied(COMMENT_TOOL, comment(&shipped_with(bad)), text);
        }
    }

    #[test]
    fn date_comma_allowed() {
        allowed(COMMENT_TOOL, comment(&SHIPPED.replace("October 5, 2026", "Oct 1, 2026 and Oct 2, 2026 and Oct 3, 2026")));
        allowed(COMMENT_TOOL, comment(&SHIPPED.replace("2026-10-05", "Oct 1, 2026")));
    }

    #[test]
    fn queue_is_allowed() {
        allowed(COMMENT_TOOL, comment(&SHIPPED.replace("Other columns", "The queue and flag look the same. Other columns")));
    }

    #[test]
    fn bad_key_and_date() {
        denied(COMMENT_TOOL, comment(&FOLLOWUP.replace("COR-12", "COR-x1")), "COR-");
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("2026-10-05", "2026-13-45")), "date");
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("October 5, 2026", "October 32, 2026")), "date");
    }

    #[test]
    fn no_team_section() {
        denied(COMMENT_TOOL, comment("Shipped: something\n\nnothing else"), "For the team");
    }

    // IssueTests

    #[test]
    fn valid_create() {
        allowed(ISSUE_TOOL, json!({"title": "Next visit", "team": "COR", "description": ISSUE_BODY, "parentId": "COR-3"}));
    }

    #[test]
    fn create_without_parent_project_or_template() {
        let reason = denied(ISSUE_TOOL, json!({"title": "x", "description": ISSUE_BODY}), "a created issue needs parentId, project, or template");
        assert!(reason.contains("Description fields required: Asked by"), "{reason}");
    }

    #[test]
    fn create_with_project_is_allowed() {
        allowed(ISSUE_TOOL, json!({"title": "x", "team": "COR", "description": ISSUE_BODY, "project": "Billing"}));
    }

    #[test]
    fn create_with_template_is_allowed() {
        allowed(ISSUE_TOOL, json!({"title": "x", "team": "COR", "description": ISSUE_BODY, "template": "Bug"}));
    }

    #[test]
    fn create_with_empty_project_is_denied() {
        denied(ISSUE_TOOL, json!({"title": "x", "description": ISSUE_BODY, "project": ""}), "project");
    }

    #[test]
    fn create_missing_done_when() {
        let body = ISSUE_BODY.replace("Done when: The patient list shows a next visit column.\n", "");
        denied(ISSUE_TOOL, json!({"title": "x", "description": body, "parentId": "COR-3"}), "Done when");
    }

    #[test]
    fn create_style_rules() {
        let body = ISSUE_BODY.replace("Coaches see", "Coaches (all) see");
        denied(ISSUE_TOOL, json!({"title": "x", "description": body, "parentId": "COR-3"}), "parenthesis");
        let long = ISSUE_BODY.replace("Open the patient list in Corrtex.", &format!("{}.", vec!["word"; 21].join(" ")));
        denied(ISSUE_TOOL, json!({"title": "x", "description": long, "parentId": "COR-3"}), "20 words");
    }

    #[test]
    fn update_with_state_done_is_allowed() {
        // The owner changed the rule on 2026-10-01: an existing issue may be set to Done.
        allowed(ISSUE_TOOL, json!({"id": "COR-5", "state": "Done"}));
    }

    #[test]
    fn update_with_any_other_state_is_denied_and_names_done() {
        for state in [
            json!("In Progress"), json!("In Testing"), json!("Canceled"), json!("Triage"),
            json!("done"), json!("completed"), json!(null), json!(1),
        ] {
            let reason = denied(ISSUE_TOOL, json!({"id": "COR-5", "state": state}), "'state'");
            assert!(reason.contains("Done"), "{reason}");
        }
    }

    #[test]
    fn update_with_done_still_may_not_set_priority() {
        denied(ISSUE_TOOL, json!({"id": "COR-5", "state": "Done", "priority": 1}), "priority");
    }

    #[test]
    fn update_with_priority() {
        denied(ISSUE_TOOL, json!({"id": "COR-5", "priority": 1}), "priority");
    }

    #[test]
    fn plain_update_allowed() {
        allowed(ISSUE_TOOL, json!({"id": "COR-5", "title": "New title"}));
    }

    // PassThroughTests

    #[test]
    fn other_tool() {
        allowed("Bash", json!({"command": "ls"}));
    }

    #[test]
    fn malformed_payload() {
        assert!(evaluate("not json").is_none());
    }

    // The Triage addition

    fn create_with_state(state: Value) -> Value {
        json!({"title": "x", "description": ISSUE_BODY, "parentId": "COR-3", "state": state})
    }

    #[test]
    fn create_may_set_state_triage() {
        allowed(ISSUE_TOOL, create_with_state(json!("Triage")));
    }

    #[test]
    fn create_with_any_other_state_is_denied_and_names_triage() {
        for state in [json!("Todo"), json!("Done"), json!("triage"), json!(null), json!(1)] {
            let reason = denied(ISSUE_TOOL, create_with_state(state), "'state'");
            assert!(reason.contains("Triage"), "{reason}");
        }
    }

    #[test]
    fn update_may_not_set_state_triage() {
        denied(ISSUE_TOOL, json!({"id": "COR-5", "state": "Triage"}), "state");
    }

    #[test]
    fn create_with_triage_still_may_not_set_priority() {
        let mut input = create_with_state(json!("Triage"));
        input["priority"] = json!(2);
        denied(ISSUE_TOOL, input, "priority");
    }

    #[test]
    fn create_with_triage_still_needs_the_issue_shape() {
        denied(ISSUE_TOOL, json!({"title": "x", "state": "Triage", "parentId": "COR-3"}), "For the team");
    }

    // Fail-open edges of the payload

    #[test]
    fn non_object_tool_input_and_missing_keys_allow() {
        assert!(evaluate(r#"{"tool_name":"mcp__linear__save_comment","tool_input":"x"}"#).is_none());
        assert!(evaluate(r#"{"tool_name":"mcp__linear__save_comment"}"#).is_none());
        assert!(evaluate("[1,2]").is_none());
    }

    #[test]
    fn comment_without_a_body_is_denied() {
        denied(COMMENT_TOOL, json!({"issueId": "COR-1"}), "comment body is required");
    }

    #[test]
    fn carriage_returns_are_line_breaks_like_python_splitlines() {
        for (name, body) in [("SHIPPED", SHIPPED), ("BLOCKED", BLOCKED), ("FOLLOWUP", FOLLOWUP)] {
            for newline in ["\r\n", "\r"] {
                let converted = body.replace('\n', newline);
                assert!(run(COMMENT_TOOL, comment(&converted)).is_none(), "{name} with {newline:?}");
            }
        }
        let bad = SHIPPED.replace("Other columns look the same.", "It is (small).").replace('\n', "\r");
        denied(COMMENT_TOOL, comment(&bad), "parenthesis");
        let issue = ISSUE_BODY.replace('\n', "\r");
        allowed(ISSUE_TOOL, json!({"title": "x", "description": issue, "parentId": "COR-3"}));
    }

    #[test]
    fn a_date_in_year_zero_is_rejected() {
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("2026-10-05", "0000-01-01")), "unparseable date '0000-01-01'");
        denied(COMMENT_TOOL, comment(&SHIPPED.replace("October 5, 2026", "October 5, 0000")), "unparseable date 'October 5, 0000'");
    }

    #[test]
    fn payloads_the_parser_cannot_read_are_allowed() {
        let lone_surrogate = r#"{"tool_name":"mcp__linear__save_comment","tool_input":{"body":"Done: x \ud83d"}}"#;
        assert!(evaluate(lone_surrogate).is_none());
        let deep = format!(r#"{{"tool_name":"mcp__linear__save_comment","tool_input":{{"body":"Done: x","junk":{}{}}}}}"#, "[".repeat(200), "]".repeat(200));
        assert!(evaluate(&deep).is_none());
        assert!(evaluate("").is_none());
    }

    #[test]
    fn ruleset_is_named_and_versioned() {
        assert_eq!((LINEAR_UPDATES.name, LINEAR_UPDATES.version), ("linear-updates", 3));
    }
}
