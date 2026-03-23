# Memoria Memory V2 HTTP API（当前实现）

> 状态：已实现的 `/v2/*` HTTP 合同说明。本文以当前代码为准，不把尚未落地的设计稿包装成已上线能力。

## 范围

Memory V2 是与 V1 并行存在的一套独立系统，当前重点覆盖 agent 风格的记忆写入、检索、展开、关联浏览、反馈闭环和异步富化观察面。

V2 共享的仅是基础设施：认证、SQL 连接池、embedding/LLM、后台 worker 与通用 observability。V2 **不共享** V1 的路由、写路径、评分流水线或表作为 source of truth。

## MCP tool namespace

当前 Rust MCP 层也遵循同样的版本分离原则：

- 旧的 `memory_*` tools 继续对应 V1 行为
- V2 通过显式的 `memory_v2_*` tools 暴露，不会把同名 V1 tool 静默重定向到 V2
- `memory_v2_*` tool 的请求/响应字段与本文档中的 `/v2/*` HTTP contract 保持一致

当前已实现的显式 V2 MCP tools 包括：

- `memory_v2_remember`
- `memory_v2_recall`
- `memory_v2_list`
- `memory_v2_profile`
- `memory_v2_expand`
- `memory_v2_focus`
- `memory_v2_history`
- `memory_v2_update`
- `memory_v2_forget`
- `memory_v2_reflect`

## 当前实现状态

### 已实现

| 能力 | 方法与路径 |
|---|---|
| 写入记忆 | `POST /v2/memory/remember` |
| 批量写入记忆 | `POST /v2/memory/batch-remember` |
| 检索记忆 | `POST /v2/memory/recall` |
| 展开记忆 | `POST /v2/memory/expand` |
| 遗忘记忆 | `POST /v2/memory/forget` |
| 批量遗忘记忆 | `POST /v2/memory/batch-forget` |
| 设置 focus | `POST /v2/memory/focus` |
| 更新记忆 | `PATCH /v2/memory/update` |
| 列出记忆 | `GET /v2/memory/list` |
| 生成 V2 reflect candidates | `POST /v2/memory/reflect` |
| 读取 profile memories | `GET /v2/memory/profile` |
| 抽取 V2 entities | `POST /v2/memory/entities/extract` |
| 浏览 V2 entities | `GET /v2/memory/entities` |
| 聚合标签 | `GET /v2/memory/tags` |
| V2 统计总览 | `GET /v2/memory/stats` |
| 单条记忆事件历史 | `GET /v2/memory/:id/history` |
| 观察异步 jobs | `GET /v2/memory/jobs` |
| 浏览 direct links | `GET /v2/memory/links` |
| 浏览 related memories | `GET /v2/memory/related` |
| 记录/读取单条记忆反馈 | `GET/POST /v2/memory/:id/feedback` |
| 单条记忆反馈历史 | `GET /v2/memory/:id/feedback/history` |
| 全局 feedback feed | `GET /v2/feedback/history` |
| 全局 feedback stats | `GET /v2/feedback/stats` |
| 按 trust tier 聚合 feedback | `GET /v2/feedback/by-tier` |

## 通用约定

### type

当前 V2 HTTP contract 使用 `type` 作为 memory kind 字段，值与 V1 保持一致：

- `semantic`
- `episodic`
- `working`
- `profile`
- `procedural`

说明：

- `remember` / `recall` 请求使用 `type`
- `list` 查询过滤使用 `type`
- item-like 响应（如 recall verbose / list / links / related / reflect candidates / stats by_type）也统一输出 `type`
- 旧的 `memory_type` 不再属于 V2 HTTP contract

### trust_tier

当前实现接受 `T1` / `T2` / `T3` / `T4`。

### 反馈信号

当前 V2 feedback 仅支持：

- `useful`
- `irrelevant`
- `outdated`
- `wrong`

## 核心接口

### 1. remember

`POST /v2/memory/remember`

请求体：

