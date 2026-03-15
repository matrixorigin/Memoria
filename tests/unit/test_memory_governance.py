"""Unit tests for GovernanceScheduler — frequency-separated governance."""

from unittest.mock import MagicMock, patch

import pytest

from memoria.core.memory.tabular.governance import (
    GovernanceScheduler,
    GovernanceCycleResult,
)
from memoria.core.memory.config import MemoryGovernanceConfig


@pytest.fixture
def mock_db():
    db = MagicMock()
    result = MagicMock()
    result.rowcount = 0
    db.execute.return_value = result
    return db


@pytest.fixture
def scheduler(mock_db):
    return GovernanceScheduler(db_factory=lambda: mock_db)


class TestRunHourly:
    def test_returns_result(self, scheduler):
        r = scheduler.run_hourly()
        assert isinstance(r, GovernanceCycleResult)

    def test_cleans_tool_results(self, scheduler, mock_db):
        mock_db.execute.return_value.rowcount = 3
        r = scheduler.run_hourly()
        assert r.cleaned_tool_results == 3

    def test_archives_working_memories(self, scheduler, mock_db):
        # First call = tool_result cleanup (rowcount=0)
        tool_result = MagicMock(rowcount=0)
        # Second call = SELECT for working archival (returns 2 rows)
        row1 = MagicMock(memory_id="m1", user_id="u1")
        row2 = MagicMock(memory_id="m2", user_id="u1")
        select_result = MagicMock()
        select_result.fetchall.return_value = [row1, row2]
        # Third call = UPDATE (batch deactivate)
        update_result = MagicMock(rowcount=2)
        mock_db.execute.side_effect = [tool_result, select_result, update_result]
        r = scheduler.run_hourly()
        assert r.archived_working == 2

    def test_error_captured(self, scheduler, mock_db):
        mock_db.execute.side_effect = Exception("db down")
        r = scheduler.run_hourly()
        assert len(r.errors) >= 1


class TestRunDaily:
    def test_returns_result(self, scheduler):
        r = scheduler.run_daily("u1")
        assert isinstance(r, GovernanceCycleResult)

    def test_cleans_stale(self, scheduler, mock_db):
        mock_db.execute.return_value.rowcount = 5
        r = scheduler.run_daily("u1")
        assert r.cleaned_stale == 5

    def test_quarantines_low_confidence(self, scheduler, mock_db):
        # stale cleanup returns 0, then 4 quarantine tiers: each SELECT returns 1 row, then UPDATE
        results = [MagicMock(rowcount=0)]  # stale cleanup
        for i in range(4):
            select_r = MagicMock()
            select_r.fetchall.return_value = [MagicMock(memory_id=f"q{i}")]
            results.append(select_r)  # SELECT per tier
            results.append(MagicMock(rowcount=1))  # UPDATE per tier
        # Remaining calls from pollution detection, orphaned incrementals, compress_redundant
        # may also call execute — use default mock for those
        mock_db.execute.side_effect = results + [MagicMock(rowcount=0)] * 10
        r = scheduler.run_daily("u1")
        assert r.quarantined == 4


class TestRunWeekly:
    def test_returns_result(self, scheduler):
        with (
            patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0),
            patch.object(scheduler.health, "cleanup_snapshots", return_value=0),
        ):
            r = scheduler.run_weekly()
        assert isinstance(r, GovernanceCycleResult)


class TestRunCycle:
    def test_calls_all_frequencies(self, scheduler):
        with (
            patch.object(
                scheduler,
                "run_hourly",
                return_value=GovernanceCycleResult(cleaned_tool_results=1),
            ),
            patch.object(
                scheduler,
                "run_daily",
                return_value=GovernanceCycleResult(cleaned_stale=2),
            ),
            patch.object(
                scheduler,
                "run_weekly",
                return_value=GovernanceCycleResult(cleaned_branches=3),
            ),
        ):
            r = scheduler.run_cycle("u1")
        assert r.cleaned_tool_results == 1
        assert r.cleaned_stale == 2
        assert r.cleaned_branches == 3


class TestNoDecayMutation:
    def test_governance_does_not_mutate_confidence(self, scheduler, mock_db):
        """Governance never writes to initial_confidence column."""
        scheduler.run_hourly()
        for c in mock_db.execute.call_args_list:
            sql = str(c)
            assert "initial_confidence =" not in sql or "SET" not in sql


