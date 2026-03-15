# Episodic Memory 改进提案

> **状态**: 设计评审阶段  
> **作者**: Memoria Team  
> **日期**: 2026-03-15  
> **关联**: Phase 1.4 (Zero-Friction Capture)  
> **最后修订**: 2026-03-15 (简化 MVP，明确边界)

---

## 📝 修订说明（2026-03-15）

**核心调整**：
1. ✅ 明确 episodic vs working 的差异（粒度、生命周期、生成方式）
2. ✅ 简化 Phase 1 范围：仅手动触发 + full 模式 + 默认异步（可选同步）
3. ✅ 添加不适用场景（短会话、纯查询、隐私敏感）
4. ✅ 明确自动合并的冲突处理规则（保留历史状态）
5. ✅ 调整成本预算（Phase 1: $0.25/天，Phase 2: $0.10/天）
6. ✅ 澄清与 Graph Layer 的关系（Phase 1-2 不涉及 Graph）

**第二轮优化（采纳反馈）**：
7. ✅ 明确 Phase 1 API：仅允许 `mode=full`，lightweight 在 Phase 2 开放
8. ✅ 统一异步策略：默认异步（202 + 后台任务），可选同步（`?sync=true`）
9. ✅ 降低 schema 成本：Phase 1 在 `metadata` JSON 字段存储，无需新表
10. ✅ 敏感内容过滤：复用 memory_store 的过滤策略，检测到敏感信息自动设 `scope="session"`
11. ✅ 检索权重配置化：避免硬编码 0.6/0.4，改为配置项 `retrieval_weights`
12. ✅ 补充核心指标：episodic 命中率（>50%）、用户回忆成功率（>60%）

**第三轮优化（可行性补充）**：
13. ✅ 标注 Phase 2 接口：`/v1/observe/batch` 标注"非 Phase 1 范围"
14. ✅ 任务轮询接口规范：定义 `/v1/tasks/{id}` 最小字段表（status, result, error）
15. ✅ 延迟表述修正：改为"目标"而非"实际"，避免与验证阶段混淆
16. ✅ no_episodic 标记落点：session metadata 字段，通过 `POST /v1/sessions/{id}/metadata` 设置
17. ✅ 输入截断策略：最近 200 条消息或 16K tokens，返回 `truncated: true` 标记
18. ✅ Embedding 可选化：`generate_embedding: false` 参数，支持延迟生成（lazy embedding）
19. ✅ 配置层级说明：系统默认 → 用户级覆盖 → 请求级覆盖（优先级最高）

**风险缓解**：
- Token 成本：Phase 1 手动触发，Phase 2 添加速率限制
- 存储膨胀：Phase 1 复用现有表，Phase 2 自动合并，Phase 3 embedding 压缩
- 质量风险：Phase 1 人工审核，用户可编辑/删除，收集反馈指标

---

## 1. 背景与动机

### 1.1 当前问题

现有记忆类型无法有效回答"之前讨论了什么"类问题：

| 类型 | 问题 |
|------|------|
| `working` | 仅当前 session 可见，细粒度原始事件，无法跨会话恢复上下文 |
| `semantic` | 存储事实知识，缺少"何时发生"的时间维度和会话上下文 |
| `scene` (graph) | 用于推理，不适合快速检索会话历史 |

### 1.2 为什么不扩展 working？

**方案对比**：

| 方案 | 优点 | 缺点 |
|------|------|------|
| 给 working 添加 scope 字段 | 实现简单，复用现有逻辑 | 混淆了"原始事件"和"提炼摘要"两种语义 |
| 新增 episodic 类型 | 语义清晰，独立治理策略 | 增加类型复杂度 |

**选择 episodic 的理由**：

```
working:  "User ran pytest, 21 passed" (原始事件，细粒度，短期)
episodic: "User completed CI testing for PR #27" (提炼摘要，粗粒度，长期)
```

核心差异：
- **粒度**：working 是原始事件流，episodic 是会话级摘要
- **生命周期**：working 随任务完成清理，episodic 长期保留
- **生成方式**：working 实时记录，episodic LLM 提炼生成

### 1.2 用户场景

