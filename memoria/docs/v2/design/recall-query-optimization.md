# V2 Recall 查询优化设计

## 实现状态

- ✅ 已实现：`/v2/memory/recall` 调用 `store.recall`，在存储层并发执行 `fulltext / vector / entity` 三路候选检索（`tokio::join!`）。
- ✅ 已实现：三路检索 SQL 已在查询阶段 `JOIN heads + index_docs/content_versions`，直接返回 `abstract/overview` 等内容字段，不是“先搜 ID 再单独查内容”。
- ✅ 已实现：候选合并后会批量补充元数据（access_count、feedback、link_count、focus、tag focus），并在 `with_links=true` 时批量加载 link refs。
- ❌ 未实现：`batch_recall` API 及“多 query 共享 embedding / 内容读取”的链路。
- ⚠️ 本文以下优化均为**候选方案**，需先基于 profiling 证明瓶颈后再实施。

## 背景与动机

当前 recall 主路径（仓库现状）大致为：

1. API 层先调用 embedding 服务得到 `query_embedding`。
2. `store.recall` 并发执行 `search_fulltext_candidates / search_vector_candidates / search_entity_candidates`。
3. 三路结果按 `memory_id` 合并，并做统一排序打分。
4. 批量补充候选元数据（focus、feedback、link_count 等）。
5. `expand_links=true` 时，对种子候选做链接扩展并回填候选。
6. `with_links=true` 时，对最终返回集合批量查询链接引用。

因此，优化重点不应假设“内容字段独立二次读取”，而应聚焦：高并发下查询次数、metadata 批量查询成本、以及链接扩展路径的稳定性。

---

## 目标

- 在不破坏现有可读性的前提下，降低 recall 高频场景的尾延迟与连接压力。
- 为未来并行能力增强（尤其是批量 recall）预留可验证的优化路径。
- 以 profiling 驱动优化顺序，避免过早 SQL 复杂化。

---

## 候选优化一：合并 metadata 批量查询（需 profiling 证明）

`populate_recall_candidate_metadata` 当前会分别查询 access/feedback/link_count/focus/tag-focus。  
候选方向：按实际热点把其中可合并项做 JOIN 或物化视图，减少小查询数量。

**预期收益**：降低高 QPS 时连接与往返开销。  
**风险**：SQL 可维护性下降，且可能引入过度 JOIN 导致慢查询。

---

## 候选优化二：links 扩展链路的分层批处理

`expand_links=true` 时已做种子批量加载，但在更高 fanout / 深度场景仍可能放大查询成本。  
候选方向：继续约束扩展预算（seed 数、深度、token 预算），并基于 trace 数据评估是否需要进一步批处理策略。

**预期收益**：避免并发场景下的扩展级联抖动。  
**风险**：过度限制会影响召回质量。

---

## 候选优化三：`batch_recall` 共享读取路径（未实现）

> **状态：提案，当前仓库未实现 `batch_recall` 端点/存储接口。**

若后续新增 `POST /v2/memory/batch-recall`，多个 query 可考虑：
1. 批量调用 embedding API（单次 HTTP 请求，多个文本）。
2. 并发执行多路向量/关键词/entity 检索。
3. 合并所有 query 命中的 `memory_id` 并集后，共享 metadata/links 读取与组装。

**预期收益**：当 query 间命中集合重叠时，显著降低重复读取。  
**约束**：需验证 MatrixOne 在并发 recall 子查询下的稳定性与限流策略。

---

## 优先级建议

| 候选项 | 收益 | 实现复杂度 | 建议优先级 |
|-------|------|----------|-----------|
| metadata 批量查询整合 | 中 | 中 | 中 |
| `batch_recall` 共享路径（未实现） | 高（batch 场景） | 中-高 | 高（若立项） |
| links 扩展预算与批处理增强 | 中 | 中 | 中 |

---

## 实施前置条件

1. **先做 profiling**：用 tracing + 慢查询日志定位真实瓶颈。
2. **先做安全演进**：优先保证并行稳定性（限流、超时、回退）再做激进优化。
3. **回归验证**：优化改动需覆盖 recall/links 相关回归测试后再合并。
