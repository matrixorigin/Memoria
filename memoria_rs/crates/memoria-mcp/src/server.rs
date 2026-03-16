use crate::{git_tools, tools};
use anyhow::Result;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

pub async fn run_stdio(
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() { continue; }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // Log the raw line to stderr for debugging
                eprintln!("PARSE_ERROR: {e} | RAW: {line}");
                let resp = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}});
                write_line(&mut stdout, &resp).await?;
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        // Notifications (no id) must not receive a response
        if req.id.is_none() {
            let _ = dispatch(&req.method, req.params, &service, &git, &user_id).await;
            continue;
        }

        let result = dispatch(&req.method, req.params, &service, &git, &user_id).await;

        let resp = match result {
            Ok(v) => {
                // result must be an object {}, not null — rmcp rejects null result
                let result_val = if v.is_null() { json!({}) } else { v };
                Response { jsonrpc: "2.0", id, result: Some(result_val), error: None }
            }
            Err(e) => Response {
                jsonrpc: "2.0", id,
                result: None,
                error: Some(json!({"code": -32000, "message": e.to_string()})),
            },
        };
        write_line(&mut stdout, &resp).await?;
    }
    Ok(())
}

async fn write_line(stdout: &mut tokio::io::Stdout, v: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

async fn dispatch(
    method: &str,
    params: Option<Value>,
    service: &Arc<MemoryService>,
    git: &Arc<GitForDataService>,
    user_id: &str,
) -> Result<Value> {
    let p = params.unwrap_or(Value::Null);
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "memoria-mcp-rs", "version": "0.1.0"}
        })),
        "tools/list" => {
            let mut all_tools = tools::list().as_array().unwrap().clone();
            all_tools.extend(git_tools::list().as_array().unwrap().clone());
            Ok(json!({"tools": all_tools}))
        }
        "tools/call" => {
            let name = p["name"].as_str().unwrap_or("").to_string();
            let args = p["arguments"].clone();
            // Route to correct handler
            let git_tool_names = ["memory_snapshot", "memory_snapshots", "memory_snapshot_delete",
                "memory_rollback", "memory_branch", "memory_branches", "memory_checkout",
                "memory_merge", "memory_branch_delete"];
            if git_tool_names.contains(&name.as_str()) {
                git_tools::call(&name, args, git, service, user_id).await
            } else {
                tools::call(&name, args, service, user_id).await
            }
        }
        "notifications/initialized" => Ok(Value::Null),
        _ => Err(anyhow::anyhow!("Method not found: {method}")),
    }
}
