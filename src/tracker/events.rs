//! Provider-neutral tracker event schema, owned by Wardwell.
//!
//! Adapters translate vendor data into these events; nothing vendor-named
//! appears outside `raw`, which is a lossless archive read only by the
//! adapter that wrote it. Does NOT read or write files (see `log`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;

/// First line of every `tracker.jsonl`. The indexer skips lines starting
/// with `{"_schema"`, so keep this compact form.
pub const SCHEMA_HEADER: &str = r#"{"_schema":"tracker","_version":"1.0"}"#;

/// Vault filename for the mirror. Names the concept, never the vendor.
pub const FILE_NAME: &str = "tracker.jsonl";

/// Sidecar beside the log holding each event's raw provider payload, one
/// line per event id. The indexer and watcher skip `*.raw.jsonl`.
pub const RAW_FILE_NAME: &str = "tracker.raw.jsonl";

/// First line of every `tracker.raw.jsonl`.
pub const RAW_SCHEMA_HEADER: &str = r#"{"_schema":"tracker_raw","_version":"1.0"}"#;

/// Fields every tracker event carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Common {
    /// Stable dedup key: provider + entity id + the entity's update time.
    pub id: String,
    /// Rows written before a second provider existed may lack it; they are
    /// Linear's.
    #[serde(default = "default_provider")]
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
    /// Raw provider payload. New log rows leave it out (it lives in the
    /// sidecar); rows written before the sidecar may still carry it inline.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub raw: Value,
}

fn default_provider() -> String {
    "linear".to_string()
}

/// One entry in the tracker log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    IssueUpserted {
        #[serde(flatten)]
        common: Common,
        issue: Box<IssueSnapshot>,
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
    /// A change merged into a repository, such as a merged pull request.
    ChangeMerged {
        #[serde(flatten)]
        common: Common,
        change: Box<MergedChange>,
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
    /// Marks a pull that holds the project lock and is about to call the
    /// provider, with the id of the process that runs it. A later
    /// pull_completed, full_resync or pull_failed of the same provider ends
    /// it. Readers before 0.13.1 skip the row as unreadable.
    PullStarted {
        #[serde(flatten)]
        common: Common,
        pid: u32,
    },
    /// Marks a pull that stopped early, with a closed reason and no provider
    /// text. Never moves the cursor.
    PullFailed {
        #[serde(flatten)]
        common: Common,
        code: FailureCode,
        /// True when the pull was a full pull Wardwell started because a
        /// full resync was due, not one a person asked for.
        #[serde(default, skip_serializing_if = "is_false")]
        automatic_full: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Closed reason a pull or a doctor check failed. Carries no provider text,
/// so it can never leak a secret into the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// Credential file missing, unreadable, or with loose permissions.
    Credential,
    /// The binding names a provider Wardwell has no adapter for.
    UnsupportedProvider,
    /// A compaction or another pull held the project lock past the wait.
    LockBusy,
    LogRead,
    LogWrite,
    /// The provider could not be reached or answered with an error.
    Provider,
    /// The provider refused the token.
    Auth,
    /// The bound team or project key does not exist at the provider.
    TeamNotFound,
    /// A full pull returned no issues while the mirror held open ones.
    EmptyFullResult,
    /// Doctor only: the team key equals the project's native kanban prefix.
    PrefixCollision,
    /// The pull ran past its hard deadline and was stopped.
    Timeout,
    /// A refresh could not start the detached pull process.
    Spawn,
}

impl FailureCode {
    /// The code as written in the log and printed by the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::UnsupportedProvider => "unsupported_provider",
            Self::LockBusy => "lock_busy",
            Self::LogRead => "log_read",
            Self::LogWrite => "log_write",
            Self::Provider => "provider",
            Self::Auth => "auth",
            Self::TeamNotFound => "team_not_found",
            Self::EmptyFullResult => "empty_full_result",
            Self::PrefixCollision => "prefix_collision",
            Self::Timeout => "timeout",
            Self::Spawn => "spawn",
        }
    }
}