```
场景 1: 多会话任务恢复
  用户: "上周我在优化什么来着？"
  期望: 找到上周的 episodic 记忆
  结果: "上周你优化了 Memoria 的线程安全问题，完成了 _get_service 函数"

场景 2: 项目进度跟踪
  用户: "CI 那个 PR 后来怎么样了？"
  期望: 找到相关讨论历史
  结果: "你在 3 个 session 中讨论过 CI 优化，最后一次是 review PR #27"

场景 3: 习惯学习
  系统: "注意到你经常在周五 review 代码，今天需要吗？"
```

### 1.3 不适用场景

以下情况 **不应生成** episodic 记忆：

| 场景 | 原因 | 替代方案 |
|------|------|---------|
| 短会话（< 5 条消息） | 生成成本 > 价值 | 保留 working 记忆即可 |
| 纯查询会话（无状态变更） | 无需记录 | 不生成任何记忆 |
| 隐私敏感会话 | 用户可能不希望跨会话可见 | 添加 `no_episodic` 标记 |
| 测试/调试会话 | 临时性质，无长期价值 | 使用 memory branch 隔离 |

---

## 2. 设计方案

### 2.1 核心概念

**Episodic Memory**: 会话级事件记忆，记录"谁在什么时候做了什么"

特点：
- 跨 session 可见（与 working 不同）
- 自然语言形式（与结构化 scene 不同）
- 用于检索提示和上下文恢复（与 semantic 互补）

### 2.2 数据模型（Phase 1 简化版）

**Phase 1**：复用现有 `mem_memories` 表，无需新增专表

```python
class MemoryType(Enum):
    PROFILE = "profile"
    SEMANTIC = "semantic"
    PROCEDURAL = "procedural"
    WORKING = "working"
    TOOL_RESULT = "tool_result"
    EPISODIC = "episodic"  # 新增

# Phase 1: 在现有 metadata JSON 字段中存储
# mem_memories.metadata 示例:
{
    "topic": "CI optimization",           # 主题
    "action": "reviewed",                 # 动作
    "outcome": "completed",               # 结果状态
    "source_event_ids": ["evt_1", "evt_2"]  # 溯源
}

# 复用现有字段:
# - session_id: 来源 session
# - confidence: 置信度
# - trust_tier: 信任层级 (T1-T4)
# - observed_at: 发生时间
# - half_life_days: 衰减周期
```

**Phase 2/3**：如需高频查询优化，再考虑：
- 添加 `topic`, `action`, `outcome` 索引列
- 或创建专表 `mem_episodic` 用于结构化查询

### 2.3 生成策略

#### 触发时机（Phase 1 仅实现手动触发）

| 时机 | 模式 | 产出 | Phase |
|------|------|------|-------|
| 会话结束（手动） | full | 完整摘要 + 结构化 episodic | Phase 1 ✅ |
| 每 N 条消息 | lightweight | 3-5 条要点 | Phase 2 🔄 |
| 重要事件 | immediate | 单条关键事件 | Phase 3 🔮 |

**Phase 1 策略**：
- 用户显式调用 `POST /v1/sessions/{id}/summary`
- **仅支持 `mode=full`**（lightweight 在 Phase 2 开放）
- 默认异步返回（202 Accepted + 后台任务），可选同步（`?sync=true`，目标延迟 2-4s）
- **不自动生成**，避免成本失控
- **敏感过滤**：生成前走与 memory_store 相同的敏感内容过滤策略

**输入截断策略**（避免超长 session）：
- 最大输入：最近 200 条消息或 16K tokens（以先达到为准）
- 超长 session 自动截断，保留最近内容
- 返回 `truncated: true` 标记提示用户

**Embedding 生成策略**（降低成本与延迟）：
- Phase 1 可选：`generate_embedding: false` 参数，跳过 embedding 生成
- 延迟生成：首次检索时才生成 embedding（lazy embedding）
- 适用场景：用户仅需摘要文本，不立即检索

**no_episodic 标记**（隐私控制）：
- 位置：session metadata 字段 `{"no_episodic": true}`
- 设置方式：`POST /v1/sessions/{id}/metadata {"no_episodic": true}`
- 效果：该 session 拒绝生成 episodic，返回 403 Forbidden
- 用途：隐私敏感会话、测试调试会话

