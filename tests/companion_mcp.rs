#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

struct Mcp {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
}

impl Mcp {
    fn start(config: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wardwell"))
            .arg("serve")
            .env("WARDWELL_CONFIG_DIR", config)
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, messages) = channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str(&line)
                    && send.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut mcp = Self {
            child,
            input,
            messages,
            reader: Some(reader),
            next_id: 0,
        };
        let init = mcp.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"companion-regression","version":"1"}}));
        assert!(init.get("result").is_some(), "{init}");
        writeln!(
            mcp.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        mcp
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        loop {
            let response = self
                .messages
                .recv_timeout(Duration::from_secs(20))
                .expect("MCP response timeout");
            if response["id"] == self.next_id {
                return response;
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn tool_data(result: Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn installed_stdio_surface_keeps_kanban_and_memory_working_without_hank() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let vault = directory.path().join("vault");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(vault.join("personal/cli-proof")).unwrap();
    std::fs::write(
        vault.join("personal/cli-proof/INDEX.md"),
        "# CLI proof\nLocal context retained.\n",
    )
    .unwrap();
    std::fs::write(config.join("config.yml"), format!("vault_path: {}\ndomains:\n  personal:\n    paths: [{}]\nsession_sources: []\nkanban:\n  enabled: true\n", serde_json::to_string(&vault).unwrap(), serde_json::to_string(&vault).unwrap())).unwrap();
    let mut mcp = Mcp::start(&config);
    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "wardwell_search",
        "wardwell_write",
        "wardwell_kanban",
        "wardwell_companion",
    ] {
        assert!(names.contains(&expected), "Missing {expected}");
    }
    let disconnected = mcp.tool("wardwell_companion", json!({"action":"status"}));
    assert_eq!(disconnected["isError"], true);
    assert!(
        disconnected["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not configured")
    );

    let created = tool_data(mcp.tool("wardwell_kanban", json!({"action":"create","domain":"personal","project":"cli-proof","title":"Preserve local kanban"})));
    let id = created["item"]["ticket_id"]
        .as_str()
        .or_else(|| created["ticket_id"].as_str())
        .expect("Created ticket ID");
    let fetched = tool_data(mcp.tool("wardwell_kanban", json!({"action":"get","ticket_id":id})));
    assert_eq!(fetched["item"]["title"], "Preserve local kanban");
    let written = tool_data(mcp.tool("wardwell_write", json!({"action":"write_file","domain":"personal","project":"cli-proof","path":"docs/local-check.md","body":"Local writes still work."})));
    assert_eq!(written["written"], true);
    assert_eq!(
        std::fs::read_to_string(vault.join("personal/cli-proof/docs/local-check.md")).unwrap(),
        "Local writes still work."
    );
    assert!(!config.join("hank/connection.json").exists());
}