impl Event {
    /// The fields every event kind carries.
    pub fn common(&self) -> &Common {
        match self {
            Event::IssueUpserted { common, .. }
            | Event::CommentUpserted { common, .. }
            | Event::StateChanged { common, .. }
            | Event::LinkAdded { common, .. }
            | Event::ChangeMerged { common, .. }
            | Event::IssueRemoved { common }
            | Event::FullResync { common, .. }
            | Event::PullCompleted { common, .. }
            | Event::PullStarted { common, .. }
            | Event::PullFailed { common, .. } => common,
        }
    }
}

/// A merged change as Wardwell holds it. The event's `occurred_at` is the
/// pull request's update time when it was read, for the first event and for
/// every revision alike; `merged_at` holds the merge time. The update time
/// is never earlier than the merge time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MergedChange {
    /// The change's number in its repository.
    pub number: u64,
    /// The change's own title.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub merged_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The branch the change merged into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Ticket keys in the title, in the pattern search quotes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

impl MergedChange {
    /// Short stable fingerprint of what a reader of the change sees: the
    /// title, the body and the keys. An update that leaves them alone, such
    /// as a new comment, keeps it.
    pub fn content_digest(&self) -> String {
        let canonical = serde_json::json!([self.title, self.body, self.keys]);
        let digest = sha2::Sha256::digest(canonical.to_string().as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }
}

/// Full field snapshot of an issue at `occurred_at`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// Key of the parent issue, e.g. `COR-5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_key: Option<String>,
    /// Links to other issues in Wardwell's four kinds. A provider link type
    /// outside them stays only in the raw payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
    /// Version-control branch the tracker suggests for the issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
}

impl IssueSnapshot {
    /// Short stable fingerprint of the structural fields (parent, relations,
    /// branch, labels, project, assignee, priority, url), order-insensitive
    /// for lists. None when the snapshot carries no parent, relation or
    /// branch, so snapshots a schema-1.0 adapter wrote keep their event ids.
    pub fn structure_digest(&self) -> Option<String> {
        if self.parent_key.is_none() && self.relations.is_empty() && self.branch_name.is_none() {
            return None;
        }
        let mut relations = self.relations.clone();
        relations.sort();
        let mut labels = self.labels.clone();
        labels.sort();
        let canonical = serde_json::json!([
            self.parent_key, relations, self.branch_name, labels,
            self.project, self.assignee, self.priority, self.url,
        ]);
        let digest = sha2::Sha256::digest(canonical.to_string().as_bytes());
        Some(digest.iter().take(6).map(|b| format!("{b:02x}")).collect())
    }
}

/// A link from one issue to another, by the other issue's key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Relation {
    pub kind: RelationKind,
    pub key: String,
}

/// Wardwell's issue relation kinds, read from the issue's own side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Related,
    Blocks,
    BlockedBy,
    DuplicateOf,
}

