//! Optional hosted Companion access. Local vault and kanban calls never enter here.
pub mod connection;
mod journal;
mod transport;

use connection::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, io::Read, path::Path};

const PUBLISH_ARGUMENTS_FILE_LIMIT: u64 = 200_000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompanionParams {
    /// status or schema; discover, publish, read, consume, or acknowledge one conversation.
    pub action: String,
    /// Stable identity from this conversation's Companion journal, never a project-wide key.
    pub source_key: Option<String>,
    /// Exact hosted tool arguments. Use schema to discover their current contract.
    pub arguments: Option<Value>,
    /// Absolute path to a bounded local JSON object, accepted only by publish.
    pub arguments_file: Option<String>,
}

/// Customer-zero credential bootstrap; interactive account sign-in is a later slice.
pub async fn connect(token: &str) -> Result<Value, String> {
    if token.is_empty() || token.len() > 8192 || token.chars().any(char::is_control) {
        return Err("Invalid installation credential".into());
    }
    let connection = Connection {
        endpoint: "https://api.wardwell.app/mcp".into(),
        token: token.into(),
    };
    remote_tool(&connection, "work_plan_list", json!({})).await?;
    connection::save(&connection::default_path()?, token)?;
    Ok(
        json!({"status":"connected", "local_tools":"independent", "publication_authority":"checked by Hank on each publication"}),
    )
}

pub async fn execute(params: CompanionParams) -> Result<Value, String> {
    let args = resolve_arguments(&params)?;
    validate(&params, &args)?;
    let connection = connection::load(&connection::default_path()?)?;
    execute_connected(&connection, params, args).await
}

