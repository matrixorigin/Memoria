# V2 多级缓存设计

## 实现状态

- **当前实现（已落地）**：`MemoryV2Store` 目前仅包含 `pool` 与 `embedding_dim` 字段，未内置 `family_cache` / `content_cache`。
- **本文状态（提案）**：L1/L2/L3 为目标架构设计，用于指导后续增量实现，不代表当前代码已具备对应字段或行为。
- **已验证事实**：`ensure_user_tables` 在多条路径被重复调用，存在可优化空间；本文不再给出未经稳定基线支持的固定次数结论。

## 背景与动机

当前 `MemoryV2Store` 存在两个主要的重复计算热点：

1. **`ensure_user_tables`**：每次操作（remember / recall / jobs / feedback 等）都会执行一组 `CREATE TABLE IF NOT EXISTS` DDL 语句，即使 table family 已经存在，是高频冗余 I/O 来源。
2. **热点 memory 的 abstract / overview**：高频 recall 用户的顶部 memory 内容会被反复从 DB 读取，没有本地缓存。

> 注：Redis 依赖属于架构级变化，应作为可选层，不能成为运行时必须项。

---

## 目标

- 消除 `ensure_user_tables` 的重复 DDL round-trip
- 为热点 memory 内容提供进程内短生命周期缓存
- 不引入必须依赖（Redis 可选）
- 缓存失效逻辑简单、不影响数据一致性

---

## 设计方案

### L1：`family_cache` —— user table family 初始化标记

> 提案字段：以下结构体字段为设计草案，当前代码尚未包含该字段。

```rust
use dashmap::DashMap;
use std::sync::Arc;

pub struct MemoryV2Store {
    pool: MySqlPool,
    embedding_dim: usize,
    /// 已完成 ensure_user_tables 的用户集合；value = table family suffix
    family_cache: Arc<DashMap<String, String>>,
}
```

**逻辑**：
1. `ensure_user_tables(user_id)` 先检查 `family_cache.contains_key(user_id)`。
2. 若命中，直接返回缓存的 `MemoryV2TableFamily`，跳过所有 DDL。
3. 若未命中，执行 13 条建表语句后写入 `family_cache`。

**失效策略**：
- 进程重启时自动清空（in-memory map）。
- 如果某用户 table family 因异常被删除，可提供 `invalidate_family_cache(user_id)` 方法供 admin 调用。
- **不需要 TTL**：table family 是持久结构，一旦创建就不会消失（除非 admin 手动删表）。

**并发安全**：
- `DashMap` 是分片锁，支持并发读写。
- 第一次初始化可能多个协程同时进入 DDL 路径，但 `CREATE TABLE IF NOT EXISTS` 是幂等的，无副作用。

### L2：`content_cache` —— 热点 memory 内容缓存（可选）

> 提案字段：以下结构体字段与缓存类型为设计草案，当前代码尚未包含该字段。

```rust
use lru::LruCache;
use tokio::sync::RwLock;
use std::num::NonZeroUsize;

const CONTENT_CACHE_CAPACITY: usize = 512;
const CONTENT_CACHE_TTL_SECS: u64 = 300;

#[derive(Clone)]
pub struct CachedMemoryContent {
    pub abstract_text: Option<String>,
    pub overview: Option<String>,
    pub expires_at: std::time::Instant,
}

pub struct MemoryV2Store {
    // ...
    /// key = "{user_suffix}:{memory_id}"
    content_cache: Arc<RwLock<LruCache<String, CachedMemoryContent>>>,
}
```

**失效策略**：
- **TTL = 5 分钟**：recall 场景对轻微延迟可接受，但 abstract/overview 在 job 完成后会更新，5 分钟 TTL 平衡了一致性和性能。
- `remember` / `mark_job_done` 路径主动 `invalidate(memory_id)` 以立即生效。
- LRU 容量 512 条，内存占用约 2–5 MB（取决于 abstract 长度）。

**命中路径**（recall 时）：
```
recall_memories()
  → 先查 content_cache
  → 命中则跳过 SELECT abstract,overview FROM idx_table WHERE memory_id=?
  → 未命中则从 DB 读取后写入缓存
```

### L3：Redis 缓存（可选，未来扩展）

| 特性 | L1 family_cache | L2 content_cache | L3 Redis |
|------|----------------|-----------------|---------|
| 作用域 | 进程内 | 进程内 | 跨进程/跨实例 |
| 粒度 | user init 状态 | memory content | 任意 |
| 依赖 | 无 | 无 | Redis |
| 失效 | 重启 | TTL + 主动 | TTL + 主动 |
| 优先级 | **Tier 1 立即实现** | Tier 2 | Tier 3 |

Redis 仅在多实例水平扩展时才有必要引入，单实例部署下 L1 + L2 已足够。若引入 Redis，建议：
- 通过 `MEMORIA_REDIS_URL` 环境变量控制，不设置则降级到 L2。
- 只缓存 overview/abstract（不缓存 embedding vector，避免内存爆炸）。
- Key 格式：`memoria:v2:mem:{user_suffix}:{memory_id}:abstract`，TTL 10 分钟。

---

## 实现步骤

1. 添加 `dashmap` 到 `memoria-storage/Cargo.toml`。
2. 在 `MemoryV2Store::new()` 初始化 `family_cache: Arc::new(DashMap::new())`。
3. 改造 `ensure_user_tables`：检查缓存 → 命中直接返回 → 未命中建表后写缓存。
4. （可选 Tier 2）添加 `lru` crate，初始化 `content_cache`，在 recall / job 完成路径接入。

---

## 风险与注意事项

- `family_cache` 不跨进程，多实例部署时每个实例独立热身，无一致性问题。
- `content_cache` 在极端情况下可能返回过期的 abstract（job 完成后 5 分钟内），属于可接受的最终一致性。
- 不缓存 embedding vector（体积大，且 MatrixOne 向量索引已有内部缓存）。
