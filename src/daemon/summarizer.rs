use crate::daemon::indexer::{ConversationMessage, SessionStore, UnsummarizedSession};
use std::path::{Path, PathBuf};

/// Errors from session summarization.
#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    #[error("claude CLI error: {0}")]
    Cli(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("session error: {0}")]
    Session(#[from] crate::daemon::indexer::SessionError),
}

/// Stats from a summarization run.
#[derive(Debug, Default)]
pub struct SummaryStats {
    pub summarized: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// Summarize all unsummarized sessions using the claude CLI.
pub async fn summarize_pending(
    session_store: &SessionStore,
    session_sources: &[PathBuf],
    summaries_dir: &Path,
    model: &str,
    verbose: bool,
) -> Result<SummaryStats, SummaryError> {
    let mut stats = SummaryStats::default();
    let unsummarized = session_store.unsummarized()?;
    let total = unsummarized.len();

    std::fs::create_dir_all(summaries_dir)?;

    let mut cli_calls_in_batch: usize = 0;

    for (i, session) in unsummarized.iter().enumerate() {
        // Idempotent: skip if summary file already exists
        let summary_path = summaries_dir.join(format!("{}.md", session.session_id));
        if summary_path.exists() {
            session_store.mark_summarized(&session.session_id)?;
            stats.skipped += 1;
            continue;
        }

        // Find the JSONL file
        let jsonl_path = find_session_file(session, session_sources);
        let jsonl_path = match jsonl_path {
            Some(p) => p,
            None => {
                stats.skipped += 1;
                continue;
            }
        };

        // Skip large sessions (>1MB)
        let file_size = std::fs::metadata(&jsonl_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if file_size > 1_048_576 {
            if verbose {
                eprintln!("wardwell: skipping large session {} ({} bytes)", session.session_id, file_size);
            }
            session_store.mark_summarized(&session.session_id)?;
            stats.skipped += 1;
            continue;
        }

        // Extract conversation
        let conversation = match crate::daemon::indexer::extract_conversation(&jsonl_path) {
            Ok(c) => c,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };

        // Skip very short sessions (< 3 user messages)
        let user_msgs = conversation.iter().filter(|m| m.role == "user").count();
        if user_msgs < 3 {
            session_store.mark_summarized(&session.session_id)?;
            stats.skipped += 1;
            continue;
        }

        if verbose {
            let size_kb = file_size as f64 / 1024.0;
            eprintln!(
                "  [{:>3}/{total}] {} — {} messages — {size_kb:.1}KB",
                i + 1,
                session.project_path,
                session.user_message_count,
            );
        }

        // Rate limiting: pause after every 5 claude calls
        if cli_calls_in_batch > 0 && cli_calls_in_batch.is_multiple_of(5) {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        // Summarize via claude CLI
        match call_claude(&conversation, &session.project_path, model).await {
            Ok(summary) => {
                let frontmatter = build_summary_frontmatter(session);
                let content = format!("{frontmatter}\n{summary}");
                std::fs::write(&summary_path, content)?;
                session_store.mark_summarized(&session.session_id)?;
                stats.summarized += 1;
                cli_calls_in_batch += 1;
            }
            Err(e) => {
                eprintln!("wardwell: summary failed for {}: {e}", session.session_id);
                stats.errors += 1;
                cli_calls_in_batch += 1;
            }
        }
    }

    Ok(stats)
}

/// Find the JSONL file for a session across session sources.
fn find_session_file(session: &UnsummarizedSession, session_sources: &[PathBuf]) -> Option<PathBuf> {
    for source in session_sources {
        let path = source
            .join(&session.project_dir)
            .join(format!("{}.jsonl", session.session_id));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Find a session JSONL file by session ID across all session sources.
/// Walks each source's subdirectories looking for `{session_id}.jsonl`.
pub fn find_session_file_by_id(session_id: &str, session_sources: &[PathBuf]) -> Option<PathBuf> {
    let filename = format!("{session_id}.jsonl");
    for source in session_sources {
        if !source.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(source) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }
            let candidate = project_dir.join(&filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn build_summary_frontmatter(session: &UnsummarizedSession) -> String {
    let domain_line = session.domain.as_ref()
        .map(|d| format!("domain: {d}\n"))
        .unwrap_or_default();
    format!(
        "---\ntype: thread\n{domain_line}project: {project}\nstatus: resolved\nconfidence: inferred\nsummary: Session summary for {project}\n---\n",
        project = session.project_path
    )
}

/// Build a condensed conversation for the prompt.
/// Truncates to stay within token budget (~100k chars ≈ 25k tokens).
pub fn build_conversation_payload(conversation: &[ConversationMessage]) -> String {
    let mut payload = String::new();
    let max_chars: usize = 100_000;

    for msg in conversation {
        let role_label = if msg.role == "user" { "User" } else { "Assistant" };
        // Truncate individual messages that are very long
        let text = if msg.text.len() > 5000 {
            // Find a valid char boundary at or before 5000
            let end = msg.text.floor_char_boundary(5000);
            format!("{}...[truncated]", &msg.text[..end])
        } else {
            msg.text.clone()
        };
        let entry = format!("**{role_label}:** {text}\n\n");

        if payload.len() + entry.len() > max_chars {
            payload.push_str("\n[...conversation truncated for length...]\n");
            break;
        }
        payload.push_str(&entry);
    }

    payload
}

pub const SUMMARY_PROMPT: &str = r#"You are analyzing a Claude Code session transcript. Your job is to extract ONLY signals that would be useful across projects and over time.

Ignore:
- Implementation details (which gem, which config, which file)
- Debugging steps
- Code review feedback
- One-off fixes
- Anything someone would find in a git log

Extract:

## Decisions
Architectural or strategic choices where the user faced a real tradeoff and chose a direction. Not "used Faraday" but "chose client-level retry over application-level retry because [reason]." Only include if the reasoning would apply to future projects.

## Patterns
Repeated behaviors or preferences. How does this person approach problems? What do they reach for first? What do they avoid? Examples:
- "Always writes failing tests before implementation"
- "Prefers detection+correction over strict validation"
- "Architects error handling at the transport layer"
Only include if you see it demonstrated, not just discussed.

## Mental Models
Frameworks, heuristics, or principles the user applied or articulated. Not "hide errors from users" (obvious) but "status pages should be narrative (single inline status) not inventory (equal-weight stat cards) because [reasoning]."

## Context Changes
State changes worth tracking: project phase shifts, blockers hit or resolved, scope changes, new collaborators, deadlines. Only if they affect future sessions.

If a section has nothing worth extracting, omit it entirely. Do not pad with low-signal observations.

For a 30-minute session, 0-3 extractions is normal. Returning nothing is better than returning noise."#;

/// Call the claude CLI to summarize a conversation.
async fn call_claude(
    conversation: &[ConversationMessage],
    project_path: &str,
    model: &str,
) -> Result<String, SummaryError> {
    let condensed = build_conversation_payload(conversation);
    let prompt = format!(
        "{SUMMARY_PROMPT}\n\n---\n\nThis session was for the project at `{project_path}`.\n\n---\n\n{condensed}"
    );

    claude_cli_call(&prompt, model).await
}

/// Execute a prompt via `claude -p` and return the text result.
pub async fn claude_cli_call(prompt: &str, model: &str) -> Result<String, SummaryError> {
    let output = tokio::process::Command::new("claude")
        .args([
            "-p",
            "--model", model,
            "--output-format", "json",
            "--no-session-persistence",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let mut child = match output {
        Ok(c) => c,
        Err(e) => return Err(SummaryError::Cli(format!("failed to spawn claude: {e}"))),
    };

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            return Err(SummaryError::Cli(format!("failed to write to claude stdin: {e}")));
        }
        // stdin drops here, closing the pipe
    }

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    ).await {
        Ok(result) => result.map_err(|e| SummaryError::Cli(format!("claude process error: {e}")))?,
        Err(_) => return Err(SummaryError::Cli("claude timed out after 120s".to_string())),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SummaryError::Cli(format!("claude exited with {}: {stderr}", output.status)));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse JSON output — claude outputs {"result": "...", ...}
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| SummaryError::Cli(format!("failed to parse claude output: {e}")))?;

    let result = parsed
        .get("result")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_conversation_payload_basic() {
        let msgs = vec![
            ConversationMessage { role: "user".to_string(), text: "Hello".to_string() },
            ConversationMessage { role: "assistant".to_string(), text: "Hi there".to_string() },
        ];
        let payload = build_conversation_payload(&msgs);
        assert!(payload.contains("**User:** Hello"));
        assert!(payload.contains("**Assistant:** Hi there"));
    }

    #[test]
    fn build_conversation_payload_truncates_long_messages() {
        let long_msg = "x".repeat(10000);
        let msgs = vec![
            ConversationMessage { role: "user".to_string(), text: long_msg },
        ];
        let payload = build_conversation_payload(&msgs);
        assert!(payload.contains("[truncated]"));
        assert!(payload.len() < 10000);
    }

    #[test]
    fn build_summary_frontmatter_with_domain() {
        let session = UnsummarizedSession {
            session_id: "abc-123".to_string(),
            project_dir: "-Users-test".to_string(),
            project_path: "/Users/test/project".to_string(),
            domain: Some("work".to_string()),
            user_message_count: 10,
            file_size: 2048,
        };
        let fm = build_summary_frontmatter(&session);
        assert!(fm.contains("domain: work"));
        assert!(fm.contains("type: thread"));
        assert!(fm.contains("confidence: inferred"));
    }

    #[test]
    fn build_summary_frontmatter_without_domain() {
        let session = UnsummarizedSession {
            session_id: "def-456".to_string(),
            project_dir: "-Users-test".to_string(),
            project_path: "/Users/test".to_string(),
            domain: None,
            user_message_count: 5,
            file_size: 1024,
        };
        let fm = build_summary_frontmatter(&session);
        assert!(!fm.contains("domain:"));
    }
}
