//! Optional hosted Companion access. Local vault and kanban calls never enter here.
pub mod connection;
pub mod install;
mod journal;
pub mod lifecycle;
mod outbox;
mod transport;

use connection::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, io::Read, path::Path};

const PUBLISH_ARGUMENTS_FILE_LIMIT: u64 = 200_000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompanionParams {
    /// Wrapper action: status, schema, discover, capture, publish, list, get, responses,
    /// consume, acknowledge, outbox_status, or flush. Call schema for hosted and local contracts.
    pub action: String,
    /// Stable identity from this conversation's Companion journal, never a project-wide key.
    pub source_key: Option<String>,
    /// Action-specific JSON object. Hosted actions forward the documented hosted arguments;
    /// local consume/acknowledge/outbox actions follow schema.local_actions.
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
    let mut args = prepare_arguments(&params)?;
    let key = params.source_key.as_deref().unwrap_or_default();
    if params.action == "outbox_status" {
        return outbox::status(key);
    }
    if params.action == "flush" {
        let connection = connection::load(&connection::default_path()?)?;
        return flush_outbox(&connection, key).await;
    }
    if params.action == "publish"
        && args.get("response_cursor").is_none()
        && let Ok(Some(cursor)) = journal::source_cursor(key)
    {
        args["response_cursor"] = Value::String(cursor);
    }
    if matches!(params.action.as_str(), "capture" | "publish") {
        let loaded = connection::load(&connection::default_path()?);
        let principal = loaded.as_ref().ok().map(|connection| {
            outbox::principal_fingerprint(&connection.endpoint, &connection.token)
        });
        let request_id = outbox::stage(key, &params.action, &args, principal.as_deref(), true)?;
        if let Some(receipt) = outbox::completed_receipt(key, &request_id)? {
            return Ok(json!({
                "status": "already_completed",
                "local_outbox_receipt": receipt
            }));
        }
        let connection = loaded.map_err(|error| {
            let _ = outbox::mark_pending(&request_id, "connection_unavailable");
            format!("{error}. The exact request remains pending in the local Companion outbox.")
        })?;
        return deliver(&connection, key, &request_id, &params.action, args).await;
    }
    let connection = connection::load(&connection::default_path()?)?;
    execute_connected(&connection, params, args).await
}

pub fn verified_receipt(source_key: &str, receipt_id: &str) -> Result<Option<Value>, String> {
    outbox::verified_receipt(source_key, receipt_id)
}

pub fn unchanged_eligible(source_key: &str) -> Result<bool, String> {
    outbox::unchanged_eligible(source_key)
}

pub fn verified_publish_receipt(source_key: &str, receipt_id: &str) -> Result<bool, String> {
    Ok(verified_receipt(source_key, receipt_id)?.is_some())
}

pub fn verified_publish_receipt_after(
    source_key: &str,
    receipt_id: &str,
    opened_at: &str,
) -> Result<bool, String> {
    outbox::verified_publish_receipt_after(source_key, receipt_id, opened_at)
}

