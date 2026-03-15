"""Unit tests for GET /memories/{memory_id}/history endpoint."""

from __future__ import annotations

from contextlib import contextmanager
from datetime import datetime, timezone
from unittest.mock import MagicMock

import pytest


def _make_record(memory_id, content, superseded_by=None, is_active=True):
    r = MagicMock()
    r.memory_id = memory_id
    r.content = content
    r.is_active = is_active
    r.superseded_by = superseded_by
    r.observed_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
    r.memory_type = "semantic"
    return r


def _build_db(records_by_id: dict, records_by_superseded: dict):
    """Return a mock db whose query().filter_by().first() dispatches correctly."""
    db = MagicMock()

    def query_side_effect(model):
        q = MagicMock()

        def filter_by_side_effect(**kwargs):
            fq = MagicMock()
            if "memory_id" in kwargs:
                fq.first.return_value = records_by_id.get(kwargs["memory_id"])
            elif "superseded_by" in kwargs:
                fq.first.return_value = records_by_superseded.get(
                    kwargs["superseded_by"]
                )
            else:
                fq.first.return_value = None
            return fq

        q.filter_by.side_effect = filter_by_side_effect
        return q

    db.query.side_effect = query_side_effect
    return db


def _db_factory(db):
    @contextmanager
    def factory():
        yield db

    return factory


def _call_endpoint(memory_id, db):
    from memoria.api.routers.memory import get_memory_history

    return get_memory_history(
        memory_id=memory_id,
        user_id="user1",
        db_factory=_db_factory(db),
    )


class TestGetMemoryHistory:
    def test_single_version(self):
        r1 = _make_record("m1", "v1", superseded_by=None)
        db = _build_db({"m1": r1}, {})
        result = _call_endpoint("m1", db)
        assert result["memory_id"] == "m1"
        assert result["total"] == 1
        assert result["versions"][0]["memory_id"] == "m1"

    def test_two_versions_query_from_old(self):
        """Start from oldest (m1 → m2). Should return [m1, m2]."""
        r1 = _make_record("m1", "v1", superseded_by="m2", is_active=False)
        r2 = _make_record("m2", "v2", superseded_by=None)
        db = _build_db({"m1": r1, "m2": r2}, {"m2": r1})
        result = _call_endpoint("m1", db)
        assert result["total"] == 2
        assert [v["memory_id"] for v in result["versions"]] == ["m1", "m2"]

    def test_two_versions_query_from_new(self):
        """Start from newest (m2). Should still return [m1, m2]."""
        r1 = _make_record("m1", "v1", superseded_by="m2", is_active=False)
        r2 = _make_record("m2", "v2", superseded_by=None)
        # records_by_superseded: m1 is superseded_by=m2, so key="m2" → r1
        db = _build_db({"m1": r1, "m2": r2}, {"m2": r1})
        result = _call_endpoint("m2", db)
        assert result["total"] == 2
        assert [v["memory_id"] for v in result["versions"]] == ["m1", "m2"]

    def test_three_version_chain(self):
        r1 = _make_record("m1", "v1", superseded_by="m2", is_active=False)
        r2 = _make_record("m2", "v2", superseded_by="m3", is_active=False)
        r3 = _make_record("m3", "v3", superseded_by=None)
        db = _build_db(
            {"m1": r1, "m2": r2, "m3": r3},
            {"m2": r1, "m3": r2},
        )
        result = _call_endpoint("m2", db)
        assert result["total"] == 3
        assert [v["memory_id"] for v in result["versions"]] == ["m1", "m2", "m3"]

    def test_not_found_raises_404(self):
        from fastapi import HTTPException

        db = _build_db({}, {})
        with pytest.raises(HTTPException) as exc_info:
            _call_endpoint("missing", db)
        assert exc_info.value.status_code == 404

    def test_response_fields(self):
        r1 = _make_record("m1", "hello", superseded_by=None)
        db = _build_db({"m1": r1}, {})
        result = _call_endpoint("m1", db)
        v = result["versions"][0]
        assert v["content"] == "hello"
        assert v["is_active"] is True
        assert v["superseded_by"] is None
        assert v["observed_at"] == "2026-01-01T00:00:00+00:00"
        assert v["memory_type"] == "semantic"
