# Memoria V1 → V2 迁移指南（当前实现）

> 本文描述的是当前代码已经支持的迁移路径。V2 不是对 V1 的透明别名层，而是一套独立的数据面与 HTTP 合同。

## 先说结论

- **路由分离**：V1 在 `/v1/*`，V2 在 `/v2/*`
- **数据分离**：V2 使用独立 tables / projections / jobs，不复用 V1 memory tables 作为 source of truth
- **MCP 分离**：V1 MCP tools 继续使用 `memory_*`，V2 MCP tools 使用显式的 `memory_v2_*`
- **能力不对等**：V2 已覆盖 remember/batch-remember/recall/expand/forget/batch-forget/focus/update/tags/stats/history/jobs/links/related/feedback，但并未实现设计稿中的全部能力
- **迁移方式**：客户端按调用面逐步切换到 `/v2/*`，而不是期待服务端把 V1 请求自动映射到 V2

## 路由映射

| V1 | 当前推荐的 V2 | 备注 |
|---|---|---|
| `POST /v1/memories` | `POST /v2/memory/remember` | 基本写入能力对应 |
| `POST /v1/memories/batch` | `POST /v2/memory/batch-remember` | 当前 V2 已支持批量写入，单次上限 100 |
| `POST /v1/memories/retrieve` | `POST /v2/memory/recall` | V2 recall 有 token 预算与 focus 排序 |
| `POST /v1/memories/search` | `POST /v2/memory/recall` | V2 不再拆 retrieve/search 两个入口 |
| `GET /v1/memories` | `GET /v2/memory/list` | V2 list 是独立轻量列表接口 |
| `POST /v1/reflect` | `POST /v2/memory/reflect` | V2 支持 candidates 与 `internal` synthesized write-back，两者都只作用于 V2 数据面 |
| `GET /v1/profiles/:target_user_id` | `GET /v2/memory/profile` | V2 profile 只读取 V2 `profile` memories，不复用 V1 profile 读面 |
| `POST /v1/extract-entities` | `POST /v2/memory/entities/extract` | V2 初版只刷新 V2 自己的实体投影，不触碰 V1 graph/entity 表 |
| `GET /v1/entities` | `GET /v2/memory/entities` | V2 entities 只浏览 V2 active/current content version 的实体关联 |
| `GET /v1/memories/:id/history` | `GET /v2/memory/:id/history` | V2 返回 event history，不是 V1 的版本链 |
| `PUT /v1/memories/:id/correct` / `POST /v1/memories/correct` | `PATCH /v2/memory/update` | V2 update 支持内容、标签、重要性、trust tier |
| `DELETE /v1/memories/:id` / purge 类能力 | `POST /v2/memory/forget` | 单条遗忘 |
| 无直接等价 | `POST /v2/memory/batch-forget` | 当前 V2 已支持按 memory_id 列表批量遗忘 |
| 无等价 | `POST /v2/memory/focus` | V2 新增能力 |
| 无等价 | `GET /v2/memory/stats` | V2 新增只读统计总览 |
| 无等价 | `GET /v2/memory/links` | V2 新增能力 |
| 无等价 | `GET /v2/memory/related` | V2 新增能力 |
| 无等价 | `GET/POST /v2/memory/:id/feedback` | V2 feedback 独立闭环 |

如果你走的是 MCP 而不是直接 HTTP，对应迁移方式是：

- `memory_store` → `memory_v2_remember`
- `memory_retrieve` / `memory_search` → `memory_v2_recall`
- `memory_list` → `memory_v2_list`
- `memory_profile` → `memory_v2_profile`
- `memory_correct` → `memory_v2_update`
- `memory_purge` / delete-like flows → `memory_v2_forget`
- `memory_reflect` → `memory_v2_reflect`

V2 MCP 也是显式迁移，不存在把旧 `memory_*` tool 自动解释成 V2 请求的兼容层。

## 字段映射与差异

### remember

V1 常见写法：

```json
{
  "content": "用户偏好使用 Rust",
  "memory_type": "profile",
  "session_id": "sess_123"
}
```

V2 当前写法：

```json
{
  "content": "用户偏好使用 Rust",
  "type": "profile",
  "session_id": "sess_123",
  "importance": 0.8,
  "trust_tier": "T2",
  "tags": ["rust", "preference"]
}
```

注意：

- 当前实现使用 `type`，不是 `memory_type`
- V2 remember 走严格字段校验；旧的 `memory_type` 不再属于 V2 contract
- `links_to` 尚未实现为 remember 的入参
- V2 返回 `has_overview` / `has_detail`，表示异步富化是否已完成

### batch remember

当前 V2 还提供：

```json
{
  "memories": [
    {
      "content": "用户偏好使用 Rust",
      "type": "profile",
      "session_id": "sess_123"
    },
    {
      "content": "用户维护基础设施 runbook",
      "type": "procedural"
    }
  ]
}
```

注意：

- 单次 batch 上限 100 条
- 每条 item 仍沿用单条 remember 的字段合同
- 当前实现会尽量复用 batch embedding，并以单个 V2 事务提交

