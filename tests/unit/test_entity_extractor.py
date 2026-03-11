"""Unit tests for entity extraction (lightweight regex-based)."""

import pytest
from memoria.core.memory.graph.entity_extractor import extract_entities_lightweight


class TestLightweightExtraction:
    def test_tech_terms(self):
        entities = extract_entities_lightweight("I use Python and PostgreSQL for this project")
        names = {e.name for e in entities}
        assert "python" in names
        assert "postgresql" in names
        assert all(e.entity_type == "tech" for e in entities if e.name in ("python", "postgresql"))

    def test_mentions(self):
        entities = extract_entities_lightweight("Ask @alice and @bob about this")
        names = {e.name for e in entities}
        assert "alice" in names
        assert "bob" in names
        assert all(e.entity_type == "person" for e in entities if e.name in ("alice", "bob"))

    def test_repo_pattern(self):
        entities = extract_entities_lightweight("Check matrixorigin/matrixone for details")
        names = {e.name for e in entities}
        assert "matrixorigin/matrixone" in names

    def test_camel_case(self):
        entities = extract_entities_lightweight("The GraphBuilder handles node creation")
        names = {e.name for e in entities}
        assert "graphbuilder" in names

    def test_dedup(self):
        entities = extract_entities_lightweight("Python is great. I love Python. Python rocks.")
        python_entities = [e for e in entities if e.name == "python"]
        assert len(python_entities) == 1

    def test_empty_text(self):
        assert extract_entities_lightweight("") == []

    def test_no_entities(self):
        assert extract_entities_lightweight("hello world") == []

    def test_mixed(self):
        text = "Deploy with Docker on AWS, ask @devops for the matrixorigin/matrixone config"
        entities = extract_entities_lightweight(text)
        names = {e.name for e in entities}
        assert "docker" in names
        assert "aws" in names
        assert "devops" in names
        assert "matrixorigin/matrixone" in names