pub fn outbox_status(source_key: &str) -> Result<Value, String> {
    outbox::status(source_key)
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
            Ok(json!({
                "tools": tools.iter().filter(|tool| matches!(tool["name"].as_str(), Some("capture_submit" | "work_plan_publish" | "work_plan_list" | "work_plan_get" | "work_plan_responses"))).collect::<Vec<_>>(),
                "local_actions": local_action_schema()
            }))
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
            let compact = args
                .get("compact")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let session = args.get("session_id").and_then(Value::as_str);
            if let Some(pending) = journal::consume_view(key, id, compact, session)? {
                return Ok(pending);
            }
            let plan = remote_tool(
                connection,
                "work_plan_get",
                json!({"id":id, "compact":true}),
            )
            .await?;
            require_plan_key(&plan, key)?;
            let mut request = json!({"id":id});
            if let Some(cursor) = journal::cursor(key, id)? {
                request["cursor"] = Value::String(cursor);
            }
            let page = remote_tool(connection, "work_plan_responses", request).await?;
            require_plan_key(&page, key)?;
            let staged = journal::stage(key, &page)?;
            if compact {
                Ok(journal::consume_view(key, id, true, session)?.unwrap_or(staged))
            } else {
                Ok(staged)
            }
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

fn prepare_arguments(params: &CompanionParams) -> Result<Value, String> {
    let mut arguments = resolve_arguments(params)?;
    if let (Some(source_key), Some(arguments)) =
        (params.source_key.as_deref(), arguments.as_object_mut())
    {
        let identity_field = match params.action.as_str() {
            "capture" => Some("conversation_key"),
            "publish" => Some("source_key"),
            _ => None,
        };
        if let Some(field) = identity_field {
            arguments
                .entry(field)
                .or_insert_with(|| Value::String(source_key.to_owned()));
        }
    }
    validate(params, &arguments)?;
    Ok(arguments)
}

#[cfg(unix)]
fn open_arguments_file(path: &Path) -> Result<fs::File, String> {
    use std::os::unix::fs::MetadataExt;
    let before = fs::symlink_metadata(path).map_err(|_| "Unable to read arguments_file")?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return Err("arguments_file must be a regular file, not a symlink".into());
    }
    let file = fs::File::open(path).map_err(|_| "Unable to read arguments_file")?;
    let opened = file
        .metadata()
        .map_err(|_| "Unable to read arguments_file")?;
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
            | "outbox_status"
            | "flush"
            | "list"
            | "get"
            | "responses"
            | "consume"
            | "acknowledge"
    ) {
        return Err(
            "Use status, schema, discover, capture, publish, list, get, responses, consume, acknowledge, outbox_status, or flush".into(),
        );
    }
    if !arguments.is_object() {
        return Err("Companion arguments must be an object".into());
    }
    let args = Some(arguments);
    if matches!(
        params.action.as_str(),
        "status" | "schema" | "outbox_status" | "flush"
    ) && arguments.as_object().is_some_and(|args| !args.is_empty())
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
    if matches!(params.action.as_str(), "outbox_status" | "flush") {
        return Ok(());
    }
    if let Some(field) = identity_field
        && args
            .and_then(|args| args.get(field))
            .and_then(Value::as_str)
            != Some(key)
    {
        return Err(match params.action.as_str() {
            "capture" => "Capture arguments.conversation_key must match wrapper source_key; arguments.source identifies the adapter kind, not the conversation identity",
            "publish" => "Publish arguments.source_key must match wrapper source_key",
            _ => unreachable!(),
        }
        .into());
    }
    if params.action == "publish"
        && arguments
            .get("expected_revision")
            .and_then(Value::as_u64)
            .is_none()
    {
        return Err("Publish requires a nonnegative integer expected_revision".into());
    }
    if params.action == "capture"
        && !arguments
            .get("external_id")
            .and_then(Value::as_str)
            .is_some_and(|id| {
                !id.trim().is_empty() && id.len() <= 500 && !id.chars().any(char::is_control)
            })
    {
        return Err("Capture requires a stable external_id of at most 500 characters".into());
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
            _ => {
                args.keys()
                    .all(|name| matches!(name.as_str(), "id" | "compact" | "session_id"))
                    && args.get("compact").is_none_or(Value::is_boolean)
                    && args
                        .get("session_id")
                        .is_none_or(|value| value.as_str().is_some())
            }
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

fn local_action_schema() -> Value {
    json!({
        "consume": {
            "source_key": "required stable conversation source",
            "arguments": {"id": "required plan UUID", "compact": "optional boolean bounded response view", "session_id": "optional presentation session id"},
            "meaning": "durably stage the next owner-response page; does not acknowledge or execute it"
        },
        "acknowledge": {
            "source_key": "required stable conversation source",
            "arguments": {"id": "required plan UUID", "observation_ids": "required array of 1..100 staged observation UUIDs"},
            "meaning": "records local persistence only; never execution or completion"
        },
        "outbox_status": {
            "source_key": "required stable conversation source",
            "arguments": {},
            "meaning": "bounded local counts and pending/blocked operation metadata; no request payloads"
        },
        "flush": {
            "source_key": "required stable conversation source",
            "arguments": {},
            "meaning": "explicitly reconcile and replay this source's pending requests under the bound installation principal"
        }
    })
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

enum PublishReconciliation {
    SafeToSend,
    Completed { plan_id: String, revision: u64 },
    Conflict,
}

#[derive(Debug)]
enum PublishDelivery {
    Reconciled { plan_id: String, revision: u64 },
    Published(Value),
    Conflict,
}

async fn flush_outbox(connection: &Connection, source_key: &str) -> Result<Value, String> {
    let principal = outbox::principal_fingerprint(&connection.endpoint, &connection.token);
    let operations = outbox::pending(source_key)?;
    let mut receipts = Vec::new();
    let mut blocked = 0_u64;
    let mut pending = 0_u64;
    for operation in operations {
        if operation.state == "blocked" {
            blocked += 1;
            continue;
        }
        if operation.principal_fingerprint.as_deref() != Some(principal.as_str()) {
            outbox::mark_blocked(&operation.request_id, "principal_mismatch")?;
            blocked += 1;
            continue;
        }
        match deliver(
            connection,
            source_key,
            &operation.request_id,
            &operation.action,
            operation.arguments,
        )
        .await
        {
            Ok(result) => {
                if let Some(receipt) = result.get("local_outbox_receipt") {
                    receipts.push(receipt.clone());
                }
            }
            Err(_) => pending += 1,
        }
    }
    Ok(json!({
        "source_key": source_key,
        "verified_receipts": receipts,
        "pending": pending,
        "blocked": blocked,
        "status": outbox::status(source_key)?
    }))
}

async fn deliver(
    connection: &Connection,
    source_key: &str,
    request_id: &str,
    action: &str,
    arguments: Value,
) -> Result<Value, String> {
    if action == "capture" {
        let mut result = remote_tool(connection, "capture_submit", arguments)
            .await
            .map_err(|error| {
                let _ = outbox::mark_pending(request_id, "transport_or_remote_failure");
                format!("{error} The exact capture remains pending in the local Companion outbox.")
            })?;
        let capture_id = result
            .get("capture_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                let _ = outbox::mark_pending(request_id, "invalid_receipt");
                "Missing capture receipt; the exact request remains pending".to_string()
            })?;
        uuid::Uuid::parse_str(capture_id).map_err(|_| {
            let _ = outbox::mark_pending(request_id, "invalid_receipt");
            "Invalid capture receipt; the exact request remains pending".to_string()
        })?;
        result["local_outbox_receipt"] =
            outbox::complete(request_id, source_key, action, capture_id, None)?;
        return Ok(result);
    }

    let delivery = publish_network_with(source_key, &arguments, |name, args| {
        remote_tool(connection, name, args)
    })
    .await;
    let mut result = match delivery {
        Ok(PublishDelivery::Reconciled { plan_id, revision }) => {
            let receipt =
                outbox::complete(request_id, source_key, "publish", &plan_id, Some(revision))?;
            return Ok(json!({
                "plan_id": plan_id,
                "source_key": source_key,
                "current_revision": revision,
                "reconciled": true,
                "local_outbox_receipt": receipt
            }));
        }
        Ok(PublishDelivery::Conflict) => {
            outbox::mark_blocked(request_id, "revision_conflict")?;
            return Err("The pending publication conflicts with a changed hosted source. It remains blocked for manual reconciliation.".into());
        }
        Ok(PublishDelivery::Published(result)) => result,
        Err(error) => {
            let blocked = error.contains("stale revision");
            let _ = if blocked {
                outbox::mark_blocked(request_id, "revision_conflict")
            } else {
                outbox::mark_pending(request_id, "reconciliation_or_transport_failure")
            };
            return Err(format!(
                "{error} The exact publication remains in the local Companion outbox; no success was recorded."
            ));
        }
    };

    let expected = arguments["expected_revision"].as_u64().unwrap_or_default();
    require_plan_key(&result, source_key).map_err(|_| {
        let _ = outbox::mark_pending(request_id, "invalid_receipt");
        "Unexpected publication identity; the exact request remains pending".to_string()
    })?;
    let plan_id = result
        .get("plan_id")
        .or_else(|| result.get("id"))
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or_else(|| {
            let _ = outbox::mark_pending(request_id, "invalid_receipt");
            "Publication receipt has no valid plan id; the exact request remains pending"
                .to_string()
        })?;
    let revision = result
        .get("current_revision")
        .or_else(|| result.get("revision"))
        .and_then(Value::as_u64)
        .filter(|revision| *revision == expected || *revision == expected.saturating_add(1))
        .ok_or_else(|| {
            let _ = outbox::mark_blocked(request_id, "unexpected_revision");
            "Publication returned an unexpected revision and is blocked for reconciliation"
                .to_string()
        })?;
    let receipt = outbox::complete(request_id, source_key, "publish", plan_id, Some(revision))?;
    if let Some(page) = result.get("pending_responses")
        && page.get("observations").is_some()
    {
        result["local_response_journal"] = journal::stage(source_key, page).unwrap_or_else(|_| {
            json!({"status":"write_failed","meaning":"publication succeeded; explicitly consume responses"})
        });
    }
    result["local_outbox_receipt"] = receipt;
    Ok(result)
}

async fn publish_network_with<F, Fut>(
    source_key: &str,
    arguments: &Value,
    mut call: F,
) -> Result<PublishDelivery, String>
where
    F: FnMut(&'static str, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    match reconcile_publish_with(source_key, arguments, &mut call).await? {
        PublishReconciliation::Completed { plan_id, revision } => {
            Ok(PublishDelivery::Reconciled { plan_id, revision })
        }
        PublishReconciliation::Conflict => Ok(PublishDelivery::Conflict),
        PublishReconciliation::SafeToSend => call("work_plan_publish", arguments.clone())
            .await
            .map(PublishDelivery::Published),
    }
}

