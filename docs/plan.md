# Memoria Product Plan

Multi-phase plan for evolving Memoria from a memory backend into a context-aware,
proactive, and visual knowledge system.

Prerequisites: all P0/P1 fixes from v0.2.8 are complete (entity dedup, unified linking,
entity_type, edge weight by source). See `roadmap.md` for technical backlog.

---

## Phase 0A: Entity Linking 升级 (1–2 days)

**Goal:** Independent entity tables + high-efficiency reverse lookup. Make
"上海的面条店" vs "南京的面条店" work via entity-anchored retrieval.

**Current state:** Entities live as `node_type='entity'` rows in `memory_graph_nodes`,
linked via `memory_graph_edges`. This works for graph traversal but is inefficient for
the most common query pattern: "find all memories mentioning entity X". A dedicated
entity table with a direct foreign-key link to `mem_memories` enables single-join
reverse lookup without graph traversal overhead.

### Deliverables

#### 0A.1 Independent entity tables
New tables in `memoria/schema.py` (auto-created by `ensure_tables`):

```sql
CREATE TABLE IF NOT EXISTS `mem_entities` (
  `entity_id`    VARCHAR(32)  NOT NULL,
  `user_id`      VARCHAR(64)  NOT NULL,
  `name`         VARCHAR(200) NOT NULL,        -- normalized key (see rules below)
  `display_name` VARCHAR(200) DEFAULT NULL,    -- original surface form
  `entity_type`  VARCHAR(20)  NOT NULL DEFAULT 'concept', -- tech/person/location/time/project/concept
  `embedding`    VECF32({dim}) DEFAULT NULL,
  `created_at`   DATETIME(6)  NOT NULL DEFAULT NOW(),
  PRIMARY KEY (`entity_id`),
  UNIQUE KEY `uidx_entity_user_name` (`user_id`, `name`),
  KEY `idx_entity_user` (`user_id`)
);

CREATE TABLE IF NOT EXISTS `mem_memory_entity_links` (
  `memory_id`  VARCHAR(64) NOT NULL,
  `entity_id`  VARCHAR(32) NOT NULL,
  `user_id`    VARCHAR(64) NOT NULL,
  `source`     VARCHAR(10) NOT NULL DEFAULT 'regex', -- regex/llm/manual
  `weight`     FLOAT       NOT NULL DEFAULT 0.8,
  `created_at` DATETIME(6) NOT NULL DEFAULT NOW(),
  PRIMARY KEY (`memory_id`, `entity_id`),
  KEY `idx_link_user_entity` (`user_id`, `entity_id`),
  KEY `idx_link_entity_user` (`entity_id`, `user_id`)
);
```

- `mem_entities` is the canonical entity registry (deduplicated by user_id + name)
- `mem_memory_entity_links` is the many-to-many join between `mem_memories` and entities
- Graph nodes (`memory_graph_nodes` with `node_type='entity'`) continue to exist for
  activation propagation; `mem_entities.entity_id` maps 1:1 to the graph node_id
- Normalization rules for `mem_entities.name`:
  - NFKC normalize, trim whitespace, collapse internal whitespace
  - Lowercase ASCII only (Chinese is unchanged); keep punctuation stable
  - Store the original surface form into `display_name` for UI
- **Files:** `memoria/schema.py`, `memoria/core/memory/graph/graph_store.py` (dual-write: entity table + graph node)

#### 0A.1a Dual-write 事务语义与补偿
- **原子性边界:** `mem_entities` + `mem_memory_entity_links` + `memory_graph_nodes` +
  `memory_graph_edges` 四表写入在同一个 SQLAlchemy session（同事务 commit）。
  MatrixOne 单节点事务保证 all-or-nothing。
- **降级策略:** 如果 graph 写入失败（如 embedding 计算超时），entity 表写入仍然提交，
  graph 侧记入 `_pending_graph_sync` 队列（已有机制），下次 governance 补偿。
  即：entity 表是 source of truth，graph 是最终一致。
