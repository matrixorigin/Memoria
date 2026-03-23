use anyhow::Result;
use serde_json::Value;

use crate::remote::RemoteClient;

pub async fn call(client: &RemoteClient, name: &str, args: Value) -> Result<Option<Value>> {
    let result = match name {
        "memory_v2_remember" => {
            let response = client
                .http_client()
                .post(client.url("/v2/memory/remember"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_recall" => {
            let response = client
                .http_client()
                .post(client.url("/v2/memory/recall"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_list" => {
            let params =
                RemoteClient::query_pairs(&args, &["limit", "cursor", "type", "session_id"]);
            let response = client
                .http_client()
                .get(client.url("/v2/memory/list"))
                .query(&params)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_profile" => {
            let params = RemoteClient::query_pairs(&args, &["limit", "cursor", "session_id"]);
            let response = client
                .http_client()
                .get(client.url("/v2/memory/profile"))
                .query(&params)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_expand" => {
            let response = client
                .http_client()
                .post(client.url("/v2/memory/expand"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_focus" => {
            let response = client
                .http_client()
                .post(client.url("/v2/memory/focus"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_history" => {
            let memory_id = args["memory_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let params = RemoteClient::query_pairs(&args, &["limit"]);
            let response = client
                .http_client()
                .get(client.url(&format!("/v2/memory/{memory_id}/history")))
                .query(&params)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_update" => {
            let response = client
                .http_client()
                .patch(client.url("/v2/memory/update"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_forget" => {
            let response = client
                .http_client()
                .post(client.url("/v2/memory/forget"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        "memory_v2_reflect" => {
            let response = client
                .http_client()
                .post(client.url("/v2/memory/reflect"))
                .json(&args)
                .send()
                .await?;
            Some(RemoteClient::mcp_json(
                &RemoteClient::parse_response(response).await?,
            ))
        }
        _ => None,
    };

    Ok(result)
}