async fn execute_connected(
    connection: &Connection,
    params: CompanionParams,
    args: Value,
) -> Result<Value, String> {
    let key = params.source_key.as_deref().unwrap_or_default();
    match params.action.as_str() {
        "status" => {
            remote_tool(connection, "work_plan_list", json!({})).await?;
            Ok(
                json!({"status":"connected", "local_tools":"independent", "publication_authority":"checked by Hank on each publication"}),
            )
        }
        "schema" => {
            let result = transport::call(
                &connection.endpoint,
                &connection.token,
                "tools/list",
                json!({}),
            )
            .await?;
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or("Hank returned an invalid tool list")?;
            Ok(
                json!({"tools":tools.iter().filter(|tool| matches!(tool["name"].as_str(), Some("capture_submit" | "work_plan_publish" | "work_plan_list" | "work_plan_get" | "work_plan_responses"))).collect::<Vec<_>>()}),
            )
        }
        "capture" => {
            let result = remote_tool(connection, "capture_submit", args).await?;
            let id = result.get("capture_id").and_then(Value::as_str).ok_or(
                "Missing capture receipt; retain the request and reconcile before retrying",
            )?;
            uuid::Uuid::parse_str(id).map_err(
                |_| "Invalid capture receipt; retain the request and reconcile before retrying",
            )?;
            Ok(result)
        }
        "publish" => {
            let mut publish_args = args;
            if publish_args.get("response_cursor").is_none()
                && let Ok(Some(cursor)) = journal::source_cursor(key)
            {
                publish_args["response_cursor"] = Value::String(cursor);
            }
            let mut result = remote_tool(connection, "work_plan_publish", publish_args).await?;
            require_plan_key(&result, key).map_err(|_| "Unexpected publication identity; retain the request and reconcile before retrying")?;
            if let Some(page) = result.get("pending_responses")
                && page.get("observations").is_some()
            {
                let receipt = match journal::stage(key, page) {
                    Ok(receipt) => receipt,
                    Err(_) => json!({
                        "status": "write_failed",
                        "meaning": "publication succeeded; explicitly consume responses before retrying publication"
                    }),
                };
                result["local_response_journal"] = receipt;
            }
            Ok(result)
        }
        "list" => {
            let result = remote_tool(connection, "work_plan_list", args).await?;
            own_plans(result, key)
        }
        "discover" => {
            let result = remote_tool(connection, "work_plan_list", compact_args(&args)).await?;
            discover_plans(result, args["workstream"].as_str().unwrap_or_default())
        }
        "get" | "responses" => {
            // A plan ID alone is insufficient routing information for a shared installation.
            let plan = remote_tool(
                connection,
                "work_plan_get",
                get_args(&args, params.action == "get"),
            )
            .await?;
            require_plan_key(&plan, key)?;
            if params.action == "get" {
                Ok(plan)
            } else {
                let response = remote_tool(connection, "work_plan_responses", args).await?;
                require_plan_key(&response, key)?;
                Ok(response)
            }
        }
        "consume" => {
            let id = args["id"].as_str().unwrap_or_default();
            if let Some(pending) = journal::pending(key, id)? {
                return Ok(pending);
            }
            let plan = remote_tool(connection, "work_plan_get", json!({"id":id})).await?;
            require_plan_key(&plan, key)?;
            let mut request = json!({"id":id});
            if let Some(cursor) = journal::cursor(key, id)? {
                request["cursor"] = Value::String(cursor);
            }
            let page = remote_tool(connection, "work_plan_responses", request).await?;
            require_plan_key(&page, key)?;
            journal::stage(key, &page)
        }
        "acknowledge" => {
            let id = args["id"].as_str().unwrap_or_default();
            let ids = args["observation_ids"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            journal::acknowledge(key, id, &ids)
        }
        _ => Err("Unsupported Companion action".into()),
    }
}

fn resolve_arguments(params: &CompanionParams) -> Result<Value, String> {
    if params.arguments.is_some() && params.arguments_file.is_some() {
        return Err("Use either arguments or arguments_file, not both".into());
    }
    let Some(path) = params.arguments_file.as_deref() else {
        return Ok(params.arguments.clone().unwrap_or_else(|| json!({})));
    };
    if params.action != "publish" {
        return Err("arguments_file is accepted only by publish".into());
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("arguments_file must be an absolute path".into());
    }
    let file = open_arguments_file(path)?;
    let metadata = file
        .metadata()
        .map_err(|_| "Unable to read arguments_file")?;
    if !metadata.file_type().is_file() {
        return Err("arguments_file must be a regular file, not a symlink".into());
    }
    if metadata.len() > PUBLISH_ARGUMENTS_FILE_LIMIT {
        return Err("arguments_file exceeds the 200000-byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(PUBLISH_ARGUMENTS_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Unable to read arguments_file")?;
    if bytes.len() as u64 > PUBLISH_ARGUMENTS_FILE_LIMIT {
        return Err("arguments_file exceeds the 200000-byte limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "arguments_file must contain valid JSON".to_string())
}

#[cfg(unix)]
fn open_arguments_file(path: &Path) -> Result<fs::File, String> {
    use std::os::unix::fs::MetadataExt;
    let before = fs::symlink_metadata(path).map_err(|_| "Unable to read arguments_file")?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return Err("arguments_file must be a regular file, not a symlink".into());
    }
    let file = fs::File::open(path).map_err(|_| "Unable to read arguments_file")?;
    let opened = file.metadata().map_err(|_| "Unable to read arguments_file")?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return Err("arguments_file changed while being opened".into());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_arguments_file(path: &Path) -> Result<fs::File, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "Unable to read arguments_file")?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("arguments_file must be a regular file, not a symlink".into());
    }
    fs::File::open(path).map_err(|_| "Unable to read arguments_file".into())
}

fn validate(params: &CompanionParams, arguments: &Value) -> Result<(), String> {
    if !matches!(
        params.action.as_str(),
        "status"
            | "schema"
            | "discover"
            | "capture"
            | "publish"
            | "list"
            | "get"
            | "responses"
            | "consume"
            | "acknowledge"
    ) {
        return Err(
            "Use status, schema, discover, capture, publish, list, get, responses, consume, or acknowledge".into(),
        );
    }
    if !arguments.is_object() {
        return Err("Companion arguments must be an object".into());
    }
    let args = Some(arguments);
    if matches!(params.action.as_str(), "status" | "schema")
        && arguments.as_object().is_some_and(|args| !args.is_empty())
    {
        return Err("This action accepts no hosted arguments".into());
    }
    if matches!(params.action.as_str(), "status" | "schema") {
        return Ok(());
    }
    if params.action == "discover" {
        if params.source_key.is_some() {
            return Err("Discover does not accept source_key".into());
        }
        let args = args
            .and_then(Value::as_object)
            .ok_or("A workstream is required")?;
        args.get("workstream")
            .and_then(Value::as_str)
            .filter(|workstream| {
                !workstream.trim().is_empty()
                    && workstream.len() <= 200
                    && !workstream.chars().any(char::is_control)
            })
            .ok_or("Workstream must be a nonempty string of at most 200 characters")?;
        if !args
            .keys()
            .all(|key| matches!(key.as_str(), "workstream" | "compact"))
            || args.get("compact").is_some_and(|value| !value.is_boolean())
        {
            return Err("Discover accepts only workstream and compact".into());
        }
        return Ok(());
    }
    let key = params
        .source_key
        .as_deref()
        .filter(|key| {
            !key.trim().is_empty() && key.len() <= 200 && !key.chars().any(char::is_control)
        })
        .ok_or("Use the stable source_key from this conversation's journal")?;
    let identity_field = match params.action.as_str() {
        "capture" => Some("conversation_key"),
        "publish" => Some("source_key"),
        _ => None,
    };
    if let Some(field) = identity_field
        && args
            .and_then(|args| args.get(field))
            .and_then(Value::as_str)
            != Some(key)
    {
        return Err("Hosted source identity must match this conversation's source_key".into());
    }
    if params.action == "publish"
        && arguments
            .get("expected_revision")
            .and_then(Value::as_u64)
            .is_none()
    {
        return Err("Publish requires a nonnegative integer expected_revision".into());
    }
    if params.action == "list"
        && (!arguments
            .as_object()
            .is_some_and(|args| args.keys().all(|key| key == "compact"))
            || arguments
                .get("compact")
                .is_some_and(|value| !value.is_boolean()))
    {
        return Err("List accepts only a boolean compact argument".into());
    }
    if matches!(
        params.action.as_str(),
        "get" | "responses" | "consume" | "acknowledge"
    ) {
        let args = args
            .and_then(Value::as_object)
            .ok_or("A plan id is required")?;
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or("A plan id is required")?;
        uuid::Uuid::parse_str(id).map_err(|_| "Plan id must be a UUID")?;
        let valid = match params.action.as_str() {
            "responses" => args.keys().all(|name| name == "id" || name == "cursor"),
            "acknowledge" => {
                args.keys()
                    .all(|name| name == "id" || name == "observation_ids")
                    && args
                        .get("observation_ids")
                        .and_then(Value::as_array)
                        .is_some_and(|ids| {
                            !ids.is_empty()
                                && ids.len() <= 100
                                && ids.iter().all(|id| {
                                    id.as_str()
                                        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                                })
                        })
            }
            "get" => {
                args.keys().all(|name| name == "id" || name == "compact")
                    && args.get("compact").is_none_or(Value::is_boolean)
            }
            _ => args.len() == 1,
        };
        if !valid {
            return Err("Unsupported read arguments".into());
        }
    }
    Ok(())
}

fn compact_args(args: &Value) -> Value {
    args.get("compact")
        .map_or_else(|| json!({}), |compact| json!({"compact": compact}))
}

fn get_args(args: &Value, include_compact: bool) -> Value {
    let mut result = json!({"id": args["id"]});
    if include_compact && let Some(compact) = args.get("compact") {
        result["compact"] = compact.clone();
    }
    result
}

fn require_plan_key(plan: &Value, source_key: &str) -> Result<(), String> {
    if plan.get("source_key").and_then(Value::as_str) != Some(source_key) {
        Err(
            "Plan belongs to a different Companion source; check this conversation's journal"
                .into(),
        )
    } else {
        Ok(())
    }
}

fn own_plans(result: Value, source_key: &str) -> Result<Value, String> {
    let plans = result
        .get("work_plans")
        .and_then(Value::as_array)
        .ok_or("Hank returned an invalid plan list")?;
    Ok(
        json!({"work_plans":plans.iter().filter(|plan| plan["source_key"].as_str() == Some(source_key)).collect::<Vec<_>>()}),
    )
}

fn discover_plans(result: Value, workstream: &str) -> Result<Value, String> {
    let plans = result
        .get("work_plans")
        .and_then(Value::as_array)
        .ok_or("Hank returned an invalid plan list")?;
    Ok(
        json!({"work_plans":plans.iter().filter(|plan| plan["workstream"].as_str() == Some(workstream)).collect::<Vec<_>>()}),
    )
}

async fn remote_tool(
    connection: &Connection,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let result = transport::call(&connection.endpoint, &connection.token, "tools/call", json!({"name":name,"arguments":arguments})).await.map_err(|error| {
        if matches!(name, "capture_submit" | "work_plan_publish") {
            format!("{error}. Publication outcome may be unknown; retain the exact request and reconcile the source journal before retrying.")
        } else {
            error
        }
    })?;
    decode_tool_result(result)
}

fn decode_tool_result(result: Value) -> Result<Value, String> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        // Keep known actionable error classes without echoing arbitrary upstream content.
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(if text.contains("revision_conflict") || text.contains("stale_revision") {
            "Hank rejected a stale revision. Read this Companion's current source before retrying."
        } else if text.contains("scope_denied") {
            "The installation connection lacks a required Companion permission."
        } else if text.contains("not_creator") {
            "This installation does not own that Companion. Keep the journal and reconnect its original installation."
        } else if let Some(detail) = safe_validation_detail(text) {
            return Err(format!("Hank rejected the Companion request: {detail}"));
        } else {
            "Hank rejected the Companion request. Check the current schema and source journal; no success is recorded."
        }.into());
    }
    if result.get("isError").is_some_and(|flag| !flag.is_boolean()) {
        return Err(
            "Hank returned an invalid tool result; reconcile before retrying a publication".into(),
        );
    }
    result.get("structuredContent").cloned().ok_or_else(|| {
        "Hank returned no structured result; reconcile before retrying a publication".into()
    })
}

