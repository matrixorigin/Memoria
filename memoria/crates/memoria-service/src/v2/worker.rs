use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use memoria_core::{truncate_utf8, MemoriaError};
use memoria_embedding::{ChatMessage, LlmClient};
use memoria_storage::SqlMemoryStore;
use memoria_storage::{MemoryV2JobEnricher, V2DerivedViews, V2LinkCandidate, V2LinkSuggestion};
use tracing::warn;

const V2_JOB_POLL_INTERVAL: Duration = Duration::from_millis(200);
const V2_JOB_BATCH_SIZE: usize = 8;

struct LlmV2JobEnricher {
    llm: Arc<LlmClient>,
}

impl LlmV2JobEnricher {
    fn new(llm: Arc<LlmClient>) -> Self {
        Self { llm }
    }

    fn extract_json_object(raw: &str) -> Option<&str> {
        let start = raw.find('{')?;
        let end = raw.rfind('}')?;
        (start <= end).then_some(&raw[start..=end])
    }

    fn extract_json_array(raw: &str) -> Option<&str> {
        let start = raw.find('[')?;
        let end = raw.rfind(']')?;
        (start <= end).then_some(&raw[start..=end])
    }
}

#[async_trait]
impl MemoryV2JobEnricher for LlmV2JobEnricher {
    async fn derive_views(
        &self,
        source_text: &str,
        abstract_text: &str,
    ) -> Result<Option<V2DerivedViews>, MemoriaError> {
        let prompt = format!(
            "Memory V2 derive views.\n\
             Return JSON object with keys `overview` and `detail`.\n\
             Rules: overview must be 1-2 concise sentences; detail should preserve the key facts without markdown.\n\
             Abstract:\n{}\n\nSource:\n{}\n\nJSON object:",
            truncate_utf8(abstract_text, 500),
            truncate_utf8(source_text, 3000)
        );
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }];
        let raw = match self.llm.chat(&msgs, 0.0, Some(500)).await {
            Ok(raw) => raw,
            Err(err) => {
                warn!(error = %err, "Memory V2 derive views LLM call failed");
                return Ok(None);
            }
        };
        let Some(json) = Self::extract_json_object(&raw) else {
            return Ok(None);
        };
        let parsed: serde_json::Value = match serde_json::from_str(json) {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!(error = %err, "Memory V2 derive views JSON parse failed");
                return Ok(None);
            }
        };
        let overview_text = parsed["overview"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        let detail_text = parsed["detail"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if overview_text.is_empty() && detail_text.is_empty() {
            return Ok(None);
        }
        Ok(Some(V2DerivedViews {
            overview_text,
            detail_text,
        }))
    }

    async fn refine_links(
        &self,
        source_abstract: &str,
        candidates: &[V2LinkCandidate],
    ) -> Result<Option<Vec<V2LinkSuggestion>>, MemoriaError> {
        if candidates.is_empty() {
            return Ok(None);
        }
        let prompt = format!(
            "Memory V2 refine links.\n\
             Given one source memory and candidate related memories, return a JSON array.\n\
             Each item must be {{\"memory_id\":\"...\",\"link_type\":\"supports|related|contrasts|depends_on\",\"strength\":0.0-1.0}}.\n\
             Only include candidates that are genuinely related.\n\n\
             Source abstract:\n{}\n\nCandidates:\n{}\n\nJSON array:",
            truncate_utf8(source_abstract, 600),
            serde_json::to_string(&candidates).unwrap_or_default()
        );
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }];
        let raw = match self.llm.chat(&msgs, 0.0, Some(400)).await {
            Ok(raw) => raw,
            Err(err) => {
                warn!(error = %err, "Memory V2 refine links LLM call failed");
                return Ok(None);
            }
        };
        let Some(json) = Self::extract_json_array(&raw) else {
            return Ok(None);
        };
        let parsed: Vec<serde_json::Value> = match serde_json::from_str(json) {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!(error = %err, "Memory V2 refine links JSON parse failed");
                return Ok(None);
            }
        };
        let suggestions = parsed
            .into_iter()
            .filter_map(|item| {
                let memory_id = item["memory_id"].as_str()?.trim().to_string();
                if memory_id.is_empty() {
                    return None;
                }
                let strength = item["strength"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
                if strength <= 0.0 {
                    return None;
                }
                let link_type =
                    truncate_utf8(item["link_type"].as_str().unwrap_or("related").trim(), 32)
                        .trim()
                        .to_string();
                Some(V2LinkSuggestion {
                    target_memory_id: memory_id,
                    link_type: if link_type.is_empty() {
                        "related".to_string()
                    } else {
                        link_type
                    },
                    strength,
                })
            })
            .collect::<Vec<_>>();
        if suggestions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(suggestions))
        }
    }
}

pub fn spawn_v2_job_worker(store: Arc<SqlMemoryStore>, llm: Option<Arc<LlmClient>>) {
    tokio::spawn(async move {
        let v2 = store.v2_store();
        let enricher = llm.map(LlmV2JobEnricher::new);
        loop {
            match v2
                .process_pending_jobs_with_enricher_pass(
                    V2_JOB_BATCH_SIZE,
                    enricher.as_ref().map(|e| e as &dyn MemoryV2JobEnricher),
                )
                .await
            {
                Ok(0) => tokio::time::sleep(V2_JOB_POLL_INTERVAL).await,
                Ok(_) => tokio::task::yield_now().await,
                Err(err) => {
                    warn!(error = %err, "Memory V2 job worker pass failed");
                    tokio::time::sleep(V2_JOB_POLL_INTERVAL).await;
                }
            }
        }
    });
}