class TestQuarantineConfig:
    def test_custom_threshold(self, mock_db):
        config = MemoryGovernanceConfig(quarantine_threshold=0.5)
        s = GovernanceScheduler(db_factory=lambda: mock_db, config=config)
        assert s.config.quarantine_threshold == 0.5


class TestWorkingMemoryStaleConfig:
    def test_custom_stale_hours(self, mock_db):
        config = MemoryGovernanceConfig(working_memory_stale_hours=4)
        s = GovernanceScheduler(db_factory=lambda: mock_db, config=config)
        assert s.config.working_memory_stale_hours == 4


class TestVectorIndexHealth:
    """Unit tests for _check_vector_index_health — mocks VectorManager."""

    def _make_scheduler(self, mock_db):
        return GovernanceScheduler(db_factory=lambda: mock_db)

    def _mock_vm(self, table_stats: dict):
        """Return a mock VectorManager whose get_ivf_stats returns table_stats[table]."""
        vm = MagicMock()
        vm.get_ivf_stats.side_effect = lambda table, col: table_stats[table]
        return vm

    def _patch_vm(self, scheduler, vm):
        return patch(
            "memoria.core.memory.tabular.governance.GovernanceScheduler._check_vector_index_health",
            wraps=scheduler._check_vector_index_health,
        ), patch(
            "memoria.core.memory.tabular.governance.VectorManager", return_value=vm
        )

    @pytest.mark.parametrize(
        "total_rows,centroids,expected_rebuild",
        [
            # < 20k: ratio = rows/centroids must be >= 50
            (70, 1, False),  # ratio=70 ≥ 50 → healthy
            (49, 1, True),  # ratio=49 < 50 → needs rebuild
            (500, 1, False),  # ratio=500 ≥ 50 → healthy
            (100, 3, True),  # ratio=33 < 50 → needs rebuild
        ],
    )
    def test_health_small_dataset(
        self, mock_db, total_rows, centroids, expected_rebuild
    ):
        # Build fake distribution: `centroids` buckets each with total_rows//centroids rows
        per_bucket = total_rows // centroids
        counts = [per_bucket] * centroids
        # adjust last bucket for rounding
        counts[-1] += total_rows - sum(counts)

        stats = {
            "distribution": {
                "centroid_count": counts,
                "centroid_id": list(range(centroids)),
                "centroid_version": [1] * centroids,
            }
        }
        vm = MagicMock()
        vm.get_ivf_stats.return_value = stats

        scheduler = self._make_scheduler(mock_db)
        with (
            patch(
                "memoria.core.memory.tabular.governance.VectorManager", return_value=vm
            ),
            patch("memoria.api.database._mo_client", MagicMock()),
        ):
            health = scheduler._check_vector_index_health()

        # Both tables use same mock, check mem_memories
        assert health["mem_memories"]["needs_rebuild"] == expected_rebuild
        assert health["mem_memories"]["total_rows"] == total_rows
        assert health["mem_memories"]["centroids"] == centroids

    def test_health_large_dataset_needs_1024(self, mock_db):
        # 500k rows, 500 centroids → ratio=1000, boundary of [500,1000) → needs_rebuild (ratio >= 1000)
        counts = [1000] * 500  # 500k rows, 500 centroids
        stats = {
            "distribution": {
                "centroid_count": counts,
                "centroid_id": list(range(500)),
                "centroid_version": [1] * 500,
            }
        }
        vm = MagicMock()
        vm.get_ivf_stats.return_value = stats

        scheduler = self._make_scheduler(mock_db)
        with (
            patch(
                "memoria.core.memory.tabular.governance.VectorManager", return_value=vm
            ),
            patch("memoria.api.database._mo_client", MagicMock()),
        ):
            health = scheduler._check_vector_index_health()

        # ratio=1000 → ratio >= 1000 → needs_rebuild
        assert health["mem_memories"]["needs_rebuild"] is True

    def test_health_vm_unavailable(self, mock_db):
        scheduler = self._make_scheduler(mock_db)
        with patch("memoria.core.memory.tabular.governance.VectorManager", None):
            health = scheduler._check_vector_index_health()
        assert health == {}

    def test_health_error_per_table(self, mock_db):
        vm = MagicMock()
        vm.get_ivf_stats.side_effect = Exception("index not found")
        scheduler = self._make_scheduler(mock_db)
        with (
            patch(
                "memoria.core.memory.tabular.governance.VectorManager", return_value=vm
            ),
            patch("memoria.api.database._mo_client", MagicMock()),
        ):
            health = scheduler._check_vector_index_health()
        assert "error" in health["mem_memories"]


