#!/usr/bin/env python3
"""Backfill mem_entities + mem_memory_entity_links from existing graph data.

One-time migration: populates the Phase 0A entity tables from pre-existing
entity graph nodes and entity_link edges.

Usage:
    python scripts/backfill_entity_tables.py --db-url "mysql+pymysql://root:111@localhost:6001/memoria"
    python scripts/backfill_entity_tables.py --db-url "..." --user alice   # single user
    python scripts/backfill_entity_tables.py --db-url "..." --dry-run      # preview only
"""

from __future__ import annotations

import argparse

from sqlalchemy import create_engine, text
from sqlalchemy.orm import Session, sessionmaker


def backfill(
    session_factory: sessionmaker, *, user_id: str | None = None, dry_run: bool = False
) -> dict:
    """Backfill entity tables from graph nodes + edges.

    Returns counts of rows created.
    """
    db: Session = session_factory()
    try:
        # 1. Find all entity graph nodes
        user_filter = "AND n.user_id = :uid" if user_id else ""
        params: dict = {"uid": user_id} if user_id else {}

        entity_nodes = db.execute(
            text(
                f"SELECT n.node_id, n.user_id, n.content, n.entity_type "
                f"FROM memory_graph_nodes n "
                f"WHERE n.node_type = 'entity' AND n.is_active = 1 {user_filter}"
            ),
            params,
        ).fetchall()

        # 2. Upsert into mem_entities
        entities_created = 0
        for node_id, uid, content, etype in entity_nodes:
            existing = db.execute(
                text("SELECT entity_id FROM mem_entities WHERE entity_id = :eid"),
                {"eid": node_id},
            ).first()
            if not existing:
                if not dry_run:
                    db.execute(
                        text(
                            "INSERT INTO mem_entities (entity_id, user_id, name, display_name, entity_type) "
                            "VALUES (:eid, :uid, :name, :dname, :etype)"
                        ),
                        {
                            "eid": node_id,
                            "uid": uid,
                            "name": content,
                            "dname": content,
                            "etype": etype or "concept",
                        },
                    )
                entities_created += 1

        # 3. Find entity_link edges → backfill mem_memory_entity_links
        #    edge: source_id (content node) → target_id (entity node)
        #    content node has memory_id in memory_graph_nodes
        links_created = 0
        edges = db.execute(
            text(
                f"SELECT e.source_id, e.target_id, e.weight, e.user_id "
                f"FROM memory_graph_edges e "
                f"WHERE e.edge_type = 'entity_link' {user_filter.replace('n.', 'e.')}"
            ),
            params,
        ).fetchall()

        for source_id, target_id, weight, uid in edges:
            # Resolve source node → memory_id
            row = db.execute(
                text(
                    "SELECT memory_id FROM memory_graph_nodes WHERE node_id = :nid AND memory_id IS NOT NULL"
                ),
                {"nid": source_id},
            ).first()
            if not row or not row[0]:
                continue
            memory_id = row[0]

            # Check entity exists in table
            ent = db.execute(
                text("SELECT entity_id FROM mem_entities WHERE entity_id = :eid"),
                {"eid": target_id},
            ).first()
            if not ent:
                continue

            if not dry_run:
                db.execute(
                    text(
                        "INSERT INTO mem_memory_entity_links (memory_id, entity_id, user_id, source, weight) "
                        "VALUES (:mid, :eid, :uid, :src, :w) "
                        "ON DUPLICATE KEY UPDATE weight = VALUES(weight)"
                    ),
                    {
                        "mid": memory_id,
                        "eid": target_id,
                        "uid": uid,
                        "src": "backfill",
                        "w": weight,
                    },
                )
            links_created += 1

        if not dry_run:
            db.commit()

        return {
            "entities_created": entities_created,
            "links_created": links_created,
            "dry_run": dry_run,
        }
    finally:
        db.close()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Backfill entity tables from graph data"
    )
    parser.add_argument("--db-url", required=True, help="SQLAlchemy DB URL")
    parser.add_argument("--user", default=None, help="Backfill single user only")
    parser.add_argument(
        "--dry-run", action="store_true", help="Preview without writing"
    )
    args = parser.parse_args()

    engine = create_engine(args.db_url)
    factory = sessionmaker(bind=engine)

    result = backfill(factory, user_id=args.user, dry_run=args.dry_run)
    prefix = "[DRY RUN] " if result["dry_run"] else ""
    print(f"{prefix}Entities created: {result['entities_created']}")
    print(f"{prefix}Links created: {result['links_created']}")


if __name__ == "__main__":
    main()
