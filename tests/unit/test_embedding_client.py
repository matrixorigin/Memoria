"""Tests for EmbeddingClient and providers."""

import math
import pytest
from unittest.mock import MagicMock, patch

from memoria.core.embedding.client import EmbeddingClient
from tests.conftest import TEST_EMBEDDING_DIM


# ── MockProvider ──────────────────────────────────────────────────────


class TestMockProvider:
    def test_deterministic(self):
        c = EmbeddingClient(provider="mock", model="mock", dim=TEST_EMBEDDING_DIM)
        assert c.embed("hello") == c.embed("hello")

    def test_correct_dimension(self):
        c = EmbeddingClient(provider="mock", model="mock", dim=TEST_EMBEDDING_DIM)
        assert len(c.embed("hello")) == TEST_EMBEDDING_DIM
        assert c.dimension == TEST_EMBEDDING_DIM

    def test_different_texts_differ(self):
        c = EmbeddingClient(provider="mock", model="mock", dim=TEST_EMBEDDING_DIM)
        assert c.embed("hello") != c.embed("goodbye")

    def test_custom_dimension(self):
        c = EmbeddingClient(provider="mock", model="mock", dim=128)
        assert len(c.embed("test")) == 128
        assert c.dimension == 128

    def test_model_name(self):
        c = EmbeddingClient(provider="mock", model="mock", dim=TEST_EMBEDDING_DIM)
        assert c.model_name == "mock"


# ── Fail fast ─────────────────────────────────────────────────────────


class TestFailFast:
    def test_unknown_provider_raises(self):
        with pytest.raises(ValueError, match="Unknown embedding provider"):
            EmbeddingClient(provider="nonexistent", model="x", dim=TEST_EMBEDDING_DIM)

    def test_openai_missing_api_key_raises(self):
        with pytest.raises(ValueError, match="requires api_key"):
            EmbeddingClient(
                provider="openai", model="text-embedding-3-small", dim=1536, api_key=""
            )

    @pytest.mark.local_embedding
    def test_local_dimension_mismatch_raises(self):
        """If model outputs different dim than config, fail fast."""
        with pytest.raises(ValueError, match="384.*config says 512"):
            EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=512)


# ── LocalProvider ─────────────────────────────────────────────────────


@pytest.mark.local_embedding
class TestLocalProvider:
    def test_correct_dimension(self):
        c = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
        vec = c.embed("hello world")
        assert len(vec) == 384
        assert c.dimension == 384

    def test_semantic_quality(self):
        """Similar texts should be closer than unrelated texts."""
        c = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
        v_py = c.embed("Python programming language")
        v_go = c.embed("Go programming language")
        v_weather = c.embed("The weather is sunny today")

        def l2(a, b):
            return math.sqrt(sum((x - y) ** 2 for x, y in zip(a, b)))

        assert l2(v_py, v_go) < l2(v_py, v_weather)

    def test_model_cache_reuse(self):
        """Second instantiation should reuse cached model."""
        from memoria.core.embedding.providers import _local_model_cache

        c1 = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
        assert "all-MiniLM-L6-v2" in _local_model_cache
        c2 = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
        # Same underlying model object
        assert c1._provider._model is c2._provider._model

    def test_model_name(self):
        c = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
        assert c.model_name == "all-MiniLM-L6-v2"


# ── OpenAIProvider ────────────────────────────────────────────────────


class TestOpenAIProvider:
    def test_calls_api_with_correct_params(self):
        mock_client = MagicMock()
        mock_resp = MagicMock()
        mock_resp.data = [MagicMock(embedding=[0.1] * 1536)]
        mock_client.embeddings.create.return_value = mock_resp

        with patch("openai.OpenAI", return_value=mock_client):
            c = EmbeddingClient(
                provider="openai",
                model="text-embedding-3-small",
                dim=1536,
                api_key="sk-test",
            )
            vec = c.embed("hello")

        mock_client.embeddings.create.assert_called_once_with(
            model="text-embedding-3-small",
            input="hello",
            dimensions=1536,
        )
        assert len(vec) == 1536

    def test_api_error_propagates(self):
        """No silent fallback — API errors must propagate."""
        mock_client = MagicMock()
        mock_client.embeddings.create.side_effect = RuntimeError("API down")

        with patch("openai.OpenAI", return_value=mock_client):
            c = EmbeddingClient(
                provider="openai",
                model="text-embedding-3-small",
                dim=1536,
                api_key="sk-test",
            )
            with pytest.raises(RuntimeError, match="API down"):
                c.embed("hello")

    def test_base_url_passed(self):
        mock_cls = MagicMock()
        with patch("openai.OpenAI", mock_cls):
            EmbeddingClient(
                provider="openai",
                model="m",
                dim=1536,
                api_key="sk-test",
                base_url="https://custom.api/v1",
            )
        mock_cls.assert_called_once_with(
            api_key="sk-test", base_url="https://custom.api/v1"
        )


# ── Startup validation ────────────────────────────────────────────────


class TestStartupValidation:
    def test_local_provider_without_sentence_transformers(self):
        """If sentence-transformers is not installed, local provider should fail fast."""
        with patch.dict("sys.modules", {"sentence_transformers": None}):
            from memoria.core.embedding.providers import _local_model_cache

            # Clear cache to force reload attempt
            old = _local_model_cache.copy()
            _local_model_cache.clear()
            try:
                with pytest.raises((ImportError, ModuleNotFoundError)):
                    EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
            finally:
                _local_model_cache.update(old)

    @pytest.mark.local_embedding
    def test_valid_config_succeeds(self):
        """Valid local config should work."""
        c = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384)
        assert len(c.embed("test")) == 384


