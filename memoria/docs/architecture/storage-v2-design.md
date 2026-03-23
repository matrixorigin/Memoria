# Memoria Memory V2 存储设计（当前实现）

> 本文描述的是当前已经落地的 Memory V2 存储架构，而不是早期概念草图。重点是独立数据面、每用户 table family、异步富化与检索/关联投影。

## 设计目标

Memory V2 的核心目标不是“给 V1 换个路由前缀”，而是建立一套独立的事件、头部投影、索引、关联、focus、jobs 与 feedback 数据面，以支撑 agent 风格的 remember / recall / expand / related 工作流。

## 与 V1 的边界

V2 与 V1 在同一服务内共存，但边界明确：

- **共享**：认证、SQL pool、embedding/LLM、worker 基础设施、通用 observability
- **不共享**：V1 routes、V1 write path、V1 memory tables、V1 scoring pipeline、V1 response shapes
- **迁移方式**：调用方显式进入 `/v2/*`，不依赖隐式数据迁移或兼容重写
- **回归保障**：同一用户下会同时覆盖 `/v1/*` 与 `/v2/*` 的共存用例，确保 direct lookup、list、retrieve/recall 以及 V2 job processing 不发生跨版本可见性或写入泄漏

## 每用户 table family

V2 不是一组全局表加 `user_id` 过滤；当前实现为**每个用户创建独立 table family**。

### registry

全局 registry 表：

- `mem_v2_user_tables`

作用：

- 记录用户到 table suffix 的映射
- 记录该用户的 events / heads / content_versions / index_docs / links / entities / memory_entities / focus / jobs / tags / stats / feedback 表名

### table family 组成

对任意用户，系统会生成哈希后缀，并创建：

- `mem_v2_evt_<suffix>`
- `mem_v2_head_<suffix>`
- `mem_v2_cver_<suffix>`
- `mem_v2_idx_<suffix>`
- `mem_v2_link_<suffix>`
- `mem_v2_entity_<suffix>`
- `mem_v2_ment_<suffix>`
- `mem_v2_focus_<suffix>`
- `mem_v2_job_<suffix>`
- `mem_v2_tag_<suffix>`
- `mem_v2_stat_<suffix>`
- `mem_v2_feedback_<suffix>`

## 表职责

### 1. events

职责：append-only 事件日志。

典型事件：

- `remembered`
- `updated`
- `forgotten`
- `focus_set`

特点：

- 以 `aggregate_id + created_at` 建索引
- 记录 `actor`、`payload_json`、`processing_state`
- 为后续审计、回放和派生视图更新保留事实来源
- `GET /v2/memory/:id/history` 当前直接读取 memory aggregate 事件流

### 2. heads

职责：active memory head projection。

关键字段：

- `memory_id`
- `memory_type`
- `session_id`
- `trust_tier`
- `confidence`
- `importance`
- `source_*`
- `current_content_version_id`
- `current_index_doc_id`
- `latest_event_id`
- `forgotten_at`

注意：

- 当前活跃过滤以 `forgotten_at IS NULL` 为准。
- `is_active` 仍存在于表中，但当前 V2 活跃头查询不应把它当成唯一真值。

### 3. content_versions

职责：保存每次内容版本的原文与派生文本。

关键字段：

- `source_text`
- `abstract_text`
- `overview_text`
- `detail_text`
- `has_overview`
- `has_detail`
- token estimate 字段
- `derivation_state`

行为：

- remember 会先写入 `abstract_text`
- `overview_text` / `detail_text` 初始为空，随后由异步 `derive_views` job 富化
- update 改内容时会创建新的 content version，而不是原地覆盖旧版本

### 4. index_docs

职责：供 recall 使用的索引发布面。

关键字段：

- `recall_text`
- `embedding`
- `memory_type`
- `session_id`
- `confidence`
- `published_at`

特点：

- 既支持向量召回，也支持全文检索
- 与 content version 分离，便于 append-mostly 发布与替换当前 head 指针

### 5. links

职责：保存 direct links 投影。

关键字段：

- `memory_id`
- `target_memory_id`
- `link_type`
- `strength`

补充说明：

- links 接口与 related 接口读取的是这个 direct graph 投影
- 返回给 API 的 provenance / extraction_trace 是在读取阶段结合其他投影补齐的，不是 links 表单独就能表达全部信息
- 当前 `POST /v2/memory/reflect` 也会优先基于 active links 生成候选 clusters；当没有满足阈值的 link cluster 时，再退回同 session 分组

### 6. entities / memory_entities

职责：保存 V2 自己的实体投影与 memory-to-entity 关联。

当前实现特点：

- `entities` 保存规范化后的 `name`、展示名与 `entity_type`
- `memory_entities` 记录 `memory_id + content_version_id + entity_id` 关联
- browse 查询会要求关联的 `content_version_id` 仍等于 head 的 `current_content_version_id`
- 因此内容更新后，即使还未重新抽取，也不会把旧版本实体继续暴露为 active V2 browse 结果
- 这一层不复用 V1 `mem_entities` 或 `mem_memory_entity_links`

