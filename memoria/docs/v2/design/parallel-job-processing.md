# V2 Job 并行处理设计

## 实现状态

- **当前实现（已落地）**：`process_user_pending_jobs_pass` 在 `while` 循环中逐个 `claim_next_job_for_user` 并同步处理，属于串行执行模型。
- **本文状态（提案）**：并发处理方案（批量认领、`tokio::spawn` / `FuturesUnordered`、并发度配置）为目标架构，需要在现有串行实现上分阶段引入。
- **已实现并应保留的保障**：`extract_entities` 相关写路径使用 `FOR UPDATE`（锁定当前 head 版本）保证并发重入下的幂等一致性。

## 背景与动机

当前 V2 job 处理是**严格串行**的：

```
process_user_pending_jobs_pass()
  └── while loop: claim_next_job → process_claimed_job → mark_done/failed
```

每个 job 必须完全处理完毕才能认领下一个。在以下场景下串行处理会成为瓶颈：

- 用户单次 `batch_remember` 产生大量 job（N 个 remember × 3 个 job 类型）。
- `derive_views` job 需要调用 LLM，延迟高（500ms–3s）。
- 多个用户的 job 在同一 worker 轮次中积压。

---

## 目标

- 同一用户的**不同类型**且**无依赖关系**的 job 可并发处理。
- 不同用户的 job 也可并发处理（跨用户无共享可写状态）。
- 保持 `extract_links` / `extract_entities` / `derive_views` 的语义正确性（同一 memory 的 derive_views 必须在 extract_links 之后，但不同 memory 间无依赖）。
- 不影响现有串行路径的正确性（渐进式启用）。

---

## Job 依赖关系分析

```
remember(memory_id)
  ├── extract_links     (无前置依赖)
  ├── extract_entities  (无前置依赖)
  └── derive_views      (建议在 extract_links 后执行，但不强制)
```

跨 memory 完全无依赖。单 memory 内：
- `extract_links` 和 `extract_entities` 可并发。
- `derive_views` 若要引用 links 数据，需等待 `extract_links` 完成。

当前实现中 `derive_views` 不读取 links 结果，因此**三个 job 实际上可完全并发**。

---

## 设计方案

### 方案 A：`tokio::spawn` 批量并发（推荐）

在 `process_user_pending_jobs_pass` 中改为批量认领 + 并发执行（目标形态）：

```rust
/// 每次 pass 最多并发处理的 job 数
const JOB_CONCURRENCY: usize = 8;

pub async fn process_user_pending_jobs_pass(
    &self,
    user_id: &str,
    pass_limit: i64,
) -> Result<i64, MemoriaError> {
    let jobs = self.claim_next_n_jobs_for_user(user_id, JOB_CONCURRENCY).await?;
    if jobs.is_empty() { return Ok(0); }

    let handles: Vec<_> = jobs.into_iter().map(|job| {
        let store = self.clone(); // Arc-based clone
        tokio::spawn(async move {
            store.process_claimed_job(job).await
        })
    }).collect();

    let mut done = 0i64;
    for h in handles {
        match h.await {
            Ok(Ok(_)) => done += 1,
            Ok(Err(e)) => tracing::warn!("job failed: {e}"),
            Err(e) => tracing::warn!("job panicked: {e}"),
        }
    }
    Ok(done)
}
```

**关键前提（需先满足）**：
- 当前 `MemoryV2Store` 为普通结构体字段（如 `pool`、`embedding_dim`），并非已验证的 `Arc<Inner>` 包装。
- 若采用 `tokio::spawn` 并发执行，需要先明确共享策略（例如引入 `Arc` 包装或调整任务函数签名）以满足 `'static` 与跨任务共享要求。

### 方案 B：`FuturesUnordered` 流式并发

适合 job 数量不定时（同样属于目标架构）：

```rust
use futures::stream::{FuturesUnordered, StreamExt};

let mut futs = FuturesUnordered::new();
while let Some(job) = self.claim_next_job_for_user(user_id).await? {
    let store = self.clone();
    futs.push(async move { store.process_claimed_job(job).await });
    if futs.len() >= JOB_CONCURRENCY { futs.next().await; }
}
while futs.next().await.is_some() {}
```

方案 B 更灵活但实现略复杂；建议先用方案 A。

---

## 并发安全分析

| 操作 | 是否安全 | 说明 |
|------|---------|------|
| `extract_entities` 并发同一 memory | ✅ | 已有 `FOR UPDATE` 行锁保证幂等 |
| `derive_views` 并发同一 memory | ✅ | 以 `content_version_id` 版本隔离，幂等写 |
| `extract_links` 并发同一 memory | ⚠️ | 需验证 link upsert 路径是否幂等（预计是，因为用 `INSERT OR IGNORE`） |
| 不同 memory 并发 | ✅ | 完全独立，无共享写路径 |

---

## `claim_next_n_jobs` 实现

新增 SQL helper，一次认领多个 job：

```sql
-- 认领最多 N 个待处理 job（乐观锁版本，每个 job 独立 UPDATE）
SELECT job_id, job_type, memory_id, attempts
FROM {jobs_table}
WHERE status = 'pending'
  AND (leased_until IS NULL OR leased_until < NOW())
ORDER BY created_at ASC
LIMIT ?;

-- 然后对每个 job_id 执行 UPDATE ... SET status='in_progress', leased_until=...
```

注意：需要在 SELECT 和 UPDATE 之间处理竞态（其他 worker 同时认领），UPDATE 后检查 affected_rows == 1，若为 0 则跳过该 job。

---

## 渐进式启用

1. 默认 `JOB_CONCURRENCY = 1`（等价于现有串行行为）。
2. 通过环境变量 `MEMORIA_JOB_CONCURRENCY` 控制并发度。
3. 在充分测试后将默认值提升到 4–8。

---

## 监控

并行处理后应记录：
- 每次 pass 的 job 数量、并发度、处理时长分布。
- job 类型维度的 P50/P95 延迟。
- 失败率（需与 `JobMetrics` 联动，见 `job-metrics.md`）。

---

## 风险

- **MatrixOne 连接池压力**：并发 job 会增加同时持有的 DB 连接数，需确保连接池大小（`max_connections`）足够。建议 `JOB_CONCURRENCY × worker_count ≤ pool_size × 0.5`。
- **LLM 并发调用**：`derive_views` 调用 LLM，高并发下需注意 rate limit，必要时引入信号量限流。
