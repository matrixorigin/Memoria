use crate::{git_tools, remote::RemoteClient, tools};
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
    #[allow(dead_code)]
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

/// Run in embedded mode (direct DB).
pub async fn run_stdio(
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<()> {
    run_loop(Mode::Embedded { service, git }, user_id).await
}

/// Run in remote mode (proxy to REST API).
pub async fn run_stdio_remote(remote: RemoteClient, user_id: String) -> Result<()> {
    run_loop(Mode::Remote(remote), user_id).await
}

/// Run SSE transport — MCP over HTTP.
/// Clients connect to GET /sse for server-sent events, POST /message to send requests.
pub async fn run_sse(
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
    port: u16,
) -> Result<()> {
    use axum::{
        extract::State,
        response::sse::{Event, Sse},
        routing::{get, post},
        Router,
    };
    use futures::stream::{self};
    use std::convert::Infallible;
    use tokio::sync::broadcast;

    #[derive(Clone)]
    struct SseState {
        tx: broadcast::Sender<String>,
        service: Arc<MemoryService>,
        git: Arc<GitForDataService>,
        user_id: String,
    }

    let (tx, _) = broadcast::channel::<String>(64);
    let state = SseState {
        tx: tx.clone(),
        service,
        git,
        user_id,
    };

    let app = Router::new()
        .route("/sse", get(|State(s): State<SseState>| async move {
            let rx = s.tx.subscribe();
            let stream = stream::unfold(rx, |mut rx| async move {
                match rx.recv().await {
                    Ok(msg) => Some((Ok::<Event, Infallible>(Event::default().data(msg)), rx)),
                    Err(_) => None,
                }
            });
            Sse::new(stream)
        }))
        .route("/message", post(|State(s): State<SseState>, body: String| async move {
            let req: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    let resp = serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}});
                    let _ = s.tx.send(serde_json::to_string(&resp).unwrap_or_default());
                    return;
                }
            };
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("").to_string();
            let params = req["params"].clone();
            let mode = Mode::Embedded { service: s.service.clone(), git: s.git.clone() };
            let result = dispatch(&method, Some(params), &mode, &s.user_id).await;
            let resp = match result {
                Ok(v) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":if v.is_null(){serde_json::json!({})}else{v}}),
                Err(e) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}}),
            };
            let _ = s.tx.send(serde_json::to_string(&resp).unwrap_or_default());
        }))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Memoria MCP SSE transport listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

enum Mode {
    Embedded {
        service: Arc<MemoryService>,
        git: Arc<GitForDataService>,
    },
    Remote(RemoteClient),
}

async fn run_loop(mode: Mode, user_id: String) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("PARSE_ERROR: {e} | RAW: {line}");
                let resp = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}});
                write_line(&mut stdout, &resp).await?;
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        if req.id.is_none() {
            let _ = dispatch(&req.method, req.params, &mode, &user_id).await;
            continue;
        }

        let result = dispatch(&req.method, req.params, &mode, &user_id).await;

        let resp = match result {
            Ok(v) => {
                let result_val = if v.is_null() { json!({}) } else { v };
                Response {
                    jsonrpc: "2.0",
                    id,
                    result: Some(result_val),
                    error: None,
                }
            }
            Err(e) => Response {
                jsonrpc: "2.0",
                id,
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
    mode: &Mode,
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
            match mode {
                Mode::Remote(client) => client.call(&name, args).await,
                Mode::Embedded { service, git } => {
                    let git_tool_names = [
                        "memory_snapshot",
                        "memory_snapshots",
                        "memory_snapshot_delete",
                        "memory_rollback",
                        "memory_branch",
                        "memory_branches",
                        "memory_checkout",
                        "memory_merge",
                        "memory_diff",
                        "memory_branch_delete",
                    ];
                    if git_tool_names.contains(&name.as_str()) {
                        git_tools::call(&name, args, git, service, user_id).await
                    } else {
                        tools::call(&name, args, service, user_id).await
                    }
                }
            }
        }
        "notifications/initialized" => Ok(Value::Null),
        _ => Err(anyhow::anyhow!("Method not found: {method}")),
    }
}
