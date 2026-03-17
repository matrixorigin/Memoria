# Memoria Roadmap

Items are grouped by priority. Completed items are marked ✅.

---

## ✅ Done (v0.2.8)

- Entity dedup: canonical lowercase content in all paths; `find_entity_node` case-insensitive
- Entity linking consolidated to `GraphStore.link_entities_batch()` — single source of truth
- `entity_type` column added to `memory_graph_nodes` (auto-migrated on startup)
- `ENTITY_LINK` edge weight now reflects source quality: regex=0.8, llm=0.9, manual=1.0
- `GET /v1/entities` endpoint: list user's entity nodes with name, entity_type, importance
- `POST /v1/extract-entities/link` response now includes created entity details (name + entity_type)
- Confirmed activation propagation already uses per-edge weight (added clarifying comment)

---

## High Priority

### Async entity extraction & association edges
**Why:** `graph_builder.ingest()` runs cosine similarity queries and entity extraction
synchronously on the write hot path, adding latency to every `store()` call.

**Plan:**
- `ingest()` creates nodes + temporal/abstraction/causal edges only (fast, no vector queries)
- Association edges and entity linking go into an async queue (e.g. background task or
  governance drain)
- `run_governance()` drains the queue in batch

---

## Medium Priority

### Hybrid entity extraction strategy (lightweight + LLM threshold)
**Why:** Lightweight regex misses domain-specific terms; full LLM extraction is expensive.

**Plan:**
- After lightweight extraction, if entity count < threshold (e.g. 2) AND content length > N,
  trigger LLM extraction automatically
- Configurable via `MemoryGovernanceConfig.entity_llm_threshold`

### Entity source weight in retrieval scoring
**Why:** ~~`ENTITY_LINK` edges with weight 0.8/0.9/1.0 are stored but not yet used in
activation retrieval scoring.~~

**Status:** ✅ Already works — `_edge_weight()` computes
`edge.weight × EDGE_TYPE_MULTIPLIER`, so regex edges propagate at 0.8×1.2=0.96,
LLM at 0.9×1.2=1.08, manual at 1.0×1.2=1.2. Added clarifying comment.

---

## Medium-Low Priority

### Multimodal node fields
**Why:** `GraphNode.content` and `embedding` are text-only. Future use cases include
image/audio/file references.

**Plan:**
- Add `modality` VARCHAR(10) DEFAULT 'text' and `uri` TEXT DEFAULT NULL to `memory_graph_nodes`
- Auto-migrate via `ensure_tables` (same pattern as `entity_type`)
- MatrixOne vector + fulltext search already works on the `content` field; `uri` enables
  cross-modal retrieval by reference

---

## Low Priority

### Entity node garbage collection
**Why:** Entity nodes with no incoming or outgoing edges accumulate over time (orphans from
deleted memories).

**Plan:**
- Add a governance step: `DELETE FROM memory_graph_nodes WHERE node_type='entity'
  AND node_id NOT IN (SELECT source_id FROM memory_graph_edges WHERE user_id=:uid)
  AND node_id NOT IN (SELECT target_id FROM memory_graph_edges WHERE user_id=:uid)`
- Run as part of `run_governance()` daily cycle

### MatrixOne hybrid entity disambiguation
**Why:** Pure vector scan for entity dedup is expensive at scale.

**Plan:**
- Use fulltext search to find candidate entity nodes by name first
- Re-rank candidates by cosine similarity
- Reduces vector scan cost from O(all_entities) to O(candidates)