/// Wardwell's workflow-state category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateCategory {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    #[default]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
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
            ..Default::default()
        };
        let events = vec![
            Event::IssueUpserted { common: common("a"), issue: Box::new(snapshot) },
            Event::CommentUpserted { common: common("b"), body: "looks wrong".into() },
            Event::StateChanged { common: common("c"), from: None, to: "Todo".into() },
            Event::LinkAdded { common: common("d"), url: "https://example.com".into(), link_title: Some("PR".into()) },
            Event::ChangeMerged {
                common: common("m"),
                change: Box::new(MergedChange {
                    number: 42,
                    title: "COR-12 Fix the claims inbox".into(),
                    body: Some("Body".into()),
                    author: Some("jdoe".into()),
                    merged_at: Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap(),
                    url: Some("https://github.com/acme/app/pull/42".into()),
                    base_branch: Some("main".into()),
                    keys: vec!["COR-12".into()],
                }),
            },
            Event::IssueRemoved { common: common("e") },
            Event::FullResync { common: common("f"), issues: 3, removed: 1, through: None },
            Event::PullCompleted { common: common("g"), through: Some(Utc.with_ymd_and_hms(2026, 9, 1, 11, 0, 0).unwrap()) },
            Event::PullStarted { common: common("s"), pid: 4242 },
            Event::PullFailed { common: common("h"), code: FailureCode::Provider, automatic_full: false },
            Event::PullFailed { common: common("t"), code: FailureCode::Timeout, automatic_full: false },
        ];
        for event in events {
            let line = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&line).unwrap();
            assert_eq!(back, event);
        }
    }

    fn structured() -> IssueSnapshot {
        IssueSnapshot {
            issue_title: "Claims inbox shows wrong payer".into(),
            state: "In Progress".into(),
            state_category: StateCategory::Started,
            priority: Priority::High,
            project: Some("Claims".into()),
            assignee: Some("Jane Doe".into()),
            labels: vec!["billing".into(), "api".into()],
            url: Some("https://example.com/COR-12".into()),
            parent_key: Some("COR-5".into()),
            relations: vec![
                Relation { kind: RelationKind::Blocks, key: "COR-14".into() },
                Relation { kind: RelationKind::BlockedBy, key: "COR-9".into() },
            ],
            branch_name: Some("jane/cor-12-claims-inbox".into()),
            ..Default::default()
        }
    }

    #[test]
    fn structure_fields_serialize_provider_neutral() {
        let value = serde_json::to_value(structured()).unwrap();
        assert_eq!(value["parent_key"], "COR-5");
        assert_eq!(value["branch_name"], "jane/cor-12-claims-inbox");
        assert_eq!(
            value["relations"],
            serde_json::json!([{"kind": "blocks", "key": "COR-14"}, {"kind": "blocked_by", "key": "COR-9"}])
        );
        let kinds: Vec<RelationKind> = ["related", "blocks", "blocked_by", "duplicate_of"]
            .iter()
            .map(|k| serde_json::from_value(serde_json::json!(k)).unwrap())
            .collect();
        assert_eq!(kinds, vec![RelationKind::Related, RelationKind::Blocks, RelationKind::BlockedBy, RelationKind::DuplicateOf]);
    }

    #[test]
    fn old_snapshot_rows_without_structure_still_parse() {
        let line = r#"{"issue_title":"T","state":"Todo","state_category":"unstarted","priority":"none"}"#;
        let snapshot: IssueSnapshot = serde_json::from_str(line).unwrap();
        assert_eq!(snapshot.parent_key, None);
        assert!(snapshot.relations.is_empty());
        assert_eq!(snapshot.structure_digest(), None, "nothing new to fingerprint");
    }

    #[test]
    fn structure_digest_changes_with_each_structural_field_and_ignores_order() {
        let base = structured().structure_digest().unwrap();
        assert_eq!(base.len(), 12);
        assert_eq!(structured().structure_digest().unwrap(), base, "stable across runs");

        let mut reordered = structured();
        reordered.relations.reverse();
        reordered.labels.reverse();
        assert_eq!(reordered.structure_digest().unwrap(), base, "provider order does not matter");

        let edits: Vec<fn(&mut IssueSnapshot)> = vec![
            |s| s.parent_key = Some("COR-6".into()),
            |s| s.parent_key = None,
            |s| s.relations.push(Relation { kind: RelationKind::Related, key: "COR-20".into() }),
            |s| s.relations[0].kind = RelationKind::DuplicateOf,
            |s| s.branch_name = Some("other".into()),
            |s| s.labels.push("urgent".into()),
            |s| s.project = None,
            |s| s.assignee = Some("John Roe".into()),
            |s| s.priority = Priority::Low,
            |s| s.url = None,
        ];
        for (i, edit) in edits.into_iter().enumerate() {
            let mut changed = structured();
            edit(&mut changed);
            assert_ne!(changed.structure_digest().unwrap(), base, "edit {i}");
        }
    }

    #[test]
    fn failure_codes_serialize_as_their_closed_names() {
        for code in [
            FailureCode::Credential, FailureCode::UnsupportedProvider, FailureCode::LockBusy, FailureCode::LogRead,
            FailureCode::LogWrite, FailureCode::Provider, FailureCode::Auth, FailureCode::TeamNotFound,
            FailureCode::Timeout, FailureCode::Spawn,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), serde_json::json!(code.as_str()));
        }
        assert_eq!(crate::tracker::lock::LOCK_BUSY, FailureCode::LockBusy.as_str());
    }

    #[test]
    fn pull_started_carries_the_process_id_and_provider() {
        let value = serde_json::to_value(Event::PullStarted { common: common("s"), pid: 4242 }).unwrap();
        assert_eq!(value["kind"], "pull_started");
        assert_eq!(value["pid"], 4242);
        assert_eq!(value["provider"], "linear");
        assert_eq!(FailureCode::Timeout.as_str(), "timeout");
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