```json
{
  "content": "Rust platform guide for systems teams",
  "type": "semantic",
  "session_id": "sess-v2",
  "importance": 0.6,
  "trust_tier": "T2",
  "tags": ["rust", "systems"],
  "source": {
    "kind": "chat",
    "app": "copilot"
  }
}
```

返回：

```json
{
  "memory_id": "...",
  "abstract": "Rust platform guide for systems teams",
  "has_overview": false,
  "has_detail": false
}
```

说明：

- `abstract` 会在写入时立即生成。
- `overview` / `detail` 通过异步 job 富化，因此新写入时通常还是 `false`。
- 标签会标准化为去重、小写后写入 V2 tags projection。
- 该请求走严格字段校验；旧的 `memory_type` 不再被 V2 接受。

### 2. recall

`POST /v2/memory/recall`

请求体：

```json
{
  "query": "rust platform",
  "top_k": 10,
  "max_tokens": 500,
  "scope": "all",
  "type": "semantic",
  "tags": ["rust", "systems"],
  "tag_filter_mode": "any",
  "expand_links": true,
  "view": "full"
}
```

返回：

```json
{
  "summary": {
    "discovered_count": 4,
    "returned_count": 2,
    "truncated": false,
    "by_retrieval_path": [
      {
        "retrieval_path": "direct_and_expanded",
        "discovered_count": 2,
        "returned_count": 2
      }
    ]
  },
  "memories": [
    {
      "id": "...",
      "abstract": "Rust platform guide for systems teams",
      "overview": "...",
      "score": 1.23,
      "type": "semantic",
      "confidence": 0.85,
      "has_overview": true,
      "has_detail": true,
      "access_count": 3,
      "link_count": 2,
      "has_related": true,
      "retrieval_path": "direct_and_expanded",
      "feedback_impact": {
        "useful": 1,
        "irrelevant": 0,
        "outdated": 0,
        "wrong": 0,
        "multiplier": 1.1
      }
    }
  ],
  "token_used": 88,
  "has_more": false
}
```

说明：

- `scope` 当前支持 `all` 与 `session`。
- 当 `scope=session` 时，必须同时提供 `session_id`。
- `tags` 可选，用于按标准化标签过滤 recall 结果；标签会按小写、去重后匹配。
- `tag_filter_mode` 当前支持 `any`（默认）与 `all`。
- `view=compact`（默认）返回 compact items：`id` / `text` / `type` / `score` / `related`。
- `view=overview` 仍返回 compact items，但 `text` 优先取 overview。
- `view=full` 返回 verbose explainability items，并可内联 `links`。
- `expand_links` 默认开启；显式传 `false` 时可得到纯 direct recall 语义。
- 时间过滤支持 `start_at` / `end_at`，也支持嵌套 `time_range`。
- `token_used` 是当前实现中的返回字段名，不是 `total_tokens`。
- 该请求走严格字段校验；旧的 `memory_type` / `with_overview` / `with_links` / `include_links` / `detail` 不再属于 V2 contract。

### 3. batch remember

`POST /v2/memory/batch-remember`

请求体：

```json
{
  "memories": [
    {
      "content": "Rust systems handbook",
      "session_id": "sess-batch",
      "tags": ["rust", "systems"]
    },
    {
      "content": "Python data handbook",
      "session_id": "sess-batch",
      "tags": ["python", "data"]
    }
  ]
}
```

返回：

```json
{
  "memories": [
    {
      "memory_id": "...",
      "abstract": "Rust systems handbook",
      "has_overview": false,
      "has_detail": false
    }
  ]
}
```

说明：

- 单次 batch 最多 100 条。
- 每条 memory 仍沿用单条 `remember` 的字段与语义。
- 当前实现会复用 batch embedding（若可用）并以单个 V2 事务提交整批写入。

### 4. expand

`POST /v2/memory/expand`

请求体：

```json
{
  "memory_id": "...",
  "level": "links"
}
```

`level` 当前支持：

- `overview`
- `detail`
- `links`

返回会始终带 `memory_id`、`level` 与 `abstract`；其中：