async fn reconcile_publish_with<F, Fut>(
    source_key: &str,
    arguments: &Value,
    mut call: F,
) -> Result<PublishReconciliation, String>
where
    F: FnMut(&'static str, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let expected = arguments["expected_revision"]
        .as_u64()
        .ok_or("Pending publication has no expected revision")?;
    let listed = call("work_plan_list", json!({"compact":true})).await?;
    let matches = listed
        .get("work_plans")
        .and_then(Value::as_array)
        .ok_or("Hank returned an invalid plan list")?
        .iter()
        .filter(|plan| plan.get("source_key").and_then(Value::as_str) == Some(source_key))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Ok(if expected == 0 {
            PublishReconciliation::SafeToSend
        } else {
            PublishReconciliation::Conflict
        });
    }
    if matches.len() != 1 {
        return Ok(PublishReconciliation::Conflict);
    }
    let plan_id = matches[0]
        .get("plan_id")
        .or_else(|| matches[0].get("id"))
        .and_then(Value::as_str)
        .ok_or("Hank returned a plan without an id")?;
    let page = call("work_plan_responses", json!({"id":plan_id})).await?;
    require_plan_key(&page, source_key)?;
    let current = page
        .get("current_revision")
        .and_then(Value::as_u64)
        .ok_or("Hank returned a response page without a revision")?;
    let requested_document = canonical_source_document(arguments)?;
    let current_document = page
        .get("source_document")
        .ok_or("Hank returned no authoritative source document")?;
    Ok(classify_publish_reconciliation(
        plan_id,
        expected,
        &requested_document,
        current,
        current_document,
    ))
}

