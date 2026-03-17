# Graph + Entity + Edge Benchmark 设计原理

这份文档解释 `graph-entity-v1.json` benchmark 数据集的设计思路，以及每个场景如何证明 Graph 架构优于纯向量/全文检索。

---

## 核心假设

**纯向量检索的局限：**
- 只能计算文本相似度，无法理解实体关系
- 无法区分同一词语的不同含义（如"Apple"公司 vs 水果）
- 无法建立跨记忆的隐性关联（如"上海项目"→"MatrixOne"→"6001端口"）
- 无法追踪观点演变和矛盾证据

**全文检索 (BM25) 的局限：**
- 依赖关键词匹配，无法理解语义关系
- 无法处理指代消解（如"那边"指代"上海项目"）
- 无法建立多跳推理链

**Graph 架构的优势：**
- **实体层 (mem_entities)**: 统一标识和消歧
- **边层 (memory_graph_edges)**: 显式关系（entity_link, temporal, causal, association）
- **传播层 (SpreadingActivation)**: 多跳推理和上下文扩散
- **观点层 (opinion evolution)**: 证据积累和置信度追踪

---

## 场景详解

### GRAPH-001: Entity-anchored retrieval beats vector ambiguity
**核心能力**: 实体消歧

**为什么纯向量会失败：**
- 查询"上海那边又出问题了"向量化后，与"上海旅行攻略"的相似度可能很高
- 两者都包含"上海"和"问题/攻略"等词
- 向量无法区分"上海的项目"vs"上海的旅行"

**Graph 如何解决：**
```
查询 → 提取实体"上海" → 反向查找 mem_memory_entity_links
→ 发现"上海"关联到"Docker部署"记忆，而非"旅行攻略"
→ SpreadingActivation 从"上海"实体节点传播
→ 激活相关技术记忆，抑制无关记忆
```

**关键代码路径：**
- `entity_extractor.extract_entities_lightweight()` - 提取"上海"
- `graph_store.get_memories_by_entity()` - 反向查找
- `retriever._entity_recall()` - 注入激活锚点

---

### GRAPH-002: Multi-hop reasoning via entity bridge
**核心能力**: 多跳推理

**为什么纯向量会失败：**
- 查询"部署问题怎么解决？"不会直接匹配"6001端口"
- 两者文本相似度低
- 但"6001端口"是解决部署问题的关键

**Graph 如何解决：**
```
"部署问题" → 匹配"部署失败"记忆 → 通过 entity_link 边连接到"MatrixOne"实体
→ "MatrixOne"实体连接到"6001端口"记忆
→ 2-hop 传播后，"6001端口"被激活
```

**关键机制：**
- `SpreadingActivation.propagate(iterations=3)` - 多轮传播
- `EdgeType.ENTITY_LINK` - 实体关联边
- `INHIBITION_BETA = 0.15` - 防止过度扩散

---

### GRAPH-003: Entity disambiguation: same name, different contexts
**核心能力**: 同名实体消歧

**为什么纯向量会失败：**
- "Apple"公司和"Apple"水果的向量表示可能相似
- 都包含"Apple"这个词，上下文词（"公司"/"水果"）可能被忽略
- 返回结果混合，用户得到无关信息

**Graph 如何解决：**
```
"Apple 的新产品" → 提取"Apple" → 查找实体
→ 发现两个"Apple"实体（company vs fruit）
→ "新产品"上下文激活 tech 相关实体
→ 通过 entity_type 和关联边过滤，选择 company 实体
```

**关键字段：**
- `mem_entities.entity_type` - tech/fruit/location/person
- `GraphNodeData.entity_type` - 节点类型标记

---

### GRAPH-004: Temporal chain reconstruction
**核心能力**: 时序链重建

**为什么纯向量会失败：**
- "项目做到哪一步了？"需要按时间顺序返回 Step 1-4
- 向量检索返回的是相似度排序，不是时间顺序
- 可能遗漏中间步骤或顺序错乱

**Graph 如何解决：**
```
查询 → 激活所有"Step X"节点 → temporal 边连接时序
→ SpreadingActivation 保留时序关系
→ 返回按 temporal 边组织的结果
```

**关键边类型：**
- `EdgeType.TEMPORAL` - 时序关联

---

### GRAPH-005: Cross-session entity persistence
**核心能力**: 跨会话实体追踪

**为什么纯向量会失败：**
- "DeepSeek"在不同会话中以不同上下文出现
- 纯向量分别处理每次提及，无法建立统一视图
- 用户问"DeepSeek 怎么样？"只能返回最近一次的提及

**Graph 如何解决：**
```
所有"DeepSeek"提及 → 连接到同一个"DeepSeek"实体节点
→ 实体节点作为 hub，连接所有相关记忆
→ 查询时从实体节点传播，聚合所有信息
```

