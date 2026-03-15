"""Memoria configuration."""

from pydantic import Field, model_validator
from pydantic_settings import BaseSettings, SettingsConfigDict


class MemoriaSettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_prefix="MEMORIA_",
        env_file=".env",
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
    )

    # Database
    db_host: str = "localhost"
    db_port: int = 6001
    db_user: str = "root"
    db_password: str = "111"
    db_name: str = "memoria"

    # Embedding — default "mock" for zero-config startup; set to "openai" or "local" in production
    embedding_provider: str = Field(
        default="mock",
        alias="EMBEDDING_PROVIDER",
        validation_alias="EMBEDDING_PROVIDER",
    )
    embedding_model: str = Field(
        default="BAAI/bge-m3",
        alias="EMBEDDING_MODEL",
        validation_alias="EMBEDDING_MODEL",
    )
    embedding_dim: int = Field(
        default=0,
        alias="EMBEDDING_DIM",
        validation_alias="EMBEDDING_DIM",
        description="0 = auto-infer",
    )
    embedding_api_key: str = Field(
        default="", alias="EMBEDDING_API_KEY", validation_alias="EMBEDDING_API_KEY"
    )
    embedding_base_url: str | None = Field(
        default=None, alias="EMBEDDING_BASE_URL", validation_alias="EMBEDDING_BASE_URL"
    )

    @model_validator(mode="after")
    def infer_embedding_dim(self) -> "MemoriaSettings":
        if self.embedding_dim == 0 and self.embedding_provider != "mock":
            from memoria.core.embedding.client import KNOWN_DIMENSIONS

            inferred = KNOWN_DIMENSIONS.get(self.embedding_model)
            if inferred is None:
                raise ValueError(
                    f"embedding_model {self.embedding_model!r} not in KNOWN_DIMENSIONS "
                    f"and EMBEDDING_DIM=0; set EMBEDDING_DIM explicitly"
                )
            self.embedding_dim = inferred
        return self

    # Auth
    master_key: str = Field(
        default="",
        description="Master API key for admin operations (min 16 chars in production)",
    )
    api_key_secret: str = Field(
        default="",
        description="Dedicated HMAC secret for API key hashing. "
        "If empty, falls back to master_key for backward compatibility.",
    )

    # LLM (optional — for reflect + entity extraction)
    llm_api_key: str = ""
    llm_base_url: str | None = None
    llm_model: str = "gpt-4o-mini"

    # Remote auth service (apikey mode)
    remote_auth_service_url: str = Field(
        default="",
        description="Base URL of the remote auth service for apikey mode. "
        "When set, API accepts X-API-Key header and resolves per-user DB connections.",
    )
    conn_cache_ttl: int = Field(
        default=60,
        description="TTL in seconds for caching remote auth service responses. 0 = no cache.",
    )

    # Limits
    snapshot_limit: int = Field(default=100, description="Max snapshots per user")

    @property
    def db_url(self) -> str:
        return (
            f"mysql+pymysql://{self.db_user}:{self.db_password}"
            f"@{self.db_host}:{self.db_port}/{self.db_name}"
            "?charset=utf8mb4"
        )

    def warn_weak_master_key(self) -> list[str]:
        """Return warning messages about auth configuration issues."""
        warnings: list[str] = []
        if self.master_key and len(self.master_key) < 16:
            warnings.append(
                f"MEMORIA_MASTER_KEY is only {len(self.master_key)} chars — use ≥16 chars in production"
            )
        if self.master_key and not self.api_key_secret:
            warnings.append(
                "MEMORIA_API_KEY_SECRET is not set — API key hashing falls back to MASTER_KEY. "
                "Set API_KEY_SECRET so you can rotate MASTER_KEY without invalidating existing keys."
            )
        return warnings


_settings: MemoriaSettings | None = None


def get_settings() -> MemoriaSettings:
    global _settings
    if _settings is None:
        _settings = MemoriaSettings()
    return _settings