- **补偿扫描:** 新增 `repair_entity_graph_consistency(user_id)` 方法：
  按 user_id 扫描 `mem_entities` 与 `memory_graph_nodes(node_type='entity')` 的差集，
  补建缺失的 graph node/edge。作为 governance 的一个可选步骤运行。
- **Files:** `memoria/core/memory/graph/graph_store.py`, `memoria/core/memory/graph/consolidation.py`

#### 0A.1b Backfill for existing graph entities
- One-time backfill: scan existing `memory_graph_nodes` where `node_type='entity'`
  and upsert into `mem_entities`, then backfill `mem_memory_entity_links` from
  `memory_graph_edges` where `edge_type='entity_link'` and `source_id` is a semantic node
- Must be idempotent; safe to re-run
- **Files:** `memoria/core/memory/strategy/activation_index.py`, `memoria/core/memory/graph/graph_store.py`

#### 0A.2 Entity-anchored reverse lookup
High-efficiency retrieval path — single JOIN, no graph traversal:

```sql
SELECT m.* FROM mem_memory_entity_links l
JOIN mem_memories m ON l.memory_id = m.memory_id
WHERE l.entity_id = :entity_id AND l.user_id = :user_id AND m.is_active = 1
ORDER BY l.weight DESC, m.created_at DESC
```

- Add `get_memories_by_entity(entity_id)` to `graph_store.py`
- In `ActivationRetriever`: when query contains a recognized entity, run reverse
  lookup first, then boost those memories' activation scores by ×1.8
- Fallback: if no entity match, pure semantic retrieval (no regression)
- **Files:** `memoria/core/memory/graph/graph_store.py`, `memoria/core/memory/graph/retriever.py`

#### 0A.3 Entity extraction 增强
- Add multilingual patterns to `entity_extractor.py`:
  - Chinese city/location names (top-50 cities)
  - Time expressions (昨天, 上周, 2026年3月, etc.)
  - Quoted strings and `backtick` terms as project/product names
- Keep lightweight — regex only, no LLM on hot path
- On store: extract entities → upsert into `mem_entities` → insert links into
  `mem_memory_entity_links` → create/reuse graph node
- **Files:** `memoria/core/memory/graph/entity_extractor.py`, `memoria/core/memory/graph/graph_builder.py`

### Acceptance Criteria
- Query "上海的面条店" retrieves Shanghai noodle memories, not Nanjing ones
- Reverse lookup via `mem_memory_entity_links` is single-JOIN (no graph walk)
- Entity boost ×1.8 for context-matching entities in retrieval scoring
- Entity extraction covers top-50 Chinese cities + common time expressions

### Milestone M0A (Day 2)
Entity-anchored retrieval live. "Where did I eat in Shanghai?" works.

---

## Phase 0B: Scene Nodes + Opinion Evolution 完整化 (2–3 days)

**Goal:** Three-stage Reflector + more aggressive opinion deltas + trust tier
promotion chain (T4 → T3 → T2).

**Current state:** `ReflectionEngine` does 2 stages (candidates → LLM synthesis).
`OpinionEvolver` exists with configurable deltas. Scene nodes stored as
`node_type='scene'` in graph. Quarantine threshold is 0.3.

### Deliverables

#### 0B.1 Three-stage Reflector
Upgrade `ReflectionEngine.reflect()`:

1. **Collect high-activation subgraph** — gather activated neighborhood around
   candidate cluster via 1-iteration spreading activation (already implemented
   in `candidates.py`; wire it into the engine)
2. **Synthesize scene** — LLM synthesis with richer context from step 1
   (current: only candidate memories; new: + neighbor nodes + entity context
   from `mem_entities` table)
3. **Create/Update scene node + opinion evolution** — persist scene node,
   then immediately run opinion evolution against existing scenes to detect
   reinforcement or contradiction. If new scene contradicts an existing scene
   with high confidence, flag for user review instead of auto-creating.

