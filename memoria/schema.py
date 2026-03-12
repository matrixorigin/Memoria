"""Self-contained DDL for Memoria Lite memory tables.

No dependency on core/ or api/ — works in standalone pip-install mode.
The embedding dimension is configurable via ``EMBEDDING_DIM`` env var.
When unset, inferred from ``EMBEDDING_MODEL`` (384 for all-MiniLM-L6-v2,
1024 for BAAI/bge-m3, etc.).  Final fallback: 1024.

Usage::

    from memoria.schema import ensure_tables
    ensure_tables(engine)          # idempotent, skips existing tables
    ensure_tables(engine, dim=768) # override embedding dimension
"""

from __future__ import annotations

import logging
import os
import re
from typing import Any

from sqlalchemy import create_engine as _create_engine, text
from sqlalchemy.engine import Engine

logger = logging.getLogger(__name__)


def _infer_default_dim() -> int:
    """Infer embedding dimension from env vars.

    Priority: EMBEDDING_DIM (explicit) > model name lookup > 1024.
    Duplicates a small subset of KNOWN_DIMENSIONS to stay self-contained
    (no dependency on core/).
    """
    raw = os.environ.get("EMBEDDING_DIM") or ""
    val = int(raw) if raw.strip() else 0
    if val > 0:
        return val
    model = os.environ.get("EMBEDDING_MODEL") or ""
    # Keys use both short and fully-qualified names (users may configure either).
    # Canonical source: core.embedding.client.KNOWN_DIMENSIONS (not imported
    # here — schema.py must stay self-contained with no core/ dependency).
    _MODEL_DIMS = {
        "all-MiniLM-L6-v2": 384,
        "all-MiniLM-L12-v2": 384,
        "sentence-transformers/all-MiniLM-L6-v2": 384,
        "sentence-transformers/all-MiniLM-L12-v2": 384,
        "BAAI/bge-m3": 1024,
        "BAAI/bge-base-en-v1.5": 768,
        "text-embedding-3-small": 1536,
        "text-embedding-ada-002": 1536,
    }
    return _MODEL_DIMS.get(model, 1024)


DEFAULT_DIM = _infer_default_dim()

DEFAULT_DB_URL = "mysql+pymysql://root:111@localhost:6001/memoria"

# Table names in dependency order (no FK between them, but order is stable).
TABLE_NAMES = [
    "mem_branches",
    "mem_edit_log",
    "mem_entities",
    "mem_experiments",
    "mem_memories",
    "mem_memory_entity_links",
    "mem_user_memory_config",
    "mem_user_state",
    "memory_graph_edges",
    "memory_graph_nodes",
]


