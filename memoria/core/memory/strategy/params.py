"""Strategy parameter schemas — Pydantic validation for tunable params.

Each retrieval strategy defines a schema for its tunable parameters.
Params are validated at write time (experiment create, user config update).

See docs/design/memory/backend-management.md §8.5
"""

from __future__ import annotations

from pydantic import BaseModel, Field


class VectorV1Params(BaseModel):
    """Tunable params for vector:v1 retrieval strategy."""

    semantic_weight: float = Field(0.4, ge=0.0, le=1.0)
    temporal_weight: float = Field(0.3, ge=0.0, le=1.0)
    confidence_weight: float = Field(0.2, ge=0.0, le=1.0)
    importance_weight: float = Field(0.1, ge=0.0, le=1.0)


class ActivationV1Params(BaseModel):
    """Tunable params for activation:v1 retrieval strategy."""

    spreading_factor: float = Field(0.8, ge=0.0, le=1.0)
    num_iterations: int = Field(3, ge=1, le=10)
    inhibition_beta: float = Field(0.15, ge=0.0, le=1.0)
    sigmoid_theta: float = Field(0.1, ge=0.0, le=1.0)
    min_graph_nodes: int = Field(10, ge=1)


# Strategy key → params schema
STRATEGY_PARAMS_SCHEMA: dict[str, type[BaseModel]] = {
    "vector:v1": VectorV1Params,
    "activation:v1": ActivationV1Params,
}


class InvalidStrategyParamsError(ValueError):
    """Raised when strategy params fail validation."""


def validate_strategy_params(
    strategy_key: str,
    params: dict | None,
) -> dict | None:
    """Validate params against the strategy's schema.

    Args:
        strategy_key: Strategy key like 'vector:v1'.
        params: Param dict to validate (None is always valid).

    Returns:
        Validated params dict (with defaults filled in), or None.

    Raises:
        InvalidStrategyParamsError: If params fail validation.
    """
    if params is None:
        return None
    schema = STRATEGY_PARAMS_SCHEMA.get(strategy_key)
    if schema is None:
        # Unknown strategy — pass through without validation
        return params
    try:
        validated = schema(**params)
        return validated.model_dump()
    except Exception as e:
        raise InvalidStrategyParamsError(
            f"Invalid params for {strategy_key}: {e}"
        ) from e


def get_default_params(strategy_key: str) -> dict | None:
    """Get default params for a strategy, or None if unknown."""
    schema = STRATEGY_PARAMS_SCHEMA.get(strategy_key)
    if schema is None:
        return None
    return schema().model_dump()