- **Files:** `memoria/core/memory/reflection/engine.py`, `memoria/core/memory/graph/candidates.py`, `memoria/core/memory/graph/opinion.py`

#### 0B.2 Opinion evolution 调参
Pin these as defaults (configurable via `OpinionEvolver.__init__`):

| Parameter | Old | New |
|-----------|-----|-----|
| Supporting delta | configurable | **+0.05** (cap at 0.95) |
| Contradicting delta | configurable | **-0.12** |
| Quarantine threshold | 0.3 | **0.18** |
| Promotion threshold (T4→T3) | 0.8 | 0.8 (unchanged) |

- More aggressive contradicting delta (-0.12 vs previous) means scenes that
  accumulate 2-3 contradictions get quarantined faster
- Lower quarantine threshold (0.18) gives scenes slightly more room before
  being deactivated
- **Files:** `memoria/core/memory/reflection/opinion.py`, `memoria/core/memory/graph/opinion.py`

#### 0B.3 Trust tier promotion chain
Add full promotion/demotion rules:

| Transition | Condition |
|------------|-----------|
| T4 → T3 | confidence ≥ 0.8 AND age ≥ 7 days |
| T3 → T2 | confidence ≥ 0.85 AND age ≥ 30 days AND cross_session_count ≥ 3 |
| T3 → T4 (demotion) | age ≥ 60 days AND confidence < 0.8 |
| T2 → T3 (demotion) | confidence < 0.7 after contradicting evidence |

- Currently only T4→T3 promotion exists; add T3→T2 and demotion paths
- Run as part of `consolidation.py` trust tier lifecycle
- **Files:** `memoria/core/memory/graph/consolidation.py`, `memoria/core/memory/reflection/opinion.py`

#### 0B.4 Scene node enrichment
- Store entity summary in scene node content (which entities are covered)
- Track `source_count` — number of source memories that contributed
- Use source_count as a signal in importance scoring
- **Files:** `memoria/core/memory/graph/graph_builder.py`, `memoria/core/memory/reflection/engine.py`, `memoria/core/memory/reflection/importance.py`

### Acceptance Criteria
- Reflection produces richer scene summaries (include entity + neighbor context)
- Contradicting evidence quarantines scenes faster (-0.12 delta)
- Scene nodes with < 0.18 confidence are quarantined
- T3→T2 promotion works for well-established cross-session scenes

### Milestone M0B (Day 4–5)
3-stage reflection + tuned opinion evolution + trust tier chain live.

---

## Phase 0C: Lateral Inhibition + Spreading Activation 补全 (2–3 days)

**Goal:** Add Dual Trigger (BM25 + vector) as activation anchor initialization.
Unify entity graph and activation graph.

**Current state:** `SpreadingActivation` already implements lateral inhibition
(β=0.15), 3-round propagation, task-specific edge boosts. Anchor selection is
vector-only (cosine similarity top-K). No lexical/BM25 anchor path.

### Deliverables

#### 0C.1 Dual-trigger anchor initialization
Add BM25 (fulltext) + vector dual-trigger at the top of
`ActivationRetriever.retrieve()`:

```python
# Dual Trigger — two independent anchor sources
anchors_lex = fulltext_search(query)      # MatrixOne fulltext index on graph nodes (ft_graph_content)
anchors_sem = vector_search(embedding)    # IVF-flat cosine similarity
anchors = merge_and_dedup(anchors_lex, anchors_sem)
```

- **BM25 path:** use MatrixOne's `MATCH ... AGAINST` on `memory_graph_nodes.content`
  fulltext index. Returns nodes where exact terms match (good for entity names,
  code identifiers, proper nouns that vector search may miss).
- **Vector path:** existing cosine similarity top-K (unchanged).
- **Merge:** union both sets, dedup by node_id, take max score per node.
  BM25 anchors get initial activation 0.7, vector anchors get 1.0.