**Phase 2 优化**：
- 开放 `mode=lightweight`（实时增量）
- 速率限制：每 session 最多 3 次 lightweight 调用
- 优化异步生成 + 缓存预生成

#### 成本控制

| 模式 | Input | Output | 成本（预估） | 触发频率 | Phase 1 状态 |
|------|-------|--------|-------------|---------|-------------|
| full | 8K-16K | 500-1000 | ~$0.05 | 手动触发 | ✅ 实现 |
| lightweight | 2K-4K | 200-500 | ~$0.01 | 每 10 条消息 | ❌ Phase 2 |
| incremental | 1K-2K | 100-300 | ~$0.005 | 增量更新 | ❌ Phase 3 |

**Phase 1 成本预算**：
- 假设：每用户每天 5 个 session，每个 session 手动生成 1 次摘要
- 成本：5 × $0.05 = **$0.25/天/用户**
- 如需降低成本，可限制为"仅重要 session 生成"（用户选择）

---

## 3. API 设计

### 3.1 Session 摘要接口

```http
POST /v1/sessions/{session_id}/summary
Content-Type: application/json

{
  "mode": "full",  // Phase 1 仅支持 "full"，"lightweight" 在 Phase 2 开放
  "sync": false,   // false=异步(默认), true=同步(2-4s延迟)
  "focus_topics": ["CI", "performance"],  // 可选，聚焦主题
  "max_items": 5
}
```

**异步响应**（默认）：
```json
HTTP 202 Accepted
{
  "session_id": "sess_abc123",
  "task_id": "task_xyz789",
  "status": "processing",
  "estimated_seconds": 3
}

// 轮询状态: GET /v1/tasks/{task_id}
// 任务状态规范（最小字段表）:
{
  "task_id": "task_xyz789",
  "status": "processing" | "completed" | "failed",
  "created_at": "2026-03-15T10:30:00Z",
  "updated_at": "2026-03-15T10:30:02Z",
  "result": {  // 仅 status=completed 时存在
    "summary": "User reviewed PR #27...",
    "episodic_entries": [...]
  },
  "error": {  // 仅 status=failed 时存在
    "code": "TIMEOUT" | "LLM_ERROR" | "INVALID_SESSION",
    "message": "Session not found or empty"
  }
}
```

**同步响应**（`?sync=true`）：
```json
HTTP 200 OK
{
  "session_id": "sess_abc123",
  "summary": "User reviewed PR #27 about thread safety and fixed caching issues",
  "episodic_entries": [
    {
      "memory_id": "mem_xyz789",
      "content": "User completed thread safety review for PR #27",
      "metadata": {
        "topic": "thread safety",
        "action": "reviewed",
        "outcome": "completed"
      },
      "confidence": 0.92,
      "timestamp": "2026-03-15T10:30:00Z"
    }
  ],
  "tokens_used": 1250,
  "processing_time_ms": 2850
}
```

### 3.2 增强 Observe 接口（Phase 2 范围）

**⚠️ 非 Phase 1 范围** - 此接口在 Phase 2 实现，用于实时增量生成 episodic

```http
POST /v1/observe/batch
Content-Type: application/json

{
  "user_id": "user_123",
  "session_id": "sess_abc123",
  "events": [
    {
      "content": "User asked about Memoria CI status",
      "timestamp": "2026-03-15T10:00:00Z",
      "type": "user_message"
    },
    {
      "content": "CI passed with 21 tests",
      "timestamp": "2026-03-15T10:05:00Z", 
      "type": "tool_result"
    }
  ],
  "extract_episodic": true,
  "source_session_id": "sess_abc123"
}
```

响应：
```json
{
  "extracted": [
    {
      "memory_id": "mem_ep_001",
      "memory_type": "episodic",
      "topic": "CI status",
      "action": "checked",
      "confidence": 0.88
    }
  ]
}
```

### 3.3 检索接口增强

```http
GET /v1/memories?memory_type=episodic&session_id=sess_abc123
GET /v1/memories?memory_type=episodic&start_time=2026-03-01&end_time=2026-03-15
GET /v1/memories?query=CI&memory_type=episodic&include_cross_session=true
```

