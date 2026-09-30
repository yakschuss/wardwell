#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A readonly tracker binding closes the kanban write path for its project.

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

fn text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

/// Every file under `dir`, recursively, with its contents.
fn snapshot(dir: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            match path.is_dir() {
                true => pending.push(path),
                false => {
                    files.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
    }
    files
}

fn write_config(config: &std::path::Path, vault: &std::path::Path, readonly: bool) {
    let vault = serde_json::to_string(vault).unwrap();
    std::fs::write(
        config.join("config.yml"),
        format!(
            "vault_path: {vault}\ndomains:\n  work:\n    paths: [{vault}]\nsession_sources: []\nkanban:\n  enabled: true\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: corr-linear\n    readonly: {readonly}\n"
        ),
    )
    .unwrap();
}

#[test]
fn readonly_tracker_binding_refuses_kanban_writes_and_leaves_the_log_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let vault = directory.path().join("vault");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(vault.join("work/claims")).unwrap();
    std::fs::create_dir_all(vault.join("work/scratch")).unwrap();

    // Writable binding: seed one ticket.
    write_config(&config, &vault, false);
    let id = {
        let mut mcp = Mcp::start(&config);
        let created = tool_data(mcp.tool(
            "wardwell_kanban",
            json!({"action":"create","domain":"work","project":"claims","title":"Seeded before lock"}),
        ));
        created["item"]["ticket_id"].as_str().unwrap().to_string()
    };

    write_config(&config, &vault, true);
    let log = vault.join("work/claims/kanban.jsonl");
    let before = std::fs::read(&log).unwrap();
    let folder_before = snapshot(&vault.join("work/claims"));
    let mut mcp = Mcp::start(&config);

    let writes = [
        json!({"action":"create","domain":"work","project":"claims","title":"Should not land"}),
        json!({"action":"create","project":"claims","title":"Should not land either"}),
        json!({"action":"update","ticket_id":id,"title":"Renamed"}),
        json!({"action":"move","ticket_id":id,"status":"active"}),
        json!({"action":"note","ticket_id":id,"text":"a note"}),
        json!({"action":"attach","ticket_id":id,"title":"doc.md","text":"body"}),
        json!({"action":"detach","ticket_id":id,"attachment_id":"att-1"}),
        json!({"action":"sequence","ticket_id":id,"position":1}),
        json!({"action":"sequence","project":"claims","order":[id]}),
        json!({"action":"groom","ticket_id":id}),
        json!({"action":"groom","domain":"work","project":"claims"}),
        json!({"action":"relationship_create","from_ticket_id":id,"to_ticket_id":id,"relationship_type":"blocks"}),
        json!({"action":"relationship_delete","domain":"work","project":"claims","relationship_id":"rel-1"}),
        json!({"action":"question_create","domain":"work","project":"claims","question_text":"Why?"}),
        json!({"action":"question_update","domain":"work","project":"claims","target_id":"q-1","question_text":"Why now?"}),
        json!({"action":"question_answer","domain":"work","project":"claims","target_id":"q-1","answer":"Because"}),
        json!({"action":"question_invalidate","domain":"work","project":"claims","target_id":"q-1"}),
        json!({"action":"proposal_create","domain":"work","project":"claims","title":"Plan","changes":[]}),
        json!({"action":"proposal_approve","domain":"work","project":"claims","target_id":"prop-1"}),
        json!({"action":"proposal_reject","domain":"work","project":"claims","target_id":"prop-1"}),
        json!({"action":"proposal_apply","domain":"work","project":"claims","target_id":"prop-1"}),
        json!({"action":"verify","ticket_id":id,"verification_source":"code","confidence":"verified"}),
        json!({"action":"status","domain":"work","project":"claims"}),
        json!({"action":"export_roadmap","project":"claims"}),
    ];
    let mut exercised: Vec<String> = writes.iter().map(|w| w["action"].as_str().unwrap().to_string()).collect();
    exercised.sort();
    exercised.dedup();
    let mut locked: Vec<String> = wardwell::tracker::LOCKED_KANBAN_ACTIONS.iter().map(|a| a.to_string()).collect();
    locked.sort();
    assert_eq!(exercised, locked, "every locked action is exercised once");
    for request in writes {
        let body = text(&mcp.tool("wardwell_kanban", request.clone()));
        assert!(body.contains("read-only mirror of Linear"), "{request}: {body}");
        assert!(body.contains("Edit it in Linear"), "{request}: {body}");
    }
    assert_eq!(std::fs::read(&log).unwrap(), before, "kanban.jsonl unchanged");
    assert_eq!(snapshot(&vault.join("work/claims")), folder_before, "project folder unchanged");

    // Reads still work.
    let fetched = tool_data(mcp.tool("wardwell_kanban", json!({"action":"get","ticket_id":id})));
    assert_eq!(fetched["item"]["title"], "Seeded before lock");
    let listed = text(&mcp.tool("wardwell_kanban", json!({"action":"list","project":"claims"})));
    assert!(listed.contains("Seeded before lock"), "{listed}");

    // An unbound project in the same domain stays writable.
    let other = tool_data(mcp.tool(
        "wardwell_kanban",
        json!({"action":"create","domain":"work","project":"scratch","title":"Unlocked"}),
    ));
    assert_eq!(other["item"]["title"], "Unlocked");
}

#[test]
fn readonly_lock_resolves_the_domain_the_store_writes_to_not_the_callers() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let vault = directory.path().join("vault");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(vault.join("work/claims")).unwrap();

    write_config(&config, &vault, false);
    let id = {
        let mut mcp = Mcp::start(&config);
        let created = tool_data(mcp.tool(
            "wardwell_kanban",
            json!({"action":"create","domain":"work","project":"claims","title":"Seeded before lock"}),
        ));
        created["item"]["ticket_id"].as_str().unwrap().to_string()
    };

    write_config(&config, &vault, true);
    let log = vault.join("work/claims/kanban.jsonl");
    let before = std::fs::read(&log).unwrap();
    let mut mcp = Mcp::start(&config);

    let body = text(&mcp.tool(
        "wardwell_kanban",
        json!({"action":"sequence","domain":"personal","project":"claims","order":[id]}),
    ));
    assert!(body.contains("read-only mirror of Linear"), "{body}");
    assert!(body.contains("Edit it in Linear"), "{body}");
    assert_eq!(std::fs::read(&log).unwrap(), before, "kanban.jsonl unchanged");
}