- `level=overview` 时可返回 `overview`
- `level=detail` 时可返回 `overview` 与 `detail`
- `level=links` 时可返回 `links`
- 该请求走严格字段校验；旧的 `level=full` 不再属于 V2 contract。

### 5. forget

`POST /v2/memory/forget`

```json
{
  "memory_id": "...",
  "reason": "cleanup"
}
```

返回：

```json
{
  "memory_id": "...",
  "forgotten": true
}
```

说明：

- 当前实现支持单条 forget 与 batch forget，但仍不支持 `dry_run`、按置信度过滤删除等设计稿能力。
- forget 后该 memory 会从 active head 视图中移除。

### 6. batch forget

`POST /v2/memory/batch-forget`

```json
{
  "memory_ids": ["...", "..."],
  "reason": "cleanup"
}
```

返回：

```json
{
  "memories": [
    {
      "memory_id": "...",
      "forgotten": true
    }
  ]
}
```

说明：

- 单次 batch 最多 100 条。
- 相同 `memory_id` 会按请求顺序去重后执行，避免在同一请求里重复 forget 自己。

### 7. focus

`POST /v2/memory/focus`

```json
{
  "type": "memory_id",
  "value": "...",
  "boost": 5.0,
  "ttl_secs": 300
}
```

返回：

```json
{
  "focus_id": "...",
  "type": "memory_id",
  "value": "...",
  "boost": 5.0,
  "active_until": "2026-03-21T12:00:00+00:00"
}
```

说明：

- 当前 focus 合同是通用的 `type + value` 形式。
- `type` 只接受 `topic` / `tag` / `memory_id` / `session`
- `value` 不能为空
- `ttl_secs` 使用秒，不是时长字符串。
- active focus 会影响 recall 与 related 的排序。
- 该请求走严格字段校验；旧的 `focus_on` / `ttl` 不再属于 V2 contract。

### 8. update

`PATCH /v2/memory/update`

```json
{
  "memory_id": "...",
  "content": "Updated content",
  "importance": 0.8,
  "trust_tier": "T2",
  "tags_add": ["infra"],
  "tags_remove": ["legacy"],
  "reason": "refresh"
}
```

返回：

```json
{
  "memory_id": "...",
  "abstract": "Updated content",
  "updated_at": "2026-03-21T12:00:00+00:00",
  "has_overview": false,
  "has_detail": false
}
```

说明：

- 内容更新会发布新的 content version / index doc，并重新排队 `derive_views` 与 `extract_links`。
- 只改标签时会重排 `extract_links`，不会生成新的 content version。

## 浏览与观察接口

### list

`GET /v2/memory/list?limit=50&cursor=...&type=semantic&session_id=sess-v2`

返回轻量列表项，并通过 `next_cursor` 做分页。`cursor` 是 API 内部编码值，客户端只需透传。

### profile

`GET /v2/memory/profile?limit=50&cursor=...&session_id=sess-v2`

返回当前 V2 数据面里 `type=profile` 且仍然活跃的 memories。每个 item 会携带：

- `content`
- `abstract`
- `session_id`
- `created_at` / `updated_at`
- `trust_tier`
- `confidence` / `importance`
- `has_overview` / `has_detail`

说明：

- 该接口只读取 V2 per-user table family，不会穿透到 V1 `mem_memories`。
- `session_id` 可选，用于只看某个 session 下的 profile memories。
- `cursor` 与 `list` 共用相同分页语义，按 `created_at DESC, memory_id DESC` 向后翻页。

### reflect

`POST /v2/memory/reflect`

请求体：

```json
{
  "mode": "auto",
  "limit": 10,
  "session_id": "optional-session-id",
  "min_cluster_size": 2,
  "min_link_strength": 0.35
}
```

返回：

```json
{
  "mode": "auto",
  "synthesized": false,
  "scenes_created": 0,
  "candidates": [
    {
      "signal": "cross_session_linked_cluster",
      "importance": 0.71,
      "memory_count": 3,
      "session_count": 2,
      "link_count": 2,
      "memories": [
        {
          "id": "mem_123",
          "abstract": "Bridge operations memory joining alpha and beta",
          "type": "semantic",
          "session_id": "sess-reflect-b",
          "importance": 0.8
        }
      ]
    }
  ]
}
```