---

## 4. 检索与可见性

### 4.1 跨 Session 可见性

```
默认行为:
- EPISODIC 属于 L1 (长期记忆)
- 跨 session 可见
- 受 include_cross_session 参数控制

隐私控制:
- 添加 scope 字段: "user" | "session"
- scope="session": 仅同 session 可见
- scope="user": 跨 session 可见 (默认)

敏感内容过滤:
- episodic 生成前走与 memory_store 相同的敏感内容过滤策略
- 如检测到敏感信息（PII、密钥等），自动设置 scope="session" 或拒绝生成
- 用户可通过 no_episodic 标记禁止某 session 生成 episodic
```

### 4.2 检索增强（Phase 1 简化版）

**Phase 1 策略**：仅使用 semantic + recency，权重可配置

```python
# 配置项（避免硬编码）
EPISODIC_RETRIEVAL_WEIGHTS = {
    "semantic": 0.6,   # 语义相似度权重
    "recency": 0.4,    # 时间衰减权重
}

# 时间衰减加权 (近期 episodic 优先)
recency_boost = exp(-hours_since / half_life)

# 检索分数（简化版）
score = (
    semantic_score * EPISODIC_RETRIEVAL_WEIGHTS["semantic"] +
    recency_boost * EPISODIC_RETRIEVAL_WEIGHTS["recency"]
)
```

**Phase 2 优化**：添加主题连续性和置信度

```python
EPISODIC_RETRIEVAL_WEIGHTS = {
    "semantic": 0.4,
    "recency": 0.3,
    "confidence": 0.2,
    "topic_chain": 0.1,
}

# 主题连续性加权 (同主题 session 链)
topic_chain_boost = 1.2 if same_topic_as_current_session else 1.0

# 完整分数
score = (
    semantic_score * weights["semantic"] +
    recency_boost * weights["recency"] +
    confidence * weights["confidence"] +
    topic_chain_boost * weights["topic_chain"]
)
```

**权重调优计划**：
- Phase 1: 使用配置项（默认 0.6/0.4），可通过配置文件调整
- Phase 2: A/B 测试验证最优权重
- Phase 3: 根据用户反馈动态调整

### 4.3 可解释性

```json
{
  "explain": {
    "semantic_score": 0.85,
    "temporal_boost": 1.15,
    "topic_chain": ["sess_001", "sess_003", "sess_abc123"],
    "session_context": "3 related sessions in past week",
    "confidence_source": "extracted from 5 source messages"
  }
}
```

---

## 5. 成本控制

### 5.1 Token 成本

| 模式 | Input | Output | 成本 | 触发频率 |
|------|-------|--------|------|---------|
| lightweight | 2K-4K | 200-500 | ~$0.01 | 每 10 条消息 |
| full | 8K-16K | 500-1000 | ~$0.05 | 会话结束 |
| incremental | 1K-2K | 100-300 | ~$0.005 | 增量更新 |

**优化策略**:
- 分层摘要：level1(实时) → level2(小时) → level3(天)
- 本地模型 fallback：轻量任务用本地模型
- 缓存预生成：会话进行中后台预生成

### 5.2 存储成本

```python
# 单条 episodic 大小
{
    "memory_id": "32bytes",
    "topic/action/outcome": "~100bytes",
    "embedding": "1536×4=6KB",
    "metadata": "~200bytes"
}
# 总计: ~6.5KB

# 用量估算
# 10 sessions/day × 5 episodic = 50条/天
# 50 × 6.5KB = 325KB/天/用户
# 1000用户 × 30天 = ~10GB/月
```

**优化策略**:
- Embedding 压缩：1536-dim → 384-dim (4x 压缩)
- 分层存储：热/温/冷三级存储
- 自动合并：24h 内相似主题合并 (减少 30-50%)

### 5.3 计算成本

```python
# 批量压缩作业
COMPRESSION_SCHEDULE = {
    "light": "0 2 * * *",      # 每天凌晨
    "deep": "0 3 * * 0",       # 每周日
}

# 增量处理
last_run = get_last_compression_time()
new_memories = query_since(last_run)  # 避免全表扫描
```

