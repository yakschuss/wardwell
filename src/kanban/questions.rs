use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Open,
    Answered,
    Invalidated,
}

impl QuestionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Answered => "answered",
            Self::Invalidated => "invalidated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "answered" => Some(Self::Answered),
            "invalidated" => Some(Self::Invalidated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionInteractionType {
    Question,
    Decision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_assumption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needed_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction_type: Option<QuestionInteractionType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction_options: Option<Vec<QuestionOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction_placeholder: Option<String>,
    pub status: QuestionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum QuestionEvent {
    #[serde(rename = "create_question")]
    Create(Question),
    #[serde(rename = "update_question")]
    Update {
        id: String,
        project: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        question: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_assumption: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        needed_for: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interaction_type: Option<QuestionInteractionType>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interaction_options: Option<Vec<QuestionOption>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interaction_placeholder: Option<String>,
        timestamp: String,
    },
    #[serde(rename = "answer_question")]
    Answer {
        id: String,
        project: String,
        answer: String,
        timestamp: String,
    },
    #[serde(rename = "invalidate_question")]
    Invalidate {
        id: String,
        project: String,
        reason: Option<String>,
        timestamp: String,
    },
}

pub fn validate_interaction(
    interaction_type: Option<QuestionInteractionType>,
    interaction_options: Option<&[QuestionOption]>,
    interaction_placeholder: Option<&str>,
) -> Result<(), String> {
    if interaction_placeholder.is_some_and(|value| value.chars().count() > 300) {
        return Err("'interaction_placeholder' must be at most 300 characters".into());
    }

    let options = interaction_options.unwrap_or_default();
    match interaction_type {
        Some(QuestionInteractionType::Decision) => {
            if !(2..=5).contains(&options.len()) {
                return Err("a decision requires 2 to 5 'interaction_options'".into());
            }
        }
        _ if !options.is_empty() => {
            return Err("'interaction_options' require interaction_type='decision'".into());
        }
        _ => return Ok(()),
    }

    let mut option_ids = std::collections::HashSet::new();
    for option in options {
        let id = option.id.trim();
        let label = option.label.trim();
        if id.is_empty() || id.chars().count() > 80 {
            return Err("each decision option id must be 1 to 80 characters".into());
        }
        if id.eq_ignore_ascii_case("other") {
            return Err("decision option id 'other' is reserved for the Wardwell surface".into());
        }
        if !option_ids.insert(id.to_string()) {
            return Err("decision option ids must be unique".into());
        }
        if label.is_empty() || label.chars().count() > 200 {
            return Err("each decision option label must be 1 to 200 characters".into());
        }
        if option.detail.as_ref().is_some_and(|value| value.chars().count() > 500) {
            return Err("each decision option detail must be at most 500 characters".into());
        }
    }
    Ok(())
}

pub fn jsonl_path(vault_root: &Path, domain: &str, project: &str) -> PathBuf {
    vault_root.join(domain).join(project).join("questions.jsonl")
}

pub fn append_event(vault_root: &Path, domain: &str, project: &str, event: &QuestionEvent) -> Result<(), std::io::Error> {
    let dir = vault_root.join(domain).join(project);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("questions.jsonl");
    let needs_schema = !path.exists() || path.metadata()?.len() == 0;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    if needs_schema {
        writeln!(file, r#"{{"_schema":"questions","_version":"1.0"}}"#)?;
    }
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read_all(vault_root: &Path, domain: &str, project: &str) -> Vec<Question> {
    let path = jsonl_path(vault_root, domain, project);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut questions: std::collections::HashMap<String, Question> = std::collections::HashMap::new();

    for line in content.lines() {
        if line.is_empty() || line.contains("\"_schema\"") { continue; }
        if let Ok(event) = serde_json::from_str::<QuestionEvent>(line) {
            match event {
                QuestionEvent::Create(q) => { questions.insert(q.id.clone(), q); }
                QuestionEvent::Update {
                    id,
                    question,
                    current_assumption,
                    evidence,
                    needed_for,
                    interaction_type,
                    interaction_options,
                    interaction_placeholder,
                    timestamp,
                    ..
                } => {
                    if let Some(q) = questions.get_mut(&id) {
                        if let Some(v) = question { q.question = v; }
                        if let Some(v) = current_assumption { q.current_assumption = Some(v); }
                        if let Some(v) = evidence { q.evidence = Some(v); }
                        if let Some(v) = needed_for { q.needed_for = Some(v); }
                        if let Some(v) = interaction_type { q.interaction_type = Some(v); }
                        if let Some(v) = interaction_options {
                            q.interaction_options = if v.is_empty() { None } else { Some(v) };
                        }
                        if let Some(v) = interaction_placeholder {
                            q.interaction_placeholder = if v.trim().is_empty() { None } else { Some(v) };
                        }
                        q.updated_at = timestamp;
                    }
                }
                QuestionEvent::Answer { id, answer, timestamp, .. } => {
                    if let Some(q) = questions.get_mut(&id) {
                        q.status = QuestionStatus::Answered;
                        q.answer = Some(answer);
                        q.resolved_at = Some(timestamp.clone());
                        q.updated_at = timestamp;
                    }
                }
                QuestionEvent::Invalidate { id, reason, timestamp, .. } => {
                    if let Some(q) = questions.get_mut(&id) {
                        q.status = QuestionStatus::Invalidated;
                        if let Some(r) = reason { q.answer = Some(r); }
                        q.resolved_at = Some(timestamp.clone());
                        q.updated_at = timestamp;
                    }
                }
            }
        }
    }

    questions.into_values().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_question_event() {
        let q = Question {
            id: "q-1".into(),
            project: "test".into(),
            ticket_id: Some("T-1".into()),
            question: "Who initiates TCM?".into(),
            current_assumption: Some("Coordinator".into()),
            evidence: None,
            needed_for: Some("TCM workflow".into()),
            interaction_type: None,
            interaction_options: None,
            interaction_placeholder: None,
            status: QuestionStatus::Open,
            answer: None,
            created_at: "2026-05-27T00:00:00Z".into(),
            updated_at: "2026-05-27T00:00:00Z".into(),
            resolved_at: None,
            source: Some("code".into()),
        };
        let event = QuestionEvent::Create(q);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: QuestionEvent = serde_json::from_str(&json).unwrap();
        if let QuestionEvent::Create(q) = parsed {
            assert_eq!(q.id, "q-1");
            assert_eq!(q.status, QuestionStatus::Open);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn materialize_with_answer() {
        let dir = tempfile::tempdir().unwrap();
        let q = Question {
            id: "q-1".into(),
            project: "proj".into(),
            ticket_id: None,
            question: "Who owns this?".into(),
            current_assumption: None,
            evidence: None,
            needed_for: None,
            interaction_type: None,
            interaction_options: None,
            interaction_placeholder: None,
            status: QuestionStatus::Open,
            answer: None,
            created_at: "2026-05-27T00:00:00Z".into(),
            updated_at: "2026-05-27T00:00:00Z".into(),
            resolved_at: None,
            source: None,
        };
        append_event(dir.path(), "d", "proj", &QuestionEvent::Create(q)).unwrap();
        append_event(dir.path(), "d", "proj", &QuestionEvent::Answer {
            id: "q-1".into(),
            project: "proj".into(),
            answer: "The coordinator".into(),
            timestamp: "2026-05-27T01:00:00Z".into(),
        }).unwrap();

        let qs = read_all(dir.path(), "d", "proj");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].status, QuestionStatus::Answered);
        assert_eq!(qs[0].answer.as_deref(), Some("The coordinator"));
    }

    #[test]
    fn materialize_with_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let q = Question {
            id: "q-1".into(),
            project: "proj".into(),
            ticket_id: None,
            question: "Is this needed?".into(),
            current_assumption: None,
            evidence: None,
            needed_for: None,
            interaction_type: None,
            interaction_options: None,
            interaction_placeholder: None,
            status: QuestionStatus::Open,
            answer: None,
            created_at: "2026-05-27T00:00:00Z".into(),
            updated_at: "2026-05-27T00:00:00Z".into(),
            resolved_at: None,
            source: None,
        };
        append_event(dir.path(), "d", "proj", &QuestionEvent::Create(q)).unwrap();
        append_event(dir.path(), "d", "proj", &QuestionEvent::Invalidate {
            id: "q-1".into(),
            project: "proj".into(),
            reason: Some("No longer relevant".into()),
            timestamp: "2026-05-27T02:00:00Z".into(),
        }).unwrap();

        let qs = read_all(dir.path(), "d", "proj");
        assert_eq!(qs[0].status, QuestionStatus::Invalidated);
    }

    #[test]
    fn materialize_decision_update_and_clear_it() {
        let dir = tempfile::tempdir().unwrap();
        let q = Question {
            id: "q-1".into(),
            project: "proj".into(),
            ticket_id: None,
            question: "Keep the subscription?".into(),
            current_assumption: None,
            evidence: None,
            needed_for: None,
            interaction_type: None,
            interaction_options: None,
            interaction_placeholder: None,
            status: QuestionStatus::Open,
            answer: None,
            created_at: "2026-05-27T00:00:00Z".into(),
            updated_at: "2026-05-27T00:00:00Z".into(),
            resolved_at: None,
            source: None,
        };
        append_event(dir.path(), "d", "proj", &QuestionEvent::Create(q)).unwrap();
        append_event(
            dir.path(),
            "d",
            "proj",
            &QuestionEvent::Update {
                id: "q-1".into(),
                project: "proj".into(),
                question: None,
                current_assumption: None,
                evidence: None,
                needed_for: None,
                interaction_type: Some(QuestionInteractionType::Decision),
                interaction_options: Some(vec![
                    QuestionOption { id: "keep".into(), label: "Keep it".into(), detail: None },
                    QuestionOption { id: "cancel".into(), label: "Cancel it".into(), detail: None },
                ]),
                interaction_placeholder: None,
                timestamp: "2026-05-27T01:00:00Z".into(),
            },
        ).unwrap();

        let decision = read_all(dir.path(), "d", "proj").pop().unwrap();
        assert_eq!(decision.interaction_type, Some(QuestionInteractionType::Decision));
        assert_eq!(decision.interaction_options.as_ref().map(Vec::len), Some(2));

        append_event(
            dir.path(),
            "d",
            "proj",
            &QuestionEvent::Update {
                id: "q-1".into(),
                project: "proj".into(),
                question: None,
                current_assumption: None,
                evidence: None,
                needed_for: None,
                interaction_type: Some(QuestionInteractionType::Question),
                interaction_options: Some(vec![]),
                interaction_placeholder: Some("Explain what changed".into()),
                timestamp: "2026-05-27T02:00:00Z".into(),
            },
        ).unwrap();

        let question = read_all(dir.path(), "d", "proj").pop().unwrap();
        assert_eq!(question.interaction_type, Some(QuestionInteractionType::Question));
        assert_eq!(question.interaction_options, None);
        assert_eq!(question.interaction_placeholder.as_deref(), Some("Explain what changed"));
    }

    #[test]
    fn decision_contract_rejects_ambiguous_options() {
        let options = vec![
            QuestionOption { id: "keep".into(), label: "Keep it".into(), detail: None },
            QuestionOption { id: "keep".into(), label: "Keep it anyway".into(), detail: None },
        ];
        assert_eq!(
            validate_interaction(Some(QuestionInteractionType::Decision), Some(&options), None),
            Err("decision option ids must be unique".into())
        );
        assert!(validate_interaction(Some(QuestionInteractionType::Question), Some(&options), None).is_err());
        assert!(validate_interaction(Some(QuestionInteractionType::Decision), Some(&options[..1]), None).is_err());
    }
}