def _ddl_statements(dim: int) -> list[str]:
    """Return CREATE TABLE IF NOT EXISTS statements for all memory tables."""
    return [
        # ── mem_branches ──────────────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `mem_branches` (
          `branch_id`     VARCHAR(36)  NOT NULL,
          `user_id`       VARCHAR(64)  NOT NULL,
          `name`          VARCHAR(100) NOT NULL,
          `branch_db`     VARCHAR(64)  NOT NULL,
          `base_snapshot` VARCHAR(128) DEFAULT NULL,
          `status`        VARCHAR(20)  NOT NULL DEFAULT 'active',
          `created_at`    DATETIME(6)  NOT NULL DEFAULT NOW(),
          `updated_at`    DATETIME(6)  DEFAULT NULL,
          PRIMARY KEY (`branch_id`),
          KEY `idx_branch_user` (`user_id`),
          KEY `idx_branch_user_status` (`user_id`, `status`),
          UNIQUE KEY `idx_branch_user_name` (`user_id`, `name`)
        )
        """,
        # ── mem_edit_log ──────────────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `mem_edit_log` (
          `edit_id`         VARCHAR(36)  NOT NULL,
          `user_id`         VARCHAR(64)  NOT NULL,
          `operation`       VARCHAR(20)  NOT NULL,
          `target_ids`      JSON         DEFAULT NULL,
          `reason`          TEXT         DEFAULT NULL,
          `snapshot_before` VARCHAR(64)  DEFAULT NULL,
          `created_at`      DATETIME(6)  NOT NULL DEFAULT NOW(),
          `created_by`      VARCHAR(64)  NOT NULL,
          PRIMARY KEY (`edit_id`),
          KEY `idx_edit_operation` (`operation`),
          KEY `idx_edit_user` (`user_id`)
        )
        """,
        # ── mem_entities ──────────────────────────────────────────
        f"""
        CREATE TABLE IF NOT EXISTS `mem_entities` (
          `entity_id`    VARCHAR(32)  NOT NULL,
          `user_id`      VARCHAR(64)  NOT NULL,
          `name`         VARCHAR(200) NOT NULL,
          `display_name` VARCHAR(200) DEFAULT NULL,
          `entity_type`  VARCHAR(20)  NOT NULL DEFAULT 'concept',
          `embedding`    VECF32({dim}) DEFAULT NULL,
          `created_at`   DATETIME(6)  NOT NULL DEFAULT NOW(),
          PRIMARY KEY (`entity_id`),
          UNIQUE KEY `uidx_entity_user_name` (`user_id`, `name`),
          KEY `idx_entity_user` (`user_id`)
        )
        """,
        # ── mem_experiments ───────────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `mem_experiments` (
          `experiment_id` VARCHAR(36)  NOT NULL,
          `user_id`       VARCHAR(64)  NOT NULL,
          `name`          VARCHAR(100) NOT NULL,
          `description`   TEXT         DEFAULT NULL,
          `status`        VARCHAR(20)  NOT NULL DEFAULT 'active',
          `branch_db`     VARCHAR(64)  NOT NULL,
          `base_snapshot` VARCHAR(64)  DEFAULT NULL,
          `strategy_key`  VARCHAR(32)  DEFAULT NULL,
          `params_json`   JSON         DEFAULT NULL,
          `metrics_json`  JSON         DEFAULT NULL,
          `created_at`    DATETIME(6)  NOT NULL DEFAULT NOW(),
          `committed_at`  DATETIME(6)  DEFAULT NULL,
          `expires_at`    DATETIME(6)  DEFAULT NULL,
          `created_by`    VARCHAR(64)  NOT NULL,
          PRIMARY KEY (`experiment_id`),
          KEY `idx_exp_user` (`user_id`),
          KEY `idx_exp_status` (`status`),
          KEY `idx_exp_user_status` (`user_id`, `status`),
          KEY `idx_exp_status_expires` (`status`, `expires_at`)
        )
        """,
        # ── mem_memories ──────────────────────────────────────────
        f"""
        CREATE TABLE IF NOT EXISTS `mem_memories` (
          `memory_id`          VARCHAR(64)  NOT NULL,
          `user_id`            VARCHAR(64)  NOT NULL,
          `session_id`         VARCHAR(64)  DEFAULT NULL,
          `memory_type`        VARCHAR(20)  NOT NULL,
          `content`            TEXT         NOT NULL,
          `initial_confidence` FLOAT        NOT NULL,
          `trust_tier`         VARCHAR(10)  DEFAULT NULL,
          `embedding`          VECF32({dim}) DEFAULT NULL,
          `source_event_ids`   JSON         NOT NULL,
          `superseded_by`      VARCHAR(64)  DEFAULT NULL,
          `is_active`          SMALLINT     NOT NULL DEFAULT 1,
          `observed_at`        DATETIME(6)  NOT NULL,
          `created_at`         DATETIME(6)  NOT NULL,
          `updated_at`         DATETIME(6)  DEFAULT NULL,
          PRIMARY KEY (`memory_id`),
          KEY `idx_memory_user_type_active` (`user_id`, `memory_type`, `is_active`),
          KEY `idx_memory_user_active` (`user_id`, `is_active`),
          KEY `idx_memory_user_session` (`user_id`, `session_id`),
          KEY `idx_memory_observed_at` (`observed_at`),
          KEY `idx_memory_superseded_by` (`superseded_by`),
          KEY `idx_memory_user_active_type_observed` (`user_id`, `is_active`, `memory_type`, `observed_at`),
          FULLTEXT `ft_memory_content`(`content`) WITH PARSER ngram
        )
        """,
        # ── mem_memory_entity_links ────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `mem_memory_entity_links` (
          `memory_id`  VARCHAR(64) NOT NULL,
          `entity_id`  VARCHAR(32) NOT NULL,
          `user_id`    VARCHAR(64) NOT NULL,
          `source`     VARCHAR(10) NOT NULL DEFAULT 'regex',
          `weight`     FLOAT       NOT NULL DEFAULT 0.8,
          `created_at` DATETIME(6) NOT NULL DEFAULT NOW(),
          PRIMARY KEY (`memory_id`, `entity_id`),
          KEY `idx_link_user_entity` (`user_id`, `entity_id`),
          KEY `idx_link_entity_user` (`entity_id`, `user_id`)
        )
        """,
        # ── mem_user_memory_config ────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `mem_user_memory_config` (
          `user_id`            VARCHAR(64)  NOT NULL,
          `strategy_key`       VARCHAR(32)  NOT NULL DEFAULT 'vector:v1',
          `params_json`        JSON         DEFAULT NULL,
          `migrated_from`      VARCHAR(32)  DEFAULT NULL,
          `migration_snapshot` VARCHAR(64)  DEFAULT NULL,
          `index_status`       VARCHAR(20)  NOT NULL DEFAULT 'ready',
          `created_at`         DATETIME(6)  NOT NULL DEFAULT NOW(),
          `updated_at`         DATETIME(6)  DEFAULT NOW(),
          PRIMARY KEY (`user_id`)
        )
        """,
        # ── mem_user_state ────────────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `mem_user_state` (
          `user_id`       VARCHAR(64)  NOT NULL,
          `active_branch` VARCHAR(100) NOT NULL DEFAULT 'main',
          `updated_at`    DATETIME(6)  NOT NULL DEFAULT NOW(),
          PRIMARY KEY (`user_id`)
        )
        """,
        # ── memory_graph_edges ────────────────────────────────────
        """
        CREATE TABLE IF NOT EXISTS `memory_graph_edges` (
          `source_id`  VARCHAR(32)  NOT NULL,
          `target_id`  VARCHAR(32)  NOT NULL,
          `edge_type`  VARCHAR(15)  NOT NULL,
          `weight`     FLOAT        NOT NULL,
          `user_id`    VARCHAR(64)  NOT NULL,
          PRIMARY KEY (`source_id`, `target_id`, `edge_type`),
          KEY `idx_edge_user` (`user_id`),
          KEY `idx_edge_target` (`target_id`)
        )
        """,
        # ── memory_graph_nodes ────────────────────────────────────
        f"""
        CREATE TABLE IF NOT EXISTS `memory_graph_nodes` (
          `node_id`             VARCHAR(32)  NOT NULL,
          `user_id`             VARCHAR(64)  NOT NULL,
          `node_type`           VARCHAR(10)  NOT NULL,
          `content`             TEXT         NOT NULL,
          `entity_type`         VARCHAR(20)  DEFAULT NULL,
          `embedding`           VECF32({dim}) DEFAULT NULL,
          `event_id`            VARCHAR(32)  DEFAULT NULL,
          `memory_id`           VARCHAR(64)  DEFAULT NULL,
          `session_id`          VARCHAR(64)  DEFAULT NULL,
          `confidence`          FLOAT        DEFAULT NULL,
          `trust_tier`          VARCHAR(4)   DEFAULT NULL,
          `importance`          FLOAT        NOT NULL,
          `source_nodes`        TEXT         DEFAULT NULL,
          `conflicts_with`      VARCHAR(32)  DEFAULT NULL,
          `conflict_resolution` VARCHAR(10)  DEFAULT NULL,
          `access_count`        INT          DEFAULT NULL,
          `cross_session_count` INT          DEFAULT NULL,
          `is_active`           SMALLINT     NOT NULL DEFAULT 1,
          `superseded_by`       VARCHAR(32)  DEFAULT NULL,
          `created_at`          DATETIME(6)  NOT NULL,
          PRIMARY KEY (`node_id`),
          KEY `idx_graph_event` (`event_id`),
          KEY `idx_graph_memory` (`memory_id`),
          KEY `idx_graph_conflicts` (`user_id`, `conflicts_with`),
          KEY `idx_graph_user_active` (`user_id`, `is_active`, `node_type`),
          FULLTEXT `ft_graph_content`(`content`) WITH PARSER ngram
        )
        """,
    ]