# ── KNOWN_DIMENSIONS validation ───────────────────────────────────────


class TestKnownDimensions:
    def test_known_model_wrong_dim_raises(self):
        """Known model with wrong dim must fail at init, not at embed time."""
        with pytest.raises(ValueError, match="fixed dimension 1024.*config says 768"):
            EmbeddingClient(provider="mock", model="BAAI/bge-m3", dim=768)

    def test_known_model_correct_dim_passes(self):
        c = EmbeddingClient(provider="mock", model="BAAI/bge-m3", dim=1024)
        assert c.dimension == 1024

    def test_unknown_model_any_dim_passes(self):
        """Unknown model: no check, user is responsible for dim."""
        c = EmbeddingClient(provider="mock", model="my-custom-model", dim=512)
        assert c.dimension == 512

    def test_all_known_models_correct_dim(self):
        """Smoke test: every entry in KNOWN_DIMENSIONS passes its own check."""
        from memoria.core.embedding.client import KNOWN_DIMENSIONS

        for model, dim in KNOWN_DIMENSIONS.items():
            c = EmbeddingClient(provider="mock", model=model, dim=dim)
            assert c.dimension == dim


# ── Settings auto-infer ───────────────────────────────────────────────


class TestSettingsInfer:
    def test_known_model_auto_infers_dim(self, monkeypatch):
        monkeypatch.setenv("EMBEDDING_PROVIDER", "openai")
        monkeypatch.setenv("EMBEDDING_MODEL", "BAAI/bge-m3")
        monkeypatch.setenv("EMBEDDING_DIM", "0")
        from importlib import reload
        import memoria.config as s

        reload(s)
        settings = s.MemoriaSettings()
        assert settings.embedding_dim == 1024

    def test_explicit_dim_preserved(self, monkeypatch):
        monkeypatch.setenv("EMBEDDING_MODEL", "BAAI/bge-m3")
        monkeypatch.setenv("EMBEDDING_DIM", "1024")
        from importlib import reload
        import memoria.config as s

        reload(s)
        settings = s.MemoriaSettings()
        assert settings.embedding_dim == 1024

    def test_unknown_model_no_dim_raises(self, monkeypatch):
        monkeypatch.setenv("EMBEDDING_PROVIDER", "openai")
        monkeypatch.setenv("EMBEDDING_MODEL", "my-unknown-model")
        monkeypatch.setenv("EMBEDDING_DIM", "0")
        from importlib import reload
        import memoria.config as s

        reload(s)
        with pytest.raises(Exception, match="not in KNOWN_DIMENSIONS"):
            s.MemoriaSettings()


class TestSaTypesEmbeddingDim:
    """Regression: EMBEDDING_DIM='' (empty string) must not raise ValueError."""

    def _reload_dim(self, monkeypatch, value: str | None) -> int:
        if value is None:
            monkeypatch.delenv("EMBEDDING_DIM", raising=False)
        else:
            monkeypatch.setenv("EMBEDDING_DIM", value)
        from importlib import reload
        import memoria.core.memory.models._sa_types as m

        reload(m)
        return m.EMBEDDING_DIM

    def test_empty_string_falls_back_to_1024(self, monkeypatch):
        assert self._reload_dim(monkeypatch, "") == 1024

    def test_unset_falls_back_to_1024(self, monkeypatch):
        assert self._reload_dim(monkeypatch, None) == 1024

    def test_explicit_value_used(self, monkeypatch):
        assert self._reload_dim(monkeypatch, "1024") == 1024


# ── _make_embed_client (server default path) ──────────────────────────


class TestMakeEmbedClient:
    """Regression: default local embedding path in MCP server."""

    def test_short_model_name_resolves_dim(self):
        """'all-MiniLM-L6-v2' (no prefix) must resolve to dim=384, not 1024."""
        from memoria.core.embedding.client import KNOWN_DIMENSIONS

        assert KNOWN_DIMENSIONS.get("all-MiniLM-L6-v2") == 384

    def test_import_error_returns_none(self, monkeypatch):
        """If sentence_transformers not installed, _make_embed_client returns None."""
        monkeypatch.delenv("EMBEDDING_PROVIDER", raising=False)
        monkeypatch.delenv("EMBEDDING_MODEL", raising=False)
        monkeypatch.delenv("EMBEDDING_DIM", raising=False)

        with patch(
            "memoria.core.embedding.providers.LocalProvider._load",
            side_effect=ImportError("no module"),
        ):
            from memoria.mcp_local.server import EmbeddedBackend

            result = EmbeddedBackend._make_embed_client()
        assert result is None

    def test_value_error_propagates(self, monkeypatch):
        """Dim mismatch (ValueError) must NOT be silently swallowed."""
        monkeypatch.setenv("EMBEDDING_PROVIDER", "local")
        monkeypatch.setenv("EMBEDDING_MODEL", "all-MiniLM-L6-v2")
        monkeypatch.setenv("EMBEDDING_DIM", "999")  # wrong dim

        with patch(
            "memoria.core.embedding.providers.LocalProvider._load",
            return_value=MagicMock(get_sentence_embedding_dimension=lambda: 384),
        ):
            from memoria.mcp_local.server import EmbeddedBackend

            with pytest.raises(
                ValueError, match="fixed dimension 384.*config says 999"
            ):
                EmbeddedBackend._make_embed_client()