说明：

- `auto` 与 `candidates` 当前都会返回候选 clusters，不会自动写回 synthesized memories。
- `internal` 会基于当前候选 cluster 在 V2 per-user tables 中写入 synthesized semantic memory，并返回 `scenes_created`。
- `internal` 写回使用 `source.kind=reflect_v2` 标记 synthesized memory，并为源 memories 写入 `reflection` direct links；这些 synthesized memories 不会再次参与后续 reflect 候选聚类。
- 候选优先从 V2 active direct links 聚类生成；如果没有满足阈值的 linked cluster，则回退到同一 session 内的分组。
- 该接口只读取 V2 per-user table family，不会读取 V1 graph tables 或 `/v1/reflect` 结果。

### entities extract

`POST /v2/memory/entities/extract`

请求体：

```json
{
  "limit": 50,
  "memory_id": "optional-memory-id"
}
```

返回：

```json
{
  "processed_memories": 3,
  "entities_found": 8,
  "links_written": 8
}
```

说明：

- 初版使用本地 extractor 做同步抽取，不依赖 V1 graph tables。
- 默认按当前用户 active V2 memories 扫描；传 `memory_id` 时只刷新该 memory 的当前 content version。
- 重跑会覆盖同一 memory 的旧实体关联，并只保留当前 content version 对应的 active browse 结果。

### entities

`GET /v2/memory/entities?limit=50&cursor=...&query=rust&entity_type=tech&memory_id=...`

返回当前 V2 数据面里的实体聚合列表。每个 item 包含：

- `id`
- `name`
- `display_name`
- `type`
- `memory_count`
- `created_at`
- `updated_at`

说明：

- 只统计仍指向 active V2 heads 且 content version 仍是 current 的实体关联。
- `query` 会匹配 `name` 和 `display_name`。
- `memory_id` 可选，用于只浏览单条 V2 memory 当前版本上的实体。

### tags

`GET /v2/memory/tags?limit=20&query=rust`

返回当前用户 active V2 memories 的标签聚合计数。

### stats

`GET /v2/memory/stats`

返回：

```json
{
  "total_memories": 12,
  "active_memories": 10,
  "forgotten_memories": 2,
  "distinct_sessions": 4,
  "has_overview_count": 9,
  "has_detail_count": 8,
  "active_direct_links": 14,
  "active_focus_count": 1,
  "tags": {
    "unique_count": 16,
    "assignment_count": 23
  },
  "jobs": {
    "total_count": 24,
    "pending_count": 0,
    "in_progress_count": 1,
    "done_count": 22,
    "failed_count": 1
  },
  "feedback": {
    "total": 5,
    "useful": 3,
    "irrelevant": 1,
    "outdated": 0,
    "wrong": 1
  },
  "by_type": [
    {
      "type": "semantic",
      "total_count": 8,
      "active_count": 7,
      "forgotten_count": 1
    }
  ]
}
```

说明：

- 这是只读汇总接口，不引入新的 V2 写路径。
- 统计严格来自当前用户自己的 V2 table family。
- `has_overview_count` / `has_detail_count` 只统计 active memories。
- `active_direct_links` 只统计 source 与 target 都仍 active 的 direct links。
- `tags` 只统计 active memories 上的标签聚合。
- `jobs` 会把底层 `leased` 归一化为 `in_progress`。
- `feedback` 是当前用户 V2 feedback 明细的总量汇总。

### history

`GET /v2/memory/:id/history?limit=20`

返回：

```json
{
  "memory_id": "...",
  "items": [
    {
      "event_id": "...",
      "event_type": "updated",
      "actor": "user_123",
      "processing_state": "committed",
      "payload": {
        "reason": "clarified",
        "content_updated": true
      },
      "created_at": "2026-03-21T13:00:00+00:00"
    }
  ]
}
```