def ensure_database(engine: Engine) -> None:
    """Create the target database if it doesn't exist.

    Connects without a database to execute CREATE DATABASE IF NOT EXISTS,
    then verifies the original engine can connect.
    """
    url = engine.url
    db_name = url.database
    if not db_name:
        return
    # Connect without specifying a database.
    root_url = url.set(database="")
    root_engine = _create_engine(root_url, pool_pre_ping=True)
    with root_engine.connect() as conn:
        conn.execute(text(f"CREATE DATABASE IF NOT EXISTS `{db_name}`"))
        conn.commit()
    root_engine.dispose()


_VECF32_RE = re.compile(r"vecf32\((\d+)\)", re.IGNORECASE)


def _fix_embedding_dim(conn: Any, dim: int, *, force: bool = False) -> None:
    """Warn (or ALTER) if embedding column dim doesn't match target dim.

    By default only warns — ALTER is destructive when existing vector data is present.
    Pass force=True (via ``memoria migrate --force``) to actually ALTER.
    """
    for table in ("mem_memories", "memory_graph_nodes"):
        row = conn.execute(
            text(f"SHOW COLUMNS FROM `{table}` LIKE 'embedding'")
        ).fetchone()
        if row is None:
            continue
        col_type: str = row[1]  # e.g. "vecf32(1024)"
        m = _VECF32_RE.search(col_type)
        if not m:
            continue  # unrecognised column type — skip silently
        current_dim = int(m.group(1))
        if current_dim == dim:
            continue
        if force:
            logger.info(
                "Altering %s.embedding from %s to vecf32(%d)", table, col_type, dim
            )
            conn.execute(
                text(
                    f"ALTER TABLE `{table}` MODIFY COLUMN `embedding` VECF32({dim}) DEFAULT NULL"
                )
            )
        else:
            logger.warning(
                "Embedding dim mismatch: %s.embedding is %s but EMBEDDING_DIM=%d. "
                "Existing vector data will not be re-embedded automatically. "
                "Run `memoria migrate --dim %d --force` to ALTER the column "
                "(existing embeddings will be cleared).",
                table,
                col_type,
                dim,
                dim,
            )


