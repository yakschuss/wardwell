//! Provider-neutral tracker event schema, owned by Wardwell.
//!
//! Adapters translate vendor data into these events; nothing vendor-named
//! appears outside `raw`, which is a lossless archive read only by the
//! adapter that wrote it. Does NOT read or write files (see `log`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// First line of every `tracker.jsonl`. The indexer skips lines starting
/// with `{"_schema"`, so keep this compact form.
pub const SCHEMA_HEADER: &str = r#"{"_schema":"tracker","_version":"1.0"}"#;

/// Vault filename for the mirror. Names the concept, never the vendor.
pub const FILE_NAME: &str = "tracker.jsonl";

/// Fields every tracker event carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Common {
    /// Stable dedup key: provider + entity id + the entity's update time.
    pub id: String,
    pub provider: String,
    /// Human key, e.g. `COR-12`.
    pub external_key: String,
    /// Provider's opaque issue id.
    pub external_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub occurred_at: DateTime<Utc>,
    /// Human-readable line; the indexer uses it as the chunk heading.
    pub title: String,
    #[serde(default)]
    pub raw: Value,
}

/// One entry in the tracker log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    IssueUpserted {
        #[serde(flatten)]
        common: Common,
        issue: IssueSnapshot,
    },
    CommentUpserted {
        #[serde(flatten)]
        common: Common,
        body: String,
    },
    StateChanged {
        #[serde(flatten)]
        common: Common,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        to: String,
    },
    LinkAdded {
        #[serde(flatten)]
        common: Common,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link_title: Option<String>,
    },
    /// Emitted by a full resync for an issue the tracker no longer returns.
    IssueRemoved {
        #[serde(flatten)]
        common: Common,
    },
    /// Marks the end of a full resync, with counts. Also a cursor marker:
    /// `through` is the newest provider time the resync saw.
    FullResync {
        #[serde(flatten)]
        common: Common,
        issues: usize,
        removed: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<DateTime<Utc>>,
    },
    /// Marks an incremental pull that delivered every page. The next pull
    /// starts from `through`, the newest provider time seen so far; None
    /// means nothing has been seen and the next pull starts from the beginning.
    PullCompleted {
        #[serde(flatten)]
        common: Common,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<DateTime<Utc>>,
    },
}

impl Event {
    /// The fields every event kind carries.
    pub fn common(&self) -> &Common {
        match self {
            Event::IssueUpserted { common, .. }
            | Event::CommentUpserted { common, .. }
            | Event::StateChanged { common, .. }
            | Event::LinkAdded { common, .. }
            | Event::IssueRemoved { common }
            | Event::FullResync { common, .. }
            | Event::PullCompleted { common, .. } => common,
        }
    }
}

/// Full field snapshot of an issue at `occurred_at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueSnapshot {
    pub issue_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Workflow state name as the team labels it, e.g. `In Progress`.
    pub state: String,
    pub state_category: StateCategory,
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// Set when the tracker archived the issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

/// Wardwell's workflow-state category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateCategory {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    Unknown,
}

impl StateCategory {
    /// Maps a provider workflow-state type name; unrecognised names are `Unknown`.
    pub fn from_type(value: &str) -> Self {
        match value {
            "triage" => Self::Triage,
            "backlog" => Self::Backlog,
            "unstarted" => Self::Unstarted,
            "started" => Self::Started,
            "completed" => Self::Completed,
            "canceled" | "cancelled" => Self::Canceled,
            _ => Self::Unknown,
        }
    }
}

/// Wardwell's priority scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    None,
    Urgent,
    High,
    Medium,
    Low,
}

impl Priority {
    /// Maps the common 0-4 scale (0 none, 1 urgent .. 4 low).
    pub fn from_level(level: i64) -> Self {
        match level {
            1 => Self::Urgent,
            2 => Self::High,
            3 => Self::Medium,
            4 => Self::Low,
            _ => Self::None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn common(id: &str) -> Common {
        Common {
            id: id.into(),
            provider: "linear".into(),
            external_key: "COR-12".into(),
            external_id: "issue-uuid".into(),
            actor: Some("Jane Doe".into()),
            occurred_at: Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap(),
            title: "COR-12 Claims inbox shows wrong payer: state Todo to In Progress".into(),
            raw: serde_json::json!({"id": "h1"}),
        }
    }

    #[test]
    fn serializes_kind_tag_and_flat_common_fields() {
        let event = Event::StateChanged {
            common: common("linear:history:h1:2026-09-01T12:00:00Z"),
            from: Some("Todo".into()),
            to: "In Progress".into(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["kind"], "state_changed");
        assert_eq!(value["provider"], "linear");
        assert_eq!(value["external_key"], "COR-12");
        assert_eq!(value["title"], "COR-12 Claims inbox shows wrong payer: state Todo to In Progress");
        assert_eq!(value["from"], "Todo");
        assert_eq!(value["raw"]["id"], "h1");
    }

    #[test]
    fn round_trips_every_kind() {
        let snapshot = IssueSnapshot {
            issue_title: "Claims inbox shows wrong payer".into(),
            description: None,
            state: "In Progress".into(),
            state_category: StateCategory::Started,
            priority: Priority::High,
            team: Some("COR".into()),
            project: None,
            assignee: None,
            creator: Some("Jane Doe".into()),
            labels: vec!["billing".into()],
            url: None,
            created_at: None,
            archived_at: None,
        };
        let events = vec![
            Event::IssueUpserted { common: common("a"), issue: snapshot },
            Event::CommentUpserted { common: common("b"), body: "looks wrong".into() },
            Event::StateChanged { common: common("c"), from: None, to: "Todo".into() },
            Event::LinkAdded { common: common("d"), url: "https://example.com".into(), link_title: Some("PR".into()) },
            Event::IssueRemoved { common: common("e") },
            Event::FullResync { common: common("f"), issues: 3, removed: 1, through: None },
            Event::PullCompleted { common: common("g"), through: Some(Utc.with_ymd_and_hms(2026, 9, 1, 11, 0, 0).unwrap()) },
        ];
        for event in events {
            let line = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&line).unwrap();
            assert_eq!(back, event);
        }
    }

    #[test]
    fn header_matches_indexer_schema_prefix() {
        assert!(SCHEMA_HEADER.starts_with("{\"_schema\""));
        assert_eq!(SCHEMA_HEADER, r#"{"_schema":"tracker","_version":"1.0"}"#);
    }

    #[test]
    fn priority_and_state_normalize_from_provider_values() {
        assert_eq!(Priority::from_level(1), Priority::Urgent);
        assert_eq!(Priority::from_level(4), Priority::Low);
        assert_eq!(Priority::from_level(0), Priority::None);
        assert_eq!(Priority::from_level(9), Priority::None);
        assert_eq!(StateCategory::from_type("completed"), StateCategory::Completed);
        assert_eq!(StateCategory::from_type("weird"), StateCategory::Unknown);
    }
}