---

## 6. 延迟优化

### 6.1 延迟预算

| 操作 | 目标（Phase 1） | 备注 |
|------|----------------|------|
| full 摘要（同步） | < 5s (p95) | 用户可接受的同步延迟 |
| full 摘要（异步） | < 4s (p95) | 后台任务完成时间 |
| 检索（含 episodic） | < 200ms (p95) | 与现有检索延迟一致 |
| observe 实时 | < 100ms (p95) | Phase 2 实现 |

### 6.2 优化策略

```python
# 1. 异步生成
@app.post("/v1/sessions/{id}/summary")
async def create_summary(session_id: str):
    background_tasks.add_task(generate_summary_async, session_id)
    return {"status": "processing"}  # 202 Accepted

# 2. 缓存预生成
class SummaryCache:
    def on_message_batch(self, session_id, messages):
        if len(messages) % 5 == 0:
            self.warm_cache(session_id, messages)

# 3. 流式返回
async def generate_summary_stream(session_id):
    yield {"partial": "topic: CI/CD..."}
    yield {"partial": "action: reviewed..."}
    yield {"complete": true, "summary": {...}}
```

---

## 7. 数据治理

### 7.1 自动合并策略

**Phase 1**：不自动合并，仅手动触发

**Phase 2**：添加自动合并，规则如下

```python
MERGE_WINDOW_HOURS = 24
SIMILARITY_THRESHOLD = 0.85

def should_merge(new_ep, existing):
    return (
        new_ep.topic == existing.topic and
        new_ep.action == existing.action and
        time_diff < MERGE_WINDOW_HOURS and
        semantic_similarity > SIMILARITY_THRESHOLD
    )

# 合并规则（处理冲突）
def merge_episodic(new_ep, existing):
    """
    合并策略：
    1. outcome 冲突 → 保留最新的 outcome，旧 outcome 追加到 history
    2. confidence → 取平均值
    3. source_event_ids → 合并列表
    4. observed_at → 保留最早时间，添加 updated_at 字段
    """
    return {
        "outcome": new_ep.outcome,  # 最新状态
        "outcome_history": existing.outcome_history + [existing.outcome],
        "confidence": (new_ep.confidence + existing.confidence) / 2,
        "source_event_ids": existing.source_event_ids + new_ep.source_event_ids,
        "observed_at": existing.observed_at,  # 保留最早时间
        "updated_at": new_ep.observed_at,     # 记录更新时间
    }
```

**冲突处理示例**：

```
Episodic 1 (3月10日): topic="PR #27", action="reviewed", outcome="pending"
Episodic 2 (3月12日): topic="PR #27", action="reviewed", outcome="merged"

合并后:
  topic: "PR #27"
  action: "reviewed"
  outcome: "merged"  ← 最新状态
  outcome_history: ["pending"]  ← 历史状态
  observed_at: "2026-03-10"
  updated_at: "2026-03-12"
```

### 7.2 TTL 与衰减

```python
# 复用 trust_tier 机制
EPISODIC_HALF_LIFE = {
    TrustTier.T4: 7,    # 未验证: 7天
    TrustTier.T3: 30,   # 推断: 30天
    TrustTier.T2: 90,   # 整理: 90天
    TrustTier.T1: 365,  # 验证: 1年
}

# 自动降级
# T4 → T3: confidence >= 0.8 and age >= 7 days
# T3 → T2: confidence >= 0.85 and cross_session_count >= 3
```

### 7.3 压缩与归档

```python
class EpisodicCompressor:
    def compress_session(self, session_id):
        """合并同 topic episodic，生成高层摘要"""
        
    def merge_similar(self, user_id):
        """合并相似 episodic，保留时间范围"""
        
    def archive_old(self, user_id, days=90):
        """归档旧 episodic，移除 embedding"""
```

---

## 8. 实施计划

### Phase 1: MVP (2 周)

**目标**：验证核心价值，手动触发，无自动化