def _ensure_entity_type_column(conn: Any) -> None:
    """Add entity_type column to memory_graph_nodes if missing (v0.2.8 migration)."""
    row = conn.execute(
        text("SHOW COLUMNS FROM `memory_graph_nodes` LIKE 'entity_type'")
    ).fetchone()
    if row is None:
        logger.info("Adding entity_type column to memory_graph_nodes")
        conn.execute(
            text(
                "ALTER TABLE `memory_graph_nodes` ADD COLUMN `entity_type` VARCHAR(20) DEFAULT NULL AFTER `content`"
            )
        )


def ensure_tables(
    engine: Engine, *, dim: int | None = None, force: bool = False
) -> list[str]:
    """Create database and memory tables if they don't exist.

    Idempotent — uses CREATE DATABASE/TABLE IF NOT EXISTS.
    If tables already exist but embedding dim differs, logs a warning.
    Pass force=True to ALTER the column (existing embeddings will be cleared).

    Args:
        engine: SQLAlchemy engine connected to the target database.
        dim: Embedding vector dimension (default: EMBEDDING_DIM env or 1024).
        force: If True, ALTER embedding column when dim mismatches (destructive).

    Returns:
        List of table names that were processed (all 8 tables).
    """
    ensure_database(engine)
    dim = dim or DEFAULT_DIM
    created: list[str] = []
    stmts = _ddl_statements(dim)
    with engine.connect() as conn:
        for name, ddl in zip(TABLE_NAMES, stmts):
            conn.execute(text(ddl))
            created.append(name)
        _fix_embedding_dim(conn, dim, force=force)
        _ensure_entity_type_column(conn)
        conn.commit()
    return created
