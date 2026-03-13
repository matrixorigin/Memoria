"""Tests for per-user activation param overrides in factory."""

from unittest.mock import MagicMock, patch

from memoria.core.memory.config import DEFAULT_CONFIG, MemoryGovernanceConfig


class TestPerUserParamMerge:
    """_resolve_strategy returns per-user params; create_memory_service merges them."""

    def _make_db_factory(self, strategy_key: str, params_json: dict | None = None):
        """Return a db_factory mock that returns the given strategy + params."""
        row = MagicMock()
        row.strategy_key = strategy_key
        row.index_status = "ready"
        row.params_json = params_json

        db = MagicMock()
        db.execute.return_value.fetchone.return_value = row
        db.__enter__ = lambda s: db
        db.__exit__ = MagicMock(return_value=False)

        factory = MagicMock(return_value=db)
        return factory

    def test_no_params_uses_default_config(self):
        from memoria.core.memory.factory import _lookup_user_strategy

        factory = self._make_db_factory("activation:v1", params_json=None)
        key, params = _lookup_user_strategy(factory, "user1")
        assert key == "activation:v1"
        assert params is None

    def test_params_json_returned(self):
        from memoria.core.memory.factory import _lookup_user_strategy

        overrides = {"activation_lambda_activation": 0.5}
        factory = self._make_db_factory("activation:v1", params_json=overrides)
        key, params = _lookup_user_strategy(factory, "user1")
        assert key == "activation:v1"
        assert params == overrides

    def test_config_merged_with_user_params(self):
        """create_memory_service applies user params on top of DEFAULT_CONFIG."""
        from memoria.core.memory.factory import create_memory_service

        overrides = {
            "activation_lambda_activation": 0.5,
            "activation_association_threshold": 0.6,
        }
        factory = self._make_db_factory("vector:v1", params_json=overrides)

        captured_config: list[MemoryGovernanceConfig] = []

        def fake_canonical_storage(db_factory, **kwargs):
            captured_config.append(kwargs.get("config", DEFAULT_CONFIG))
            return MagicMock()

        with (
            patch(
                "memoria.core.memory.factory.CanonicalStorage",
                side_effect=fake_canonical_storage,
            ),
            patch("memoria.core.memory.factory._registry") as mock_reg,
        ):
            mock_reg.create_strategy.return_value = MagicMock()
            mock_reg.create_index_manager.return_value = None
            create_memory_service(factory, user_id="user1")

        assert len(captured_config) == 1
        cfg = captured_config[0]
        assert cfg.activation_lambda_activation == 0.5
        assert cfg.activation_association_threshold == 0.6
        # Other fields unchanged
        assert (
            cfg.activation_lambda_semantic == DEFAULT_CONFIG.activation_lambda_semantic
        )

    def test_invalid_param_key_ignored(self):
        """Unknown keys in params_json are silently ignored."""
        from memoria.core.memory.factory import create_memory_service

        overrides = {"nonexistent_param": 999, "activation_lambda_activation": 0.45}
        factory = self._make_db_factory("vector:v1", params_json=overrides)

        captured_config: list[MemoryGovernanceConfig] = []

        def fake_canonical_storage(db_factory, **kwargs):
            captured_config.append(kwargs.get("config", DEFAULT_CONFIG))
            return MagicMock()

        with (
            patch(
                "memoria.core.memory.factory.CanonicalStorage",
                side_effect=fake_canonical_storage,
            ),
            patch("memoria.core.memory.factory._registry") as mock_reg,
        ):
            mock_reg.create_strategy.return_value = MagicMock()
            mock_reg.create_index_manager.return_value = None
            create_memory_service(factory, user_id="user1")

        cfg = captured_config[0]
        assert cfg.activation_lambda_activation == 0.45
        assert not hasattr(cfg, "nonexistent_param")

    def test_db_error_returns_none(self):
        from memoria.core.memory.factory import _lookup_user_strategy

        factory = MagicMock(side_effect=Exception("db down"))
        key, params = _lookup_user_strategy(factory, "user1")
        assert key is None
        assert params is None


class TestActivationConfigDefaults:
    """Verify the new default values are correct."""

    def test_association_threshold_default(self):
        assert DEFAULT_CONFIG.activation_association_threshold == 0.55

    def test_entity_link_multiplier_default(self):
        assert DEFAULT_CONFIG.activation_entity_link_multiplier == 1.8

    def test_lambda_sum_is_one(self):
        cfg = DEFAULT_CONFIG
        total = (
            cfg.activation_lambda_semantic
            + cfg.activation_lambda_activation
            + cfg.activation_lambda_confidence
            + cfg.activation_lambda_importance
        )
        assert abs(total - 1.0) < 1e-9