- This ensures "MatrixOne" as a query term finds the exact entity node via BM25
  even if the embedding doesn't rank it highest.
- **Files:** `memoria/core/memory/graph/retriever.py`, `memoria/core/memory/graph/graph_store.py` (add fulltext search method)

#### 0C.2 Entity-boosted edge propagation
- Entity_link edges already have weight 0.8–1.2 (source-dependent)
- Add context-aware entity boost: when query entities match a node's linked
  entities, boost that node's activation by ×1.8 (e.g., query mentions "上海",
  memories linked to the "上海" entity get ×1.8)
- Reuse existing task-specific boost infrastructure in `activation.py`
- **Files:** `memoria/core/memory/graph/activation.py`, `memoria/core/memory/graph/types.py`

#### 0C.3 Activation graph = entity graph (unification)
- No schema change needed — entity nodes are already in `memory_graph_nodes`,
  entity edges in `memory_graph_edges`
- Ensure `ActivationRetriever` treats entity nodes as full activation
  participants (not filtered out in anchor selection)
- Raise entity node type weight from 0.6 → 0.8 so entity-linked results
  aren't systematically deprioritized
- Entity nodes found via `mem_entities` table (Phase 0A) feed directly into
  the activation graph as anchor seeds
- **Files:** `memoria/core/memory/graph/retriever.py`, `memoria/core/memory/graph/types.py`

### Acceptance Criteria
- Dual-trigger (BM25 + vector) is default anchor initialization for `activation:v1`
- BM25 catches exact-match entities that vector search misses
- Entity boost ×1.8 for context-matching entities
- Lateral inhibition (β=0.15) prevents entity flooding
- No regression on non-entity queries (pure semantic still works)

### Milestone M0C (Day 7)
Dual-trigger activation with BM25 + vector anchors live.

---

## Phase 0 共通：权重/阈值配置化

Phase 0A–0C 引入了多个数值常量，落地时统一遵循以下规则，避免调参改代码：

### 配置入口
所有权重/阈值通过 `MemoryGraphConfig` dataclass 集中管理（`memoria/core/memory/config.py`），
支持三级覆盖：代码默认值 → 环境变量 → `mem_user_config` 表 per-user 覆盖。

### 常量清单

| 常量 | 默认值 | 环境变量 | 所属 Phase | 说明 |
|------|--------|----------|-----------|------|
| `ENTITY_BOOST` | 1.8 | `MEMORIA_ENTITY_BOOST` | 0A | 命中 entity 的 memory activation 倍率 |
| `BM25_INITIAL_ACTIVATION` | 0.7 | `MEMORIA_BM25_INIT_ACT` | 0C | BM25 anchor 初始激活值 |
| `VECTOR_INITIAL_ACTIVATION` | 1.0 | `MEMORIA_VEC_INIT_ACT` | 0C | Vector anchor 初始激活值 |
| `OPINION_SUPPORT_DELTA` | +0.05 | `MEMORIA_OPINION_SUPPORT` | 0B | 支持证据 confidence 增量 |
| `OPINION_CONTRADICT_DELTA` | -0.12 | `MEMORIA_OPINION_CONTRA` | 0B | 矛盾证据 confidence 减量 |
| `OPINION_QUARANTINE_THRESHOLD` | 0.18 | `MEMORIA_QUARANTINE_THR` | 0B | 低于此值 quarantine |
| `OPINION_CONFIDENCE_CAP` | 0.95 | `MEMORIA_CONFIDENCE_CAP` | 0B | confidence 上限 |
| `ENTITY_NODE_TYPE_WEIGHT` | 0.8 | `MEMORIA_ENTITY_NODE_W` | 0C | entity node 在 retrieval 中的类型权重 |
| `INHIBITION_BETA` | 0.15 | `MEMORIA_INHIBIT_BETA` | 0C | lateral inhibition 系数 |

