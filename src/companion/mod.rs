//! Optional hosted Companion access. Local vault and kanban calls never enter here.
pub mod connection;
mod transport;

use connection::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompanionParams {
    /// status or schema; capture, publish, list, get, responses for one conversation.
    pub action: String,
    /// Stable identity from this conversation's Companion journal, never a project-wide key.
    pub source_key: Option<String>,
    /// Exact hosted tool arguments. Use schema to discover their current contract.
    pub arguments: Option<Value>,
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
    validate(&params)?;
    let connection = connection::load(&connection::default_path()?)?;
    execute_connected(&connection, params).await
}

async fn execute_connected(
    connection: &Connection,
    params: CompanionParams,
) -> Result<Value, String> {
    validate(&params)?;
    let args = params.arguments.unwrap_or_else(|| json!({}));
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
            let result = remote_tool(connection, "work_plan_publish", args).await?;
            require_plan_key(&result, key).map_err(|_| "Unexpected publication identity; retain the request and reconcile before retrying")?;
            Ok(result)
        }
        "list" => {
            let result = remote_tool(connection, "work_plan_list", args).await?;
            own_plans(result, key)
        }
        "get" | "responses" => {
            // A plan ID alone is insufficient routing information for a shared installation.
            let plan = remote_tool(connection, "work_plan_get", json!({"id":args["id"]})).await?;
            require_plan_key(&plan, key)?;
            if params.action == "get" {
                Ok(plan)
            } else {
                let response = remote_tool(connection, "work_plan_responses", args).await?;
                require_plan_key(&response, key)?;
                Ok(response)
            }
        }
        _ => Err("Unsupported Companion action".into()),
    }
}

fn validate(params: &CompanionParams) -> Result<(), String> {
    if !matches!(
        params.action.as_str(),
        "status" | "schema" | "capture" | "publish" | "list" | "get" | "responses"
    ) {
        return Err("Use status, schema, capture, publish, list, get, or responses".into());
    }
    if params
        .arguments
        .as_ref()
        .is_some_and(|args| !args.is_object())
    {
        return Err("Companion arguments must be an object".into());
    }
    let args = params.arguments.as_ref();
    if matches!(params.action.as_str(), "status" | "schema" | "list")
        && args
            .and_then(Value::as_object)
            .is_some_and(|args| !args.is_empty())
    {
        return Err("This action accepts no hosted arguments".into());
    }
    if matches!(params.action.as_str(), "status" | "schema") {
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
    if matches!(params.action.as_str(), "get" | "responses") {
        let args = args
            .and_then(Value::as_object)
            .ok_or("A plan id is required")?;
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or("A plan id is required")?;
        uuid::Uuid::parse_str(id).map_err(|_| "Plan id must be a UUID")?;
        if args
            .keys()
            .any(|name| name != "id" && !(params.action == "responses" && name == "cursor"))
        {
            return Err("Unsupported read arguments".into());
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params(action: &str, key: &str, arguments: Value) -> CompanionParams {
        CompanionParams {
            action: action.into(),
            source_key: Some(key.into()),
            arguments: Some(arguments),
        }
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
    }

    #[test]
    fn sessions_cannot_accidentally_publish_another_source() {
        assert!(
            validate(&params(
                "publish",
                "session-a",
                json!({"source_key":"session-b"})
            ))
            .is_err()
        );
        assert!(
            validate(&params(
                "capture",
                "session-a",
                json!({"conversation_key":"session-b"})
            ))
            .is_err()
        );
        assert!(
            validate(&params(
                "publish",
                "session-a",
                json!({"source_key":"session-a"})
            ))
            .is_ok()
        );
    }

    #[test]
    fn companion_does_not_forward_arbitrary_tools() {
        assert!(validate(&params("artifact_publish", "session-a", json!({}))).is_err());
        assert!(validate(&params("list", "session-a", json!({"tenant_id":"other"}))).is_err());
        assert!(validate(&params("get", "session-a", json!({"id":"bad"}))).is_err());
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