fn safe_validation_detail(text: &str) -> Option<&str> {
    let text = text.trim();
    (text.len() <= 500
        && !text.chars().any(char::is_control)
        && matches!(
            text.split_once(':').map(|(code, _)| code),
            Some("invalid_work_plan" | "invalid_arguments" | "validation_error")
        ))
    .then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(action: &str, key: &str, arguments: Value) -> CompanionParams {
        CompanionParams {
            action: action.into(),
            source_key: Some(key.into()),
            arguments: Some(arguments),
            arguments_file: None,
        }
    }

    fn valid(params: &CompanionParams) -> Result<(), String> {
        let args = resolve_arguments(params)?;
        validate(params, &args)
    }

    #[test]
    fn accepts_optional_false_error_flag_and_preserves_actionable_rejections() {
        assert_eq!(
            decode_tool_result(json!({"structuredContent":{"revision":1}})),
            Ok(json!({"revision":1}))
        );
        assert_eq!(
            decode_tool_result(json!({"isError":false,"structuredContent":{"revision":1}})),
            Ok(json!({"revision":1}))
        );
        assert!(decode_tool_result(json!({"isError":"false","structuredContent":{}})).is_err());
        let error = decode_tool_result(
            json!({"isError":true,"content":[{"text":"invalid_work_plan: stale_revision"}]}),
        );
        assert!(error.is_err_and(|text| text.contains("stale revision")));
        let error = decode_tool_result(
            json!({"isError":true,"content":[{"text":"invalid_work_plan: steps[0].owner is required"}]}),
        );
        assert!(error.is_err_and(|text| text.contains("steps[0].owner is required")));
        assert!(
            decode_tool_result(json!({"isError":true,"content":[{"text":"upstream secret"}]}))
                .is_err_and(|text| !text.contains("secret"))
        );
    }

    #[test]
    fn publish_arguments_file_is_bounded_regular_json_and_source_bound() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("publish.json");
        fs::write(&good, r#"{"source_key":"session-a","expected_revision":0}"#).unwrap();
        let from_file = CompanionParams {
            action: "publish".into(),
            source_key: Some("session-a".into()),
            arguments: None,
            arguments_file: Some(good.to_string_lossy().into()),
        };
        assert!(valid(&from_file).is_ok());

        let conflicting = CompanionParams {
            arguments: Some(json!({})),
            ..from_file
        };
        assert!(valid(&conflicting).is_err());

        let malformed = dir.path().join("malformed.json");
        fs::write(&malformed, "{").unwrap();
        let foreign = dir.path().join("foreign.json");
        fs::write(
            &foreign,
            r#"{"source_key":"session-b","expected_revision":0}"#,
        )
        .unwrap();
        let oversized = dir.path().join("oversized.json");
        fs::write(
            &oversized,
            vec![b' '; PUBLISH_ARGUMENTS_FILE_LIMIT as usize + 1],
        )
        .unwrap();
        for path in [malformed, foreign, oversized] {
            assert!(
                valid(&CompanionParams {
                    action: "publish".into(),
                    source_key: Some("session-a".into()),
                    arguments: None,
                    arguments_file: Some(path.to_string_lossy().into()),
                })
                .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn publish_arguments_file_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        let link = dir.path().join("link.json");
        fs::write(
            &target,
            r#"{"source_key":"session-a","expected_revision":0}"#,
        )
        .unwrap();
        symlink(&target, &link).unwrap();
        assert!(
            valid(&CompanionParams {
                action: "publish".into(),
                source_key: Some("session-a".into()),
                arguments: None,
                arguments_file: Some(link.to_string_lossy().into()),
            })
            .is_err()
        );
    }

    #[test]
    fn companion_schema_exposes_arguments_file() {
        let schema = serde_json::to_value(schemars::schema_for!(CompanionParams)).unwrap();
        assert!(schema["properties"]["arguments_file"].is_object());
    }

    #[test]
    fn compact_is_boolean_and_forwarded_only_when_present() {
        assert!(valid(&params("list", "session-a", json!({"compact":true}))).is_ok());
        assert!(valid(&params("list", "session-a", json!({"compact":"yes"}))).is_err());
        assert_eq!(
            compact_args(&json!({"workstream":"x","compact":false})),
            json!({"compact":false})
        );
        assert_eq!(
            get_args(&json!({"id":"x","compact":true}), true),
            json!({"id":"x","compact":true})
        );
    }

    #[test]
    fn sessions_cannot_accidentally_publish_another_source() {
        assert!(
            valid(&params(
                "publish",
                "session-a",
                json!({"source_key":"session-b"})
            ))
            .is_err()
        );
        assert!(
            valid(&params(
                "capture",
                "session-a",
                json!({"conversation_key":"session-b"})
            ))
            .is_err()
        );
        assert!(
            valid(&params(
                "publish",
                "session-a",
                json!({"source_key":"session-a", "expected_revision":0})
            ))
            .is_ok()
        );
    }

    #[test]
    fn companion_does_not_forward_arbitrary_tools() {
        assert!(valid(&params("artifact_publish", "session-a", json!({}))).is_err());
        assert!(valid(&params("list", "session-a", json!({"tenant_id":"other"}))).is_err());
        assert!(valid(&params("get", "session-a", json!({"id":"bad"}))).is_err());
    }

    #[test]
    fn discover_requires_only_a_bounded_workstream_and_no_source_key() {
        assert!(
            valid(&CompanionParams {
                action: "discover".into(),
                source_key: None,
                arguments: Some(json!({"workstream":"corr/pcc"})),
                arguments_file: None,
            })
            .is_ok()
        );
        for arguments in [
            json!({}),
            json!({"workstream":""}),
            json!({"workstream":"x", "source_key":"session-a"}),
            json!({"workstream":"x".repeat(201)}),
        ] {
            assert!(
                valid(&CompanionParams {
                    action: "discover".into(),
                    source_key: None,
                    arguments: Some(arguments),
                    arguments_file: None,
                })
                .is_err()
            );
        }
        assert!(
            valid(&CompanionParams {
                action: "discover".into(),
                source_key: Some("session-a".into()),
                arguments: Some(json!({"workstream":"corr/pcc"})),
                arguments_file: None,
            })
            .is_err()
        );
    }

    #[test]
    fn discover_filters_other_sources_by_exact_workstream() {
        let result = discover_plans(
            json!({"work_plans":[
                {"source_key":"a","workstream":"corr/pcc","id":"one"},
                {"source_key":"b","workstream":"corr/pcc","id":"two"},
                {"source_key":"c","workstream":"Corr/PCC","id":"three"},
                {"source_key":"d","id":"four"}
            ]}),
            "corr/pcc",
        );
        assert_eq!(
            result,
            Ok(json!({"work_plans":[
                {"source_key":"a","workstream":"corr/pcc","id":"one"},
                {"source_key":"b","workstream":"corr/pcc","id":"two"}
            ]}))
        );
        assert!(discover_plans(json!({}), "corr/pcc").is_err());
    }

    #[test]
    fn list_and_reads_stay_on_the_requested_source() {
        let result = own_plans(
            json!({"work_plans":[{"source_key":"a","id":"one"},{"source_key":"b","id":"two"}]}),
            "a",
        );
        assert_eq!(
            result,
            Ok(json!({"work_plans":[{"source_key":"a","id":"one"}]}))
        );
        assert!(require_plan_key(&json!({"source_key":"b"}), "a").is_err());
        assert!(require_plan_key(&json!({}), "a").is_err());
    }
}
