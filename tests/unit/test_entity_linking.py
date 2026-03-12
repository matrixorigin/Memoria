"""Unit tests for Phase 0A: Entity Linking upgrade.

Tests entity extraction enhancements, normalize_entity_name,
entity table operations, and entity-anchored retrieval logic.
"""

from memoria.core.memory.graph.entity_extractor import (
    extract_entities_lightweight,
    normalize_entity_name,
)


# ── normalize_entity_name ─────────────────────────────────────────────


class TestNormalizeEntityName:
    def test_ascii_lowercase(self):
        assert normalize_entity_name("Python") == "python"

    def test_chinese_unchanged(self):
        assert normalize_entity_name("上海") == "上海"

    def test_mixed_chinese_ascii(self):
        assert normalize_entity_name("上海Docker") == "上海docker"

    def test_whitespace_collapse(self):
        assert normalize_entity_name("  hello   world  ") == "hello world"

    def test_nfkc_normalization(self):
        # Fullwidth 'Ａ' → 'a' after NFKC + lowercase
        assert normalize_entity_name("\uff21\uff22") == "ab"

    def test_empty(self):
        assert normalize_entity_name("") == ""

    def test_punctuation_preserved(self):
        assert normalize_entity_name("c++") == "c++"

    def test_repo_pattern(self):
        assert (
            normalize_entity_name("MatrixOrigin/MatrixOne") == "matrixorigin/matrixone"
        )


# ── Chinese city extraction ───────────────────────────────────────────


class TestChineseCityExtraction:
    def test_single_city(self):
        entities = extract_entities_lightweight("我在上海吃了一碗面")
        names = {e.name for e in entities}
        assert "上海" in names
        types = {e.name: e.entity_type for e in entities}
        assert types["上海"] == "location"

    def test_multiple_cities(self):
        entities = extract_entities_lightweight("从北京到上海的高铁")
        names = {e.name for e in entities}
        assert "北京" in names
        assert "上海" in names

    def test_city_not_in_list(self):
        entities = extract_entities_lightweight("我在某个小镇吃了面")
        # No city should be extracted
        assert not any(e.entity_type == "location" for e in entities)

    def test_shanghai_noodle_vs_nanjing(self):
        """Core use case: distinguish Shanghai noodles from Nanjing noodles."""
        sh = extract_entities_lightweight("上海的面条店推荐一下")
        nj = extract_entities_lightweight("南京的面条店推荐一下")
        sh_names = {e.name for e in sh}
        nj_names = {e.name for e in nj}
        assert "上海" in sh_names
        assert "南京" in nj_names
        assert "南京" not in sh_names
        assert "上海" not in nj_names

    def test_hong_kong_macau(self):
        entities = extract_entities_lightweight("香港和澳门的美食")
        names = {e.name for e in entities}
        assert "香港" in names
        assert "澳门" in names


# ── Chinese time expression extraction ────────────────────────────────


class TestChineseTimeExtraction:
    def test_relative_time(self):
        entities = extract_entities_lightweight("昨天我去了一家餐厅")
        names = {e.name for e in entities}
        assert "昨天" in names
        types = {e.name: e.entity_type for e in entities}
        assert types["昨天"] == "time"

    def test_week_day(self):
        entities = extract_entities_lightweight("周三开会讨论方案")
        names = {e.name for e in entities}
        assert "周三" in names

    def test_date_expression(self):
        entities = extract_entities_lightweight("2026年3月的计划")
        names = {e.name for e in entities}
        assert "2026年3月" in names

    def test_month_day(self):
        entities = extract_entities_lightweight("3月15日是截止日期")
        names = {e.name for e in entities}
        assert "3月15日" in names

    def test_last_week(self):
        entities = extract_entities_lightweight("上周讨论的方案")
        names = {e.name for e in entities}
        assert "上周" in names


# ── Quoted / backtick term extraction ─────────────────────────────────


class TestQuotedTermExtraction:
    def test_double_quoted(self):
        entities = extract_entities_lightweight('我们的项目叫 "Memoria" 很好用')
        names = {e.name for e in entities}
        assert "memoria" in names

    def test_backtick(self):
        entities = extract_entities_lightweight("使用 `graph_builder` 来构建图")
        names = {e.name for e in entities}
        assert "graph_builder" in names

    def test_chinese_quotes(self):
        entities = extract_entities_lightweight("项目\u201cMemoria\u201d已经上线")
        names = {e.name for e in entities}
        assert "memoria" in names

    def test_short_quoted_ignored(self):
        """Quoted strings shorter than 2 chars should be ignored."""
        entities = extract_entities_lightweight('使用 "x" 变量')
        # "x" is only 1 char, should not be extracted
        assert not any(e.name == "x" for e in entities)


# ── Mixed extraction (regression) ────────────────────────────────────