---

### GRAPH-006: Conflicting evidence via opinion evolution
**核心能力**: 观点演变追踪

**为什么纯向量会失败：**
- "适合我们的项目"和"暂时不使用"向量相似度可能都很高
- 都包含"RSC"、"项目"等关键词
- 无法判断哪个是最终结论

**Graph 如何解决：**
```
初始观点 → Scene 节点 (confidence=0.7)
新证据 → OpinionEvolver.evaluate_evidence()
→ 矛盾证据降低 confidence (delta=-0.12)
→ 低于 quarantine_threshold (0.18) 后停用
→ 最终只返回高 confidence 的观点
```

**关键机制：**
- `opinion_supporting_delta = +0.05`
- `opinion_contradicting_delta = -0.12`
- `opinion_quarantine_threshold = 0.18`

---

### GRAPH-007: Tech stack disambiguation via entity types
**核心能力**: 实体类型消歧

**为什么纯向量会失败：**
- "Go"作为语言和动词"go"的向量表示可能混淆
- 上下文不足时无法区分

**Graph 如何解决：**
```
"Go 的特点" → 提取"Go" → 查找实体
→ entity_type='tech' 的"Go"实体被激活
→ entity_type 过滤排除非技术语境
```

---

### GRAPH-008: Scene synthesis from episodic memories
**核心能力**: 模式合成

**为什么纯向量会失败：**
- 多个 episodic 记忆（周一、周三、周五）分散存储
- 向量检索返回原始片段，没有合成总结
- 用户需要阅读多条记忆才能理解全貌

**Graph 如何解决：**
```
多个 episodic 节点 → ReflectionEngine.reflect()
→ LLM 合成 scene 节点："搜索优化经验：缓存是性能优化的有效手段"
→ Scene 节点通过 source_nodes 链接到原始 episodic
→ 查询返回合成后的 scene，而非原始片段
```

---

### GRAPH-009: Person entity linking for expertise routing
**核心能力**: 人员专长路由

**为什么纯向量会失败：**
- "前端性能问题应该找谁？"不会直接匹配"@Alice"
- 需要理解"前端"→"Alice"的关联

**Graph 如何解决：**
```
"前端性能" → 提取"前端" → 查找相关实体
→ "@Alice"实体通过 entity_link 连接到"前端"
→ 激活传播找到正确的人
```

---

### GRAPH-010: Location-based memory clustering
**核心能力**: 地理位置聚类

**为什么纯向量会失败：**
- "北京办公室"和"上海办公室"向量相似度高
- 都包含"办公室"，难以区分地点

**Graph 如何解决：**
```
"北京办公室" → 提取"北京"location 实体
→ 只激活与"北京"实体关联的记忆
→ "上海"相关记忆被抑制
```

---

## 运行 Benchmark

```bash
# Run benchmark
memoria benchmark --api-url http://localhost:8100 --token sk-... --dataset graph-entity-v1
```

---

## 预期结果

| 场景 | 纯向量 | 全文 | Graph |
|------|--------|------|-------|
| GRAPH-001 | ❌ 混合旅行和技术 | ❌ 关键词匹配失败 | ✅ 实体消歧 |
| GRAPH-002 | ❌ 漏掉 6001 端口 | ❌ 无关键词匹配 | ✅ 2-hop 推理 |
| GRAPH-003 | ❌ 公司和水果混淆 | ❌ 无法消歧 | ✅ entity_type 过滤 |
| GRAPH-004 | ❌ 顺序错乱 | ❌ 顺序错乱 | ✅ temporal 边 |
| GRAPH-005 | ❌ 信息分散 | ❌ 信息分散 | ✅ 实体聚合 |
| GRAPH-006 | ❌ 矛盾观点并存 | ❌ 矛盾观点并存 | ✅ opinion evolution |
| GRAPH-007 | ❌ 语言/动词混淆 | ❌ 无法区分 | ✅ entity_type |
| GRAPH-008 | ❌ 返回原始片段 | ❌ 返回原始片段 | ✅ scene 合成 |
| GRAPH-009 | ❌ 无法路由 | ❌ 无法路由 | ✅ 人员-专长关联 |
| GRAPH-010 | ❌ 地点混淆 | ❌ 地点混淆 | ✅ location 实体聚类 |

---

## 总结

这 10 个场景覆盖了 Graph 架构的核心优势：

1. **实体层**: 消歧、聚类、统一标识
2. **边层**: 关系推理、时序、因果
3. **传播层**: 多跳推理、上下文扩散
4. **观点层**: 证据积累、矛盾检测

纯向量和全文检索在这些场景下会失败，因为它们缺乏显式的结构和关系表示。