### 7. focus

职责：active focus projection。

关键字段：

- `focus_type`
- `focus_value`
- `boost`
- `state`
- `expires_at`

特点：

- 当前以统一的 `type + value` 形式表达 focus
- 通过唯一键 `(focus_type, focus_value)` 去重 active projection
- 允许事件日志里保留重复 focus 行为，但运行时投影保持确定性

### 8. jobs

职责：承载异步富化任务。

当前主要 job 类型：

- `derive_views`
- `extract_links`

关键字段：

- `job_type`
- `aggregate_id`
- `payload_json`
- `dedupe_key`
- `status`
- `available_at`
- `leased_until`
- `attempts`
- `last_error`

状态语义：

- 存储层内部使用 `pending` / `leased` / `done` / `failed`
- API 观察面把 `leased` 归一化为 `in_progress`

worker 行为：

- `memoria-service` 在启动时会创建 V2 job worker
- worker 周期性 claim pending/expired leased jobs
- 成功后写回 derived views / links / derivation state
- 失败时增加 attempts，并在超过上限后进入 `failed`

### 9. tags

职责：memory 到标准化 tag 的投影。

特点：

- 写入前会 lower-case、trim、去重
- recall 现在也可按标准化 tags projection 过滤结果，支持 `any` / `all` 两种匹配模式

### 10. stats

职责：轻量运行时统计投影。

关键字段：

- `access_count`
- `last_accessed_at`
- `feedback_useful`
- `feedback_irrelevant`
- `feedback_outdated`
- `feedback_wrong`
- `last_feedback_at`

用途：

- recall / related 排序时使用访问热度与 feedback 影响
- API feedback summary / stats 会读取这些聚合值
- `GET /v2/memory/stats` 会把它与 heads/content/links/focus/jobs/tags/feedback 一起聚合成只读总览

### 11. feedback

职责：显式 feedback 事件明细。

关键字段：

- `feedback_id`
- `memory_id`
- `signal`
- `context`
- `created_at`

特点：

- feedback 表保存明细历史
- stats 表保存聚合计数
- recall / related 同时消费聚合后的 feedback impact

## 写入流程

### remember

一次 remember 会：

1. 确保用户 table family 已存在
2. 写入 `events`
3. 写入新的 `content_version`
4. 写入新的 `index_doc`
5. 写入/更新 `heads`
6. 写入标准化 tags
7. 初始化 `stats`
8. 入队 `derive_views` 与 `extract_links`

结果是：

- 写后立即可 recall 到 `abstract`
- `overview` / `detail` / links 由异步 jobs 补齐

### update

- 改内容：创建新的 content version 和 index doc，并重排两类 jobs
- 只改标签：不创建新的 content version，但会重排 `extract_links`
- 只改 metadata：更新 head 投影与事件日志

### forget

- 写事件日志
- 更新 head 的 `forgotten_at`
- 后续 list / recall / expand / related 都只看 active head

### focus

- 写 focus 事件与 active focus projection
- recall / related 排序时消费 active focus

### feedback

- 写 feedback 明细
- 更新 stats 聚合
- 影响 recall / related 的排序倍率

## 读取模型

### recall

recall 会综合：

- 索引文本 / 向量召回
- `memory_type` / `session_id` / `scope` 过滤
- active focus boost
- session affinity
- access heat
- feedback impact
- token budget (`max_tokens`)

### expand

expand 通过 current head 指针读取当前 content version：

- `overview`
- `detail`
- `links`

### links

links 返回 direct graph 与 observability：

- direct summary（出/入/总数）
- 每条 link 的 provenance
- 可选 `extraction_trace`

### related

related 以 direct links 为基础，支持多跳遍历，并返回：

- `lineage`
- `supporting_paths`
- `supporting_path_count`
- `supporting_paths_truncated`
- `ranking`（同 hop 排序分解）
- 多跳 summary（按 hop / link_type 聚合）

## 排序与 explainability

当前 related 排序主要偏向：

1. 更近 hop
2. active focus
3. source-session affinity
4. access heat
5. feedback impact

当前 links / related 已经暴露的重要 explainability 面包括：

- provenance 的 evidence types / primary evidence / refined
- structured evidence details（例如 tag overlap、vector distance）
- extraction trace
- related supporting path ranking 与 selection reason
- related ranking breakdown（base strength、session affinity、access、feedback、content、focus）
- summary buckets（hop 与 link type）

## 为什么这套设计是“独立 V2”

因为 V2 的事实日志、当前头部、索引发布、links、focus、jobs、tags、stats、feedback 全都在独立数据面内完成闭环；它并不是把 V1 数据读法换成一层新 JSON 皮肤。