### A/B 与回滚
- `mem_user_config` 表已有 per-user key-value 存储，用于 A/B 分组：
  写入 `{"entity_boost": 2.0}` 即可对单用户覆盖默认值
- 回滚：删除 per-user override 即回退到环境变量或代码默认值
- 所有配置值在 `MemoryService` 初始化时加载，运行时不热更新（重启生效）
- **Files:** `memoria/core/memory/config.py`, `memoria/core/memory/factory.py`

---

## Phase 1: Zero-Friction Capture + Contextual Retrieval (Weeks 2–5)

**Goal:** Automatically capture key information during conversations and retrieve
context-aware results — without interrupting the user.

**Depends on:** Phase 0A–0C (entity-anchored retrieval + dual trigger must work
before auto-capture can leverage entity context for dedup and merge decisions).

### Deliverables

#### 1.1 Confidence-tiered auto-capture
- High confidence (≥ 0.85): silent ingest, no user prompt
- Medium confidence (0.5–0.85): ingest with `working` type, promote on reuse
- Low confidence (< 0.5): surface confirmation prompt to user/LLM
- Leverage existing `initial_confidence` + `trust_tier` fields in `mem_memories`
- **Files:** `memoria/core/memory/tabular/typed_observer.py`, `memoria/core/memory/tabular/typed_pipeline.py`, `memoria/core/memory/config.py`

#### 1.2 Session-topic-anchored retrieval
- Compute session summary embedding at session close (exists: `memoria/core/memory/tabular/session_summary.py`)
- Boost retrieval score for memories from recent sessions (recency decay)
- Add topic-anchor weight: memories linked to current session's entities score higher
- **Files:** `memoria/core/memory/graph/retriever.py`, `memoria/core/memory/graph/activation.py`, `memoria/core/memory/tabular/retriever.py` (if tabular path)

#### 1.3 Dedup & merge baseline
- Current: cosine > 0.95 → supersede. Extend:
  - 0.85–0.95 range: suggest merge (surface to user or auto-merge with version note)
  - Track `superseded_by` chain for version history
  - Expose `/v1/memories/{id}/history` endpoint for version trail
- **Files:** `memoria/core/memory/tabular/governance.py`, `memoria/core/memory/graph/consolidation.py`, `memoria/api/routers/memory.py`

### Acceptance Criteria
- Capture accuracy ≥ 80%, false-positive rate ≤ 5%
- Fuzzy recall ("that approach we discussed") improves ≥ 30% vs current baseline
- Measured via `make verify-talk` cases + new golden sessions

### Resources
2 backend + 1 product/UX

### Milestone M1 (Week 5)
Auto-capture + contextual retrieval live. Core experience: "useful without being noticed."

---

## Phase 2: Proactive Prompts + Natural Language Management (Weeks 6–9)

**Goal:** Surface relevant memories at the right moment without interrupting flow.
Let users manage memories through natural conversation.

### Deliverables

#### 2.1 Scene-based triggers
- When user starts a similar project/task, retrieve related lessons/decisions
- Trigger threshold: activation score > configurable gate (default 0.6)
- Suppress if user dismissed similar prompt within N turns (anti-annoyance)
- **Files:** `memoria/core/memory/graph/activation.py`, new `memoria/core/memory/triggers.py`

#### 2.2 Memory management conversation flow
- Conflict detection: surface contradicting memories for resolution
  (existing: `consolidation.py` detects conflicts, needs user-facing flow)
- Version cleanup: "you have 3 versions of X, keep latest?" prompt
- Merge suggestions: "these 2 memories overlap, combine?"
- **Files:** `memoria/core/memory/graph/consolidation.py`, `memoria/mcp_local/server.py` (tools) or chat commands

