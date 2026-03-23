# Memoria Memory V2 实现映射

> 本文不是概念伪代码，而是当前代码结构的快速导航，帮助继续推进独立的 Memory V2。

## 代码入口

### API 层

- Router 注册：`memoria/crates/memoria-api/src/lib.rs`
- 请求/响应模型：`memoria/crates/memoria-api/src/v2/models.rs`
- V2 handlers：`memoria/crates/memoria-api/src/v2/routes.rs`

### 存储层

- V2 store：`memoria/crates/memoria-storage/src/v2/store.rs`
- 对外导出：`memoria/crates/memoria-storage/src/lib.rs`

### 服务层

- V2 job worker：`memoria/crates/memoria-service/src/v2/worker.rs`
- 服务装配：`memoria/crates/memoria-service/src/service.rs`

### MCP 层

- V1 tools：`memoria/crates/memoria-mcp/src/tools.rs`
- V2 tools：`memoria/crates/memoria-mcp/src/v2/tools.rs`
- 统一 dispatch：`memoria/crates/memoria-mcp/src/server.rs`

### 测试

- 存储验证：`memoria/crates/memoria-storage/tests/v2_store.rs`
- API 端到端：`memoria/crates/memoria-api/tests/v2_api_e2e.rs`

## 当前实现切面

### 写路径

- `remember`：立即写 abstract + index doc + head，并排队异步富化（`derive_views` / `extract_links` / `extract_entities`）
- `update`：支持内容、重要性、trust tier、标签增删
- `forget`：从 active head 视角移除记忆
- `focus`：维护 active focus 投影
- `feedback`：维护明细历史与聚合 stats
- `entities extract`：支持按需刷新当前 V2 memories 的实体投影，但默认 remember / content update 也会自动排队刷新

### 读路径

- `recall`：支持 token budget、session scope、同 session 亲和加权、entity 候选召回、默认 one-hop link expansion、type-aware temporal decay；默认返回 compact memory items，需要时再显式切到 overview / verbose explainability，并始终暴露 run-level `summary`
- `expand`：支持 `overview` / `detail` / `links`
- `reflect`：`auto` / `candidates` 返回基于 V2 links/session grouping 的候选 clusters，`internal` 会写回 V2 synthesized memory
- `profile`：读取 V2 `type=profile` 的当前内容与元数据
- `entities`：读取 V2 当前内容版本上抽取出的实体聚合
- `list` / `tags`：轻量浏览与聚合
- `stats`：基于当前 V2 projections 的只读总览聚合
- `history`：基于 V2 events 的单条 memory 事件流读取
- `links` / `related`：支持 provenance、multi-hop summary、supporting paths、related ranking breakdown
- `jobs`：暴露富化异步状态

## 当前实现约束