- [ ] 添加 `MemoryType.EPISODIC` 到 `memoria/core/memory/types.py`
- [ ] 在 `mem_memories.metadata` JSON 字段存储 topic/action/outcome（无需新表）
- [ ] 实现 full 摘要生成（LLM 提炼会话历史）
- [ ] 添加 session 摘要接口 `POST /v1/sessions/{id}/summary`
  - 仅支持 `mode=full`（lightweight 在 Phase 2 开放）
  - 默认异步（202 + 后台任务），可选同步（`?sync=true`）
- [ ] 敏感内容过滤（复用 memory_store 的过滤策略）
- [ ] 基础检索支持（semantic + recency，权重可配置）
- [ ] 手动触发（用户调用 API），不自动生成
- [ ] 单元测试 + 集成测试

**不包含**：
- ❌ lightweight 模式（Phase 2）
- ❌ 自动触发（Phase 2）
- ❌ 自动合并（Phase 2）
- ❌ 专表或索引列（Phase 2/3 按需优化）

### Phase 2: 自动化与优化 (2 周)

**前置条件**：Phase 1 用户验证通过，确认有价值

- [ ] 添加 lightweight 模式（实时增量摘要）
- [ ] 速率限制：每 session 最多 3 次 lightweight 调用
- [ ] 改为异步生成 + 后台任务
- [ ] 缓存与预生成优化
- [ ] 批量 observe 接口
- [ ] 自动合并逻辑（含冲突处理）
- [ ] 可解释性增强（topic_chain, confidence boost）
- [ ] 监控与告警

### Phase 3: 治理与成本优化 (2 周)

- [ ] 批量压缩作业（合并相似 episodic）
- [ ] TTL 与衰减机制（基于 trust_tier）
- [ ] Embedding 压缩（1536-dim → 384-dim）
- [ ] 分层存储（热/温/冷）
- [ ] 成本分析 dashboard
- [ ] A/B 测试框架（权重调优）

---

## 9. 风险评估

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|---------|
| Token 成本超支 | 中 | 高 | Phase 1 仅手动触发 + Phase 2 添加速率限制（每 session 最多 3 次 lightweight） |
| 存储膨胀 | 中 | 中 | Phase 2 自动合并 + Phase 3 embedding 压缩（1536→384 dim） |
| 检索延迟增加 | 低 | 中 | Phase 1 简化检索策略（仅 semantic+recency）+ 索引优化 |
| 隐私泄露 | 低 | 高 | scope 控制 + user_id 隔离 + 添加 `no_episodic` 标记 |
| 质量不达标 | 中 | 高 | Phase 1 人工审核 + Phase 2 反馈循环 + 用户可编辑/删除 |
| 自动合并冲突 | 中 | 中 | Phase 1 不自动合并 + Phase 2 明确冲突处理规则（保留历史） |

---

## 10. 成功指标

### 技术指标

**Phase 1 目标**：
```
- 摘要生成延迟 p95 < 5s（同步模式）
- 异步任务完成时间 p95 < 4s
- 检索延迟 p95 < 200ms
- 成本 per user per day < $0.25（假设 5 个 session，每个手动生成 1 次）
- 存储 per user per month < 100MB（Phase 1 无压缩）
```

**Phase 2 目标**：
```
- lightweight 摘要延迟 p95 < 2s
- 成本 per user per day < $0.10（添加速率限制后）
- 存储 per user per month < 500MB
```

### 业务指标

**Phase 1 验证（核心 MVP 指标）**：
```
- episodic 命中率 > 50%（用户查询时，episodic 出现在 top-5 结果中的比例）
- 用户回忆成功率 > 60%（用户通过 episodic 成功恢复上下文的比例）
- 用户接受率 > 60%（手动触发，门槛较高）
- 摘要质量评分 > 3.5/5.0（人工评审）
- 跨 session 上下文恢复准确率 > 70%
```

**Phase 2 目标**：
```
- episodic 命中率 > 70%
- 用户回忆成功率 > 80%
- 用户接受率 > 80%（自动生成，降低门槛）
- 检索相关性提升 > 30%
- 用户满意度 > 4.0/5.0
```

**指标收集方式**：
- 命中率：检索日志分析（episodic 在结果中的排名）
- 回忆成功率：用户反馈（"这个记忆有用吗？" 👍/👎）
- 接受率：生成后用户是否编辑/删除（未删除 = 接受）
- 质量评分：随机抽样 + 人工评审