class TestMixedExtraction:
    def test_tech_still_works(self):
        entities = extract_entities_lightweight("I use Python and PostgreSQL")
        names = {e.name for e in entities}
        assert "python" in names
        assert "postgresql" in names

    def test_mention_still_works(self):
        entities = extract_entities_lightweight("Ask @alice about this")
        names = {e.name for e in entities}
        assert "alice" in names

    def test_camel_case_still_works(self):
        entities = extract_entities_lightweight("The GraphBuilder handles it")
        names = {e.name for e in entities}
        assert "graphbuilder" in names

    def test_mixed_chinese_and_tech(self):
        text = "在上海用 Docker 部署了 MatrixOne，昨天刚上线"
        entities = extract_entities_lightweight(text)
        names = {e.name for e in entities}
        assert "上海" in names
        assert "docker" in names
        assert "matrixone" in names
        assert "昨天" in names

    def test_dedup_across_patterns(self):
        """Same entity matched by multiple patterns should appear once."""
        entities = extract_entities_lightweight("docker Docker DOCKER")
        docker_entities = [e for e in entities if "docker" in e.name]
        assert len(docker_entities) == 1

    def test_empty_text(self):
        assert extract_entities_lightweight("") == []


# ── Entity-anchored retrieval (unit-level) ────────────────────────────


class TestEntityAnchoredRetrieval:
    """Test the retrieval logic without DB — mock the store."""

    def test_entity_recall_injects_candidates(self):
        """Entity recall should return entity_node_ids + memory_ids for injection."""
        from unittest.mock import MagicMock

        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = MagicMock()
        retriever = ActivationRetriever(store)

        # Mock: query "上海的面条" → entity "上海" found → memory "mem1" linked
        store.find_entity_by_name.return_value = "ent_shanghai"
        store.get_memories_by_entity.return_value = [("mem1", 0.9)]

        entity_nids, memory_ids = retriever._entity_recall("user1", "上海的面条店推荐")
        assert "ent_shanghai" in entity_nids  # entity node as activation anchor
        assert "mem1" in memory_ids  # memory for candidate recall

    def test_no_entity_no_recall(self):
        """When query has no recognized entities, no recall."""
        from unittest.mock import MagicMock

        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = MagicMock()
        retriever = ActivationRetriever(store)

        entity_nids, memory_ids = retriever._entity_recall("user1", "hello world")
        assert entity_nids == set()
        assert memory_ids == set()
        store.find_entity_by_name.assert_not_called()

    def test_entity_not_in_db(self):
        """When entity is extracted but not found in DB, no recall."""
        from unittest.mock import MagicMock

        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = MagicMock()
        retriever = ActivationRetriever(store)

        store.find_entity_by_name.return_value = None
        entity_nids, memory_ids = retriever._entity_recall("user1", "上海的面条")
        assert entity_nids == set()
        assert memory_ids == set()

    def test_entity_node_id_equals_entity_id(self):
        """Entity node IDs returned should be the entity_id from mem_entities,
        which is the same as the graph node_id (1:1 mapping)."""
        from unittest.mock import MagicMock

        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = MagicMock()
        retriever = ActivationRetriever(store)

        store.find_entity_by_name.return_value = "abc123"
        store.get_memories_by_entity.return_value = [("mem1", 0.8)]

        entity_nids, _ = retriever._entity_recall("user1", "上海")
        # The entity_id "abc123" should be usable as a graph node anchor
        assert "abc123" in entity_nids


# ── Config ────────────────────────────────────────────────────────────


class TestEntityConfig:
    def test_default_entity_boost(self):
        from memoria.core.memory.config import MemoryGovernanceConfig

        config = MemoryGovernanceConfig()
        assert config.entity_boost == 1.8
        assert config.entity_node_type_weight == 0.8

    def test_opinion_defaults_match_plan(self):
        from memoria.core.memory.config import MemoryGovernanceConfig

        config = MemoryGovernanceConfig()
        assert config.opinion_contradicting_delta == -0.12
        assert config.opinion_quarantine_threshold == 0.18
        assert config.opinion_supporting_delta == 0.05
        assert config.opinion_confidence_cap == 0.95

    def test_env_override(self):
        import os

        from memoria.core.memory.config import MemoryGovernanceConfig

        os.environ["MEM_ENTITY_BOOST"] = "2.5"
        os.environ["MEM_ENTITY_NODE_TYPE_WEIGHT"] = "0.9"
        try:
            config = MemoryGovernanceConfig.from_env()
            assert config.entity_boost == 2.5
            assert config.entity_node_type_weight == 0.9
        finally:
            del os.environ["MEM_ENTITY_BOOST"]
            del os.environ["MEM_ENTITY_NODE_TYPE_WEIGHT"]
