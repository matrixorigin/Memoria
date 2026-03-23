# V2 数据完整性校验设计

## 实现状态

- ✅ 已实现：V2 记忆链路已有 jobs lease/重试与失败回退机制，可覆盖多数瞬时异常。
- ❌ 未实现：文档中的 `verify_user_data_integrity` 接口、`IntegrityReport` 结构体、以及 `/v2/admin/integrity-check*` 端点目前均为提案。
- ✅ 设计定位：应先落地“诊断优先（`auto_fix=false`）”版本，作为并行作业演进前的安全网。
- ⚠️ 最终目标：在提升并行处理能力时，先确保可观测、可审计、可回滚，再逐步启用自动修复。

## 背景与动机

V2 的异步富化架构（remember → jobs → derive_views / extract_links / extract_entities）在以下情况下可能产生数据不一致：

1. **worker 崩溃**：job 处于 `in_progress` 状态但实际未完成，超时后重试，但部分写入已存在。
2. **并发写入**：多个 worker 并发处理同一 job（已通过 `FOR UPDATE` 缓解，但不能完全排除异常）。
3. **DDL 变更**：数据库迁移中途失败，部分 table family 结构不一致。
4. **手动操作**：运维直接修改数据库，绕过业务层校验。
5. **MatrixOne 特性**：eventual consistency DDL 可能导致建表成功但查询偶发失败。

**校验系统的目标**是提供离线诊断工具（而非实时约束），优先发现问题，再决定是否修复；这对后续更激进的并行 job 调整是必要安全网。

---

## 校验范围

### 1. Heads ↔ Events 一致性

**规则**：每个 `head` 记录（当前 memory 状态）必须对应至少一个 `event`。

```sql
-- 找出 head 存在但无对应 event 的 memory
SELECT h.memory_id
FROM {heads_table} h
LEFT JOIN {events_table} e ON e.memory_id = h.memory_id
WHERE e.memory_id IS NULL;
```

**修复建议**：若 head 有效（has_content = true），补充一个 `remember` 事件；若无效，删除孤立 head。

### 2. Content Versions ↔ Index Docs 一致性

**规则**：每个 `content_version` 必须有对应的 `index_doc`（除非 job 尚未完成）。

```sql
-- 找出 content_version 存在但无 index_doc 的记录（且 derivation_state = 'complete'）
SELECT cv.content_version_id, cv.memory_id
FROM {cver_table} cv
LEFT JOIN {idx_table} idx ON idx.content_version_id = cv.content_version_id
WHERE idx.content_version_id IS NULL
  AND cv.derivation_state = 'complete';
```

**修复建议**：重新入队 `derive_views` job。

### 3. Links 双向一致性

**规则**：若 `link(A→B)` 存在，则 `link(B→A)` 的反向记录也应存在（双向 link）。

```sql
SELECT l1.memory_id, l1.target_memory_id
FROM {links_table} l1
LEFT JOIN {links_table} l2
  ON l2.memory_id = l1.target_memory_id
  AND l2.target_memory_id = l1.memory_id
WHERE l2.memory_id IS NULL
  AND l1.is_bidirectional = 1;
```

**修复建议**：补充缺失的反向 link。

### 4. 孤立 Jobs

**规则**：`in_progress` 状态且 `leased_until < NOW()` 的 job 表示 worker 已失联，应重置为 `pending`。

```sql
SELECT job_id, job_type, memory_id, attempts, leased_until
FROM {jobs_table}
WHERE status = 'in_progress'
  AND leased_until < NOW();
```

**修复建议**：`UPDATE ... SET status='pending', leased_until=NULL WHERE ...`（已有自动重试，此为手动触发版本）。

### 5. 超出重试上限的 Failed Jobs

**规则**：`attempts >= MAX_JOB_ATTEMPTS` 且 `status = 'failed'` 的 job 需人工关注。

```sql
SELECT job_id, job_type, memory_id, attempts, last_error
FROM {jobs_table}
WHERE status = 'failed'
ORDER BY updated_at DESC
LIMIT 100;
```

### 6. Memory Entities 与 Heads 一致性

**规则**：`memory_entities` 中的 `content_version_id` 应与 `heads` 中的 `current_content_version_id` 一致。

```sql
SELECT me.memory_id, me.content_version_id, h.current_content_version_id
FROM {ment_table} me
JOIN {heads_table} h ON h.memory_id = me.memory_id
WHERE me.content_version_id != h.current_content_version_id;
```

**修复建议**：触发 `extract_entities` 重新处理。

---

## API 设计

> **说明：本节均为提案接口，当前仓库尚未实现。**

### Rust 接口

```rust
#[derive(Debug, Serialize)]
pub struct IntegrityReport {
    pub user_id: String,
    pub checked_at: DateTime<Utc>,
    pub orphan_heads: Vec<String>,            // memory_id
    pub missing_index_docs: Vec<String>,      // content_version_id
    pub broken_bidirectional_links: Vec<(String, String)>, // (from, to)
    pub stale_in_progress_jobs: Vec<String>,  // job_id
    pub permanently_failed_jobs: Vec<FailedJob>,
    pub stale_entity_versions: Vec<String>,   // memory_id
}

impl MemoryV2Store {
    pub async fn verify_user_data_integrity(
        &self,
        user_id: &str,
        auto_fix: bool,  // true = 自动修复可安全修复的问题；false = 仅诊断
    ) -> Result<IntegrityReport, MemoriaError> { ... }
}
```

### HTTP 端点

```
POST /v2/admin/integrity-check
Body: { "user_id": "...", "auto_fix": false }
Response: IntegrityReport

POST /v2/admin/integrity-check-all
Body: { "auto_fix": false }
Response: { "users_checked": N, "issues_found": M, "reports": [...] }
```

> 端点应受 admin 鉴权保护，不对普通用户开放（提案）。

---

## `auto_fix` 策略

> 默认建议：`auto_fix=false`（诊断优先）。仅在确认规则稳定、可回滚后再逐步开启有限自动修复。

| 问题类型 | auto_fix=true 时的行为 |
|---------|----------------------|
| 孤立 head（无 event） | **不自动修复**（需人工确认是否是有效 memory） |
| 缺失 index_doc | 重新入队 `derive_views` job |
| 反向 link 缺失 | 补充反向 link |
| 超时 in_progress jobs | 重置为 pending |
| stale entity versions | 重新入队 `extract_entities` job |
| permanently failed jobs | **不自动修复**（需人工分析 last_error） |

---

## 实施建议

1. **先实现诊断（`auto_fix=false`）**，确保校验查询准确、输出可审计，再考虑修复逻辑。
2. **作为定时任务运行**（例如每天一次），而不是实时校验。
3. **在 admin dashboard 展示 `IntegrityReport`**，通过 `JobMetrics`（见 `job-metrics.md`）补充 job 健康度视角。
4. **注意查询性能**：`verify_user_data_integrity`（提案）会扫描全量 table，应在低峰期运行或加 LIMIT。
5. **并行改造前置检查**：在扩大 worker 并行度、放宽 lease 或引入更复杂并行 job 前，先以该诊断流程跑基线并持续回归。

---

## 与其他设计的关系

- 依赖 **`job-metrics.md`** 中的 `JobMetrics` 作为失败 job 快速入口。
- 与 **`parallel-job-processing.md`** 中的并发 job 设计互补（并发增加幂等与一致性风险，校验工具可作为安全网验证演进稳定性）。
- `stale_in_progress_jobs` 的自动重置逻辑与现有 `JOB_LEASE_SECS` 超时重试机制重叠，二者可合并或明确分工（自动重试 vs 手动 admin 工具）。