fn classify_publish_reconciliation(
    plan_id: &str,
    expected: u64,
    requested_document: &Value,
    current: u64,
    current_document: &Value,
) -> PublishReconciliation {
    if current == expected.saturating_add(1) && current_document == requested_document {
        PublishReconciliation::Completed {
            plan_id: plan_id.to_owned(),
            revision: current,
        }
    } else if current == expected {
        // The server checks an exact document fingerprint before CAS, so this is
        // safe both for a new write and a same-document idempotent publication.
        PublishReconciliation::SafeToSend
    } else {
        PublishReconciliation::Conflict
    }
}

fn canonical_source_document(arguments: &Value) -> Result<Value, String> {
    let object = arguments
        .as_object()
        .ok_or("Pending publication request is malformed")?;
    for field in ["title", "workstream", "capture_ids", "nodes"] {
        if !object.contains_key(field) {
            return Err(format!("Pending publication lacks {field}"));
        }
    }
    Ok(json!({
        "title": object["title"],
        "workstream": object["workstream"],
        "capture_ids": object["capture_ids"],
        "nodes": object["nodes"]
    }))
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
    fn prepare_arguments_fills_omitted_hosted_source_identity() {
        let capture = prepare_arguments(&params(
            "capture",
            "session-a",
            json!({"source":"codex", "external_id":"turn-1"}),
        ))
        .unwrap();
        assert_eq!(capture["conversation_key"], "session-a");
        assert_eq!(capture["source"], "codex");

        let publish = prepare_arguments(&params(
            "publish",
            "session-a",
            json!({"expected_revision":0}),
        ))
        .unwrap();
        assert_eq!(publish["source_key"], "session-a");
    }

    #[test]
    fn prepare_arguments_never_overwrites_explicit_source_identity() {
        let capture = prepare_arguments(&params(
            "capture",
            "session-a",
            json!({"source":"session-a", "conversation_key":"session-b", "external_id":"turn-1"}),
        ));
        assert_eq!(
            capture.unwrap_err(),
            "Capture arguments.conversation_key must match wrapper source_key; arguments.source identifies the adapter kind, not the conversation identity"
        );

        let publish = prepare_arguments(&params(
            "publish",
            "session-a",
            json!({"source_key":"session-b", "expected_revision":0}),
        ));
        assert_eq!(
            publish.unwrap_err(),
            "Publish arguments.source_key must match wrapper source_key"
        );
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
        assert!(
            schema["properties"]["action"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("outbox_status"))
        );
        let local = local_action_schema();
        assert_eq!(local["consume"]["arguments"]["id"], "required plan UUID");
        assert!(
            local["flush"]["meaning"]
                .as_str()
                .is_some_and(|meaning| meaning.contains("bound installation principal"))
        );
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

    #[test]
    fn lost_publish_success_requires_exact_next_revision_and_document() {
        let document =
            json!({"title":"Handoff","workstream":"switchboard","capture_ids":[],"nodes":[]});
        assert!(matches!(
            classify_publish_reconciliation("plan", 4, &document, 5, &document),
            PublishReconciliation::Completed { revision: 5, .. }
        ));
        assert!(matches!(
            classify_publish_reconciliation("plan", 4, &document, 5, &json!({"title":"foreign"})),
            PublishReconciliation::Conflict
        ));
        assert!(matches!(
            classify_publish_reconciliation("plan", 4, &document, 6, &document),
            PublishReconciliation::Conflict
        ));
    }

    #[test]
    fn same_document_at_expected_revision_is_retried_idempotently() {
        let document =
            json!({"title":"Handoff","workstream":"switchboard","capture_ids":[],"nodes":[]});
        assert!(matches!(
            classify_publish_reconciliation("plan", 4, &document, 4, &document),
            PublishReconciliation::SafeToSend
        ));
    }

    #[tokio::test]
    async fn publish_transport_reconciles_or_replays_only_at_safe_revision() {
        use std::collections::VecDeque;
        use std::future::ready;

        let plan_id = "2e2ea0dd-8b32-42e0-b102-fd18589a6214";
        let request = json!({
            "source_key":"source-a",
            "expected_revision":0,
            "title":"Handoff",
            "workstream":"switchboard",
            "capture_ids":[],
            "nodes":[]
        });
        let document = canonical_source_document(&request).unwrap();
        let list = json!({"work_plans":[{"source_key":"source-a","id":plan_id}]});

        let mut calls = Vec::new();
        let mut responses = VecDeque::from([
            Ok(list.clone()),
            Ok(
                json!({"plan_id":plan_id,"source_key":"source-a","current_revision":1,"source_document":document}),
            ),
        ]);
        let reconciled = publish_network_with("source-a", &request, |name, arguments| {
            calls.push((name, arguments));
            ready(responses.pop_front().unwrap())
        })
        .await
        .unwrap();
        assert!(matches!(
            reconciled,
            PublishDelivery::Reconciled { revision: 1, .. }
        ));
        assert_eq!(
            calls.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            ["work_plan_list", "work_plan_responses"]
        );

        let mut calls = Vec::new();
        let mut responses = VecDeque::from([
            Ok(list.clone()),
            Ok(
                json!({"plan_id":plan_id,"source_key":"source-a","current_revision":0,"source_document":canonical_source_document(&request).unwrap()}),
            ),
            Ok(json!({"plan_id":plan_id,"source_key":"source-a","current_revision":0})),
        ]);
        let retried = publish_network_with("source-a", &request, |name, arguments| {
            calls.push((name, arguments));
            ready(responses.pop_front().unwrap())
        })
        .await
        .unwrap();
        assert!(matches!(retried, PublishDelivery::Published(_)));
        assert_eq!(
            calls
                .iter()
                .filter(|(name, _)| *name == "work_plan_publish")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn publish_transport_blocks_changed_or_later_state_and_never_replays_on_read_failure() {
        use std::collections::VecDeque;
        use std::future::ready;

        let plan_id = "2e2ea0dd-8b32-42e0-b102-fd18589a6214";
        let request = json!({
            "source_key":"source-a","expected_revision":0,"title":"Handoff",
            "workstream":"switchboard","capture_ids":[],"nodes":[]
        });
        let list = json!({"work_plans":[{"source_key":"source-a","id":plan_id}]});
        for page in [
            json!({"plan_id":plan_id,"source_key":"source-a","current_revision":1,"source_document":{"title":"changed"}}),
            json!({"plan_id":plan_id,"source_key":"source-a","current_revision":2,"source_document":canonical_source_document(&request).unwrap()}),
        ] {
            let mut calls = Vec::new();
            let mut responses = VecDeque::from([Ok(list.clone()), Ok(page)]);
            let result = publish_network_with("source-a", &request, |name, arguments| {
                calls.push((name, arguments));
                ready(responses.pop_front().unwrap())
            })
            .await
            .unwrap();
            assert!(matches!(result, PublishDelivery::Conflict));
            assert!(!calls.iter().any(|(name, _)| *name == "work_plan_publish"));
        }

        let mut calls = Vec::new();
        let mut responses =
            VecDeque::from([Ok(list), Err("response read unavailable".to_string())]);
        let error = publish_network_with("source-a", &request, |name, arguments| {
            calls.push((name, arguments));
            ready(responses.pop_front().unwrap())
        })
        .await
        .unwrap_err();
        assert!(error.contains("read unavailable"));
        assert!(!calls.iter().any(|(name, _)| *name == "work_plan_publish"));
    }
}
