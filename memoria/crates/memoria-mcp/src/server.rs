use crate::{git_tools, remote::RemoteClient, tools};
use anyhow::Result;
use memoria_git::GitForDataService;
use memoria_service::{shutdown_signal, MemoryService};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Structured JSON-RPC error returned by [`dispatch`] and [`dispatch_http`].
/// Carries the standard error code so callers can forward it verbatim.
#[derive(Debug)]
pub struct McpRpcError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for McpRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for McpRpcError {}

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

enum RpcMethod {
    Initialize,
    ToolsList,
    ToolsCall,
    NotificationsInitialized,
    Unknown(String),
}

const GIT_TOOL_NAMES: &[&str] = &[
    "memory_snapshot",
    "memory_snapshots",
    "memory_snapshot_delete",
    "memory_rollback",
    "memory_branch",
    "memory_branches",
    "memory_checkout",
    "memory_merge",
    "memory_diff",
    "memory_apply",
    "memory_branch_delete",
];

fn is_git_tool(name: &str) -> bool {
    GIT_TOOL_NAMES.contains(&name)
}

/// Dispatch a single JSON-RPC method in embedded mode.
/// Used by the server-side Streamable HTTP MCP endpoint.
pub async fn dispatch_http(
    method: String,
    params: Option<Value>,
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<Value, McpRpcError> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(dispatch_embedded_owned(
            method, params, service, git, user_id,
        ))
    })
    .await
    .map_err(|e| McpRpcError {
        code: -32000,
        message: e.to_string(),
    })?
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
            let result = dispatch_http(
                method,
                Some(params),
                s.service.clone(),
                s.git.clone(),
                s.user_id.clone(),
            )
            .await;
            let resp = match result {
                Ok(v) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":if v.is_null(){serde_json::json!({})}else{v}}),
                Err(e) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":e.code,"message":e.message}}),
            };
            let _ = s.tx.send(serde_json::to_string(&resp).unwrap_or_default());
        }))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Memoria MCP SSE transport listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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

    loop {
        let line = tokio::select! {
            result = reader.next_line() => {
                match result? {
                    Some(l) => l,
                    None => break, // EOF
                }
            }
            _ = shutdown_signal() => break,
        };
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
                error: Some(json!({"code": e.code, "message": e.message})),
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
) -> Result<Value, McpRpcError> {
    let p = params.unwrap_or(Value::Null);
    let method = match method {
        "initialize" => RpcMethod::Initialize,
        "tools/list" => RpcMethod::ToolsList,
        "tools/call" => RpcMethod::ToolsCall,
        "notifications/initialized" => RpcMethod::NotificationsInitialized,
        _ => RpcMethod::Unknown(method.to_string()),
    };
    match method {
        RpcMethod::Initialize => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "memoria-mcp-rs", "version": "0.1.0"}
        })),
        RpcMethod::ToolsList => {
            let mut all_tools = tools::list().as_array().unwrap().clone();
            all_tools.extend(git_tools::list().as_array().unwrap().clone());
            Ok(json!({"tools": all_tools}))
        }
        RpcMethod::ToolsCall => {
            let name = p["name"].as_str().unwrap_or("").to_string();
            let args = p["arguments"].clone();
            let internal_err = |e: anyhow::Error| McpRpcError {
                code: -32000,
                message: e.to_string(),
            };
            match mode {
                Mode::Remote(client) => client.call(&name, args).await.map_err(internal_err),
                Mode::Embedded { service, git } => {
                    if is_git_tool(&name) {
                        git_tools::call(&name, args, git, service, user_id)
                            .await
                            .map_err(|e| McpRpcError {
                                code: -32000,
                                message: e.to_string(),
                            })
                    } else {
                        tools::call(&name, args, service, user_id)
                            .await
                            .map_err(internal_err)
                    }
                }
            }
        }
        RpcMethod::NotificationsInitialized => Ok(Value::Null),
        RpcMethod::Unknown(method) => Err(McpRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
        }),
    }
}

async fn dispatch_embedded_owned(
    method: String,
    params: Option<Value>,
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<Value, McpRpcError> {
    let p = params.unwrap_or(Value::Null);
    let method = match method.as_str() {
        "initialize" => RpcMethod::Initialize,
        "tools/list" => RpcMethod::ToolsList,
        "tools/call" => RpcMethod::ToolsCall,
        "notifications/initialized" => RpcMethod::NotificationsInitialized,
        _ => RpcMethod::Unknown(method),
    };
    match method {
        RpcMethod::Initialize => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "memoria-mcp-rs", "version": "0.1.0"}
        })),
        RpcMethod::ToolsList => {
            let mut all_tools = tools::list().as_array().unwrap().clone();
            all_tools.extend(git_tools::list().as_array().unwrap().clone());
            Ok(json!({"tools": all_tools}))
        }
        RpcMethod::ToolsCall => {
            let name = p["name"].as_str().unwrap_or("").to_string();
            let args = p["arguments"].clone();
            let internal_err = |e: anyhow::Error| McpRpcError {
                code: -32000,
                message: e.to_string(),
            };
            if is_git_tool(&name) {
                git_tools::call_owned(name, args, git, service, user_id)
                    .await
                    .map_err(|e| McpRpcError {
                        code: -32000,
                        message: e.to_string(),
                    })
            } else {
                tools::call_owned(name, args, service, user_id)
                    .await
                    .map_err(internal_err)
            }
        }
        RpcMethod::NotificationsInitialized => Ok(Value::Null),
        RpcMethod::Unknown(method) => Err(McpRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_git_tool, GIT_TOOL_NAMES};

    #[test]
    fn git_dispatch_list_includes_memory_apply() {
        assert!(is_git_tool("memory_apply"));
    }

    #[test]
    fn git_tool_dispatch_matches_declared_git_tools() {
        let declared_names: Vec<String> = crate::git_tools::list()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
            .map(str::to_string)
            .collect();

        for name in GIT_TOOL_NAMES {
            assert!(
                declared_names.iter().any(|declared| declared == name),
                "dispatch marked '{name}' as a git tool but git_tools::list() does not declare it"
            );
        }

        for name in declared_names {
            assert!(
                is_git_tool(&name),
                "git_tools::list() declares '{name}' but server dispatch does not route it"
            );
        }
    }
}