说明：

- 这是 **V2 event history**，不是 V1 的 superseded-by 版本链。
- 当前只读取该 memory 在 V2 `events` 表中的 memory aggregate 事件。
- 当前可见的 memory 事件主要是 `remembered`、`updated`、`forgotten`。
- 返回按 `created_at DESC, event_id DESC` 排序。
- `payload` 基本反映写入时记录的 V2 事件 payload；其中 remembered event 当前会把 memory kind 规范化为 `type`，不再对外暴露 `memory_type`。

### jobs

`GET /v2/memory/jobs?memory_id=...&limit=20`

返回：

- `derivation_state`
- `has_overview`
- `has_detail`
- `link_count`
- `pending_count` / `in_progress_count` / `done_count` / `failed_count`
- 按 `job_type` 聚合的阶段摘要
- 明细 jobs 列表

当前 job 类型主要有：

- `derive_views`
- `extract_links`

## Links 与 Related

### links

`GET /v2/memory/links?memory_id=...&direction=both&limit=20&link_type=related&min_strength=0.2`

说明：

- `direction` 支持 `outbound` / `inbound` / `both`
- 返回 direct-graph `summary`
- 每条 link 会返回 provenance，包括：
  - `evidence_types`
  - `primary_evidence_type`
  - `primary_evidence_strength`
  - `refined`
  - `evidence` 明细
  - 可选 `extraction_trace`

### related

`GET /v2/memory/related?memory_id=...&limit=20&min_strength=0.2&max_hops=2`

说明：

- 默认从 direct links 聚合 related memories。
- `max_hops` 允许多跳遍历。
- 每个 related item 会返回：
  - `hop_distance`
  - `strength`
  - `via_memory_ids`
  - `directions`
  - `link_types`
  - `lineage`
  - `supporting_path_count`
  - `supporting_paths_truncated`
  - `supporting_paths`
  - `feedback_impact`
  - `ranking`（同 hop 内排序分解）
- 顶层 `summary` 会返回：
  - `discovered_count`
  - `returned_count`
  - `truncated`
  - `by_hop`
  - `link_types`

排序上，当前实现会综合：

- hop distance
- active focus
- session affinity
- access heat
- feedback impact

`ranking` 当前会把同 hop 内排序拆成结构化因子，包括：

- `same_hop_score`
- `base_strength`
- `session_affinity_applied`
- `session_affinity_multiplier`
- `access_count`
- `access_multiplier`
- `feedback_multiplier`
- `content_multiplier`
- `focus_boost`
- `focus_matches`

## Feedback

### 单条记忆反馈摘要

`GET /v2/memory/:id/feedback`

返回当前记忆的 feedback 计数与 `last_feedback_at`。

### 记录反馈

`POST /v2/memory/:id/feedback`

```json
{
  "signal": "useful",
  "context": "helped with infra recall"
}
```

### 单条记忆反馈历史

`GET /v2/memory/:id/feedback/history?limit=20`

### 全局 feedback feed

`GET /v2/feedback/history?limit=50&memory_id=...&signal=useful`

### 全局聚合统计

- `GET /v2/feedback/stats`
- `GET /v2/feedback/by-tier`

说明：

- V2 feedback 写入独立的 V2 feedback / stats 数据面。
- feedback 会影响 recall 与 related 的排序，而不只是作为审计记录存在。

## V1 / V2 共存边界

当前服务进程同时暴露：

- V1：`/v1/*`
- V2：`/v2/*`

但两者是**并行系统**，不是同一写路径的两个外壳：

- V2 不复用 V1 memory tables 作为 source of truth。
- V2 不自动把 V1 数据迁入 V2。
- 客户端需要显式选择调用 `/v2/*` 才会进入 V2 数据面。
- 迁移应按客户端或场景逐步切流，而不是依赖服务端隐式兼容映射。

## 文档策略

如果未来实现了新的规划项，应先在本文补充状态说明与请求/返回合同；不要提前把设计稿写成现网事实。