#### 2.3 Memory health prompts
- Low-confidence memory ratio warning (> 30% of active memories below 0.5)
- Stale memory alert (no access in 90 days, high importance)
- Surface via `/v1/health` response or proactive prompt
- **Files:** `memoria/core/memory/tabular/health.py`, `memoria/api/routers/health.py`, `memoria/core/memory/tabular/governance.py`

### Acceptance Criteria
- Proactive prompt adoption rate ≥ 25%
- Management request completion rate ≥ 70%
- Measured via talk verification + user feedback

### Resources
2 backend + 1 frontend + 1 product/UX

### Milestone M2 (Week 9)
Proactive prompts and memory management conversation available.

---

## Phase 3: Visual Graph + Multimodal Preparation (Weeks 10–13)

**Goal:** Visualize knowledge structure. Prepare multimodal memory ingestion.

### Deliverables

#### 3.1 Graph API
- `GET /v1/graph?center={entity}&depth=2` — entity-centric subgraph
- `GET /v1/graph/timeline?session_id=...` — temporal event chain
- `GET /v1/graph/clusters` — scene-based topic clusters
- Return nodes + edges in a frontend-friendly format (e.g. vis.js / d3 compatible)
- **Files:** new `memoria/api/routers/graph.py`, `memoria/core/memory/graph/graph_store.py` queries

#### 3.2 Interactive graph MVP
- Frontend: entity-centric exploration (click entity → expand neighbors)
- Timeline view: session events as a horizontal chain
- Cluster view: scene nodes as groups with member memories
- **Stack:** lightweight SPA (React/Vue) or embedded in existing UI

#### 3.3 Multimodal schema
- Add `modality` VARCHAR(10) DEFAULT 'text' and `uri` TEXT to `memory_graph_nodes`
- Auto-migrate via `ensure_tables` (same pattern as `entity_type`)
- Prototype: store image/file URI, retrieve via text query → vector similarity
- **Files:** `memoria/core/memory/models/graph.py`, `memoria/schema.py`, `memoria/core/memory/graph/graph_store.py`

### Acceptance Criteria
- Graph displays 3 layers of interactive relationships
- Multimodal prototype: text query retrieves image/file reference
- Measured via demo + integration test

### Resources
2 backend + 2 frontend + 1 design

### Milestone M3 (Week 13)
Graph MVP + multimodal entry point.

---

## Priority Reminders

**Must do first (Week 1):**
- Phase 0A → 0B → 0C: ~7 days total, can partially overlap
- 0A (entity tables + reverse lookup) unblocks everything else
- 0B (opinion tuning) and 0C (dual trigger) can run in parallel after 0A

**Can defer:**
- Graph frontend + multimodal (Phase 3)
- Without Phase 0–2, the visual layer has limited value

**Dependencies:**
- Phase 0B and 0C both depend on 0A (entity tables must exist)
- Phase 0B and 0C are independent of each other (can parallelize)
- Phase 1 depends on Phase 0A–0C (entity-aware retrieval must exist)
- Phase 2 depends on Phase 1 (confidence tiers must exist before triggering prompts)
- Phase 3 is independent of Phase 2 (graph API can start in parallel with Phase 2)

**Timeline summary:**

| Phase | Duration | Weeks |
|-------|----------|-------|
| 0A: Entity Linking | 1–2 days | Week 1 |
| 0B: Scene + Opinion | 2–3 days | Week 1 (overlap with 0C) |
| 0C: Dual Trigger | 2–3 days | Week 1 (overlap with 0B) |
| 1: Auto-capture | 4 weeks | Weeks 2–5 |
| 2: Proactive prompts | 4 weeks | Weeks 6–9 |
| 3: Visual graph | 4 weeks | Weeks 10–13 |

---

## Existing Technical Backlog

See `roadmap.md` for implementation-level items:
- Async entity extraction (move off hot path)
- Hybrid entity extraction (regex + LLM threshold)
- Entity node garbage collection
- MatrixOne hybrid disambiguation
- Display name field for entities