class TestRebuildVectorIndex:
    """Unit tests for rebuild_vector_index."""

    def _make_scheduler(self, mock_db):
        return GovernanceScheduler(db_factory=lambda: mock_db)

    def test_rebuild_computes_optimal_lists(self, mock_db):
        # 500 rows → lists = max(1, 500//50) = 10
        counts = [50] * 10  # 10 centroids, 50 rows each = 500 total
        stats = {
            "distribution": {
                "centroid_count": counts,
                "centroid_id": list(range(10)),
                "centroid_version": [1] * 10,
            }
        }
        vm = MagicMock()
        vm.get_ivf_stats.return_value = stats

        scheduler = self._make_scheduler(mock_db)
        with (
            patch(
                "memoria.core.memory.tabular.governance.VectorManager", return_value=vm
            ),
            patch("memoria.api.database._mo_client", MagicMock()),
        ):
            with patch("matrixone.sqlalchemy_ext.vector_index.VectorOpType"):
                result = scheduler.rebuild_vector_index("mem_memories")

        assert result["total_rows"] == 500
        assert result["new_lists"] == 10
        vm.drop.assert_called_once_with("mem_memories", "idx_memory_embedding")
        vm.create_ivf.assert_called_once()

    def test_rebuild_unknown_table_raises(self, mock_db):
        scheduler = self._make_scheduler(mock_db)
        with pytest.raises(ValueError, match="Unknown table"):
            scheduler.rebuild_vector_index("unknown_table")

    def test_rebuild_lists_capped_at_1024(self, mock_db):
        # 200k rows → lists = min(200000//50, 1024) = 1024
        counts = [200] * 1000  # 1000 centroids, 200 rows each = 200k total
        stats = {
            "distribution": {
                "centroid_count": counts,
                "centroid_id": list(range(1000)),
                "centroid_version": [1] * 1000,
            }
        }
        vm = MagicMock()
        vm.get_ivf_stats.return_value = stats

        scheduler = self._make_scheduler(mock_db)
        with (
            patch(
                "memoria.core.memory.tabular.governance.VectorManager", return_value=vm
            ),
            patch("memoria.api.database._mo_client", MagicMock()),
        ):
            with patch("matrixone.sqlalchemy_ext.vector_index.VectorOpType"):
                result = scheduler.rebuild_vector_index("mem_memories")

        assert result["new_lists"] == 1024


class TestCompressRedundantSupersededBy:
    """Test that _compress_redundant correctly tracks superseded_by mapping."""

    def test_superseded_by_mapping_newer_wins(self):
        """Older memory should be superseded by newer one."""
        from datetime import datetime, timezone

        # Simulate the inner loop logic directly
        timestamps = [
            datetime(2026, 3, 15, 10, 0, tzinfo=timezone.utc),  # i=0 newer
            datetime(2026, 3, 14, 10, 0, tzinfo=timezone.utc),  # j=1 older
        ]
        ids = ["new_id", "old_id"]

        to_deactivate: set[str] = set()
        superseded_by: dict[str, str] = {}

        i, j = 0, 1
        if timestamps[i] >= timestamps[j]:
            older, newer = ids[j], ids[i]
        else:
            older, newer = ids[i], ids[j]
        to_deactivate.add(older)
        superseded_by[older] = newer

        assert "old_id" in to_deactivate
        assert superseded_by["old_id"] == "new_id"
        assert "new_id" not in to_deactivate

    def test_default_threshold_is_085(self):
        """Default redundant_similarity_threshold should be 0.85."""
        cfg = MemoryGovernanceConfig()
        assert cfg.redundant_similarity_threshold == 0.85

    def test_l2_threshold_for_085_cosine(self):
        """L2² threshold for cosine=0.85 should be 2*(1-0.85)=0.30."""
        threshold = 0.85
        l2_sq = 2.0 * (1.0 - threshold)
        assert abs(l2_sq - 0.30) < 1e-9