---

## 11. 附录

### 11.1 与现有架构的关系

**Tabular Layer** (`mem_memories` 表)：

```
现有类型:
├── PROFILE, SEMANTIC, PROCEDURAL  (长期知识)
├── WORKING, TOOL_RESULT          (短期上下文)

新增类型:
└── EPISODIC                      (会话级摘要)
    ├── topic, action, outcome    (metadata 字段)
    ├── session_id                (关联字段)
    └── source_event_ids          (溯源字段)
```

**Graph Layer** (`memory_graph_nodes` 表)：

```
现有节点类型:
├── ENTITY node                   (实体节点，如 "Python", "MatrixOne")
├── SCENE node                    (合成场景，如 "项目使用 Python + MatrixOne")

未来扩展（Phase 3）:
└── EPISODIC node                 (可选，用于时间线推理)
    └── 关系: Tabular EPISODIC → Graph EPISODIC node
```

**说明**：
- Graph Layer 当前 **没有** EPISODIC node 类型
- Phase 1-2 仅在 Tabular Layer 实现 episodic 记忆
- Phase 3 可选：将 episodic 记忆也加入 Graph Layer，用于时间线推理（如"这个 bug 在哪个 session 修复的？"）

**类型对比**：

| 维度 | working | episodic | semantic |
|------|---------|----------|----------|
| 粒度 | 原始事件 | 会话摘要 | 事实知识 |
| 生命周期 | 任务完成后清理 | 长期保留（90-365天） | 永久 |
| 生成方式 | 实时记录 | LLM 提炼 | 用户/agent 存储 |
| 跨会话可见 | ❌ | ✅ | ✅ |
| 时间维度 | ✅ (observed_at) | ✅ (observed_at + session_id) | ❌ |

### 11.2 配置项

```python
# MemoryGovernanceConfig 新增
episodic_config = {
    # Phase 1
    "enabled": True,
    "manual_trigger_only": True,  # Phase 1 仅手动触发
    "full_mode_timeout_seconds": 10,
    "default_async": True,  # 默认异步，可通过 ?sync=true 改为同步
    
    # 输入截断
    "max_input_messages": 200,  # 最大输入消息数
    "max_input_tokens": 16000,  # 最大输入 tokens
    
    # Embedding 生成
    "default_generate_embedding": True,  # 默认生成 embedding
    "lazy_embedding": False,  # Phase 2: 延迟生成（首次检索时）
    
    # 检索权重（可配置，避免硬编码）
    # 对应关系：MemoryGovernanceConfig.episodic_config.retrieval_weights
    # 用户覆盖：通过 API 参数 ?weights={"semantic": 0.7, "recency": 0.3}
    "retrieval_weights": {
        "semantic": 0.6,
        "recency": 0.4,
    },
    
    # 敏感内容过滤
    "sensitive_filter_enabled": True,  # 复用 memory_store 的过滤策略
    "auto_scope_on_sensitive": True,   # 检测到敏感信息时自动设为 scope="session"
    
    # Phase 2
    "lightweight_enabled": False,  # Phase 2 启用
    "lightweight_trigger_messages": 10,
    "lightweight_max_items": 5,
    "lightweight_rate_limit_per_session": 3,  # 速率限制
    
    # Phase 2 合并策略
    "auto_merge_enabled": False,  # Phase 2 启用
    "merge_window_hours": 24,
    "similarity_threshold": 0.85,
    
    # Phase 3 压缩
    "embedding_compression": False,  # Phase 3 启用
    "cache_ttl_minutes": 10,
}
```

**配置层级与覆盖关系**：
1. **系统默认**：`MemoryGovernanceConfig.episodic_config`（上述配置）
2. **用户级覆盖**：`user_config.episodic_weights = {"semantic": 0.7, "recency": 0.3}`
3. **请求级覆盖**：API 参数 `?weights={"semantic": 0.8, "recency": 0.2}`（优先级最高）

---

## 参考

- [plan.md](./plan.md) - Phase 1.4
- [architecture.md](./architecture.md) - 记忆类型设计
- [cost-model.md](./cost-model.md) - 成本模型