### recall

V2 当前合同：

```json
{
  "query": "Rust 异步编程",
  "top_k": 10,
  "max_tokens": 500,
  "scope": "all",
  "type": "semantic",
  "expand_links": true,
  "view": "full"
}
```

差异：

- V2 recall 当前通过 `view=compact|overview|full` 选择响应形态，而不是 `detail`
- V2 当前支持 `start_at` / `end_at`，也支持嵌套 `time_range`
- V2 返回字段名是 `token_used`，不是 `total_tokens`
- `scope=session` 时必须显式带上 `session_id`
- `expand_links` 默认开启；显式传 `false` 时可得到纯 direct recall 语义
- 默认 `view=compact` 返回 compact items：`id` / `text` / `type` / `score` / `related`
- `view=full` 返回 verbose explainability items，并带 run-level `summary`

当前仍支持：

- `tags` 与 `tag_filter_mode=any|all`

当前不再属于 V2 contract：

- `memory_type`
- `with_overview`
- `with_links`
- `include_links`
- `detail`

### history

当前 V2 提供：

```http
GET /v2/memory/:id/history?limit=20
```

差异：

- V1 history 更接近版本链浏览。
- V2 history 当前直接暴露 V2 `events` 表上的 memory aggregate 事件流。
- 当前常见事件类型是 `remembered`、`updated`、`forgotten`。
- remembered event payload 当前会把 memory kind 暴露为 `type`，不再对外暴露 `memory_type`。

### update

V2 用一个统一入口表达修正与补充：

```json
{
  "memory_id": "...",
  "content": "updated content",
  "importance": 0.9,
  "trust_tier": "T2",
  "tags_add": ["infra"],
  "tags_remove": ["legacy"],
  "reason": "refresh"
}
```

差异：

- 当前 V2 没有版本号返回字段
- 当前 V2 会在内容变更后触发新的异步富化，而不是同步产出完整 overview/detail/links

### forget

当前 V2 仅支持：

```json
{
  "memory_id": "...",
  "reason": "cleanup"
}
```

设计稿里的这些能力当前都**未实现**：

- `dry_run`
- `confidence_below`
- 按主题/时间范围 forget

### batch forget

当前 V2 已支持：

```json
{
  "memory_ids": ["...", "..."],
  "reason": "cleanup"
}
```

注意：

- 单次 batch 上限 100 条
- 相同 `memory_id` 会在同一请求里去重，避免重复 forget 自冲突

### focus

当前 V2 focus 合同是：

```json
{
  "type": "memory_id",
  "value": "...",
  "boost": 2.0,
  "ttl_secs": 3600
}
```

差异：

- 当前没有 `action=set|clear`
- 当前没有 `topic` / `entity` / `memory_ids` 等专用字段族
- 统一通过 `type + value` 表达焦点对象
- `type` 只接受 `topic` / `tag` / `memory_id` / `session`
- `ttl_secs` 是唯一 TTL 字段

当前不再属于 V2 contract：

- `focus_on`
- `ttl`

## 哪些能力先不要迁移

以下能力在设计稿里出现过，但当前 V2 HTTP API 尚未提供：

- `remember.links_to`
- `forget.dry_run`
- 按主题/时间范围 forget

如果你的客户端当前依赖这些能力，应继续保留在 V1 或等待 V2 后续迭代，不要假设 `/v2/*` 已有对等实现。

## 推荐迁移顺序

1. **先迁 remember / recall**
   - 这是最核心、最稳定的 V2 面。
   - 可以最早获得 token 预算、focus、feedback 排序和 links/related 能力。

2. **再迁 expand / links / related**
   - 适合需要 agent-style 上下文压缩与按需展开的客户端。

3. **再迁 reflect / entities / profile 等 browse 面**
   - 适合需要结构化用户偏好或概念浏览的客户端。
   - 当前 reflect 支持 candidates 与 `internal` synthesized write-back；entities 是 V2 自己的投影；两者都不会读取 V1 结果作为 source of truth。

4. **最后迁 update / feedback / jobs 观察面**
   - 当你需要更完整的闭环与可观测性时再引入。

## 共存策略

当前推荐的服务端/客户端姿势：

- 服务端同时保留 `/v1/*` 与 `/v2/*`
- 新客户端或新场景优先接入 `/v2/*`
- 老客户端继续留在 `/v1/*`，直到调用面完成改造
- 不要通过“隐式兼容重写”把 V1 请求偷偷导向 V2，这会模糊两套系统的边界

## 验证建议

迁移到 V2 后，至少验证以下行为：

- remember 后，初始 `has_overview=false` / `has_detail=false` 是否符合预期
- jobs 视图中 `derive_views` / `extract_links` 是否完成
- recall 的 `token_used`、`has_more`、`summary` 是否符合客户端预算策略
- 默认 compact recall 与 `view=full` explainability 两种消费路径是否都符合预期
- related 与 `view=full` recall links 的 provenance、summary 字段是否被正确消费
- feedback 是否确实影响 recall / related 排序
