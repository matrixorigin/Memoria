# Per-User Database 隔离架构设计

> **状态**: RFC Draft
> **作者**: Copilot + ghs-mo
> **日期**: 2026-04-03
> **影响范围**: memoria-storage, memoria-git, memoria-service, memoria-mcp, memoria-api, memoria-cli

---

## 目录

1. [Executive Summary](#1-executive-summary)
2. [动机与问题分析](#2-动机与问题分析)
3. [架构总览](#3-架构总览)
4. [数据库布局设计](#4-数据库布局设计)
5. [核心基础设施变更](#5-核心基础设施变更)
6. [Snapshot / Branch / Restore 隔离设计](#6-snapshot--branch--restore-隔离设计)
7. [受影响模块逐项分析](#7-受影响模块逐项分析)
8. [后台任务与监控](#8-后台任务与监控)
9. [CLI 与 Init 适配](#9-cli-与-init-适配)
10. [迁移策略](#10-迁移策略)
11. [风险与缓解](#11-风险与缓解)
12. [MatrixOne 待验证项](#12-matrixone-待验证项)

---

## 1. Executive Summary

**现状**: 所有用户共享单个 MatrixOne 数据库 `memoria`，通过 `user_id` 列做行级过滤隔离。

**问题**: `memory_rollback` 操作会 DELETE + INSERT 整个 `mem_memories` 表，**一个用户的回滚会摧毁所有其他用户的数据**。Snapshot 是 Account 级别，同样影响全局。

**目标**: 每用户独占一个 database，全局共享表提取到独立共享库。使 Git-for-Data 能力（snapshot / branch / restore）可以安全地 per-user 使用，互不影响。

**核心改造**:

| 改造项 | 现状 | 目标 |
|--------|------|------|
| 数据库粒度 | 单 DB，行级隔离 | Per-user DB，物理隔离 |
| Snapshot 级别 | `FOR ACCOUNT sys` | `FOR DATABASE {user_db}` |
| Rollback 安全性 | ❌ 影响所有用户 | ✅ 仅影响当前用户 |
| Branch 命名 | UUID 前缀防冲突 | 用户 DB 内天然隔离 |
| 用户删除 | 多表 DELETE WHERE | `DROP DATABASE` |
| 共享表 | 混在同一 DB | 独立 `memoria_shared` 库 |

---

## 2. 动机与问题分析

### 2.1 当前架构

```
┌───────────────────────────────────────────────┐
│              Database: memoria                │
│                                               │
│  mem_memories      (ALL users, row filter)    │
│  mem_edit_log      (ALL users, row filter)    │
│  mem_branches      (ALL users, row filter)    │
│  mem_snapshots     (ALL users, row filter)    │
│  memory_graph_*    (ALL users, row filter)    │
│  mem_plugin_*      (global, no user_id)       │
│  mem_distributed_* (global, no user_id)       │
│  br_a1b2c3d4_*     (branch tables, mixed)     │
│  ...共 27 张表                                 │
└───────────────────────────────────────────────┘
```

27 张表中，约 18 张有 `user_id` 列做行级过滤，约 8 张是全局共享表，1 张混合使用。

### 2.2 安全隐患一览

#### P0 致命：Rollback 摧毁所有用户数据

`memory_rollback` 硬编码 `"mem_memories"` 表名（`git_tools.rs:420`），不使用 `active_table()` 分支解析：

```rust
// git_tools.rs:420-425 — 当前实现
git.restore_table_from_snapshot("mem_memories", &internal).await?;
for table in &["memory_graph_nodes", "memory_graph_edges", "mem_edit_log"] {
    let _ = git.restore_table_from_snapshot(table, &internal).await;
}
```

`restore_table_from_snapshot` 执行的 SQL（`service.rs:137-143`）：

```sql
DELETE FROM mem_memories;                                            -- 删除 ALL 用户
INSERT INTO mem_memories SELECT * FROM mem_memories {SNAPSHOT = 'x'};  -- 恢复 ALL 用户到快照点
```

**灾难场景**：

```
T1: User A 创建 snapshot_1 → 记录 A(100条) + B(200条) 的状态
T2: User B 写入 50 条新记忆 → B 现有 250 条
T3: User A 执行 memory_rollback(snapshot_1)
    → DELETE FROM mem_memories (删除所有人的数据)
    → INSERT ... {SNAPSHOT='snapshot_1'} (恢复到 T1 状态)
    → User B 丢失 50 条新记忆 ❌
```

Graph 表 (`memory_graph_nodes`, `memory_graph_edges`) 和审计表 (`mem_edit_log`) 同样被整表恢复。

#### P1 高危：Snapshot 是 Account 级别

```rust
// service.rs:69
exec_ddl(&self.pool, &format!("CREATE SNAPSHOT {safe} FOR ACCOUNT sys"))
```

`FOR ACCOUNT sys` 快照覆盖**整个 account 下所有数据库**。Safety snapshot（`store.rs:1357`）也用 `FOR ACCOUNT sys`。

#### P2 中危：Branch Diff Account 级别泄露

`data branch diff` 是 account 级命令，返回所有用户的行，靠 Rust 层过滤（`service.rs:225-264`）：

```rust
// service.rs:249-251 — 应用层过滤，非数据库层
let rows: Vec<DiffRow> = all_rows.into_iter()
    .filter(|r| r.user_id == user_id)
    .collect();
```

### 2.3 完整风险矩阵

| 操作 | 风险等级 | 影响描述 | 代码位置 |
|------|---------|---------|---------|
| `memory_rollback` | P0 致命 | 整表 DELETE+INSERT，影响所有用户 | `git_tools.rs:420-425` |
| `CREATE SNAPSHOT` | P1 高危 | Account 级快照覆盖所有 DB | `service.rs:69` |
| Safety snapshot | P1 高危 | 同上 | `store.rs:1357` |
| `data branch diff` | P2 中危 | Account 级，应用层过滤 | `service.rs:225-264` |
| `admin/trigger_governance` | P3 低危 | Master 权限限制，但操作面广 | `admin.rs:198-287` |


---

## 3. 架构总览

### 3.1 目标架构

```
MatrixOne Account (sys)
|
+-- memoria_shared                    <-- 全局共享库（系统表）
|   +-- mem_user_registry             <-- 新增：user_id -> db_name 映射
|   +-- mem_api_keys                  <-- API 密钥管理
|   +-- mem_plugin_signers            \
|   +-- mem_plugin_packages           |
|   +-- mem_plugin_bindings           +-- 插件系统（6 张表）
|   +-- mem_plugin_reviews            |
|   +-- mem_plugin_binding_rules      |
|   +-- mem_plugin_audit_events       /
|   +-- mem_governance_runtime_state  <-- 全局熔断器
|   +-- mem_distributed_locks         <-- 分布式锁
|   +-- mem_async_tasks               <-- 跨实例异步任务
|
+-- mem_u_{hash_alice}                <-- User Alice 独立库
|   +-- mem_memories                  <-- 只有 Alice 的记忆
|   +-- mem_memories_stats            <-- 访问/反馈统计
|   +-- mem_user_state                <-- 活跃分支状态
|   +-- mem_branches                  <-- 分支注册表
|   +-- mem_snapshots                 <-- 快照注册表
|   +-- mem_edit_log                  <-- 审计日志
|   +-- mem_entity_links              <-- 实体链接
|   +-- mem_entities                  <-- 实体注册
|   +-- mem_memory_entity_links       <-- 记忆-实体映射
|   +-- memory_graph_nodes            <-- 知识图谱节点
|   +-- memory_graph_edges            <-- 知识图谱边
|   +-- mem_retrieval_feedback        <-- 检索反馈
|   +-- mem_user_retrieval_params     <-- 自适应检索参数
|   +-- mem_governance_cooldown       <-- 治理限流
|   +-- mem_tool_usage                <-- 工具使用统计
|   +-- mem_api_call_log              <-- API 调用日志
|   +-- br_mybranch                   <-- 分支表（无需 UUID 前缀）
|
+-- mem_u_{hash_bob}                  <-- User Bob 独立库
|   +-- (同上 schema)
|
+-- ...更多用户库
```

### 3.2 表分类清单

#### 共享库表（11 张）

| 表名 | 有 user_id | 用途 | 备注 |
|------|-----------|------|------|
| `mem_user_registry` | 是 (PK) | 用户注册与 DB 映射 | **新增** |
| `mem_api_keys` | 是 | API 密钥认证 | 需跨用户查询验证 token |
| `mem_async_tasks` | 是 (nullable) | 异步任务跟踪 | 需路由到用户 DB |
| `mem_distributed_locks` | 否 | 分布式互斥锁 | 全局 |
| `mem_governance_runtime_state` | 否 | 熔断器状态 | 全局 |
| `mem_plugin_signers` | 否 | 插件签名者 | 全局 |
| `mem_plugin_packages` | 否 | 插件包 | 全局 |
| `mem_plugin_bindings` | 否 | 插件绑定 | 全局 |
| `mem_plugin_reviews` | 否 | 插件评审 | 全局 |
| `mem_plugin_binding_rules` | 否 | 插件规则 | 全局 |
| `mem_plugin_audit_events` | 否 | 插件审计 | 全局 |

> **Restore 边界规则**: `memory_rollback` 只允许作用于用户库中的业务数据表。`memoria_shared` 中的认证、任务、锁、插件、用户注册等控制平面数据永不纳入用户 restore 作用域；共享库只允许 admin / 运维级备份恢复。

#### 用户库表（17 张）

| 表名 | 当前有 user_id | 改造后 | 用途 |
|------|---------------|--------|------|
| `mem_memories` | 是 | 可选保留 | 核心记忆存储 |
| `mem_memories_stats` | 是 | 可选保留 | 访问/反馈统计 |
| `mem_user_state` | 是 (PK) | 可简化 | 活跃分支 |
| `mem_branches` | 是 | 可选保留 | 分支注册 |
| `mem_snapshots` | 是 | 可选保留 | 快照注册 |
| `mem_edit_log` | 是 | 可选保留 | 审计日志 |
| `mem_entity_links` | 是 | 可选保留 | 实体链接 |
| `mem_entities` | 是 | 可选保留 | 实体注册 |
| `mem_memory_entity_links` | 是 | 可选保留 | 记忆-实体映射 |
| `memory_graph_nodes` | 是 | 可选保留 | 图谱节点 |
| `memory_graph_edges` | 是 | 可选保留 | 图谱边 |
| `mem_retrieval_feedback` | 是 | 可选保留 | 检索反馈 |
| `mem_user_retrieval_params` | 是 (PK) | 可简化 | 自适应参数 |
| `mem_governance_cooldown` | 是 | 可选保留 | 治理限流 |
| `mem_tool_usage` | 是 | 可选保留 | 工具统计 |
| `mem_api_call_log` | 是 | 可选保留 | 调用日志 |
| `br_*` (动态) | 是 | 去掉 UUID 前缀 | 分支表 |

> **控制元数据说明**: `mem_snapshots`、`mem_branches`、`mem_user_state`、`mem_governance_cooldown` 虽位于用户库，但属于控制元数据而非业务数据；它们负责 snapshot / branch 配额、当前分支与治理节流，默认**不随 `memory_rollback` 回退**。
>
> 当前配额实现仍沿用现状：snapshot 上限读取 `mem_snapshots`（当前 MCP 常量 `MAX_USER_SNAPSHOTS = 20`），branch 上限读取 `mem_branches`（`MAX_BRANCHES = 20`）。因此这两张表必须保持“当前态”，不能被 restore 回旧版本。

> **Phase 1 策略**: 保留所有 `user_id` 列和 `WHERE user_id = ?` 过滤，降低改造风险，方便灰度回退。
> **Phase 2 可选优化**: 移除 `user_id` 列，简化索引和查询。

---

## 4. 数据库布局设计

### 4.1 用户注册表

```sql
-- 位于 memoria_shared 库
CREATE TABLE IF NOT EXISTS mem_user_registry (
    user_id     VARCHAR(64)  PRIMARY KEY,
    db_name     VARCHAR(128) NOT NULL UNIQUE,
    status      VARCHAR(20)  NOT NULL DEFAULT 'active',  -- active / suspended / deleted
    created_at  DATETIME(6)  NOT NULL,
    updated_at  DATETIME(6)  NOT NULL,
    INDEX idx_status (status)
);
```

### 4.2 用户 DB 命名规则

```
db_name = "mem_u_" + sha256(user_id)[0:16]

示例:
  user_id = "alice@example.com"  -> db_name = "mem_u_a1b2c3d4e5f6g7h8"
  user_id = "github|12345"       -> db_name = "mem_u_9f8e7d6c5b4a3210"
```

- 使用 SHA-256 前 16 位 hex：避免特殊字符，确保唯一性
- 通过 `mem_user_registry` 保证全局唯一
- MatrixOne database 名最大 128 字符

### 4.3 首次访问自动创建

用户首次请求时自动创建 database + 初始化 schema（lazy provisioning）：

```
HTTP Request (user_id: alice)
  -> DbRouter.resolve_user_db("alice")
    -> Cache miss -> 查 mem_user_registry
      -> 不存在 -> CREATE DATABASE mem_u_xxx
                -> 执行 migrate() 初始化表结构
                -> INSERT INTO mem_user_registry
    -> 返回 db_name
  -> USE mem_u_xxx
  -> 执行业务 SQL
```


---

## 5. 核心基础设施变更

### 5.1 DbRouter — 新增核心组件

**位置**: `memoria-storage/src/router.rs`（新文件）

```rust
/// 数据库路由器：管理共享库和用户库连接
pub struct DbRouter {
    /// 共享库连接池（memoria_shared）
    /// 用于 auth、plugin、lock 等全局操作
    shared_pool: MySqlPool,

    /// 用户库连接池
    /// 通过 USE {db_name} 切换数据库上下文
    user_pool: MySqlPool,

    /// 用户注册表缓存 (user_id -> db_name), TTL 60s
    user_db_cache: Cache<String, String>,

    /// 嵌入向量维度（创建新用户库时需要）
    embedding_dim: usize,
}
```

#### 核心方法

```rust
impl DbRouter {
    /// 获取用户 DB 的独占连接（已切换到用户库）
    pub async fn user_conn(&self, user_id: &str) -> Result<PoolConnection<MySql>> {
        let db_name = self.resolve_user_db(user_id).await?;
        let mut conn = self.user_pool.acquire().await?;
        sqlx::query(&format!("USE {}", validate_identifier(&db_name)?))
            .execute(&mut *conn)
            .await?;
        Ok(conn)
    }

    /// 获取共享库连接（始终在 memoria_shared 上下文）
    pub async fn shared_conn(&self) -> Result<PoolConnection<MySql>> {
        self.shared_pool.acquire().await
            .map_err(|e| MemoriaError::Database(e.to_string()))
    }

    /// 获取用户的 db_name（不获取连接）
    pub async fn user_db_name(&self, user_id: &str) -> Result<String> {
        self.resolve_user_db(user_id).await
    }

    /// 解析 user_id -> db_name, 不存在则自动创建
    async fn resolve_user_db(&self, user_id: &str) -> Result<String> {
        // 1. 查缓存
        if let Some(db) = self.user_db_cache.get(user_id) {
            return Ok(db);
        }
        // 2. 查注册表
        let row = sqlx::query_scalar::<_, String>(
            "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'"
        )
        .bind(user_id)
        .fetch_optional(&self.shared_pool)
        .await?;

        if let Some(db_name) = row {
            self.user_db_cache.insert(user_id.to_string(), db_name.clone());
            return Ok(db_name);
        }
        // 3. 首次访问，创建用户库
        let db_name = self.provision_user_db(user_id).await?;
        self.user_db_cache.insert(user_id.to_string(), db_name.clone());
        Ok(db_name)
    }

    /// 创建用户库 + 初始化 schema + 注册
    async fn provision_user_db(&self, user_id: &str) -> Result<String> {
        let hash = &sha256_hex(user_id)[..16];
        let db_name = format!("mem_u_{hash}");
        let safe_db = validate_identifier(&db_name)?;

        // CREATE DATABASE
        sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS {safe_db}"))
            .execute(&self.shared_pool).await?;

        // 初始化 schema (USE + migrate)
        let mut conn = self.user_pool.acquire().await?;
        sqlx::query(&format!("USE {safe_db}")).execute(&mut *conn).await?;
        self.migrate_user_db(&mut conn).await?;

        // 注册
        sqlx::query(
            "INSERT INTO mem_user_registry \
             (user_id, db_name, status, created_at, updated_at) \
             VALUES (?, ?, 'active', NOW(6), NOW(6))"
        )
        .bind(user_id).bind(&db_name)
        .execute(&self.shared_pool).await?;

        Ok(db_name)
    }
}
```

### 5.2 SqlMemoryStore 改造

**文件**: `memoria-storage/src/store.rs`

**改造前**:

```rust
pub struct SqlMemoryStore {
    pool: MySqlPool,           // 单一连接池
    embedding_dim: usize,
    instance_id: String,
    // ...caches
}
```

**改造后**:

```rust
pub struct SqlMemoryStore {
    router: Arc<DbRouter>,     // 替换 pool
    embedding_dim: usize,
    instance_id: String,
    // ...caches (不变)
}
```

**connect() 方法改造**（`store.rs:577-628`）:

```rust
// 改造前
pub async fn connect(database_url: &str, embedding_dim: usize, instance_id: String) -> Result<Self>

// 改造后
pub async fn connect(
    shared_db_url: &str,    // 指向 memoria_shared
    user_db_url: &str,      // 不指定具体 DB，用 USE 切换
    embedding_dim: usize,
    instance_id: String,
) -> Result<Self>
```

**查询方法改造模式** — 以 `insert_memory` 为例:

```rust
// 改造前
pub async fn insert_memory(&self, user_id: &str, memory: &Memory) -> Result<String> {
    let table = self.active_table(user_id).await?;
    sqlx::query(&format!("INSERT INTO {table} ..."))
        .execute(&self.pool)  // <-- 直接用 pool
        .await?;
}

// 改造后
pub async fn insert_memory(&self, user_id: &str, memory: &Memory) -> Result<String> {
    let mut conn = self.router.user_conn(user_id).await?;  // <-- 获取用户 DB 连接
    let table = self.active_table_from_conn(&mut conn, user_id).await?;
    sqlx::query(&format!("INSERT INTO {table} ..."))
        .execute(&mut *conn)  // <-- 使用用户连接
        .await?;
}
```

**共享表操作模式** — 以 `verify_api_key` 为例:

```rust
// 改造后
pub async fn verify_api_key(&self, key_hash: &str) -> Result<Option<ApiKey>> {
    let mut conn = self.router.shared_conn().await?;  // <-- 共享库连接
    sqlx::query_as("SELECT * FROM mem_api_keys WHERE key_hash = ? AND is_active = 1")
        .bind(key_hash)
        .fetch_optional(&mut *conn)
        .await
}
```

### 5.3 GitForDataService 改造

**文件**: `memoria-git/src/service.rs`

**改造前**:

```rust
pub struct GitForDataService {
    pool: MySqlPool,
    db_name: String,        // 固定一个 DB 名
}
```

**改造后**:

```rust
pub struct GitForDataService {
    pool: MySqlPool,
    // db_name 移除 — 由调用方传入 user_db
}
```

**所有 public 方法增加 `user_db: &str` 参数**:

| 方法 | 改造点 |
|------|--------|
| `create_snapshot(name, user_db)` | `CREATE SNAPSHOT {name} FOR DATABASE {user_db}` |
| `list_snapshots()` | `SHOW SNAPSHOTS` + 过滤 `DATABASE_NAME` |
| `drop_snapshot(name)` | `DROP SNAPSHOT {name}` (验证 database 作用域) |
| `restore_table_from_snapshot(table, snap, user_db)` | 全限定名 `{user_db}.{table}` |
| `create_branch(branch, source, user_db)` | `data branch create table {user_db}.{branch} from {user_db}.{source}` |
| `create_branch_from_snapshot(...)` | 同上 + `{snapshot = '...'}` |
| `drop_branch(branch, user_db)` | `data branch delete table {user_db}.{branch}` |
| `merge_branch(branch, main, user_db)` | 全限定名 |
| `diff_branch_rows(branch, main, user_db, limit)` | 全限定名，**去掉应用层 user_id 过滤**（天然隔离） |
| `count_at_snapshot(table, snap, user_db)` | 全限定名 |

### 5.4 MemoryService 改造

**文件**: `memoria-service/src/service.rs`

`MemoryService` 持有 `Arc<SqlMemoryStore>` 和 `Arc<GitForDataService>`。主要影响：

1. **构造函数** (`new_sql_with_llm`, L377-475):
   - `SqlMemoryStore` 改用 `DbRouter`，构造方式变化
   - 后台 worker pool（entity extraction, graph isolation）需要感知 user DB

2. **EditLogBuffer** (L95-267):
   - 当前 flush 到共享的 `mem_edit_log`
   - 改造后: flush 时需根据 `user_id` 路由到用户 DB
   - 批量 flush 按 user_id 分组

3. **AccessCounter** (L95-161):
   - 当前 flush 到共享的 `mem_memories_stats`
   - 改造后: 按 user_id 分组路由

4. **所有 public 方法** — 已有 `user_id` 参数，改为通过 `router.user_conn(user_id)` 获取连接即可。

### 5.5 Config 改造

**文件**: `memoria-service/src/config.rs`

```rust
// 改造前
pub struct Config {
    pub db_url: String,      // 单一 DB URL
    pub db_name: String,     // 单一 DB 名
    // ...
}

// 改造后
pub struct Config {
    pub shared_db_url: String,  // 共享库 URL (memoria_shared)
    pub user_db_url: String,    // 用户库 URL (不含具体 DB，用 USE 切换)
    pub db_name: String,        // 保留向后兼容
    // ...
}
```

环境变量:

| 变量 | 用途 | 默认值 |
|------|------|--------|
| `DATABASE_URL` | 向后兼容（单 DB 模式） | `mysql://root:111@localhost:6001/memoria` |
| `SHARED_DB_URL` | 共享库连接（新） | 自动从 DATABASE_URL 推导 |
| `MEMORIA_MULTI_DB` | 启用 per-user DB 模式 | `false`（Phase 1 灰度开关） |


---

## 6. Snapshot / Branch / Restore 隔离设计

### 6.1 Snapshot 隔离

| 维度 | 改造前 | 改造后 |
|------|--------|--------|
| 创建 | `CREATE SNAPSHOT s1 FOR ACCOUNT sys` | `CREATE SNAPSHOT s1 FOR DATABASE mem_u_xxx` |
| 粒度 | Account 级（所有 DB） | Database 级（单用户） |
| 列表 | `SHOW SNAPSHOTS` (全部) | `SHOW SNAPSHOTS` + 过滤 `DATABASE_NAME = user_db` |
| 时间旅行 | `{SNAPSHOT = 's1'}` (全部) | `{SNAPSHOT = 's1'}` (仅该 DB) |
| 删除 | `DROP SNAPSHOT s1` | `DROP SNAPSHOT s1`（需验证 database 作用域） |
| 命名空间 | 全局共享，需 prefix 区分 | Per-database，天然隔离 |

**Safety Snapshot 改造**（`store.rs:1348-1383`）:

```rust
// 改造前
let sql = format!("CREATE SNAPSHOT {name} FOR ACCOUNT sys");

// 改造后
let user_db = self.router.user_db_name(user_id).await?;
let sql = format!("CREATE SNAPSHOT {name} FOR DATABASE {user_db}");
```

**Snapshot 命名简化**:

Per-user DB 后，snapshot 命名空间天然隔离。可以考虑：
- 保留 `mem_snap_` 前缀（兼容性）
- 但**不再需要** UUID 段来防用户间冲突
- `mem_milestone_` 自动快照仍然保留

### 6.2 Restore / Rollback 隔离

**这是本次改造解决的核心安全问题。**

```
改造前（致命）:
  DELETE FROM mem_memories;                               <-- 删除 ALL 用户
  INSERT INTO mem_memories SELECT * FROM ... {SNAPSHOT};   <-- 恢复 ALL 用户

改造后（安全）:
  DELETE FROM mem_u_alice.mem_memories;                    <-- 只删 Alice
  INSERT INTO mem_u_alice.mem_memories SELECT * FROM ...;  <-- 只恢复 Alice
```

由于 `mem_memories` 在用户 DB 中只有该用户的数据，即使 `memory_rollback` 继续硬编码 `"mem_memories"` 也是安全的。但建议同时修复为使用 `active_table()` 以支持分支回滚场景。

**Graph 表恢复同理安全**:

```rust
// 改造后，graph 表也在用户 DB 内，天然隔离
for table in &["memory_graph_nodes", "memory_graph_edges", "mem_edit_log"] {
    let _ = git.restore_table_from_snapshot(table, &internal, &user_db).await;
}
```

**Restore 必须保持“业务数据域恢复”，不能演进成整库 restore**:

| 域 | 典型表 | restore 行为 | 原因 |
|----|--------|--------------|------|
| 业务数据域 | `mem_memories`, `memory_graph_nodes`, `memory_graph_edges`, `mem_edit_log` | 回退到 snapshot | 用户真正希望回滚的数据 |
| 用户控制元数据域 | `mem_snapshots`, `mem_branches`, `mem_user_state`, `mem_governance_cooldown` | 保持当前态 | 防止 quota / active branch / cooldown 被用户回滚 |
| 共享控制平面 | `memoria_shared.*` | 严禁用户 restore | 认证、锁、异步任务、插件与用户路由均属全局控制面 |

**为什么不能回滚控制元数据**:

- 回滚 `mem_snapshots` 会让快照注册表回到旧状态，但 MatrixOne 中后续创建的 DB snapshot 仍存在，形成 orphan snapshot，用户不可见但继续占配额。
- 回滚 `mem_branches` 会让仍存在的 `br_*` 表失去注册记录，导致分支不可管理，或后续创建时出现重名冲突。
- 回滚 `mem_user_state` 可能把 `active_branch` 指针静默切回旧值，造成用户无感切分支。
- 回滚 `mem_governance_cooldown` 可能绕过或异常放大治理节流。

**恢复后对账机制（新增强制步骤）**:

1. **缓存失效**：清理当前用户的 active-branch / cooldown 等本地缓存，避免实例继续使用 restore 前状态。
2. **Snapshot 对账**：
   - 事实源：`git.list_snapshots()` / `SHOW SNAPSHOTS`，并过滤当前 `user_db`
   - 控制面：`mem_snapshots`
   - `注册有 / 实际无` → 将 `mem_snapshots.status` 标记为 `deleted` 并告警
   - `实际有 / 注册无`（`mem_snap_` 前缀） → 自动补注册，确保用户可见、可删、并正确计入配额
3. **Branch 对账**：
   - 事实源：`SHOW TABLES LIKE 'br_%'` / `information_schema`
   - 控制面：`mem_branches`
   - `注册有 / 实际无` → 标记 `deleted` 并告警
   - `实际有 / 注册无` → 自动补注册，避免“有表无控制面”
4. **配额统计**：snapshot 上限继续基于 `mem_snapshots`，branch 上限继续基于 `mem_branches`；但 create / list / delete 之前先执行轻量对账或读取最近一次对账缓存。

> **安全原则**: 对账优先“补注册”而不是“自动删除”，避免误删用户仍需保留的数据。共享库中的 `mem_api_keys`、`mem_distributed_locks`、`mem_async_tasks`、`mem_user_registry` 与 `mem_plugin_*` 永远不受用户 restore 影响。

### 6.3 Branch 隔离

| 维度 | 改造前 | 改造后 |
|------|--------|--------|
| 创建 | `data branch create table br_a1b2c3d4_mybranch from mem_memories` | `data branch create table mem_u_xxx.br_mybranch from mem_u_xxx.mem_memories` |
| 命名 | `br_{uuid8}_{name}` 防冲突 | `br_{name}` 天然隔离 |
| 删除 | `data branch delete table memoria.br_xxx` | `data branch delete table mem_u_xxx.br_xxx` |
| Merge | 全表 merge + 应用层过滤 | 用户 DB 内 merge，无需过滤 |
| Diff | Account 级 + Rust 过滤 | 用户 DB 内 diff，天然只有该用户数据 |

**分支表命名简化**（`git_tools.rs:486-487`）:

```rust
// 改造前 — 需要 UUID 防冲突
let table_name = format!("br_{}_{}", &Uuid::new_v4().simple().to_string()[..8], safe);

// 改造后 — 用户 DB 内唯一即可
let table_name = format!("br_{}", safe);
```

### 6.4 改造后安全矩阵

| 操作 | 当前风险 | 改造后 | 说明 |
|------|---------|--------|------|
| `memory_rollback` | P0 致命 | 安全 | 只恢复业务数据表；共享/控制元数据不回退；restore 后执行 snapshot/branch 对账 |
| `memory_snapshot` | P1 高危 | 安全 | `FOR DATABASE` 只快照用户 DB |
| `memory_snapshot_delete` | P2 中危 | 安全 | Database 作用域 |
| `memory_branch` | 安全 | 安全 | 用户 DB 内创建 |
| `memory_merge` | 安全 | 安全 | 用户 DB 内合并 |
| `memory_diff` | P2 中危 | 安全 | 用户 DB 内 diff，无需过滤 |
| `memory_checkout` | 安全 | 安全 | 切换用户 DB 内分支 |
| `memory_store` | 安全 | 安全 | INSERT 到用户 DB |
| `memory_retrieve` | 安全 | 安全 | SELECT 从用户 DB |
| `memory_purge` | 安全 | 安全 | DELETE 在用户 DB |
| `memory_governance` | 安全 | 安全 | Safety snapshot 改 FOR DATABASE |
| `admin/delete_user` | 安全 | 更佳 | `DROP DATABASE` 一步到位 |


---

## 7. 受影响模块逐项分析

### 7.1 memoria-storage (核心改造)

| 文件 | 影响 | 改造内容 |
|------|------|---------|
| `store.rs` SqlMemoryStore struct | 大 | `pool` -> `router: Arc<DbRouter>` |
| `store.rs` `connect()` (L577-628) | 大 | 初始化 DbRouter，创建双池 |
| `store.rs` `migrate()` (L662-1200) | 中 | 拆分为 `migrate_shared()` + `migrate_user()` |
| `store.rs` `active_table()` (L1530-1571) | 小 | 改用 conn 参数而非 pool |
| `store.rs` 所有查询方法 (~80个) | 中 | `&self.pool` -> `&mut *conn` |
| `store.rs` `create_safety_snapshot()` (L1348) | 小 | `FOR ACCOUNT` -> `FOR DATABASE` |
| `store.rs` snapshot/branch 注册方法 (L1530-1776) | 小 | 已在用户 DB，逻辑不变 |
| `graph/store.rs` graph 表操作 | 中 | 同上模式，改用 conn |
| `graph/retriever.rs` 检索管线 | 中 | 同上模式 |
| **新增** `router.rs` DbRouter | 新 | 全新模块 |

### 7.2 memoria-git (Snapshot/Branch DDL)

| 文件 | 影响 | 改造内容 |
|------|------|---------|
| `service.rs` struct | 中 | 移除 `db_name` 字段 |
| `service.rs` `create_snapshot()` (L65-75) | 中 | `FOR ACCOUNT sys` -> `FOR DATABASE {user_db}` |
| `service.rs` `list_snapshots()` (L78-101) | 小 | 增加 database 过滤 |
| `service.rs` `drop_snapshot()` (L109-112) | 小 | 验证 database 作用域 |
| `service.rs` `restore_table_from_snapshot()` (L114-147) | 中 | 全限定名 `{user_db}.{table}` |
| `service.rs` `create_branch()` (L151-164) | 中 | 全限定名 |
| `service.rs` `create_branch_from_snapshot()` (L166-184) | 中 | 全限定名 |
| `service.rs` `drop_branch()` (L186-190) | 小 | 改用 `{user_db}.{branch}` |
| `service.rs` `merge_branch()` (L192-207) | 中 | 全限定名 |
| `service.rs` `diff_branch_rows()` (L209-264) | 中 | 全限定名 + **去掉应用层过滤** |
| `service.rs` `count_at_snapshot()` (L267-283) | 小 | 全限定名 |

### 7.3 memoria-service (业务层)

| 文件 | 影响 | 改造内容 |
|------|------|---------|
| `service.rs` `MemoryService::new_sql_with_llm()` (L377-475) | 中 | 构造 DbRouter, 传递给 store |
| `service.rs` EditLogBuffer (L95-267) | 中 | flush 按 user_id 分组路由 |
| `service.rs` AccessCounter (L95-161) | 中 | flush 按 user_id 分组路由 |
| `service.rs` `store_memory()` (L883) | 小 | 透传到 store（store 负责路由） |
| `service.rs` `retrieve()` (L1103) | 小 | 同上 |
| `service.rs` 其他业务方法 (~30个) | 小 | 同上 |
| `config.rs` Config struct (L32-69) | 小 | 增加 `shared_db_url`, `multi_db` 开关 |
| `governance.rs` 定时任务 (L332-338) | 中 | 遍历用户 DB 执行治理 |
| `vector_index_monitor.rs` | 小 | 需感知 user DB |

### 7.4 memoria-mcp (MCP 工具层)

| 文件 | 影响 | 改造内容 |
|------|------|---------|
| `git_tools.rs` `call()` 分发 (L304+) | 中 | 获取 `user_db` 传给 git service |
| `git_tools.rs` `memory_rollback` (L414-427) | 高 | 核心修复点 |
| `git_tools.rs` `memory_snapshot` (L307-344) | 中 | 传 user_db |
| `git_tools.rs` `memory_branch` (L429-503) | 中 | 去掉 UUID 前缀，传 user_db |
| `git_tools.rs` `memory_merge` (L552-713) | 中 | 全限定名 |
| `git_tools.rs` `memory_diff` (L738-796) | 小 | 去掉应用层过滤 |
| `git_tools.rs` `visible_snapshots_for_user()` (L106-152) | 小 | 过滤 database 级快照 |
| `tools.rs` 核心 MCP tools (L151-1166) | 小 | 透传到 service（不直接操作 DB） |
| `server.rs` `dispatch()` (L230-291) | 小 | 不变，user_id 已正确传递 |

### 7.5 memoria-api (REST API 层)

| 文件 | 影响 | 改造内容 |
|------|------|---------|
| `routes/memory.rs` CRUD endpoints | 小 | 透传 service（不直接操作 DB） |
| `routes/snapshots.rs` snapshot/branch endpoints | 小 | 透传 git_tools |
| `routes/governance.rs` 治理端点 | 小 | 透传 service |
| `routes/admin.rs` `system_stats` | 中 | 需遍历用户 DB 聚合 |
| `routes/admin.rs` `list_users` | 小 | 改查 `mem_user_registry` |
| `routes/admin.rs` `delete_user` | 中 | 改为 `DROP DATABASE` |
| `routes/admin.rs` `trigger_governance` | 中 | 切换到用户 DB 执行 |
| `routes/admin.rs` health endpoints | 小 | 用户级别已有 user_id |
| `routes/auth.rs` key management | 小 | 改用 `shared_conn()` |
| `routes/metrics.rs` `/metrics` | 中 | 需跨库聚合（见第 8 章） |
| `lib.rs` AppState & middleware | 小 | 持有 DbRouter 引用 |

### 7.6 API 端点完整影响清单

#### 无需修改逻辑的端点（透传 service/git_tools）

这些端点只调用 service 层方法，service 内部通过 DbRouter 自动路由：

| Method | Path | Handler |
|--------|------|---------|
| GET | `/health` | `health` |
| GET | `/health/instance` | `health_instance` |
| GET/POST | `/v1/memories` | `list_memories`, `store_memory` |
| POST | `/v1/memories/batch` | `batch_store` |
| POST | `/v1/memories/retrieve` | `retrieve` |
| POST | `/v1/memories/search` | `search` |
| GET | `/v1/memories/:id` | `get_memory` |
| PUT | `/v1/memories/:id/correct` | `correct_memory` |
| POST | `/v1/memories/correct` | `correct_by_query` |
| DELETE | `/v1/memories/:id` | `delete_memory` |
| POST | `/v1/memories/purge` | `purge_memories` |
| GET | `/v1/memories/:id/history` | `get_memory_history` |
| GET | `/v1/profiles/:id` | `get_profile` |
| POST | `/v1/observe` | `observe_turn` |
| POST | `/v1/memories/:id/feedback` | `record_feedback` |
| GET | `/v1/feedback/*` | feedback stats |
| GET/PUT | `/v1/retrieval-params` | retrieval params |
| POST | `/v1/retrieval-params/tune` | auto-tune |
| POST | `/v1/governance` | governance |
| POST | `/v1/consolidate` | consolidate |
| POST | `/v1/reflect` | reflect |
| POST | `/v1/extract-entities` | entity extraction |
| GET | `/v1/entities` | entity list |
| POST | `/v1/pipeline/run` | pipeline |
| POST | `/v1/sessions/:id/summary` | session summary |
| GET/POST/DELETE | `/v1/snapshots/*` | snapshot ops |
| GET/POST/DELETE | `/v1/branches/*` | branch ops |
| GET | `/v1/health/*` | user health checks |

#### 需要修改的端点

| Method | Path | 改造点 |
|--------|------|--------|
| GET | `/admin/stats` | 遍历 `mem_user_registry` 聚合跨库统计 |
| GET | `/admin/users` | 改查 `memoria_shared.mem_user_registry` |
| GET | `/admin/users/:id/stats` | 路由到用户 DB 查询 |
| DELETE | `/admin/users/:id` | `DROP DATABASE` + 清理注册表 + 撤销 keys |
| POST | `/admin/governance/:id/trigger` | 路由到用户 DB 执行治理 |
| GET | `/admin/health/hygiene` | 遍历用户 DB 聚合 |
| POST/GET/DELETE | `/auth/keys/*` | 改用 `shared_conn()` 操作 `mem_api_keys` |
| GET | `/metrics` | 跨库聚合（见第 8 章） |

### 7.7 MCP Tools 影响分析

| Tool | 当前风险 | 改造方式 | 改造后安全性 |
|------|----------|----------|------------|
| `memory_store` | 无 | 切换 DB | 安全 |
| `memory_retrieve` | 无 | 切换 DB | 安全 |
| `memory_correct` | 无 | 切换 DB | 安全 |
| `memory_purge` | 无 | 切换 DB + safety snapshot 改 FOR DATABASE | 安全 |
| `memory_rollback` | **P0 致命** | 切换 DB（核心修复） | 安全 |
| `memory_snapshot` | **P1 高危** | FOR DATABASE | 安全 |
| `memory_snapshot_delete` | P2 中危 | 验证 database 作用域 | 安全 |
| `memory_branch` | 无 | 全限定表名 | 安全 |
| `memory_merge` | 无 | 全限定表名 | 安全 |
| `memory_diff` | **P2 中危** | 全限定表名（天然隔离） | 安全 |
| `memory_checkout` | 无 | 切换 DB | 安全 |
| `memory_governance` | 无 | 切换 DB + safety snapshot 改 FOR DATABASE | 安全 |
| `memory_consolidate` | 无 | 切换 DB | 安全 |
| `memory_reflect` | 无 | 切换 DB | 安全 |
| `memory_feedback` | 无 | 切换 DB | 安全 |


---

## 8. 后台任务与监控

### 8.1 后台任务改造

| 任务 | 当前行为 | 改造方案 |
|------|---------|---------|
| **EditLogBuffer** (2s flush) | 批量 INSERT `mem_edit_log` | 按 `user_id` 分组，每组路由到对应用户 DB |
| **AccessCounter** (5s flush) | 批量 UPDATE `mem_memories_stats` | 按 `user_id` 分组路由 |
| **VectorIndexMonitor** | 监控单 DB 查询延迟 | 分用户 DB 监控，或聚合后判断 |
| **ConnectionPoolMonitor** (30s) | 监控单池 | 监控 shared_pool + user_pool 两个池 |
| **RestoreReconciler** (post-rollback) | 无 | `memory_rollback` 完成后触发；在 shared `mem_async_tasks` 记录任务，执行 snapshot/branch 对账与缓存失效 |

> `RestoreReconciler` 的任务状态保存在共享库，因此用户 restore 不会把“修复控制面元数据”的任务自己回滚掉。

### 8.2 定时治理任务

**文件**: `governance.rs`

当前三个定时任务：

| 任务 | 间隔 | 改造方案 |
|------|------|---------|
| Hourly | 3600s | 遍历 `mem_user_registry` -> 对每个活跃用户 DB 执行 |
| Daily | 86400s | 同上 |
| Weekly | 604800s | 同上 + 清理 per-user DB 级快照/分支 |

**关键改造**: `list_active_users()` 当前从 `mem_memories` 聚合 `DISTINCT user_id`，改为从 `mem_user_registry` 查询。

```rust
// 改造前 (governance.rs:62)
let users = store.list_active_users().await?;
// -> SELECT DISTINCT user_id FROM mem_memories WHERE is_active = 1

// 改造后
let users = router.shared_conn().await?
    .fetch_all("SELECT user_id, db_name FROM mem_user_registry WHERE status = 'active'")
    .await?;

for (user_id, db_name) in users {
    let mut conn = router.user_conn(&user_id).await?;
    // 在用户 DB 上下文中执行治理
    governance_for_user(&mut conn, &user_id).await?;
}
```

### 8.3 Prometheus Metrics 改造

**文件**: `routes/metrics.rs`

当前查询共享的 `mem_memories` 等表聚合全局统计。改造后需跨库聚合：

| Metric | 当前数据源 | 改造方案 |
|--------|-----------|---------|
| `memoria_memories_total` | `COUNT(*) FROM mem_memories` | 遍历用户 DB 或用注册表估算 |
| `memoria_users_total` | `COUNT(DISTINCT user_id)` | 直接查 `mem_user_registry` |
| `memoria_feedback_total` | `COUNT(*) FROM mem_retrieval_feedback` | 遍历用户 DB |
| `memoria_graph_nodes_total` | `COUNT(*) FROM memory_graph_nodes` | 遍历用户 DB |
| `memoria_branches_total` | `COUNT(DISTINCT ...)` | 遍历用户 DB |

**优化方案**:
- Metrics 缓存 TTL 适当延长（如 60s）
- 后台定时聚合线程，而非请求时遍历
- 或在 `mem_user_registry` 增加 `memory_count` 等聚合字段，写入时异步更新

---

## 9. CLI 与 Init 适配

### 9.1 CLI 命令改造

**文件**: `memoria-cli/src/main.rs`

#### `memoria serve` (L354-409)

```rust
// 改造前
let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim, ...).await?;
let pool = MySqlPool::connect(&cfg.db_url).await?;
let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

// 改造后
let router = Arc::new(DbRouter::connect(
    &cfg.shared_db_url, &cfg.user_db_url, cfg.embedding_dim
).await?);
router.migrate_shared().await?;  // 迁移共享库 schema
let store = SqlMemoryStore::new(router.clone(), cfg.embedding_dim, cfg.instance_id);
let git = Arc::new(GitForDataService::new(router.user_pool().clone()));
```

#### `memoria mcp` -- Embedded Mode (L453-521)

```rust
// 改造后: 支持 multi-db
if cfg.multi_db {
    let router = Arc::new(DbRouter::connect(...).await?);
    // ...
} else {
    // 向后兼容单 DB 模式
    let store = SqlMemoryStore::connect_legacy(&cfg.db_url, ...).await?;
}
```

#### `memoria mcp` -- Remote Mode (L440-451)

**无需改造**: Remote 模式通过 HTTP 代理到 API 服务器，数据库路由在服务端处理。

### 9.2 Init 命令适配

**文件**: `memoria-cli/src/main.rs` -- `mcp_entry()` (L970-1040)

当前生成的 MCP 配置中包含 `--db-url` 参数。多 DB 模式下：

| 参数 | 单 DB 模式 | 多 DB 模式 |
|------|-----------|-----------|
| `--db-url` | `mysql://root:111@localhost:6001/memoria` | 不指定具体 DB: `mysql://root:111@localhost:6001` |
| `--shared-db` | 不需要 | `memoria_shared`（新增） |
| `--multi-db` | 不需要 | flag 开关（新增） |
| `--user` | 有效 | 有效（用于 DB 路由） |

### 9.3 user_id 流转（不变）

当前 user_id 流转路径无需改变：

```
CLI --user "alice"
  -> Config.user
    -> run_stdio(service, git, cfg.user)
      -> dispatch(method, params, mode, user_id)
        -> tools::call(name, args, service, user_id)
          -> service.store_memory(user_id, ...)    // user_id 用于 DB 路由
            -> store.router.user_conn(user_id)     // 获取用户 DB 连接
```

---

## 10. 迁移策略

### 10.1 灰度开关

通过 `MEMORIA_MULTI_DB=true` 环境变量启用新架构。Phase 1 期间两种模式并存：

```rust
enum StoreMode {
    /// 传统单 DB 模式（默认）
    Legacy { pool: MySqlPool },
    /// 多 DB 模式
    MultiDb { router: Arc<DbRouter> },
}
```

### 10.2 正式 migration CLI 形态

新增正式离线迁移入口：

```bash
# 1) dry-run（默认，不改目标库）
memoria migrate legacy-to-multi-db \
  --legacy-db-url mysql://root:111@host:6001/memoria \
  --shared-db-url mysql://root:111@host:6001/memoria_shared \
  --report-out migration-plan.json

# 2) execute（真正执行）
memoria migrate legacy-to-multi-db \
  --legacy-db-url mysql://root:111@host:6001/memoria \
  --shared-db-url mysql://root:111@host:6001/memoria_shared \
  --execute \
  --report-out migration-run.json
```

**命令语义**：

- 默认 **dry-run**：只读取 legacy DB，输出计划与风险，不创建 shared DB / user DB，不写任何目标表。
- `--execute`：执行真实迁移。该模式要求业务写入已冻结（maintenance window / offline cutover），并且会先 **创建一个 MatrixOne account 级 safety snapshot**；只有 snapshot 创建成功后，才会继续 reset 目标 shared DB 和目标 user DB，再执行纯 `INSERT ... SELECT` 复制。
- `--user alice --user bob`：仅迁移指定用户的**用户态数据**，主要用于 rehearsal / troubleshooting；共享持久表仍按全集同步，不建议把它当成正式灰度切流工具。
- `--report-out`：输出完整 JSON 报告，便于变更单、审计和复盘。

**当前 CLI 的安全边界**：

1. 复制 **共享持久表**：`mem_api_keys`、`mem_governance_runtime_state`、`mem_plugin_*`
2. **不复制共享运行态表**：`mem_distributed_locks`、`mem_async_tasks`，目标集群启动后重新建立
3. 对每个用户：
   - 重建对应的 `mem_u_*` 目标库
   - 按 `user_id` 复制用户级表
   - 复制 `mem_memories_stats`（按 `memory_id` 归属）
   - 复制物理分支表 `br_*`
   - 校验源/目标行数
4. **Fail fast**：如果 legacy DB 中仍存在 `mem_snapshots.status = 'active'` 的快照，CLI 会拒绝 execute；当前版本**不会**把旧 shared-db snapshot 物化成新的 per-user DB snapshot。

### 10.3 运维执行流程（operator runbook）

**谁执行**：运维 / SRE / 发布负责人。不是普通用户命令，也不是每次发版都要跑。

**什么时候执行**：

- 旧单库架构 -> 新 per-user DB 架构的正式 cutover
- staging / pre-prod 预演
- 用 legacy 备份重建一套 multi-db 环境

**推荐步骤**：

1. **预检查（可在线）**
   - 确认新版本代码已部署好，但仍保持 `MEMORIA_MULTI_DB=false`
   - 先跑一次 dry-run，导出 `migration-plan.json`
   - 检查报告中的：
     - 用户数是否符合预期
     - 是否存在 active legacy snapshots（若有，先清理或接受手工重建）
     - branch 表数量、共享表数量是否异常

2. **进入 maintenance window**
   - 停止旧 API 写流量，冻结 MCP / REST 写入
   - 保留 legacy DB 作为回滚基线，不要提前删库

3. **执行迁移**
   - 运行 `memoria migrate legacy-to-multi-db ... --execute`
   - 观察 stdout / `migration-run.json`
   - execute 会先自动创建一个 account snapshot；如果这一步失败，迁移会直接终止，不会继续改目标库
   - account snapshot 名保持 `mem_migrate_account_pre_*_<uuid8>` 前缀风格；如果 legacy 库名过长，会自动截断中间的库名片段，确保不超过 MatrixOne 的 64 字符标识符限制
   - 注意：`--execute` 会清空并重建目标 `shared_db_url` 以及本次涉及的每个 `mem_u_*`，不要把它指向仍需保留业务数据的库
   - 期望结果：
      - 报告中能看到 pre-execute account snapshot 名称，便于必要时整账号 restore
      - `memoria_shared` 只包含 control-plane 表
      - 每个用户都映射到唯一 `mem_u_*`
     - 物理 branch 表 `br_*` 已复制到目标用户库

4. **切换新架构**
   - 启动新 API / MCP，设置 `MEMORIA_MULTI_DB=true`
   - smoke check：
     - 老 API key 仍可认证
     - 关键用户的 branch / checkout / merge 正常
     - snapshot / rollback 对新创建快照正常
     - `/admin/users`、`/admin/stats`、`/metrics` 正常

5. **观察期**
   - legacy DB 至少保留 7 天
   - 观察跨实例写入、后台任务、治理任务、恢复路径

6. **回滚**
   - 若 cutover 后发现问题：停止 multi-db API
   - 优先根据 execute 报告里记录的 pre-execute account snapshot 执行整账号 restore
   - 恢复旧配置：`MEMORIA_MULTI_DB=false`，重新指向 legacy `DATABASE_URL`
   - 确认 legacy DB 已回到 cutover 前状态后，再继续使用 legacy DB 提供服务
   - 保留已生成的 shared / `mem_u_*` 目标库用于排障，不要立即销毁

### 10.4 分支表迁移

分支表（`br_*`）需特殊处理；当前正式 CLI 的策略是**保留物理表名原样复制**，不做 rename：

```sql
-- 1. 查 mem_branches 获取 user_id -> table_name 映射
SELECT user_id, name, table_name
FROM memoria.mem_branches
WHERE status = 'active';

-- 2. 在目标用户 DB 中重建同名物理表
CREATE TABLE mem_u_xxx.br_a1b2c3d4_mybranch LIKE memoria.br_a1b2c3d4_mybranch;

-- 3. 复制数据
INSERT INTO mem_u_xxx.br_a1b2c3d4_mybranch
SELECT * FROM memoria.br_a1b2c3d4_mybranch;
```

**原因**：

- 当前运行时 `mem_branches.table_name` 直接作为 active table / merge / diff 的事实源
- 保留原表名可以避免迁移阶段额外做 metadata rewrite
- branch metadata 与 physical table 都按显式列复制，避免列顺序漂移

**验证点**：

- `mem_branches` 中每条 active row 都能在目标用户库找到对应 `br_*` 物理表
- `mem_user_state.active_branch` 若不为 `main`，则对应 branch 必须存在
- branch 表行数必须与源库一致


---

## 11. 风险与缓解

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|----------|
| `USE {db}` 连接池并发安全 | 数据串用到其他用户 | 中 | 使用独占连接（acquire 后整个请求持有），不 release 回池直到请求结束 |
| 大量 DB 导致 MO 元数据膨胀 | DDL 性能下降 | 中 | 监控 DB 数量；设置上限（如 10000）；定期清理 deleted 用户 |
| Admin 跨库聚合查询慢 | 管理体验差 | 高 | 异步后台聚合 + 缓存；metrics 延长 TTL；注册表增加聚合字段 |
| 迁移期间数据不一致 | 数据丢失/重复 | 低 | 先 dry-run，再 offline execute；execute 前冻结写流量，目标侧先 reset 再纯 INSERT 复制；保留旧 DB 作备份 |
| 控制元数据被 restore 回退 | orphan snapshot、quota 失真、branch 漂移 | 中 | 明确 restore 白名单只含业务数据表；restore 后执行 snapshot/branch 对账；禁止用户 restore shared DB |
| Snapshot 配额问题 | 用户操作失败 | 中 | Database 级 snapshot 配额独立管理；保留 safety snapshot 清理逻辑 |
| 首次访问 DB 创建延迟 | 首请求 latency 高 | 中 | 异步预创建；或接受 ~100ms 首次延迟 |
| embedded CLI 模式兼容性 | 单用户场景退化 | 低 | 保留 legacy 模式作为默认；embedded 可自动创建单用户 DB |
| execute 前 account snapshot 创建失败 | 无法安全回滚 | 中 | execute 必须先成功创建 account safety snapshot；失败则直接终止，不进入目标库 reset / copy |
| legacy active snapshot 无法自动物化为 per-user snapshot | 旧快照能力丢失 | 中 | execute 前 fail fast；运维先清理旧快照，或 cutover 后按需要重新创建用户级 snapshot |
| 跳过运行态 shared 表 | 锁/异步任务状态丢失 | 低 | `mem_distributed_locks`、`mem_async_tasks` 在新集群启动后重建；迁移必须在停写窗口执行 |

---

## 12. MatrixOne 待验证项

> 已确认: MatrixOne 支持 `CREATE SNAPSHOT FOR DATABASE` 和 `FOR TABLE` 级别的快照。

| # | 验证项 | 重要性 | 状态 |
|---|--------|--------|------|
| 1 | `CREATE SNAPSHOT FOR DATABASE {db}` | P0 | 已确认支持 |
| 2 | Database 级 snapshot 的 `{SNAPSHOT = 'x'}` 时间旅行 | P0 | 待验证 |
| 3 | `DROP SNAPSHOT` 是否需 database 限定 | P0 | 待验证 |
| 4 | `data branch create table db1.t from db1.s` 全限定名 | P0 | 待验证 |
| 5 | `data branch diff db1.t against db1.s` 全限定名 | P1 | 待验证 |
| 6 | `data branch merge db1.t into db1.s` 全限定名 | P1 | 待验证 |
| 7 | `USE {db}` 在 sqlx 连接池的行为 | P1 | 待验证 |
| 8 | 跨库 SELECT (`SELECT * FROM shared_db.table`) | P1 | 待验证 |
| 9 | 每 account 最大 database 数量 | P2 | 待验证 |
| 10 | 大量 DB (>1000) 时 `SHOW DATABASES` 性能 | P2 | 待验证 |
| 11 | `SHOW SNAPSHOTS` / `list_snapshots()` 是否能稳定过滤到当前 `user_db` | P1 | 待验证 |
| 12 | `SHOW TABLES LIKE 'br_%'` / `information_schema` 是否适合做 branch 对账 | P1 | 待验证 |

---

## 附录 A: 改造收益总结

| 维度 | 改造前 | 改造后 |
|------|--------|--------|
| **数据隔离** | 行级 (WHERE user_id) | 物理级（独立 DB） |
| **Rollback 安全性** | 摧毁所有用户 | 仅影响当前用户 |
| **Snapshot 粒度** | Account 级 | Database 级 |
| **Branch 命名** | UUID 前缀防冲突 | 天然隔离 |
| **用户删除** | 多表 DELETE WHERE | `DROP DATABASE` |
| **审计合规** | 逻辑隔离 | 物理隔离（更强） |
| **备份恢复** | 全量或逐行 | 按用户库独立 |
| **查询性能** | 大表 + index 过滤 | 小表精准查询 |
| **Git-for-Data** | 需小心使用 | 安全开箱即用 |

## 附录 B: 代码位置速查

| 组件 | 文件 | 关键行号 |
|------|------|---------|
| SqlMemoryStore | `memoria-storage/src/store.rs` | L447-628 (struct + connect) |
| active_table() | `memoria-storage/src/store.rs` | L1530-1571 |
| Safety snapshot | `memoria-storage/src/store.rs` | L1348-1406 |
| Schema migration | `memoria-storage/src/store.rs` | L662-1200 |
| Graph store | `memoria-storage/src/graph/store.rs` | L67-154 |
| GitForDataService | `memoria-git/src/service.rs` | L46-283 |
| Snapshot create | `memoria-git/src/service.rs` | L65-75 |
| Restore (rollback) | `memoria-git/src/service.rs` | L114-147 |
| Branch operations | `memoria-git/src/service.rs` | L151-264 |
| MCP git tools | `memoria-mcp/src/git_tools.rs` | L304+ (dispatch) |
| memory_rollback | `memoria-mcp/src/git_tools.rs` | L414-427 |
| memory_branch | `memoria-mcp/src/git_tools.rs` | L429-503 |
| memory_merge | `memoria-mcp/src/git_tools.rs` | L552-713 |
| MCP core tools | `memoria-mcp/src/tools.rs` | L151-1166 |
| MCP dispatch | `memoria-mcp/src/server.rs` | L230-291 |
| REST routes | `memoria-api/src/routes/*.rs` | various |
| Metrics | `memoria-api/src/routes/metrics.rs` | L1-301 |
| Admin | `memoria-api/src/routes/admin.rs` | L1-360 |
| Auth | `memoria-api/src/routes/auth.rs` | L1-327 |
| Config | `memoria-service/src/config.rs` | L32-144 |
| Governance scheduler | `memoria-service/src/governance.rs` | L332-454 |
| MemoryService init | `memoria-service/src/service.rs` | L377-475 |
| EditLogBuffer | `memoria-service/src/service.rs` | L95-267 |
| CLI entry | `memoria-cli/src/main.rs` | L2898-3024 |
| CLI serve | `memoria-cli/src/main.rs` | L354-409 |
| CLI mcp | `memoria-cli/src/main.rs` | L413-522 |
| CLI init | `memoria-cli/src/main.rs` | L2204-2258 |