- V2 是独立系统，不复用 V1 memory tables 作为 source of truth
- 每个用户拥有独立 table family，而不是共享全局 V2 表
- V2 的 recall / entities / jobs 演进应优先在 V2 内部闭环完成；如果能只改 V2，就不要把能力耦合回 V1
- 跨版本写操作必须显式失败：V1 路由打到 V2 memory_id、或 V2 路由打到 V1 memory_id，当前都应返回 `404/NotFound`，而不是静默成功
- admin `DELETE /admin/users/:user_id` 需要同时停用该用户的 V1 memories 和已存在的 V2 head rows；V2 table family 保留，但 active heads 必须变为 forgotten/inactive
- admin `stats` / `users` / `users/:id/stats` 需要把 active V2 heads 计入统计，不能只看 V1 `mem_memories`
- `/metrics` 需要把 active V2 heads 计入 memory/user totals；混合用户在总 `memoria_users_total` 里只能计一次，同时应暴露按 `version=v1|v2` 拆分的观测指标
- admin `POST /admin/users/:user_id/reset-access-counts` 需要同时清零 V1 stats 和该用户 V2 stats table 的 `access_count`
- 当前 admin `POST /admin/governance/:user_id/trigger` 仍是 **V1 scope**；返回体应显式标明 `scope: "v1"`，并且不能写入 V2 entities / heads / jobs surfaces
- recall 返回字段名当前是 `token_used`
- recall 在 `scope=all` 且显式传入 `session_id` 时，会对同 session memories 做轻量排序加权，而不是只把 `session_id` 用于 session scope 过滤
- recall 当前会把 query text 的 regex NER 结果接入候选召回；即使 fulltext/vector signal 较弱，只要当前内容版本已有实体投影，V2 recall 仍可直接召回相关 memory
- recall 当前默认会对 top direct candidates 做 one-hop link expansion，把强 direct hit 的 linked memories 以衰减 bonus 并入候选池；如果调用方需要纯 direct recall，可显式传 `expand_links: false`
- recall 请求当前使用严格 V2 contract：过滤字段是 `type`，响应形态字段是 `view`
- recall 当前默认返回 compact item：`id`、`text`、`type`、`score`、`related`；其中 `text` 默认取 abstract
- recall 可通过 `view=overview` 请求 compact-overview 响应，此时 `text` 优先取 overview
- recall 可通过 `view=full` 请求 verbose explainability 响应；verbose item 会暴露 `abstract`、`overview`、`has_related`、`retrieval_path`、ranking breakdown、`links` 等 explainability 字段
- list / links / related / reflect candidate memories 等 V2 item response 当前也统一输出 `type`，不再输出 `memory_type`
- list 当前也使用 `type` 作为 canonical query filter；旧的 `memory_type` query 键不再属于 V2 contract
- history 当前会把 remembered event payload 里的 memory kind 暴露为 `type`，不再对外泄露 `memory_type`
- recall 当前还会按 `memory_type` 和 age 做 temporal decay：`working`/`episodic` 衰减更快，`semantic`/`procedural`/`profile` 衰减更慢；对应 explainability 会暴露 `age_hours`、`temporal_half_life_hours` 与 `temporal_multiplier`
- recall 当前还暴露 run-level `summary`，包含 discovered/returned/truncated，以及按 `retrieval_path` 聚合的 bucket 计数，便于区分候选池与最终返回结果
- verbose recall item 当前额外暴露 `has_related`，语义等同于 `link_count > 0`
- verbose recall item 当前还暴露 `retrieval_path`，用于区分 `direct` / `expanded_only` / `direct_and_expanded`
- verbose recall item 当前还暴露 ranking breakdown，可解释 direct score 组成（vector / keyword / confidence / importance / entity）、link bonus、link expansion sources、temporal decay，以及 session/access/feedback/focus multiplier
- focus 返回字段名当前是 `active_until`
- remember / recall 请求当前都使用 `type` 作为 memory type 字段；旧的 `memory_type` 请求键不再属于 V2 contract
- focus 请求当前使用 `type`、`value`、`boost`、`ttl_secs`；`type=session` 表示 session focus
- expand 请求当前只接受 `level=overview|detail|links`
- recall / remember / focus / expand 请求当前对未知字段走严格校验，不再为 V2 保留兼容别名

## 继续开发时优先参考

1. 先看 `v2/store.rs` 是否已有投影或辅助结构，避免重复建模
2. 再看 `v2/models.rs` 与 `v2/routes.rs`，保证 API 形状与存储能力一致
3. 最后补 `v2_store.rs` / `v2_api_e2e.rs` / `v2_runtime_isolation.rs` 的针对性验证
4. 任何新增写路径都要补一条 cross-version rejection 用例，至少覆盖 “目标版本不存在/不属于当前版本” 的显式错误

## 最小验证集

优先使用仓库根目录的分层聚合命令：

```bash
make test-v2
make test-v2-isolation
make test-v2-all
```

- `make test-v2`：纯 V2 面验证，不夹带 V1/V2 共存断言
- `make test-v2-isolation`：只测 V1/V2 共存、跨版本拒绝、admin/metrics/runtime 隔离
- `make test-v2-all`：顺序执行上面两组

如果需要在 `memoria/` 目录下逐条执行：

- **纯 V2 面**：

```bash
cargo test -p memoria-storage --test v2_store -- --nocapture --test-threads=1
cargo test -p memoria-api --test v2_api_e2e -- --nocapture --test-threads=1
cargo test -p memoria-mcp --test v2_tools_unit -- --nocapture
cargo test -p memoria-mcp --test v2_tools_e2e -- --nocapture
cargo test -p memoria-mcp --test v2_mcp_remote_capabilities -- --nocapture
```

- **V1/V2 隔离**：

```bash
cargo test -p memoria-storage --test v2_store_isolation -- --nocapture
cargo test -p memoria-api --test v2_api_isolation -- --nocapture
cargo test -p memoria-api --test v2_api_db_isolation -- --nocapture
cargo test -p memoria-service --test v2_runtime_isolation -- --nocapture
cargo test -p memoria-mcp --test v2_mcp_remote_isolation -- --nocapture
```

注意：V2 纯功能与 V1/V2 隔离验证现在分别落在独立测试文件中；纯 V2 storage/API 套件仍固定串行执行，以避开 MatrixOne/隔离数据库启动窗口导致的并发噪声。
