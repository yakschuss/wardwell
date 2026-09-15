use std::time::Duration;

use reqwest::{Url, redirect::Policy};
use serde_json::{Value, json};

const REQUEST_LIMIT: usize = 256 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

/// Calls one method on the configured, stateless MCP HTTP endpoint.
///
/// The returned value is the JSON-RPC `result`, including a tool result's
/// `isError` field when present.
pub async fn call(
    endpoint: &str,
    token: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    call_inner(endpoint, token, method, params, false, TIMEOUT).await
}

async fn call_inner(
    endpoint: &str,
    token: &str,
    method: &str,
    params: Value,
    allow_loopback_http: bool,
    timeout: Duration,
) -> Result<Value, String> {
    let endpoint = validate_endpoint(endpoint, allow_loopback_http)?;
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    }))
    .map_err(|_| "could not encode MCP request".to_owned())?;

    if body.len() > REQUEST_LIMIT {
        return Err("MCP request exceeds 256 KiB".to_owned());
    }

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| "could not create MCP HTTP client".to_owned())?;

    let mut response = client
        .post(endpoint)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| "MCP request failed".to_owned())?;

    if response.status().is_redirection() {
        return Err("MCP endpoint redirects are not allowed".to_owned());
    }
    if !response.status().is_success() {
        return Err(format!(
            "MCP endpoint returned HTTP {}",
            response.status().as_u16()
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("text/event-stream") {
        return Err("MCP SSE responses are unsupported".to_owned());
    }
    if !content_type.starts_with("application/json") {
        return Err("MCP endpoint returned an unsupported content type".to_owned());
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "could not read MCP response".to_owned())?
    {
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err("MCP response exceeds 1 MiB".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }

    let envelope: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "MCP endpoint returned invalid JSON".to_owned())?;
    decode_envelope(envelope)
}

fn validate_endpoint(endpoint: &str, allow_loopback_http: bool) -> Result<Url, String> {
    let url = Url::parse(endpoint).map_err(|_| "invalid MCP endpoint URL".to_owned())?;
    let allowed = url.scheme() == "https"
        || (allow_loopback_http
            && url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }));
    if !allowed {
        return Err("MCP endpoint must use HTTPS".to_owned());
    }
    Ok(url)
}

fn decode_envelope(envelope: Value) -> Result<Value, String> {
    if envelope.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("MCP endpoint returned an invalid JSON-RPC response".to_owned());
    }
    if envelope.get("id") != Some(&json!(1)) {
        return Err("MCP endpoint returned a mismatched JSON-RPC id".to_owned());
    }
    if let Some(error) = envelope.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        return Err(code.map_or_else(
            || "MCP endpoint returned a JSON-RPC error".to_owned(),
            |code| format!("MCP endpoint returned JSON-RPC error {code}"),
        ));
    }
    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| "MCP endpoint response has no result".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(&response).await.unwrap();
        });
        format!("http://{address}/mcp")
    }

    async fn test_call(endpoint: &str) -> Result<Value, String> {
        call_inner(
            endpoint,
            "very-secret-token",
            "tools/call",
            json!({}),
            true,
            TIMEOUT,
        )
        .await
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut result = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        result.extend_from_slice(body);
        result
    }

    #[tokio::test]
    async fn returns_result_and_preserves_tool_error() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[]}}"#;
        let endpoint = serve(response(
            "200 OK",
            "Content-Type: application/json\r\n",
            body,
        ))
        .await;
        let value = test_call(&endpoint).await.unwrap();
        assert_eq!(value["isError"], true);
    }

    #[tokio::test]
    async fn sanitizes_json_rpc_errors() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"very-secret-token"}}"#;
        let endpoint = serve(response(
            "200 OK",
            "Content-Type: application/json\r\n",
            body,
        ))
        .await;
        let error = test_call(&endpoint).await.unwrap_err();
        assert_eq!(error, "MCP endpoint returned JSON-RPC error -32000");
        assert!(!error.contains("very-secret-token"));
    }

    #[tokio::test]
    async fn rejects_mismatched_response_id() {
        let body = br#"{"jsonrpc":"2.0","id":2,"result":{}}"#;
        let endpoint = serve(response(
            "200 OK",
            "Content-Type: application/json\r\n",
            body,
        ))
        .await;
        assert_eq!(
            test_call(&endpoint).await.unwrap_err(),
            "MCP endpoint returned a mismatched JSON-RPC id"
        );
    }

    #[tokio::test]
    async fn rejects_oversized_request_before_connecting() {
        let params = json!({"content": "x".repeat(REQUEST_LIMIT)});
        let error = call_inner(
            "http://127.0.0.1:1/mcp",
            "secret",
            "tools/call",
            params,
            true,
            TIMEOUT,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "MCP request exceeds 256 KiB");
    }

    #[tokio::test]
    async fn applies_total_request_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((_socket, _)) = listener.accept().await else {
                return;
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let error = call_inner(
            &format!("http://{address}/mcp"),
            "secret",
            "tools/call",
            json!({}),
            true,
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "MCP request failed");
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_response() {
        let body = vec![b'x'; RESPONSE_LIMIT + 1];
        let response_head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut wire = response_head.to_vec();
        wire.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        wire.extend_from_slice(&body);
        wire.extend_from_slice(b"\r\n0\r\n\r\n");
        let endpoint = serve(wire).await;
        assert_eq!(
            test_call(&endpoint).await.unwrap_err(),
            "MCP response exceeds 1 MiB"
        );
    }

    #[tokio::test]
    async fn rejects_redirects_without_following_them() {
        let endpoint = serve(response(
            "302 Found",
            "Location: https://example.com/steal\r\n",
            b"",
        ))
        .await;
        assert_eq!(
            test_call(&endpoint).await.unwrap_err(),
            "MCP endpoint redirects are not allowed"
        );
    }

    #[tokio::test]
    async fn rejects_http_outside_test_helper() {
        let error = call("http://127.0.0.1:1/mcp", "secret", "tools/call", json!({}))
            .await
            .unwrap_err();
        assert_eq!(error, "MCP endpoint must use HTTPS");
    }

    #[tokio::test]
    async fn reports_sse_as_unsupported() {
        let endpoint = serve(response(
            "200 OK",
            "Content-Type: text/event-stream\r\n",
            b"data: {}\n\n",
        ))
        .await;
        assert_eq!(
            test_call(&endpoint).await.unwrap_err(),
            "MCP SSE responses are unsupported"
        );
    }
}
