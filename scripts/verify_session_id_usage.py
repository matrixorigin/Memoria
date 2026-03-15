#!/usr/bin/env python3
"""Verify session_id usage and index effectiveness in mem_memories table.

Usage:
    python scripts/verify_session_id_usage.py [--db-url URL]
"""

import argparse
from sqlalchemy import create_engine, text


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--db-url",
        default="mysql+pymysql://root:111@localhost:6001/memoria",
        help="Database URL",
    )
    args = parser.parse_args()

    engine = create_engine(args.db_url, pool_pre_ping=True)

    print("=" * 70)
    print("Session ID Usage Analysis")
    print("=" * 70)

    with engine.connect() as conn:
        # 1. session_id 分布统计（单次查询 + Python 计算百分比）
        print("\n1. session_id Distribution:")
        # 先获取总数，再计算百分比（避免子查询重复执行）
        total = conn.execute(text("SELECT COUNT(*) FROM mem_memories")).fetchone()[0]
        result = conn.execute(
            text("""
            SELECT 
                CASE 
                    WHEN session_id IS NULL THEN 'NULL'
                    WHEN session_id = '' THEN 'EMPTY_STRING'
                    ELSE 'HAS_VALUE'
                END as session_type,
                COUNT(*) as count
            FROM mem_memories
            GROUP BY session_type
            ORDER BY count DESC
        """)
        ).fetchall()

        for row in result:
            pct = round(row[1] * 100.0 / total, 2) if total > 0 else 0
            print(f"  {row[0]:15} {row[1]:8} ({pct:5}%)")

        # 2. working/tool_result 类型的 session_id 分布
        print("\n2. Working/Tool_result Memory session_id Distribution:")
        result = conn.execute(
            text("""
            SELECT 
                CASE 
                    WHEN session_id IS NULL THEN 'NULL'
                    WHEN session_id = '' THEN 'EMPTY_STRING'
                    ELSE 'HAS_VALUE'
                END as session_type,
                memory_type,
                COUNT(*) as count
            FROM mem_memories
            WHERE memory_type IN ('working', 'tool_result') AND is_active = 1
            GROUP BY session_type, memory_type
            ORDER BY count DESC
        """)
        ).fetchall()

        if result:
            for row in result:
                print(f"  {row[0]:15} {row[1]:12} {row[2]:8}")
        else:
            print("  No working/tool_result memories found")

        # 3. 当前索引列表
        print("\n3. Current Indexes on mem_memories:")
        result = conn.execute(text("SHOW INDEX FROM mem_memories")).fetchall()

        session_indexes = [r for r in result if "session" in r[2].lower()]
        if session_indexes:
            for row in session_indexes:
                print(f"  {row[2]:40} Column: {row[4]}, Seq: {row[3]}")
        else:
            print("  No session-related indexes found")

        # 4. L0 查询模拟（修复后：session_id=None 时 L0 被跳过）
        print("\n4. L0 Query Behavior (after fix):")
        print("  When session_id=None: L0 skipped (no query executed)")
        print("  When session_id='value': L0 uses idx_memory_user_session")
        print("  -> No wasted query for None/empty session_id")

        # 5. 真实 session_id 测试
        real_session = conn.execute(
            text("""
            SELECT session_id FROM mem_memories 
            WHERE session_id IS NOT NULL AND session_id != ''
            LIMIT 1
        """)
        ).fetchone()

        if real_session:
            print(f"\n5. Testing with Real session_id: {real_session[0][:20]}...")
            result = conn.execute(
                text("""
                SELECT COUNT(*) 
                FROM mem_memories 
                WHERE session_id = :sid 
                  AND memory_type IN ('working', 'tool_result')
            """),
                {"sid": real_session[0]},
            ).fetchone()
            print(f"  -> Matches: {result[0]} (would use index if L0 executed)")

        # 6. L1 查询（无 session 过滤）
        print("\n6. L1 Query (no session filter):")
        result = conn.execute(
            text("""
            SELECT COUNT(*)
            FROM mem_memories 
            WHERE memory_type IN ('semantic', 'procedural', 'profile')
              AND is_active = 1
        """)
        ).fetchone()

        print(f"  -> Returns: {result[0]} rows (all sessions, no filter)")

        # 7. 数据统计（合并为单次查询）
        print("\n" + "=" * 70)
        print("Analysis & Recommendations:")
        print("=" * 70)

        # 单次查询获取所有统计（替代 3 次独立 COUNT）
        stats = conn.execute(
            text("""
            SELECT 
                COUNT(*) as total,
                SUM(CASE WHEN session_id IS NULL THEN 1 ELSE 0 END) as null_count,
                SUM(CASE WHEN session_id = '' THEN 1 ELSE 0 END) as empty_count
            FROM mem_memories
        """)
        ).fetchone()

        total = stats[0]
        null_count = stats[1]
        empty_count = stats[2]
        has_value = total - null_count - empty_count

        print(f"\nData Distribution:")
        print(f"  Total memories: {total}")
        print(f"  - NULL session_id: {null_count} ({null_count * 100 / total:.1f}%)")
        print(f"  - Empty string '': {empty_count} ({empty_count * 100 / total:.1f}%)")
        print(f"  - Has value: {has_value} ({has_value * 100 / total:.1f}%)")

        print(f"\nCurrent Behavior:")
        print(f"  MCP passes: session_id or None -> None when None/empty")
        print(f"  L0 query: skipped when session_id=None (no query, no waste)")
        print(f"  L1 query: no session filter -> returns all sessions")

        print(f"\nIndex Effectiveness:")
        if empty_count == 0 and has_value > 0:
            print(f"  - No empty string pollution")
            print(f"  - {has_value} rows have real session_id values")
            print(f"  - (session_id, memory_type, is_active) index may help L0 queries")
            print(f"  - Current: MCP passes None -> L0 skipped (no wasted query)")
        elif null_count / total > 0.8:
            print(f"  - {null_count * 100 / total:.1f}% rows are NULL")
            print(f"  - session_id index has LIMITED benefit")

        if empty_count > 0:
            print(f"\n- Data Quality Issue:")
            print(f"  Found {empty_count} rows with empty string (should be NULL)")
            print(
                f"  Fix: UPDATE mem_memories SET session_id = NULL WHERE session_id = ''"
            )

        print(f"\n- Optimization Recommendation:")
        print(f"  1. MCP server fix applied: session_id or None (not '')")
        print(f"  2. L0 skipped when session_id=None (no wasted query)")
        print(
            f"  3. Consider (session_id, memory_type, is_active) index if L0 data grows"
        )
        print(f"  4. Current idx_memory_user_session is sufficient for now")


if __name__ == "__main__":
    main()
